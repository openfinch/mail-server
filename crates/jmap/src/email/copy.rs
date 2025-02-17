/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of Stalwart Mail Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use jmap_proto::{
    error::{method::MethodError, set::SetError},
    method::{
        copy::{CopyRequest, CopyResponse, RequestArguments},
        set::{self, SetRequest},
    },
    object::Object,
    request::{
        method::{MethodFunction, MethodName, MethodObject},
        reference::MaybeReference,
        Call, RequestMethod,
    },
    response::references::EvalObjectReferences,
    types::{
        acl::Acl,
        blob::BlobId,
        collection::Collection,
        date::UTCDate,
        id::Id,
        keyword::Keyword,
        property::Property,
        state::{State, StateChange},
        type_state::TypeState,
        value::{MaybePatchValue, Value},
    },
};
use mail_parser::parsers::fields::thread::thread_name;
use store::{
    fts::term_index::TokenIndex,
    query::RawValue,
    write::{BatchBuilder, F_BITMAP, F_VALUE},
    BlobKind,
};
use utils::map::vec_map::VecMap;

use crate::{auth::AccessToken, JMAP};

use super::{
    index::{EmailIndexBuilder, TrimTextValue, MAX_SORT_FIELD_LENGTH},
    ingest::IngestedEmail,
};

impl JMAP {
    pub async fn email_copy(
        &self,
        request: CopyRequest<RequestArguments>,
        access_token: &AccessToken,
        next_call: &mut Option<Call<RequestMethod>>,
    ) -> Result<CopyResponse, MethodError> {
        let account_id = request.account_id.document_id();
        let from_account_id = request.from_account_id.document_id();

        if account_id == from_account_id {
            return Err(MethodError::InvalidArguments(
                "From accountId is equal to fromAccountId".to_string(),
            ));
        }
        let old_state = self
            .assert_state(account_id, Collection::Email, &request.if_in_state)
            .await?;
        let mut response = CopyResponse {
            from_account_id: request.from_account_id,
            account_id: request.account_id,
            new_state: old_state.clone(),
            old_state,
            created: VecMap::with_capacity(request.create.len()),
            not_created: VecMap::new(),
            state_change: None,
        };

        let from_message_ids = self
            .owned_or_shared_messages(access_token, from_account_id, Acl::ReadItems)
            .await?;
        let mailbox_ids = self.mailbox_get_or_create(account_id).await?;
        let can_add_mailbox_ids = if access_token.is_shared(account_id) {
            self.shared_documents(access_token, account_id, Collection::Mailbox, Acl::AddItems)
                .await?
                .into()
        } else {
            None
        };
        let on_success_delete = request.on_success_destroy_original.unwrap_or(false);
        let mut destroy_ids = Vec::new();

        // Obtain quota
        let account_quota = self.get_quota(access_token, account_id).await?;

        'create: for (id, create) in request.create {
            let id = id.unwrap();
            let from_message_id = id.document_id();
            if !from_message_ids.contains(from_message_id) {
                response.not_created.append(
                    id,
                    SetError::not_found().with_description(format!(
                        "Item {} not found not found in account {}.",
                        id, response.from_account_id
                    )),
                );
                continue;
            }

            let mut mailboxes = Vec::new();
            let mut keywords = Vec::new();
            let mut received_at = None;

            for (property, value) in create.properties {
                let value = match response.eval_object_references(value) {
                    Ok(value) => value,
                    Err(err) => {
                        response.not_created.append(id, err);
                        continue 'create;
                    }
                };

                match (property, value) {
                    (Property::MailboxIds, MaybePatchValue::Value(Value::List(ids))) => {
                        mailboxes = ids
                            .into_iter()
                            .map(|id| id.unwrap_id().document_id())
                            .collect();
                    }

                    (Property::MailboxIds, MaybePatchValue::Patch(patch)) => {
                        let mut patch = patch.into_iter();
                        let document_id = patch.next().unwrap().unwrap_id().document_id();
                        if patch.next().unwrap().unwrap_bool() {
                            if !mailboxes.contains(&document_id) {
                                mailboxes.push(document_id);
                            }
                        } else {
                            mailboxes.retain(|id| id != &document_id);
                        }
                    }

                    (Property::Keywords, MaybePatchValue::Value(Value::List(keywords_))) => {
                        keywords = keywords_
                            .into_iter()
                            .map(|keyword| keyword.unwrap_keyword())
                            .collect();
                    }

                    (Property::Keywords, MaybePatchValue::Patch(patch)) => {
                        let mut patch = patch.into_iter();
                        let keyword = patch.next().unwrap().unwrap_keyword();
                        if patch.next().unwrap().unwrap_bool() {
                            if !keywords.contains(&keyword) {
                                keywords.push(keyword);
                            }
                        } else {
                            keywords.retain(|k| k != &keyword);
                        }
                    }
                    (Property::ReceivedAt, MaybePatchValue::Value(Value::Date(value))) => {
                        received_at = value.into();
                    }
                    (property, _) => {
                        response.not_created.append(
                            id,
                            SetError::invalid_properties()
                                .with_property(property)
                                .with_description("Invalid property or value.".to_string()),
                        );
                        continue 'create;
                    }
                }
            }

            // Make sure message belongs to at least one mailbox
            if mailboxes.is_empty() {
                response.not_created.append(
                    id,
                    SetError::invalid_properties()
                        .with_property(Property::MailboxIds)
                        .with_description("Message has to belong to at least one mailbox."),
                );
                continue 'create;
            }

            // Verify that the mailboxIds are valid
            for mailbox_id in &mailboxes {
                if !mailbox_ids.contains(*mailbox_id) {
                    response.not_created.append(
                        id,
                        SetError::invalid_properties()
                            .with_property(Property::MailboxIds)
                            .with_description(format!("mailboxId {mailbox_id} does not exist.")),
                    );
                    continue 'create;
                } else if matches!(&can_add_mailbox_ids, Some(ids) if !ids.contains(*mailbox_id)) {
                    response.not_created.append(
                        id,
                        SetError::forbidden().with_description(format!(
                            "You are not allowed to add messages to mailbox {mailbox_id}."
                        )),
                    );
                    continue 'create;
                }
            }

            // Add response
            match self
                .copy_message(
                    from_account_id,
                    from_message_id,
                    account_id,
                    account_quota,
                    mailboxes,
                    keywords,
                    received_at,
                )
                .await?
            {
                Ok(email) => {
                    response.created.append(id, email.into());
                }
                Err(err) => {
                    response.not_created.append(id, err);
                }
            }

            // Add to destroy list
            if on_success_delete {
                destroy_ids.push(id);
            }
        }

        // Update state
        if !response.created.is_empty() {
            response.new_state = self.get_state(account_id, Collection::Email).await?;
            if let State::Exact(change_id) = &response.new_state {
                response.state_change = StateChange::new(account_id)
                    .with_change(TypeState::Email, *change_id)
                    .with_change(TypeState::Mailbox, *change_id)
                    .with_change(TypeState::Thread, *change_id)
                    .into()
            }
        }

        // Destroy ids
        if on_success_delete && !destroy_ids.is_empty() {
            *next_call = Call {
                id: String::new(),
                name: MethodName::new(MethodObject::Email, MethodFunction::Set),
                method: RequestMethod::Set(SetRequest {
                    account_id: request.from_account_id,
                    if_in_state: request.destroy_from_if_in_state,
                    create: None,
                    update: None,
                    destroy: MaybeReference::Value(destroy_ids).into(),
                    arguments: set::RequestArguments::Email,
                }),
            }
            .into();
        }

        Ok(response)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn copy_message(
        &self,
        from_account_id: u32,
        from_message_id: u32,
        account_id: u32,
        account_quota: i64,
        mailboxes: Vec<u32>,
        keywords: Vec<Keyword>,
        received_at: Option<UTCDate>,
    ) -> Result<Result<IngestedEmail, SetError>, MethodError> {
        // Obtain term index and metadata
        let (mut metadata, token_index) = if let (Some(metadata), Some(token_index)) = (
            self.get_property::<Object<Value>>(
                from_account_id,
                Collection::Email,
                from_message_id,
                Property::BodyStructure,
            )
            .await?,
            self.get_term_index::<RawValue<TokenIndex>>(
                from_account_id,
                Collection::Email,
                from_message_id,
            )
            .await?,
        ) {
            (metadata, token_index)
        } else {
            return Ok(Err(SetError::not_found().with_description(format!(
                "Message not found not found in account {}.",
                Id::from(from_account_id)
            ))));
        };

        // Check quota
        if account_quota > 0
            && metadata.get(&Property::Size).as_uint().unwrap_or_default() as i64
                + self.get_used_quota(account_id).await?
                > account_quota
        {
            return Ok(Err(SetError::over_quota()));
        }

        // Set receivedAt
        if let Some(received_at) = received_at {
            metadata.set(Property::ReceivedAt, Value::Date(received_at));
        }

        // Obtain threadId
        let mut references = vec![];
        let mut subject = "";
        for (property, value) in &metadata.properties {
            match property {
                Property::MessageId
                | Property::InReplyTo
                | Property::References
                | Property::EmailIds => match value {
                    Value::Text(text) => {
                        references.push(text.as_str());
                    }
                    Value::List(list) => {
                        references.extend(list.iter().filter_map(|v| v.as_string()));
                    }
                    _ => (),
                },
                Property::Subject => {
                    if let Some(value) = value.as_string() {
                        subject = thread_name(value).trim_text(MAX_SORT_FIELD_LENGTH);
                    }
                }
                _ => (),
            }
        }
        let thread_id = if !references.is_empty() {
            self.find_or_merge_thread(account_id, subject, &references)
                .await
                .map_err(|_| MethodError::ServerPartialFail)?
        } else {
            None
        };

        // Copy blob
        let message_id = self
            .assign_document_id(account_id, Collection::Email)
            .await?;
        let mut email = IngestedEmail {
            blob_id: BlobId::new(BlobKind::LinkedMaildir {
                account_id,
                document_id: message_id,
            }),
            size: metadata.get(&Property::Size).as_uint().unwrap_or(0) as usize,
            ..Default::default()
        };
        self.store
            .copy_blob(
                &BlobKind::LinkedMaildir {
                    account_id: from_account_id,
                    document_id: from_message_id,
                },
                &email.blob_id.kind,
                None,
            )
            .await
            .map_err(|err| {
                tracing::error!(
                    event = "error",
                    context = "email_copy",
                    from_account_id = from_account_id,
                    from_message_id = from_message_id,
                    account_id = account_id,
                    message_id = message_id,
                    error = ?err,
                    "Failed to copy blob.");
                MethodError::ServerPartialFail
            })?;

        // Prepare batch
        let mut batch = BatchBuilder::new();
        batch.with_account_id(account_id);

        // Build change log
        let mut changes = self.begin_changes(account_id).await?;
        let thread_id = if let Some(thread_id) = thread_id {
            changes.log_child_update(Collection::Thread, thread_id);
            thread_id
        } else {
            let thread_id = self
                .assign_document_id(account_id, Collection::Thread)
                .await?;
            batch
                .with_collection(Collection::Thread)
                .create_document(thread_id);
            changes.log_insert(Collection::Thread, thread_id);
            thread_id
        };
        email.id = Id::from_parts(thread_id, message_id);
        email.change_id = changes.change_id;
        changes.log_insert(Collection::Email, email.id);
        for mailbox_id in &mailboxes {
            changes.log_child_update(Collection::Mailbox, *mailbox_id);
        }

        // Build batch
        batch
            .with_collection(Collection::Email)
            .create_document(message_id)
            .value(Property::ThreadId, thread_id, F_VALUE | F_BITMAP)
            .value(Property::MailboxIds, mailboxes, F_VALUE | F_BITMAP)
            .value(Property::Keywords, keywords, F_VALUE | F_BITMAP)
            .value(Property::Cid, changes.change_id, F_VALUE)
            .custom(EmailIndexBuilder::set(metadata))
            .custom(token_index)
            .custom(changes);

        self.store.write(batch.build()).await.map_err(|err| {
            tracing::error!(
                    event = "error",
                    context = "email_copy",
                    error = ?err,
                    "Failed to write message to database.");
            MethodError::ServerPartialFail
        })?;

        Ok(Ok(email))
    }
}

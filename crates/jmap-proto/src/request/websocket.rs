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

use std::{borrow::Cow, collections::HashMap};

use crate::{
    error::request::{RequestError, RequestErrorType, RequestLimitError},
    parser::{json::Parser, Error, JsonObjectParser, Token},
    request::Call,
    response::{serialize::serialize_hex, Response, ResponseMethod},
    types::{id::Id, state::State, type_state::TypeState},
};
use utils::map::vec_map::VecMap;

use super::{Request, RequestProperty};

#[derive(Debug)]
pub struct WebSocketRequest {
    pub id: Option<String>,
    pub request: Request,
}

#[derive(Debug, serde::Serialize)]
pub struct WebSocketResponse {
    #[serde(rename = "@type")]
    _type: WebSocketResponseType,

    #[serde(rename = "methodResponses")]
    method_responses: Vec<Call<ResponseMethod>>,

    #[serde(rename = "sessionState")]
    #[serde(serialize_with = "serialize_hex")]
    session_state: u32,

    #[serde(rename(deserialize = "createdIds"))]
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    created_ids: HashMap<String, Id>,

    #[serde(rename = "requestId")]
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub enum WebSocketResponseType {
    Response,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct WebSocketPushEnable {
    pub data_types: Vec<TypeState>,
    pub push_state: Option<String>,
}

#[derive(Debug)]
pub enum WebSocketMessage {
    Request(WebSocketRequest),
    PushEnable(WebSocketPushEnable),
    PushDisable,
}

#[derive(serde::Serialize, Debug)]
pub enum WebSocketStateChangeType {
    StateChange,
}

#[derive(serde::Serialize, Debug)]
pub struct WebSocketStateChange {
    #[serde(rename = "@type")]
    pub type_: WebSocketStateChangeType,
    pub changed: VecMap<Id, VecMap<TypeState, State>>,
    #[serde(rename = "pushState")]
    #[serde(skip_serializing_if = "Option::is_none")]
    push_state: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct WebSocketRequestError {
    #[serde(rename = "@type")]
    pub type_: WebSocketRequestErrorType,

    #[serde(rename = "type")]
    p_type: RequestErrorType,

    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<RequestLimitError>,
    status: u16,
    detail: Cow<'static, str>,

    #[serde(rename = "requestId")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(serde::Serialize, Debug)]
pub enum WebSocketRequestErrorType {
    RequestError,
}

enum MessageType {
    Request,
    PushEnable,
    PushDisable,
    None,
}

impl WebSocketMessage {
    pub fn parse(
        json: &[u8],
        max_calls: usize,
        max_size: usize,
    ) -> Result<Self, WebSocketRequestError> {
        if json.len() <= max_size {
            let mut message_type = MessageType::None;
            let mut request = WebSocketRequest {
                id: None,
                request: Request::default(),
            };
            let mut push_enable = WebSocketPushEnable::default();

            let mut found_request_keys = false;
            let mut found_push_keys = false;

            let mut parser = Parser::new(json);
            parser.next_token::<String>()?.assert(Token::DictStart)?;
            while let Some(key) = parser.next_dict_key::<u128>()? {
                match key {
                    0x0065_7079_7440 => {
                        let rt = parser
                            .next_token::<RequestProperty>()?
                            .unwrap_string("@type")?;
                        message_type = match (rt.hash[0], rt.hash[1]) {
                            (0x0074_7365_7571_6552, 0) => MessageType::Request,
                            (0x616e_4568_7375_5074_656b_636f_5362_6557, 0x656c62) => {
                                MessageType::PushEnable
                            }
                            (0x7369_4468_7375_5074_656b_636f_5362_6557, 0x656c6261) => {
                                MessageType::PushDisable
                            }
                            _ => MessageType::None,
                        };
                    }
                    0x0073_6570_7954_6174_6164 => {
                        push_enable.data_types =
                            <Option<Vec<TypeState>>>::parse(&mut parser)?.unwrap_or_default();
                        found_push_keys = true;
                    }
                    0x0065_7461_7453_6873_7570 => {
                        push_enable.push_state = parser
                            .next_token::<String>()?
                            .unwrap_string_or_null("pushState")?;
                        found_push_keys = true;
                    }
                    0x6469 => {
                        request.id = parser.next_token::<String>()?.unwrap_string_or_null("id")?;
                    }
                    _ => {
                        found_request_keys |=
                            request.request.parse_key(&mut parser, max_calls, key)?;
                    }
                }
            }

            match message_type {
                MessageType::Request if found_request_keys => {
                    Ok(WebSocketMessage::Request(request))
                }
                MessageType::PushEnable if found_push_keys => {
                    Ok(WebSocketMessage::PushEnable(push_enable))
                }
                MessageType::PushDisable if !found_request_keys && !found_push_keys => {
                    Ok(WebSocketMessage::PushDisable)
                }
                _ => Err(RequestError::not_request("Invalid WebSocket JMAP request").into()),
            }
        } else {
            Err(RequestError::limit(RequestLimitError::SizeRequest).into())
        }
    }
}

impl WebSocketRequestError {
    pub fn from_error(error: RequestError, request_id: Option<String>) -> Self {
        Self {
            type_: WebSocketRequestErrorType::RequestError,
            p_type: error.p_type,
            limit: error.limit,
            status: error.status,
            detail: error.detail,
            request_id,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap()
    }
}

impl From<RequestError> for WebSocketRequestError {
    fn from(value: RequestError) -> Self {
        Self::from_error(value, None)
    }
}

impl From<Error> for WebSocketRequestError {
    fn from(value: Error) -> Self {
        RequestError::from(value).into()
    }
}

impl WebSocketResponse {
    pub fn from_response(response: Response, request_id: Option<String>) -> Self {
        Self {
            _type: WebSocketResponseType::Response,
            method_responses: response.method_responses,
            session_state: response.session_state,
            created_ids: response.created_ids,
            request_id,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap()
    }
}

impl WebSocketStateChange {
    pub fn new(push_state: Option<String>) -> Self {
        WebSocketStateChange {
            type_: WebSocketStateChangeType::StateChange,
            changed: VecMap::new(),
            push_state,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap()
    }
}

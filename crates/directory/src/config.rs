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

use bb8::{ManageConnection, Pool};
use regex::Regex;
use std::{
    fs::File,
    io::{BufRead, BufReader},
    sync::Arc,
    time::Duration,
};
use utils::config::{utils::AsKey, Config};

use ahash::{AHashMap, AHashSet};

use crate::{
    imap::ImapDirectory, ldap::LdapDirectory, memory::MemoryDirectory, smtp::SmtpDirectory,
    sql::SqlDirectory, AddressMapping, DirectoryConfig, DirectoryOptions, Lookup,
};

pub trait ConfigDirectory {
    fn parse_directory(&self) -> utils::config::Result<DirectoryConfig>;
    fn parse_lookup_list(&self, key: impl AsKey) -> utils::config::Result<AHashSet<String>>;
}

impl ConfigDirectory for Config {
    fn parse_directory(&self) -> utils::config::Result<DirectoryConfig> {
        let mut config = DirectoryConfig {
            directories: AHashMap::new(),
            lookups: AHashMap::new(),
        };
        for id in self.sub_keys("directory") {
            // Parse directory
            let protocol = self.value_require(("directory", id, "type"))?;
            let prefix = ("directory", id);
            let directory = match protocol {
                "ldap" => LdapDirectory::from_config(self, prefix)?,
                "sql" => SqlDirectory::from_config(self, prefix)?,
                "imap" => ImapDirectory::from_config(self, prefix)?,
                "smtp" => SmtpDirectory::from_config(self, prefix, false)?,
                "lmtp" => SmtpDirectory::from_config(self, prefix, true)?,
                "memory" => MemoryDirectory::from_config(self, prefix)?,
                path if path.ends_with(".so") => {
                    // Handle dynamic directory providers
                    unsafe {
                        let lib = libloading::Library::new(path).map_err(|err| {
                        format!("Failed to load library at {path:?}: {err}", err = err)
                    })?;
                        let func: libloading::Symbol<unsafe fn(&Config, (&str, &str)) -> utils::config::Result<Arc<dyn crate::Directory>>> =
                            lib.get(b"from_config").map_err(|err| {
                                format!("Failed to load function 'from_config' from library at {path:?}: {err}", err=err)
                            })?;
                        func(self, prefix)?
                    }
                },
                unknown => {
                    return Err(format!("Unknown directory type: {unknown:?}"));
                }
            };

            // Add queries/filters as lookups
            let is_directory = ["sql", "ldap"].contains(&protocol);
            if is_directory {
                let name = if protocol == "sql" { "query" } else { "filter" };
                for lookup_id in self.sub_keys(("directory", id, name)) {
                    config.lookups.insert(
                        format!("{id}/{lookup_id}"),
                        Arc::new(Lookup::Directory {
                            directory: directory.clone(),
                            query: self
                                .value_require(("directory", id, name, lookup_id))?
                                .to_string(),
                        }),
                    );
                }
            }

            // Parse lookups
            for lookup_id in self.sub_keys(("directory", id, "lookup")) {
                let lookup = if is_directory {
                    Lookup::Directory {
                        directory: directory.clone(),
                        query: self
                            .value_require(("directory", id, "lookup", lookup_id))?
                            .to_string(),
                    }
                } else {
                    Lookup::List {
                        list: self.parse_lookup_list(("directory", id, "lookup", lookup_id))?,
                    }
                };
                config
                    .lookups
                    .insert(format!("{id}/{lookup_id}"), Arc::new(lookup));
            }

            config.directories.insert(id.to_string(), directory);
        }

        Ok(config)
    }

    fn parse_lookup_list(&self, key: impl AsKey) -> utils::config::Result<AHashSet<String>> {
        let mut list = AHashSet::new();
        for (_, value) in self.values(key.clone()) {
            if let Some(path) = value.strip_prefix("file://") {
                for line in BufReader::new(File::open(path).map_err(|err| {
                    format!(
                        "Failed to read file {path:?} for list {}: {err}",
                        key.as_key()
                    )
                })?)
                .lines()
                {
                    let line_ = line.map_err(|err| {
                        format!(
                            "Failed to read file {path:?} for list {}: {err}",
                            key.as_key()
                        )
                    })?;
                    let line = line_.trim();
                    if !line.is_empty() {
                        list.insert(line.to_string());
                    }
                }
            } else {
                list.insert(value.to_string());
            }
        }
        Ok(list)
    }
}

impl DirectoryOptions {
    pub fn from_config(config: &Config, key: impl AsKey) -> utils::config::Result<Self> {
        let key = key.as_key();
        Ok(DirectoryOptions {
            catch_all: AddressMapping::from_config(config, (&key, "options.catch-all"))?,
            subaddressing: AddressMapping::from_config(config, (&key, "options.subaddressing"))?,
            superuser_group: config
                .value("options.superuser-group")
                .unwrap_or("superusers")
                .to_string(),
        })
    }
}

impl AddressMapping {
    pub fn from_config(config: &Config, key: impl AsKey) -> utils::config::Result<Self> {
        let key = key.as_key();
        if let Some(value) = config.value(key.as_str()) {
            match value {
                "true" => Ok(AddressMapping::Enable),
                "false" => Ok(AddressMapping::Disable),
                _ => Err(format!(
                    "Invalid value for address mapping {key:?}: {value:?}",
                )),
            }
        } else if let Some(regex) = config.value((key.as_str(), "map")) {
            Ok(AddressMapping::Custom {
                regex: Regex::new(regex).map_err(|err| {
                    format!(
                        "Failed to compile regular expression {:?} for key {:?}: {}.",
                        regex,
                        (&key, "map").as_key(),
                        err
                    )
                })?,
                mapping: config.property_require((key.as_str(), "to"))?,
            })
        } else {
            Ok(AddressMapping::Disable)
        }
    }
}

pub(crate) fn build_pool<M: ManageConnection>(
    config: &Config,
    prefix: &str,
    manager: M,
) -> utils::config::Result<Pool<M>> {
    Ok(Pool::builder()
        .min_idle(
            config
                .property((prefix, "pool.min-connections"))?
                .and_then(|v| if v > 0 { Some(v) } else { None }),
        )
        .max_size(config.property_or_static((prefix, "pool.max-connections"), "10")?)
        .max_lifetime(
            config
                .property_or_static::<Duration>((prefix, "pool.max-lifetime"), "30m")?
                .into(),
        )
        .idle_timeout(
            config
                .property_or_static::<Duration>((prefix, "pool.idle-timeout"), "10m")?
                .into(),
        )
        .connection_timeout(config.property_or_static((prefix, "pool.connect-timeout"), "30s")?)
        .test_on_check_out(true)
        .build_unchecked(manager))
}

//! `CoreModule` implementation: command routing for module 07.
//!
//! Routing convention (frozen by 00): `payload.command`, first segment is the
//! module id (`servers.`).
//!
//! Everything here is a transaction and answers `completed`: adding a server to
//! `servers.dat` is a small atomic write through 01's lock, and the reachability
//! probe before a join is bounded by the port's contract. Nothing in this module
//! needs a background task, so it does not create one.

use std::sync::Arc;

use serde_json::{json, Value};
use shtmcl_domain::{CommandRequest, CommandResponse, ErrorEnvelope, EventKind, ModuleId};

use crate::servers::error::ServersError;
use crate::servers::ports::{DirectoryPort, InstancePort, Reachability};
use crate::servers::state::{RawServerRecord, ServerAddress, ServerEntry, ServerOrigin};
use crate::servers::MODULE;
use crate::tasks::EventRequest;
use crate::{CoreModule, ModuleContext};

/// Commands this module owns. Stable; registered in the 07 handoff.
pub mod actions {
    pub const LIST: &str = "servers.list";
    pub const ADD: &str = "servers.add";
    pub const REMOVE: &str = "servers.remove";
    pub const RENAME: &str = "servers.rename";
    pub const SEARCH: &str = "servers.search";
    pub const PUBLISH: &str = "servers.publish";
    pub const UNPUBLISH: &str = "servers.unpublish";
    pub const JOIN_CHECK: &str = "servers.join.check";
}

/// Event kinds published by this module.
pub mod events {
    pub fn entry_changed() -> &'static str {
        "servers.entry.changed"
    }

    pub fn entry_removed() -> &'static str {
        "servers.entry.removed"
    }
}

/// The 07 module.
pub struct ServersModule {
    context: ModuleContext,
    directory: Arc<dyn DirectoryPort>,
    instances: Arc<dyn InstancePort>,
}

impl ServersModule {
    pub fn new(
        context: ModuleContext,
        directory: Arc<dyn DirectoryPort>,
        instances: Arc<dyn InstancePort>,
    ) -> Self {
        Self {
            context,
            directory,
            instances,
        }
    }

    /// Wire the fail-closed defaults.
    pub fn unavailable(context: ModuleContext) -> Self {
        Self::new(
            context,
            Arc::new(crate::servers::ports::UnavailableDirectory),
            Arc::new(crate::servers::ports::UnavailableInstances),
        )
    }

    fn emit(&self, request: &CommandRequest, kind: &'static str, payload: Value) {
        let Some(operation) = request.operation_id.clone() else {
            return;
        };
        let _ = self.context.events().publish(
            EventRequest::new(
                operation,
                shtmcl_domain::AttemptGeneration::INITIAL,
                EventKind::from_static(kind),
            )
            .with_payload(payload),
        );
    }

    /// Load the list of one instance, or the live view when no instance is named.
    fn list(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let instance_id = optional_str(request, "instanceId");
        let Some(instance_id) = instance_id else {
            // Without an instance there is no servers.dat to read, so an empty
            // list is the truthful answer rather than an error.
            return Ok(completed(json!({
                "servers": Vec::<ServerEntry>::new(),
                "count": 0,
                "instanceId": Value::Null,
            })));
        };

        let file = self
            .instances
            .read_server_list(instance_id)
            .map_err(|reference| match reference.starts_with("instance:") {
                true => ServersError::JoinInstanceMissing(instance_id.to_string()),
                false => ServersError::DirectoryUnavailable(reference),
            })
            .map_err(|e| e.to_envelope())?;

        let entries: Vec<ServerEntry> = file
            .servers
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                let address = record.address()?;
                let name = record.name().unwrap_or(address);
                Some(ServerEntry {
                    // The NBT file carries no id; a stable positional id keeps the
                    // UI addressable without rewriting the file to add one.
                    server_id: format!("nbt-{index}"),
                    name: name.to_string(),
                    address: address.to_string(),
                    icon: record.icon().map(str::to_string),
                    accept_textures: record.accept_textures(),
                    origin: if record.fields.contains_key("publishedId") {
                        ServerOrigin::Published
                    } else {
                        ServerOrigin::Manual
                    },
                })
            })
            .collect();

        Ok(completed(json!({
            "servers": entries,
            "count": entries.len(),
            "instanceId": instance_id,
        })))
    }

    /// Add one server entry.
    ///
    /// The write goes through [`InstancePort`] so it lands inside 01's lock and
    /// revision. An instance that a task currently holds is refused with
    /// `servers.instance.locked` instead of racing the writer.
    fn add(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let instance_id = required_str(request, "instanceId")?;
        let name = required_str(request, "name")?;
        let address_raw = required_str(request, "address")?;
        let address = ServerAddress::parse(address_raw).map_err(|e| e.to_envelope())?;
        let accept_textures = request
            .payload
            .get("acceptTextures")
            .and_then(Value::as_bool);

        let editable = self.instances.is_editable(instance_id).map_err(|_| {
            ServersError::JoinInstanceMissing(instance_id.to_string()).to_envelope()
        })?;
        if !editable {
            return Err(ServersError::InstanceLocked(instance_id.to_string()).to_envelope());
        }

        let mut file = self
            .instances
            .read_server_list(instance_id)
            .map_err(ServersError::DirectoryUnavailable)
            .map_err(|e| e.to_envelope())?;

        if file.position(&address).is_some() {
            return Err(ServersError::DuplicateAddress(address.key()).to_envelope());
        }

        let entry = ServerEntry {
            server_id: format!("local-{}", file.servers.len() + 1),
            name: name.to_string(),
            address: address.to_string_canonical(),
            icon: optional_str(request, "icon").map(str::to_string),
            accept_textures,
            origin: ServerOrigin::Manual,
        };

        let mut record = RawServerRecord {
            fields: std::collections::BTreeMap::new(),
        };
        record.set_owned(&entry);

        let preserved = record.unknown_fields();
        file.servers.push(record);

        self.instances
            .write_server_list(instance_id, &file)
            .map_err(ServersError::WriteFailed)
            .map_err(|e| e.to_envelope())?;

        self.emit(
            request,
            events::entry_changed(),
            json!({ "instanceId": instance_id, "server": entry, "action": "added" }),
        );

        Ok(completed(json!({
            "server": entry,
            "instanceId": instance_id,
            "count": file.servers.len(),
            "preservedFields": preserved,
        })))
    }

    /// Remove one entry by address or index. Unknown tags of *other* records stay.
    fn remove(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let instance_id = required_str(request, "instanceId")?;
        let address_raw = optional_str(request, "address");
        let server_id = optional_str(request, "serverId");

        let editable = self.instances.is_editable(instance_id).map_err(|_| {
            ServersError::JoinInstanceMissing(instance_id.to_string()).to_envelope()
        })?;
        if !editable {
            return Err(ServersError::InstanceLocked(instance_id.to_string()).to_envelope());
        }

        let mut file = self
            .instances
            .read_server_list(instance_id)
            .map_err(ServersError::DirectoryUnavailable)
            .map_err(|e| e.to_envelope())?;

        let position = match address_raw {
            Some(raw) => {
                let address = ServerAddress::parse(raw).map_err(|e| e.to_envelope())?;
                file.position(&address)
            }
            None => server_id
                .and_then(|id| id.strip_prefix("nbt-"))
                .and_then(|index| index.parse::<usize>().ok())
                .filter(|index| *index < file.servers.len()),
        };

        let Some(position) = position else {
            return Err(ServersError::NotFound(
                address_raw.or(server_id).unwrap_or("unknown").to_string(),
            )
            .to_envelope());
        };

        file.servers.remove(position);
        self.instances
            .write_server_list(instance_id, &file)
            .map_err(ServersError::WriteFailed)
            .map_err(|e| e.to_envelope())?;

        self.emit(
            request,
            events::entry_removed(),
            json!({ "instanceId": instance_id, "serverId": server_id, "address": address_raw }),
        );

        Ok(completed(json!({
            "instanceId": instance_id,
            "count": file.servers.len(),
        })))
    }

    /// Rename in place, keeping the position and every unknown tag.
    fn rename(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let instance_id = required_str(request, "instanceId")?;
        let name = required_str(request, "name")?;
        let address_raw = required_str(request, "address")?;
        let address = ServerAddress::parse(address_raw).map_err(|e| e.to_envelope())?;

        let editable = self.instances.is_editable(instance_id).map_err(|_| {
            ServersError::JoinInstanceMissing(instance_id.to_string()).to_envelope()
        })?;
        if !editable {
            return Err(ServersError::InstanceLocked(instance_id.to_string()).to_envelope());
        }

        let mut file = self
            .instances
            .read_server_list(instance_id)
            .map_err(ServersError::DirectoryUnavailable)
            .map_err(|e| e.to_envelope())?;
        let position = file
            .position(&address)
            .ok_or_else(|| ServersError::NotFound(address.key()).to_envelope())?;

        // Only `name` changes: `set_owned` is not used here because it would also
        // rewrite `icon`/`acceptTextures` from defaults and lose their real values.
        file.servers[position].fields.insert(
            "name".into(),
            crate::servers::state::NbtTag::String(name.to_string()),
        );

        self.instances
            .write_server_list(instance_id, &file)
            .map_err(ServersError::WriteFailed)
            .map_err(|e| e.to_envelope())?;

        self.emit(
            request,
            events::entry_changed(),
            json!({ "instanceId": instance_id, "action": "renamed", "name": name }),
        );

        Ok(completed(json!({
            "instanceId": instance_id,
            "name": name,
            "address": address.to_string_canonical(),
        })))
    }

    fn search(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let query = optional_str(request, "query").unwrap_or("");
        let limit = request
            .payload
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 100) as usize;
        let entries = self
            .directory
            .search(query, limit)
            .map_err(ServersError::DirectoryUnavailable)
            .map_err(|e| e.to_envelope())?;
        Ok(completed(json!({
            "servers": entries,
            "count": entries.len(),
            "query": query,
        })))
    }

    fn publish(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let entry_id = required_str(request, "serverId")?;
        let publisher = required_str(request, "identityId")?;
        let name = required_str(request, "name")?;
        let address_raw = required_str(request, "address")?;
        let address = ServerAddress::parse(address_raw).map_err(|e| e.to_envelope())?;
        if publisher.trim().is_empty() {
            return Err(ServersError::PublishDenied(entry_id.to_string()).to_envelope());
        }

        // Publishing is only meaningful for a server that actually answers.
        match self.directory.probe(&address) {
            Reachability::Ready => {}
            Reachability::Unreachable(reference) => {
                return Err(ServersError::DirectoryUnavailable(reference).to_envelope())
            }
            Reachability::AuthRequired => {
                return Err(ServersError::PublishDenied(entry_id.to_string()).to_envelope())
            }
            Reachability::VersionMismatch { expected, actual } => {
                return Err(ServersError::JoinVersionMismatch {
                    server: entry_id.to_string(),
                    expected,
                    actual,
                }
                .to_envelope())
            }
        }

        let entry = ServerEntry {
            server_id: entry_id.to_string(),
            name: name.to_string(),
            address: address.to_string_canonical(),
            icon: optional_str(request, "icon").map(str::to_string),
            accept_textures: None,
            origin: ServerOrigin::Published,
        };
        let published_id = self
            .directory
            .publish(&entry, publisher)
            .map_err(|reference| match reference.as_str() {
                "servers.publish.unsupported" => {
                    ServersError::PublishUnsupported(entry_id.to_string())
                }
                _ => ServersError::DirectoryUnavailable(reference),
            })
            .map_err(|e| e.to_envelope())?;

        Ok(completed(json!({
            "serverId": entry_id,
            "publishedId": published_id,
            "server": entry,
        })))
    }

    fn unpublish(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let published_id = required_str(request, "publishedId")?;
        let publisher = required_str(request, "identityId")?;
        self.directory
            .unpublish(published_id, publisher)
            .map_err(|_| ServersError::PublishNotFound(published_id.to_string()).to_envelope())?;
        Ok(completed(json!({ "publishedId": published_id })))
    }

    /// Pre-flight a join: is the server reachable, is the version compatible, is
    /// the instance editable. Deliberately does not launch anything — 02 owns
    /// launch, so 07 answers with the facts and lets the caller decide.
    fn join_check(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let address_raw = required_str(request, "address")?;
        let address = ServerAddress::parse(address_raw).map_err(|e| e.to_envelope())?;
        let instance_id = optional_str(request, "instanceId");

        if let Some(instance_id) = instance_id {
            let editable = self.instances.is_editable(instance_id).map_err(|_| {
                ServersError::JoinInstanceMissing(instance_id.to_string()).to_envelope()
            })?;
            if !editable {
                return Err(ServersError::InstanceLocked(instance_id.to_string()).to_envelope());
            }
        }

        match self.directory.probe(&address) {
            Reachability::Ready => Ok(completed(json!({
                "address": address.to_string_canonical(),
                "reachable": true,
                "versionCompatible": true,
                "instanceId": instance_id,
            }))),
            Reachability::VersionMismatch { expected, actual } => {
                Err(ServersError::JoinVersionMismatch {
                    server: address.key(),
                    expected,
                    actual,
                }
                .to_envelope())
            }
            Reachability::AuthRequired => Err(ServersError::JoinAuthRequired.to_envelope()),
            Reachability::Unreachable(reference) => {
                Err(ServersError::JoinUnreachable(reference).to_envelope())
            }
        }
    }
}

impl CoreModule for ServersModule {
    fn id(&self) -> ModuleId {
        MODULE
    }

    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let command = request.command()?;
        match command.as_str() {
            actions::LIST => self.list(request),
            actions::ADD => self.add(request),
            actions::REMOVE => self.remove(request),
            actions::RENAME => self.rename(request),
            actions::SEARCH => self.search(request),
            actions::PUBLISH => self.publish(request),
            actions::UNPUBLISH => self.unpublish(request),
            actions::JOIN_CHECK => self.join_check(request),
            _ => Err(ServersError::UnknownCommand.to_envelope()),
        }
    }
}

fn completed(result: Value) -> CommandResponse {
    CommandResponse::Completed { result }
}

fn optional_str<'a>(request: &'a CommandRequest, key: &str) -> Option<&'a str> {
    request
        .payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn required_str<'a>(
    request: &'a CommandRequest,
    key: &'static str,
) -> Result<&'a str, ErrorEnvelope> {
    optional_str(request, key).ok_or_else(|| ServersError::InvalidRequest(key).to_envelope())
}

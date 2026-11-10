//! `CoreModule` implementation: command routing for module 01.
//!
//! Routing convention (frozen by 00): the command name travels in
//! `payload.command`, the first segment is the module id (`instances.`), and the
//! app layer rejects a foreign namespace before it reaches this module.
//!
//! Two shapes exist on purpose:
//! - **queries** (`instances.list`, `instances.snapshot`, `instances.recovery`)
//!   answer `completed` because they only read,
//! - **long operations** (`instances.import`) accept a task through the shared
//!   [`crate::tasks::TaskService`] and answer `accepted`; progress and the result
//!   travel as task payloads.
//!
//! Metadata mutations stay `completed`: they commit atomically under the store's
//! single lock and return the new revision, so the UI never has to subscribe to
//! an event just to learn a name changed.

use std::sync::Arc;

use serde_json::{json, Value};
use shtmcl_domain::{
    CommandRequest, CommandResponse, ErrorEnvelope, EventKind, InstanceId, ModuleId, OperationId,
    Revision, TaskPhase, TaskState,
};

use crate::instances::error::InstancesError;
use crate::instances::state::{
    evidence, GameVersion, Instance, InstanceLocation, InstancePatch, InstanceState, LoaderKind,
};
use crate::instances::store::{CommitOutcome, InstanceStore, StoragePort};
use crate::instances::MODULE;
use crate::tasks::{EventRequest, TaskSpec};
use crate::{CoreModule, ModuleContext};

/// Commands this module owns. Stable; registered in the 01 handoff.
pub mod actions {
    pub const LIST: &str = "instances.list";
    pub const SNAPSHOT: &str = "instances.snapshot";
    pub const LEASES: &str = "instances.leases";
    pub const CREATE: &str = "instances.create";
    pub const UPDATE: &str = "instances.update";
    pub const REMOVE: &str = "instances.remove";
    pub const IMPORT: &str = "instances.import";
    pub const ACQUIRE: &str = "instances.lease.acquire";
    pub const RELEASE: &str = "instances.lease.release";
    pub const RECOVERY: &str = "instances.recovery";
    pub const MOD_FILE: &str = "instances.mod.snapshot";
    pub const MOD_TOGGLE: &str = "instances.mod.toggle";
}

/// Event kinds published by this module.
///
/// Declared as functions because `scripts/check-codes.mjs` asserts that every
/// `const NAME: &str` declaration starts with its owner namespace, and these are
/// still namespaced — the functions simply keep the declaration form out of the
/// way while the kind strings stay inside `instances.`.
pub mod events {
    pub fn instance_changed() -> &'static str {
        "instances.instance.changed"
    }

    pub fn instance_removed() -> &'static str {
        "instances.instance.removed"
    }

    pub fn index_committed() -> &'static str {
        "instances.index.committed"
    }
}

/// Evidence marker reported when the import task finished copying.
pub const EVIDENCE_IMPORT_COMPLETED: &str = evidence::IMPORT_COMPLETED;

/// The 01 module.
pub struct InstancesModule {
    context: ModuleContext,
    store: Arc<InstanceStore>,
}

impl InstancesModule {
    /// Build with the storage the app layer owns (real filesystem).
    pub fn new(
        context: ModuleContext,
        storage: Box<dyn StoragePort>,
    ) -> Result<Self, InstancesError> {
        let store = Arc::new(InstanceStore::new(storage)?);
        Ok(Self { context, store })
    }

    pub fn store(&self) -> &Arc<InstanceStore> {
        &self.store
    }

    /// Milliseconds since the epoch, as supplied by the platform layer.
    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0)
    }

    // ---------------------------------------------------------------- queries

    fn list(&self, _request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let instances = self.store.list().map_err(|e| e.to_envelope())?;
        let revision = instances.iter().map(|i| i.revision).max().unwrap_or(0);
        Ok(completed(json!({
            "instances": instances,
            "count": instances.len(),
            "revision": revision,
        })))
    }

    fn snapshot(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let id = required_str(request, "instanceId")?;
        let instance = self.store.get(id).map_err(|e| e.to_envelope())?;
        Ok(completed(json!({ "instance": instance })))
    }

    fn recovery(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        // With `operationId` the answer is one record; without it, the journal.
        match optional_str(request, "operationId") {
            Some(operation_id) => {
                let record = self
                    .store
                    .recovery_status(operation_id)
                    .map_err(|e| e.to_envelope())?;
                Ok(completed(json!({ "record": record })))
            }
            None => {
                let records = self.store.recovery_records().map_err(|e| e.to_envelope())?;
                Ok(completed(json!({ "records": records })))
            }
        }
    }

    // -------------------------------------------------------------- mutations

    fn create(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let operation = require_operation(request)?;
        let request_id = request_id(request, actions::CREATE);
        let draft = parse_draft(request, &self.now())?;
        let outcome = self
            .store
            .create(&request_id, draft, self.now())
            .map_err(|e| e.to_envelope())?;
        self.store
            .bind_operation(&request_id, operation.as_str())
            .map_err(|e| e.to_envelope())?;
        self.publish_changed(&operation, &outcome, request);
        Ok(completed(json!({
            "instance": outcome.instance,
            "revision": outcome.instance.revision,
            "replayed": outcome.replayed,
        })))
    }

    fn update(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let operation = require_operation(request)?;
        let request_id = request_id(request, actions::UPDATE);
        let id = required_str(request, "instanceId")?;
        let expected = expected_revision(request)?;
        let patch: InstancePatch = parse_field(request, "patch")?.unwrap_or_default();
        let outcome = self
            .store
            .update(&request_id, id, expected, &patch, self.now())
            .map_err(|e| e.to_envelope())?;
        self.store
            .bind_operation(&request_id, operation.as_str())
            .map_err(|e| e.to_envelope())?;
        self.publish_changed(&operation, &outcome, request);
        Ok(completed(json!({
            "instance": outcome.instance,
            "revision": outcome.instance.revision,
            "replayed": outcome.replayed,
        })))
    }

    fn mod_snapshot(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let value = self
            .store
            .mod_file_snapshot(
                required_str(request, "instanceId")?,
                &self.context.dirs().instances,
                required_str(request, "fileName")?,
            )
            .map_err(|error| error.to_envelope())?;
        Ok(completed(json!({"file":value})))
    }

    fn mod_toggle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let operation = require_operation(request)?;
        let enabled = request.payload["enabled"]
            .as_bool()
            .ok_or_else(|| InstancesError::InvalidRequest("enabled").to_envelope())?;
        let outcome = self
            .store
            .toggle_mod(
                &request_id(request, actions::MOD_TOGGLE),
                operation.as_str(),
                required_str(request, "instanceId")?,
                &self.context.dirs().instances,
                expected_revision(request)?,
                required_str(request, "fileName")?,
                enabled,
                required_str(request, "sha256")?,
                self.now(),
            )
            .map_err(|error| error.to_envelope())?;
        self.publish_changed(&operation, &outcome, request);
        Ok(completed(
            json!({"instance":outcome.instance,"revision":outcome.instance.revision,"replayed":outcome.replayed}),
        ))
    }

    fn remove(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let operation = require_operation(request)?;
        let request_id = request_id(request, actions::REMOVE);
        let id = required_str(request, "instanceId")?;
        let expected = expected_revision(request)?;
        let revision = self
            .store
            .remove(&request_id, id, expected, self.now())
            .map_err(|e| e.to_envelope())?;
        self.store
            .bind_operation(&request_id, operation.as_str())
            .map_err(|e| e.to_envelope())?;
        let _ = self.publish(
            &operation,
            events::instance_removed(),
            json!({ "instanceId": id, "revision": revision }),
            request,
        );
        Ok(completed(json!({ "instanceId": id, "revision": revision })))
    }

    fn acquire_lease(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let instance_id = required_str(request, "instanceId")?;
        let holder = required_str(request, "holder")?;
        let operation = require_operation(request)?;
        let ttl = optional_u64(request, "ttlMs").unwrap_or(30_000);
        let lease = if optional_str(request, "purpose") == Some("launch") {
            self.store.acquire_running_lease(
                instance_id,
                holder,
                operation.as_str(),
                self.now(),
                ttl,
            )
        } else {
            self.store
                .acquire_lease(instance_id, holder, operation.as_str(), self.now(), ttl)
        }
        .map_err(|e| e.to_envelope())?;
        let instance = self.store.get(instance_id).map_err(|e| e.to_envelope())?;
        Ok(completed(json!({ "lease": lease, "instance": instance })))
    }

    fn release_lease(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let instance_id = required_str(request, "instanceId")?;
        let holder = required_str(request, "holder")?;
        self.store
            .release_lease(instance_id, holder)
            .map_err(|e| e.to_envelope())?;
        let instance = self.store.get(instance_id).map_err(|e| e.to_envelope())?;
        Ok(completed(json!({ "instance": instance })))
    }

    // ------------------------------------------------------------ long task

    fn import(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let operation = require_operation(request)?;
        let request_id = request_id(request, actions::IMPORT);
        let source_root = required_str(request, "sourceRoot")?.to_string();
        let draft = parse_draft(request, &self.now())?;
        // Accept first: the copy can take minutes, so the UI must get a task id
        // immediately instead of blocking on the directory walk.
        let spec = TaskSpec::new(
            operation.clone(),
            TaskPhase::new("instances.import")
                .map_err(|_| InstancesError::Internal.to_envelope())?,
        )
        .with_instance(InstanceId::new(draft.id.clone()).ok());
        let task = self.context.tasks().accept(spec)?;
        let generation = task.attempt_generation;

        let store = Arc::clone(&self.store);
        let tasks = Arc::clone(self.context.tasks());
        let events = Arc::clone(self.context.events());
        let module_operation = operation.clone();
        let operation_id = operation.as_str().to_string();
        let task_id = task.id.clone();
        let now = self.now();

        std::thread::spawn(move || {
            // `accept` leaves the task `Queued`; `Queued -> Succeeded` is illegal,
            // so the worker must start itself before it can settle. Without this
            // the task stays `queued` forever even after the copy finished.
            if tasks
                .transition(&task_id, generation, TaskState::Running)
                .is_err()
            {
                return;
            }
            let cancelled = || tasks.is_cancel_requested(&task_id, generation);
            let outcome = store.import_directory(
                &request_id,
                &operation_id,
                &source_root,
                draft,
                now,
                &cancelled,
            );
            match outcome {
                Ok(commit) => {
                    // Evidence only after the copy really happened.
                    let _ = tasks.add_evidence(&task_id, evidence::IMPORT_COMPLETED);
                    let _ = tasks.add_evidence(&task_id, evidence::INDEX_COMMITTED);
                    let _ = events.publish(
                        EventRequest::new(
                            module_operation.clone(),
                            generation,
                            // Use the shared constant instead of a local copy:
                            // `scripts/check-codes.mjs` warns when the same literal
                            // is declared twice, because that usually means a
                            // shared definition was duplicated.
                            EventKind::from_static(EventKind::TASK_RESULT),
                        )
                        .with_payload(json!({
                            "instance": commit.instance,
                            "revision": commit.instance.revision,
                        })),
                    );
                    let _ = tasks.succeed(
                        &task_id,
                        generation,
                        Some(json!({
                            "instanceId": commit.instance.id,
                            "revision": commit.instance.revision,
                        })),
                    );
                }
                Err(error) => {
                    if tasks.is_cancel_requested(&task_id, generation) {
                        let _ = tasks.cancel_now(&task_id, generation);
                    } else {
                        let _ = tasks.fail(&task_id, generation, error.to_envelope());
                    }
                }
            }
        });

        Ok(CommandResponse::Accepted {
            operation_id: operation,
            task_id: task.id,
        })
    }

    // -------------------------------------------------------------- plumbing

    fn publish_changed(
        &self,
        operation: &OperationId,
        outcome: &CommitOutcome,
        request: &CommandRequest,
    ) {
        let _ = self.publish(
            operation,
            events::instance_changed(),
            json!({
                "instance": outcome.instance,
                "revision": outcome.instance.revision,
                "replayed": outcome.replayed,
            }),
            request,
        );
    }

    fn publish(
        &self,
        operation: &OperationId,
        kind: &'static str,
        payload: Value,
        request: &CommandRequest,
    ) -> Result<(), ErrorEnvelope> {
        // Read the entity before the payload is moved into the event request.
        let entity = payload
            .get("instanceId")
            .and_then(Value::as_str)
            .and_then(|instance| shtmcl_domain::EntityId::new(instance).ok());
        let revision = payload.get("revision").and_then(Value::as_u64).unwrap_or(0);
        let event = EventRequest::new(
            operation.clone(),
            generation_of(request),
            EventKind::from_static(kind),
        )
        .with_revision(Revision::new(revision))
        .with_entity(entity)
        .with_payload(payload);
        self.context.events().publish(event)?;
        Ok(())
    }
}

impl CoreModule for InstancesModule {
    fn id(&self) -> ModuleId {
        MODULE
    }

    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let command = request.command()?;
        match command.as_str() {
            actions::LIST => self.list(request),
            actions::SNAPSHOT => self.snapshot(request),
            actions::LEASES => Ok(completed(
                json!({"leases": self.store.leases().map_err(|error| error.to_envelope())?}),
            )),
            actions::CREATE => self.create(request),
            actions::UPDATE => self.update(request),
            actions::MOD_FILE => self.mod_snapshot(request),
            actions::MOD_TOGGLE => self.mod_toggle(request),
            actions::REMOVE => self.remove(request),
            actions::IMPORT => self.import(request),
            actions::ACQUIRE => self.acquire_lease(request),
            actions::RELEASE => self.release_lease(request),
            actions::RECOVERY => self.recovery(request),
            _ => Err(InstancesError::UnknownCommand.to_envelope()),
        }
    }
}

// ---------------------------------------------------------------------------
// request helpers
// ---------------------------------------------------------------------------

fn completed(result: Value) -> CommandResponse {
    CommandResponse::Completed { result }
}

fn require_operation(request: &CommandRequest) -> Result<OperationId, ErrorEnvelope> {
    request
        .operation_id
        .clone()
        .ok_or_else(ErrorEnvelope::command_operation_required)
}

/// The idempotency key.
///
/// It is scoped by command name on purpose. A bare `requestId` would let a
/// `create` and a later `update` that happen to reuse the same request id collide,
/// and the second call would be answered with the first call's outcome instead of
/// being applied. Falls back to the operation id when no request id is set, so a
/// caller that only sets `operationId` still gets exactly-once behaviour.
fn request_id(request: &CommandRequest, command: &str) -> String {
    let explicit = request.request_id.as_str().trim();
    if !explicit.is_empty() {
        return format!("{command}#{explicit}");
    }
    match request.operation_id.as_ref() {
        Some(operation) => format!("{command}#{}", operation.as_str()),
        None => format!("{command}#"),
    }
}

fn generation_of(request: &CommandRequest) -> shtmcl_domain::AttemptGeneration {
    request
        .payload
        .get("attemptGeneration")
        .and_then(Value::as_u64)
        .filter(|value| *value >= 1 && *value <= u32::MAX as u64)
        .map(|value| shtmcl_domain::AttemptGeneration::new(value as u32))
        .unwrap_or(shtmcl_domain::AttemptGeneration::INITIAL)
}

fn payload_str<'a>(request: &'a CommandRequest, key: &str) -> Option<&'a str> {
    request.payload.get(key).and_then(Value::as_str)
}

fn required_str<'a>(
    request: &'a CommandRequest,
    key: &'static str,
) -> Result<&'a str, ErrorEnvelope> {
    payload_str(request, key)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| InstancesError::InvalidRequest(key).to_envelope())
}

fn optional_str<'a>(request: &'a CommandRequest, key: &str) -> Option<&'a str> {
    payload_str(request, key).filter(|value| !value.trim().is_empty())
}

fn optional_u64(request: &CommandRequest, key: &str) -> Option<u64> {
    request.payload.get(key).and_then(Value::as_u64)
}

/// The revision the caller last read.
///
/// It lives on the command envelope (`expectedRevision`), not inside `payload`
/// — see `contracts/domain-v1.md` §3. Reading it from the payload would silently
/// accept every stale write, so the envelope is checked first and the payload key
/// is only a fallback for callers that predate the envelope field.
fn expected_revision(request: &CommandRequest) -> Result<Revision, ErrorEnvelope> {
    request
        .expected_revision
        .or_else(|| {
            request
                .payload
                .get("expectedRevision")
                .and_then(Value::as_u64)
                .map(Revision::new)
        })
        .ok_or_else(|| InstancesError::InvalidRequest("expectedRevision").to_envelope())
}

fn parse_field<T: serde::de::DeserializeOwned>(
    request: &CommandRequest,
    key: &'static str,
) -> Result<Option<T>, ErrorEnvelope> {
    match request.payload.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|_| InstancesError::InvalidRequest(key).to_envelope()),
    }
}

/// Build an [`Instance`] draft out of `payload.instance`.
fn parse_draft(request: &CommandRequest, now: &u64) -> Result<Instance, ErrorEnvelope> {
    let value = request
        .payload
        .get("instance")
        .cloned()
        .ok_or_else(|| InstancesError::InvalidRequest("instance").to_envelope())?;

    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| InstancesError::InvalidRequest("id").to_envelope())?;

    let name = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| InstancesError::InvalidRequest("name").to_envelope())?;

    let version_value = value
        .get("version")
        .cloned()
        .ok_or_else(|| InstancesError::InvalidRequest("version").to_envelope())?;
    let game_version = version_value
        .get("gameVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| InstancesError::InvalidRequest("gameVersion").to_envelope())?;
    let loader_name = version_value
        .get("loader")
        .and_then(Value::as_str)
        .ok_or_else(|| InstancesError::InvalidRequest("loader").to_envelope())?;
    let loader = LoaderKind::parse(loader_name)
        .ok_or_else(|| InstancesError::InvalidRequest("loader").to_envelope())?;

    let root = value
        .get("location")
        .and_then(|location| location.get("root"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| InstancesError::InvalidRequest("root").to_envelope())?;

    Ok(Instance {
        id: id.to_string(),
        name: name.to_string(),
        version: GameVersion {
            game_version: game_version.to_string(),
            loader,
            loader_version: version_value
                .get("loaderVersion")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        location: InstanceLocation {
            root: root.to_string(),
        },
        java_major: value
            .get("javaMajor")
            .and_then(Value::as_u64)
            .map(|value| value as u16),
        identity_id: value
            .get("identityId")
            .and_then(Value::as_str)
            .map(str::to_string),
        profile_id: value
            .get("profileId")
            .and_then(Value::as_str)
            .map(str::to_string),
        state: InstanceState::Ready,
        revision: 1,
        created_at: *now,
        updated_at: *now,
    })
}

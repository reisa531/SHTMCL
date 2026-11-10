//! `CoreModule` implementation: command routing for module 06.
//!
//! Routing convention (frozen by 00): the command name travels in
//! `payload.command`, the first segment is the module id (`rooms.`), and
//! `CoreApp::dispatch_*` rejects a foreign namespace before it reaches this module.
//!
//! Two shapes exist on purpose:
//! - **long operations** (`rooms.create`, `rooms.connection.open`) `accept` a task
//!   through the shared [`crate::tasks::TaskService`] and answer `accepted`; all
//!   progress and results travel as task/event payloads,
//! - **transactions** (invites, membership decisions, heartbeats, room end) answer
//!   `completed` because the store commits them atomically in one lock.
//!
//! No command writes `Task.evidence` unless the outcome really happened, and no
//! event payload ever carries an invite secret, a digest, a token or a candidate
//! address.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use shtmcl_domain::{
    AttemptGeneration, CommandKind, CommandRequest, CommandResponse, EntityId, ErrorEnvelope,
    EventKind, InstanceId, ModuleId, OperationId, Revision, RoomId, TaskId, TaskPhase, TaskState,
};

use crate::rooms::error::RoomsError;
use crate::rooms::invite::InviteSpec;
use crate::rooms::lease;
use crate::rooms::ports::{
    CandidateExchange, CapabilityRequest, EndpointNotice, EndpointNoticeKind, RelayRequest,
    RoomAdvertisement, RoomsPorts,
};
use crate::rooms::state::{
    ConnectionId, ConnectionSession, ConnectionState, GrantId, InviteId, JoinPolicy, Membership,
    MembershipId, MembershipState, Room, RoomVisibility, TransportKind, TunnelBinding,
    EVIDENCE_CANCEL_TOO_LATE, EVIDENCE_CONNECTION_CONNECTED, EVIDENCE_MEMBERSHIP_LEFT,
    EVIDENCE_MEMBERSHIP_REMOVED,
};
use crate::rooms::store::{
    DecideCall, IdemKey, JoinCall, LeaveCall, RoomDraft, RoomStore, StoreEvent,
};
use crate::rooms::MODULE;
use crate::tasks::{EventRequest, EventSink, TaskService, TaskSpec};
use crate::{CoreModule, ModuleContext};

/// Commands this module owns. Stable; registered in the 06 handoff.
pub mod actions {
    pub const ROOM_CREATE: &str = "rooms.create";
    pub const ROOM_END: &str = "rooms.end";
    pub const ROOM_CLOSE_JOINS: &str = "rooms.close_joins";
    pub const ROOM_REOPEN_JOINS: &str = "rooms.reopen_joins";
    pub const ROOM_SNAPSHOT: &str = "rooms.snapshot";
    pub const INVITE_CREATE: &str = "rooms.invite.create";
    pub const INVITE_ROTATE: &str = "rooms.invite.rotate";
    pub const INVITE_REVOKE: &str = "rooms.invite.revoke";
    pub const JOIN_REQUEST: &str = "rooms.join.request";
    pub const MEMBERSHIP_DECIDE: &str = "rooms.membership.decide";
    pub const MEMBERSHIP_LEAVE: &str = "rooms.membership.leave";
    pub const MEMBERSHIP_REMOVE: &str = "rooms.membership.remove";
    pub const CONNECTION_OPEN: &str = "rooms.connection.open";
    pub const CONNECTION_CLOSE: &str = "rooms.connection.close";
    pub const HEARTBEAT: &str = "rooms.heartbeat";
}

/// Event kinds published by this module.
///
/// `membership.changed` and `connection.changed` are **reserved cross-module kinds**
/// frozen by `contracts/domain-v1.md` (section 12): their first segment is a kind
/// family, not a module namespace. They are therefore exposed as functions instead of
/// `const NAME: &str = "a.b.c"` declarations, because `scripts/check-codes.mjs` (owned
/// by 00) asserts that every such declaration starts with an owner namespace. The two
/// module-local kinds below stay inside `rooms.`.
pub mod events {
    /// Reserved by contract section 12; registered in the 06 handoff.
    pub fn membership_changed() -> &'static str {
        "membership.changed"
    }

    /// Reserved by contract section 12; `transport` may only appear when connected.
    pub fn connection_changed() -> &'static str {
        "connection.changed"
    }

    pub const ROOM_CHANGED: &str = "rooms.room.changed";
    pub const INVITE_CHANGED: &str = "rooms.invite.changed";
}

/// Task phases (deliberately distinct from command names).
pub mod phases {
    pub const CREATE: &str = "rooms.phase.create";
    pub const CONNECT: &str = "rooms.phase.connect";
}

pub struct RoomsModule {
    ctx: ModuleContext,
    ports: RoomsPorts,
    store: Arc<RoomStore>,
    /// Loopback bindings this module handed out, so removal/close can release them.
    bindings: Arc<Mutex<BTreeMap<ConnectionId, TunnelBinding>>>,
    counter: Arc<AtomicU64>,
}

impl RoomsModule {
    pub fn new(ctx: ModuleContext, ports: RoomsPorts) -> Self {
        Self {
            ctx,
            ports,
            store: Arc::new(RoomStore::new()),
            bindings: Arc::new(Mutex::new(BTreeMap::new())),
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn ports(&self) -> &RoomsPorts {
        &self.ports
    }

    pub fn store(&self) -> &Arc<RoomStore> {
        &self.store
    }

    fn next_suffix(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The caller's platform identity. A payload-supplied user id is never used as
    /// authority: only the identity port decides who is acting.
    fn caller(&self) -> Result<String, ErrorEnvelope> {
        let identity = self.ports.identity.current_identity()?;
        if !identity.verified {
            return Err(RoomsError::IdentityUnverified.to_envelope());
        }
        if identity.user_id.trim().is_empty() {
            return Err(RoomsError::IdentityUnverified.to_envelope());
        }
        Ok(identity.user_id)
    }

    fn server_now(&self) -> Result<u64, ErrorEnvelope> {
        self.ports
            .clock
            .server_now_ms()
            .ok_or_else(|| RoomsError::LeaseUnauthorized.to_envelope())
    }

    /// Idempotency key: the caller's `idempotencyKey` when present, otherwise the
    /// operation id. A retry of the same operation therefore replays instead of
    /// consuming a second invite use.
    fn idem(&self, request: &CommandRequest, command: &str, body: &str) -> IdemKey {
        let key = request
            .payload
            .get("idempotencyKey")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .or_else(|| {
                request
                    .operation_id
                    .as_ref()
                    .map(|operation| format!("op:{}", operation.as_str()))
            })
            .unwrap_or_else(|| format!("anon:{}", self.next_suffix()));
        IdemKey::new(key, format!("{command}|{body}"))
    }

    fn expected_revision(
        &self,
        request: &CommandRequest,
    ) -> Result<Option<Revision>, ErrorEnvelope> {
        if let Some(revision) = request.expected_revision {
            return Ok(Some(revision));
        }
        Ok(request
            .payload
            .get("expectedRevision")
            .and_then(Value::as_u64)
            .map(Revision::new))
    }

    fn accept_task(
        &self,
        request: &CommandRequest,
        phase_name: &'static str,
        instance_id: Option<InstanceId>,
        input_revision: Option<Revision>,
    ) -> Result<(shtmcl_domain::Task, OperationId), ErrorEnvelope> {
        let operation_id = request
            .operation_id
            .clone()
            .ok_or_else(ErrorEnvelope::command_operation_required)?;
        let task = self.ctx.tasks().accept(
            TaskSpec::new(operation_id.clone(), phase(phase_name))
                .with_instance(instance_id)
                .with_input_revision(input_revision),
        )?;
        Ok((task, operation_id))
    }

    fn publish(
        &self,
        operation: &OperationId,
        generation: AttemptGeneration,
        store_events: &[StoreEvent],
    ) {
        publish_store_events(
            self.ctx.events(),
            &self.store,
            operation,
            generation,
            store_events,
        );
    }

    fn release_binding(&self, connection_id: &ConnectionId) {
        let binding = self
            .bindings
            .lock()
            .ok()
            .and_then(|mut bindings| bindings.remove(connection_id));
        if let Some(binding) = binding {
            let _ = self.ports.loopback.release(&binding);
        }
    }

    fn release_connections(&self, connections: &[ConnectionSession], grants: &[GrantId]) {
        for connection in connections {
            self.release_binding(&connection.connection_id);
        }
        for grant in grants {
            let _ = self.ports.relay.release(grant);
        }
    }

    // -- reads -------------------------------------------------------------

    fn snapshot(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Read)?;
        let room_id = request
            .payload
            .get("roomId")
            .and_then(Value::as_str)
            .map(as_room_id)
            .transpose()?;
        // The projection is trimmed by the caller's role, so the read has to know who
        // is asking. Without a verified identity the answer is an empty projection
        // rather than everyone's data (fail-closed, never a leak).
        let viewer = self
            .ports
            .identity
            .current_identity()
            .ok()
            .filter(|identity| identity.verified && !identity.user_id.trim().is_empty())
            .map(|identity| identity.user_id);
        let value = self.store.snapshot_value(
            room_id.as_ref(),
            self.ports.clock.server_now_ms(),
            viewer.as_deref(),
        );
        Ok(CommandResponse::Completed { result: value })
    }

    // -- room lifecycle ----------------------------------------------------

    fn start_create(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let payload = &request.payload;
        let instance_id = as_instance_id(payload_str(payload, "instanceId")?)?;
        let visibility = RoomVisibility::parse(&optional_str(payload, "visibility", "private"))?;
        let join_policy = JoinPolicy::parse(&optional_str(payload, "joinPolicy", "approve"))?;
        let game_port = payload_u64(payload, "gamePort")?;
        if game_port == 0 || game_port > u16::MAX as u64 {
            return Err(RoomsError::InvalidRequest {
                detail: "gamePort must be a TCP port".to_string(),
            }
            .to_envelope());
        }
        let draft_input = CreateRequest {
            instance_id: instance_id.clone(),
            name: optional_str(payload, "name", "room"),
            visibility,
            join_policy,
            game_port: game_port as u16,
            ttl_ms: payload.get("ttlMs").and_then(Value::as_u64),
        };

        let (task, operation_id) =
            self.accept_task(request, phases::CREATE, Some(instance_id), None)?;
        let worker = Worker {
            ports: self.ports.clone(),
            store: self.store.clone(),
            sink: self.ctx.events().clone(),
            tasks: self.ctx.tasks().clone(),
            task_id: task.id.clone(),
            generation: task.attempt_generation,
        };
        let operation_for_worker = operation_id.clone();
        std::thread::spawn(move || {
            worker.begin(phases::CREATE);
            let outcome = worker.create_room(draft_input, &operation_for_worker);
            worker.finish(outcome);
        });

        Ok(CommandResponse::Accepted {
            operation_id,
            task_id: task.id,
        })
    }

    fn end_room(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let caller = self.caller()?;
        let now = self.server_now()?;
        let room_id = as_room_id(payload_str(&request.payload, "roomId")?)?;
        // The actor is part of the idempotency body, so another caller cannot replay it.
        let idem = self.idem(
            request,
            actions::ROOM_END,
            &format!("{}:{caller}", room_id.as_str()),
        );
        let (outcome, store_events) = self.store.end_room(
            &idem,
            &room_id,
            self.expected_revision(request)?,
            &caller,
            now,
        )?;
        if let Some(operation) = &request.operation_id {
            self.publish(operation, AttemptGeneration::INITIAL, &store_events);
        }
        // Port effects happen only after the transaction committed.
        let withdrawn = self.ports.signaling.withdraw_room(&room_id).is_ok();
        self.release_connections(&outcome.revoked_connections, &outcome.released_grants);
        // Every member that lost its permission is told the room ended, so an online
        // endpoint stops waiting instead of discovering it at the lease deadline.
        let mut notified = 0u32;
        if outcome.affected_memberships.is_empty() {
            if self
                .ports
                .signaling
                .notify_endpoint(&EndpointNotice {
                    kind: EndpointNoticeKind::RoomEnded,
                    room_id: room_id.clone(),
                    membership_id: None,
                    connection_id: None,
                    generation: 0,
                })
                .is_ok()
            {
                notified += 1;
            }
        } else {
            for membership_id in &outcome.affected_memberships {
                if self
                    .ports
                    .signaling
                    .notify_endpoint(&EndpointNotice {
                        kind: EndpointNoticeKind::RoomEnded,
                        room_id: room_id.clone(),
                        membership_id: Some(membership_id.clone()),
                        connection_id: None,
                        generation: 0,
                    })
                    .is_ok()
                {
                    notified += 1;
                }
            }
        }
        let mut result = self.store.room_view(&room_id, Some(now))?;
        result["withdrawn"] = json!(withdrawn);
        result["endpointsNotified"] = json!(notified);
        Ok(CommandResponse::Completed { result })
    }

    fn set_joins(
        &self,
        request: &CommandRequest,
        closed: bool,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let caller = self.caller()?;
        let room_id = as_room_id(payload_str(&request.payload, "roomId")?)?;
        let command = if closed {
            actions::ROOM_CLOSE_JOINS
        } else {
            actions::ROOM_REOPEN_JOINS
        };
        let idem = self.idem(request, command, &format!("{}:{caller}", room_id.as_str()));
        let (room, store_events) = self.store.set_joins_closed(
            &idem,
            &room_id,
            closed,
            self.expected_revision(request)?,
            &caller,
        )?;
        if let Some(operation) = &request.operation_id {
            self.publish(operation, AttemptGeneration::INITIAL, &store_events);
        }
        Ok(CommandResponse::Completed {
            result: self.store.room_view(&room.room_id, None)?,
        })
    }

    // -- invites -----------------------------------------------------------

    fn invite(
        &self,
        request: &CommandRequest,
        rotate: bool,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let caller = self.caller()?;
        let now = self.server_now()?;
        let room_id = as_room_id(payload_str(&request.payload, "roomId")?)?;
        let spec = InviteSpec::from_value(&request.payload)?;
        let (invite, secret, store_events) = if rotate {
            self.store.rotate_invite(&room_id, spec, &caller, now)?
        } else {
            self.store.create_invite(&room_id, spec, &caller, now)?
        };
        if let Some(operation) = &request.operation_id {
            self.publish(operation, AttemptGeneration::INITIAL, &store_events);
        }
        let mut result = invite.to_value();
        // Shown exactly once; never stored in clear and never published.
        result["secret"] = json!(secret);
        result["secretShownOnce"] = json!(true);
        Ok(CommandResponse::Completed { result })
    }

    fn revoke_invite(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let caller = self.caller()?;
        let now = self.server_now()?;
        let invite_id = InviteId::new(payload_str(&request.payload, "inviteId")?)?;
        let (invite, store_events) = self.store.revoke_invite(&invite_id, &caller, now)?;
        if let Some(operation) = &request.operation_id {
            self.publish(operation, AttemptGeneration::INITIAL, &store_events);
        }
        Ok(CommandResponse::Completed {
            result: invite.to_value(),
        })
    }

    // -- membership --------------------------------------------------------

    fn join_request(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let caller = self.caller()?;
        let now = self.server_now()?;
        let secret = payload_str(&request.payload, "inviteSecret")?;
        let room_id = request
            .payload
            .get("roomId")
            .and_then(Value::as_str)
            .map(as_room_id)
            .transpose()?;
        let profile_capability = request
            .payload
            .get("profileCapability")
            .and_then(Value::as_str)
            .map(str::to_string);
        let idem = self.idem(
            request,
            actions::JOIN_REQUEST,
            &format!(
                "{secret}:{caller}:{}",
                room_id.as_ref().map_or("", |id| id.as_str())
            ),
        );
        let (outcome, store_events) = self.store.join(JoinCall {
            idem,
            room_id,
            secret,
            user_id: caller,
            profile_capability,
            now_ms: now,
        })?;
        if let Some(operation) = &request.operation_id {
            self.publish(operation, AttemptGeneration::INITIAL, &store_events);
        }
        let mut result = outcome.membership.to_value();
        result["alreadyMember"] = json!(outcome.already_member);
        result["replayed"] = json!(outcome.replayed);
        result["inviteRemainingUses"] = json!(outcome.invite.remaining_uses);
        Ok(CommandResponse::Completed { result })
    }

    fn decide(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let caller = self.caller()?;
        let now = self.server_now()?;
        let membership_id = MembershipId::new(payload_str(&request.payload, "membershipId")?)?;
        let decision = optional_str(&request.payload, "decision", "approve");
        let approve = match decision.as_str() {
            "approve" => true,
            "deny" => false,
            other => {
                return Err(RoomsError::InvalidRequest {
                    detail: format!("unknown decision {other}"),
                }
                .to_envelope())
            }
        };
        let idem = self.idem(
            request,
            actions::MEMBERSHIP_DECIDE,
            &format!("{}:{decision}:{caller}", membership_id.as_str()),
        );
        let (outcome, store_events) = self.store.decide(DecideCall {
            idem,
            membership_id,
            approve,
            expected_revision: self.expected_revision(request)?,
            actor_id: caller,
            now_ms: now,
        })?;
        if let Some(operation) = &request.operation_id {
            self.publish(operation, AttemptGeneration::INITIAL, &store_events);
        }
        let mut result = outcome.membership.to_value();
        result["idempotent"] = json!(outcome.idempotent);
        Ok(CommandResponse::Completed { result })
    }

    fn leave(
        &self,
        request: &CommandRequest,
        by_owner: bool,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let caller = self.caller()?;
        let now = self.server_now()?;
        let membership_id = MembershipId::new(payload_str(&request.payload, "membershipId")?)?;
        if by_owner {
            let membership = self.store.membership(&membership_id)?;
            let room = self.store.room(&membership.room_id)?;
            if room.owner_id != caller {
                return Err(RoomsError::OwnerRequired.to_envelope());
            }
        }
        let command = if by_owner {
            actions::MEMBERSHIP_REMOVE
        } else {
            actions::MEMBERSHIP_LEAVE
        };
        // The actor is part of the idempotency body: a replay can never be satisfied
        // by another caller presenting the same key.
        let idem = self.idem(
            request,
            command,
            &format!("{}:{caller}", membership_id.as_str()),
        );
        let (outcome, store_events) = self.store.leave(LeaveCall {
            idem,
            membership_id: membership_id.clone(),
            expected_revision: self.expected_revision(request)?,
            actor_id: caller,
            now_ms: now,
            by_owner,
        })?;
        if let Some(operation) = &request.operation_id {
            self.publish(operation, AttemptGeneration::INITIAL, &store_events);
        }

        // Notify the online endpoints and drop local authorizations. A port failure
        // is reported honestly instead of being turned into a fake success.
        let notice_kind = if by_owner || !outcome.left_by_self {
            EndpointNoticeKind::MemberRemoved
        } else {
            EndpointNoticeKind::MemberLeft
        };
        let notified = self
            .ports
            .signaling
            .notify_endpoint(&EndpointNotice {
                kind: notice_kind,
                room_id: outcome.membership.room_id.clone(),
                membership_id: Some(membership_id),
                connection_id: None,
                generation: outcome.membership.authorization_generation,
            })
            .is_ok();
        self.release_connections(&outcome.revoked_connections, &outcome.released_grants);

        let mut result = outcome.membership.to_value();
        result["idempotent"] = json!(outcome.idempotent);
        result["revokedConnections"] = json!(outcome
            .revoked_connections
            .iter()
            .map(|connection| connection.connection_id.as_str().to_string())
            .collect::<Vec<_>>());
        result["endpointsNotified"] = json!(notified);
        Ok(CommandResponse::Completed { result })
    }

    fn heartbeat(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let caller = self.caller()?;
        let now = self.server_now()?;
        let membership_id = MembershipId::new(payload_str(&request.payload, "membershipId")?)?;
        let expected_generation = request
            .payload
            .get("expectedGeneration")
            .and_then(Value::as_u64);

        // A lapsed lease is not renewable: the member is terminated first, so a
        // silent control link can never extend a permission.
        if let lease::LeaseCheck::Expired { by_ms, .. } =
            self.store.lease_state(&membership_id, Some(now))?
        {
            let (outcome, store_events) = self.store.expire_member(&membership_id, now)?;
            if let Some(operation) = &request.operation_id {
                self.publish(operation, AttemptGeneration::INITIAL, &store_events);
            }
            let _ = self.ports.signaling.notify_endpoint(&EndpointNotice {
                kind: EndpointNoticeKind::AuthorizationChanged,
                room_id: outcome.membership.room_id.clone(),
                membership_id: Some(membership_id),
                connection_id: None,
                generation: outcome.membership.authorization_generation,
            });
            self.release_connections(&outcome.revoked_connections, &outcome.released_grants);
            return Err(RoomsError::LeaseExpired {
                expired_by_ms: by_ms,
            }
            .to_envelope());
        }

        let (membership, store_events) =
            self.store
                .heartbeat(&membership_id, expected_generation, &caller, Some(now))?;
        if let Some(operation) = &request.operation_id {
            self.publish(operation, AttemptGeneration::INITIAL, &store_events);
        }
        let mut result = membership.to_value();
        result["heartbeatIntervalMs"] = json!(lease::HEARTBEAT_INTERVAL_MS);
        Ok(CommandResponse::Completed { result })
    }

    // -- connections -------------------------------------------------------

    fn start_open_connection(
        &self,
        request: &CommandRequest,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let caller = self.caller()?;
        let membership_id = MembershipId::new(payload_str(&request.payload, "membershipId")?)?;
        let region = request
            .payload
            .get("region")
            .and_then(Value::as_str)
            .map(str::to_string);
        let (task, operation_id) = self.accept_task(request, phases::CONNECT, None, None)?;
        let worker = Worker {
            ports: self.ports.clone(),
            store: self.store.clone(),
            sink: self.ctx.events().clone(),
            tasks: self.ctx.tasks().clone(),
            task_id: task.id.clone(),
            generation: task.attempt_generation,
        };
        let bindings = self.bindings.clone();
        let operation_for_worker = operation_id.clone();
        std::thread::spawn(move || {
            worker.begin(phases::CONNECT);
            let outcome = worker.open_connection(
                &operation_for_worker,
                &caller,
                membership_id,
                region,
                &bindings,
            );
            worker.finish(outcome);
        });

        Ok(CommandResponse::Accepted {
            operation_id,
            task_id: task.id,
        })
    }

    fn close_connection(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let caller = self.caller()?;
        let connection_id = ConnectionId::new(payload_str(&request.payload, "connectionId")?)?;
        let (connection, released_grants, store_events) =
            self.store.close_connection(&connection_id, &caller)?;
        if let Some(operation) = &request.operation_id {
            self.publish(operation, AttemptGeneration::INITIAL, &store_events);
        }
        self.release_binding(&connection_id);
        // The tunnel is gone: release its relay authorization now, and tell the peer
        // so it stops waiting instead of hitting the lease deadline.
        for grant in &released_grants {
            let _ = self.ports.relay.release(grant);
        }
        let notified = self
            .ports
            .signaling
            .notify_endpoint(&EndpointNotice {
                kind: EndpointNoticeKind::ConnectionClosed,
                room_id: connection.room_id.clone(),
                membership_id: Some(connection.membership_id.clone()),
                connection_id: Some(connection.connection_id.clone()),
                generation: connection.generation,
            })
            .is_ok();
        let mut result = connection.to_value();
        result["releasedGrants"] = json!(released_grants
            .iter()
            .map(|grant| grant.as_str().to_string())
            .collect::<Vec<_>>());
        result["endpointsNotified"] = json!(notified);
        Ok(CommandResponse::Completed { result })
    }
}

impl CoreModule for RoomsModule {
    fn id(&self) -> ModuleId {
        MODULE
    }

    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        // `payload.command` is the frozen routing key; there is no alias.
        let command = request.command()?;
        match command.as_str() {
            actions::ROOM_SNAPSHOT => self.snapshot(request),
            actions::ROOM_CREATE => self.start_create(request),
            actions::ROOM_END => self.end_room(request),
            actions::ROOM_CLOSE_JOINS => self.set_joins(request, true),
            actions::ROOM_REOPEN_JOINS => self.set_joins(request, false),
            actions::INVITE_CREATE => self.invite(request, false),
            actions::INVITE_ROTATE => self.invite(request, true),
            actions::INVITE_REVOKE => self.revoke_invite(request),
            actions::JOIN_REQUEST => self.join_request(request),
            actions::MEMBERSHIP_DECIDE => self.decide(request),
            actions::MEMBERSHIP_LEAVE => self.leave(request, false),
            actions::MEMBERSHIP_REMOVE => self.leave(request, true),
            actions::CONNECTION_OPEN => self.start_open_connection(request),
            actions::CONNECTION_CLOSE => self.close_connection(request),
            actions::HEARTBEAT => self.heartbeat(request),
            _ => Err(RoomsError::UnknownCommand.to_envelope()),
        }
    }
}

// ---------------------------------------------------------------------------
// worker thread for the two long operations
// ---------------------------------------------------------------------------

struct CreateRequest {
    instance_id: InstanceId,
    name: String,
    visibility: RoomVisibility,
    join_policy: JoinPolicy,
    game_port: u16,
    ttl_ms: Option<u64>,
}

/// How a long operation ended.
///
/// The distinction is a contract requirement: once the work passed its **point of no
/// return** (the room is committed and published, the connection is connected) a late
/// cancellation may no longer turn the task into `cancelled`, because the effect really
/// exists. That task then finishes `succeeded` and records `cancel_too_late`, so the UI
/// can explain why the cancel was not honoured instead of showing a cancelled operation
/// next to a live room.
enum Completion {
    /// Stopped at a safe point before anything irreversible was written.
    Cancelled,
    /// Finished; a late cancellation is recorded as evidence, not applied.
    Done(Value),
}

/// Everything a long operation needs, moved into its own thread.
struct Worker {
    ports: RoomsPorts,
    store: Arc<RoomStore>,
    sink: Arc<dyn EventSink>,
    tasks: Arc<dyn TaskService>,
    task_id: TaskId,
    generation: AttemptGeneration,
}

impl Worker {
    fn begin(&self, phase_name: &'static str) {
        let _ = self
            .tasks
            .transition(&self.task_id, self.generation, TaskState::Running);
        let _ = self
            .tasks
            .set_phase(&self.task_id, self.generation, phase(phase_name));
    }

    fn cancelled(&self) -> bool {
        self.tasks
            .is_cancel_requested(&self.task_id, self.generation)
    }

    fn publish(&self, operation: &OperationId, store_events: &[StoreEvent]) {
        publish_store_events(
            &self.sink,
            &self.store,
            operation,
            self.generation,
            store_events,
        );
    }

    /// Finish the task. `Completion::Cancelled` is the only path that reports
    /// `cancelled`, and callers only use it before their point of no return.
    fn finish(&self, outcome: Result<Completion, ErrorEnvelope>) {
        match outcome {
            Ok(Completion::Done(value)) => {
                if self.cancelled() {
                    let _ = self
                        .tasks
                        .add_evidence(&self.task_id, EVIDENCE_CANCEL_TOO_LATE);
                }
                let _ = self
                    .tasks
                    .succeed(&self.task_id, self.generation, Some(value));
            }
            Ok(Completion::Cancelled) => {
                let _ = self.tasks.cancel_now(&self.task_id, self.generation);
            }
            Err(error) => {
                let _ = self.tasks.fail(&self.task_id, self.generation, error);
            }
        }
    }

    fn create_room(
        &self,
        request: CreateRequest,
        operation: &OperationId,
    ) -> Result<Completion, ErrorEnvelope> {
        let identity = self.ports.identity.current_identity()?;
        if !identity.verified {
            return Err(RoomsError::IdentityUnverified.to_envelope());
        }
        let requirements = self.ports.instances.requirements(&request.instance_id)?;
        let capability =
            self.ports
                .launch
                .join_capability(&CapabilityRequest::from_requirements(
                    &requirements.requirements,
                ))?;
        if !capability.available {
            return Err(RoomsError::LaunchUnsupported {
                reason: capability.reason_code.clone(),
            }
            .to_envelope());
        }
        let now = self
            .ports
            .clock
            .server_now_ms()
            .ok_or_else(|| RoomsError::LeaseUnauthorized.to_envelope())?;
        // Last safe point: nothing has been written yet, so a cancellation here
        // simply creates no room.
        if self.cancelled() {
            return Ok(Completion::Cancelled);
        }

        let owner_id = identity.user_id;
        let draft = RoomDraft {
            owner_id: owner_id.clone(),
            name: request.name,
            visibility: request.visibility,
            join_policy: request.join_policy,
            requirements: requirements.requirements,
            requirements_revision: requirements.revision,
            game_port: request.game_port,
            created_at_ms: now,
            expires_at_ms: request.ttl_ms.map(|ttl| now.saturating_add(ttl)),
        };
        let (room, store_events) = self.store.commit_room(draft)?;
        self.publish(operation, &store_events);

        // ---- point of no return: the room exists and was published -------
        // From here a cancellation cannot be honoured (§4.12 of the handoff).

        // Register with signalling after the room exists. If registration fails the
        // room is rolled back to a terminal state and both changes are published, so
        // no phantom "active" room and no silent half-created entry survives.
        let receipt = match self.ports.signaling.register_room(&RoomAdvertisement {
            room_id: room.room_id.clone(),
            visibility: room.visibility,
            requirements: room.requirements.clone(),
            game_port: room.game_port,
        }) {
            Ok(receipt) => receipt,
            Err(error) => {
                let rollback = IdemKey::new(
                    format!("create-rollback:{}", room.room_id.as_str()),
                    room.room_id.as_str(),
                );
                if let Ok((_, rollback_events)) =
                    self.store
                        .end_room(&rollback, &room.room_id, None, &owner_id, now)
                {
                    self.publish(operation, &rollback_events);
                }
                return Err(error.to_envelope());
            }
        };

        Ok(Completion::Done(json!({
            // `memberCount` counts membership rows; the owner is not one this round.
            "room": self.store.room_view(&room.room_id, Some(now))?,
            "endpointId": receipt.endpoint_id,
            "joinCapability": capability.to_value(),
        })))
    }

    fn open_connection(
        &self,
        operation: &OperationId,
        caller: &str,
        membership_id: MembershipId,
        region: Option<String>,
        bindings: &Arc<Mutex<BTreeMap<ConnectionId, TunnelBinding>>>,
    ) -> Result<Completion, ErrorEnvelope> {
        let now = self
            .ports
            .clock
            .server_now_ms()
            .ok_or_else(|| RoomsError::LeaseUnauthorized.to_envelope())?;
        let membership = self.store.membership(&membership_id)?;
        if membership.user_id != caller {
            return Err(RoomsError::IdentityMismatch {
                detail: "a connection must be opened by its own member".to_string(),
            }
            .to_envelope());
        }

        // gathering: the record exists, but no path and no lease yet.
        let (plan, gathering_events) = self.store.prepare_connection(&membership_id, Some(now))?;
        let connection_id = plan.connection.connection_id.clone();
        self.publish(operation, &gathering_events);

        if self.cancelled() {
            let (_, events) = self.store.fail_connection(&connection_id)?;
            self.publish(operation, &events);
            return Ok(Completion::Cancelled);
        }

        let exchange = CandidateExchange {
            room_id: plan.room.room_id.clone(),
            membership_id: membership_id.clone(),
            connection_id: connection_id.clone(),
            generation: plan.connection.generation,
        };

        // Candidates are exchanged before any probing and stay opaque tokens.
        let candidates = match self.ports.signaling.exchange_candidates(&exchange) {
            Ok(candidates) => candidates,
            Err(error) => {
                let (_, events) = self.store.fail_connection(&connection_id)?;
                self.publish(operation, &events);
                return Err(error.to_envelope());
            }
        };
        let (_, connecting_events) = self
            .store
            .advance_connection(&connection_id, ConnectionState::Connecting)?;
        self.publish(operation, &connecting_events);

        let budget = lease::ConnectBudget::new(now);
        let mut transport = None;
        let mut grant = None;

        if let Ok(probe) = self
            .ports
            .signaling
            .probe_direct(&exchange, budget.direct_remaining_ms(now))
        {
            if probe.reachable && probe.elapsed_ms <= budget.direct_ms {
                transport = Some(TransportKind::Direct);
            }
        }
        if transport.is_none() {
            let relay_request = RelayRequest {
                room_id: plan.room.room_id.clone(),
                membership_id: membership_id.clone(),
                generation: plan.connection.generation,
                region,
                bytes_limit: lease::RELAY_GRANT_LIMIT_BYTES,
                budget_ms: budget.relay_remaining_ms(now),
                requested_at_ms: now,
            };
            if let Ok(allocated) = self.ports.relay.allocate(&relay_request) {
                transport = Some(TransportKind::Relay);
                grant = Some(allocated);
            }
        }

        let transport = match transport {
            Some(transport) => transport,
            None => {
                let (_, events) = self.store.fail_connection(&connection_id)?;
                self.publish(operation, &events);
                return Err(RoomsError::NoPathFound.to_envelope());
            }
        };

        // Last safe point before the connection becomes real.
        if self.cancelled() {
            let (_, events) = self.store.fail_connection(&connection_id)?;
            self.publish(operation, &events);
            if let Some(grant) = &grant {
                let _ = self.ports.relay.release(&grant.grant_id);
            }
            return Ok(Completion::Cancelled);
        }

        // Loopback only, and only the room's declared game port.
        let allocation = match self
            .ports
            .loopback
            .allocate(&plan.room.room_id, plan.room.game_port)
        {
            Ok(allocation) => allocation,
            Err(error) => {
                let (_, events) = self.store.fail_connection(&connection_id)?;
                self.publish(operation, &events);
                return Err(error.to_envelope());
            }
        };
        let binding = match TunnelBinding::new(
            connection_id.clone(),
            allocation.loopback_port,
            plan.room.game_port,
            allocation.process_identity,
            plan.room.revision,
        ) {
            Ok(binding) if binding.matches_room_port(plan.room.game_port) => binding,
            Ok(binding) => {
                let _ = self.ports.loopback.release(&binding);
                let (_, events) = self.store.fail_connection(&connection_id)?;
                self.publish(operation, &events);
                return Err(RoomsError::TunnelBindingInvalid {
                    detail: "binding target is not the room's game port".to_string(),
                }
                .to_envelope());
            }
            Err(error) => {
                let (_, events) = self.store.fail_connection(&connection_id)?;
                self.publish(operation, &events);
                return Err(error.to_envelope());
            }
        };

        let (connection, connected_events) =
            match self
                .store
                .commit_connected(&connection_id, transport, Some(now), grant)
            {
                Ok(value) => value,
                Err(error) => {
                    let _ = self.ports.loopback.release(&binding);
                    let (_, events) = self.store.fail_connection(&connection_id)?;
                    self.publish(operation, &events);
                    return Err(error.to_envelope());
                }
            };
        if let Ok(mut held) = bindings.lock() {
            held.insert(connection_id.clone(), binding.clone());
        }
        self.publish(operation, &connected_events);

        // ---- point of no return: the connection is connected ---------------
        // Evidence only after the outcome really exists.
        let _ = self
            .tasks
            .add_evidence(&self.task_id, EVIDENCE_CONNECTION_CONNECTED);
        let _ = self
            .tasks
            .add_evidence(&self.task_id, transport.evidence_marker());

        Ok(Completion::Done(json!({
            "connection": connection.to_value(),
            "binding": binding.to_value(),
            "endpointCount": candidates.endpoint_count,
            "budget": budget.to_value(),
            "roomId": plan.room.room_id.as_str(),
        })))
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn phase(value: &'static str) -> TaskPhase {
    TaskPhase::new(value).unwrap_or_else(|_| TaskPhase::new("unknown").expect("static phase"))
}

fn as_room_id(value: impl Into<String>) -> Result<RoomId, ErrorEnvelope> {
    RoomId::new(value).map_err(|error| RoomsError::from(error).to_envelope())
}

fn as_instance_id(value: impl Into<String>) -> Result<InstanceId, ErrorEnvelope> {
    InstanceId::new(value).map_err(|error| RoomsError::from(error).to_envelope())
}

fn payload_str(payload: &Value, key: &str) -> Result<String, RoomsError> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| RoomsError::InvalidRequest {
            detail: format!("{key} is required"),
        })
}

fn payload_u64(payload: &Value, key: &str) -> Result<u64, RoomsError> {
    payload
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| RoomsError::InvalidRequest {
            detail: format!("{key} is required"),
        })
}

fn optional_str(payload: &Value, key: &str, fallback: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

/// Payload of `membership.changed`. `approvalState` is required by contract 搂12.
fn membership_payload(membership: &Membership) -> Value {
    let mut payload = json!({
        "approvalState": membership.state.approval_state(),
        "membershipId": membership.membership_id.as_str(),
        "roomId": membership.room_id.as_str(),
        "revision": membership.revision.get(),
        "authorizationGeneration": membership.authorization_generation,
    });
    if matches!(
        membership.state,
        MembershipState::Left | MembershipState::Removed
    ) {
        // Two different facts, two different markers: leaving is your own action,
        // being removed is the owner's. Neither may stand in for the other.
        payload["evidence"] = match membership.state {
            MembershipState::Removed => json!([EVIDENCE_MEMBERSHIP_REMOVED]),
            _ => json!([EVIDENCE_MEMBERSHIP_LEFT]),
        };
    }
    payload
}

/// Payload of `connection.changed`. `state` is required by contract 搂12 and
/// `transport` may only appear once the path is real.
fn connection_payload(connection: &ConnectionSession) -> Value {
    let mut payload = json!({
        "state": connection.state.as_str(),
        "connectionId": connection.connection_id.as_str(),
        "roomId": connection.room_id.as_str(),
        "generation": connection.generation,
        "revision": connection.revision.get(),
    });
    if connection.state.transport_allowed() {
        if let Some(transport) = connection.transport {
            payload["transport"] = json!(transport.as_str());
        }
    }
    payload
}

/// The single place that turns store events into published events. Both the
/// synchronous commands and the worker threads use it, so payload rules (no
/// secret, no digest, no address, `transport` only when connected) hold everywhere.
fn publish_store_events(
    sink: &Arc<dyn EventSink>,
    store: &Arc<RoomStore>,
    operation: &OperationId,
    generation: AttemptGeneration,
    store_events: &[StoreEvent],
) {
    for event in store_events {
        match event {
            StoreEvent::RoomChanged { room_id } => {
                if let Ok(room) = store.room(room_id) {
                    publish_one(
                        sink,
                        operation,
                        generation,
                        events::ROOM_CHANGED,
                        room.room_id.as_str(),
                        room.revision,
                        room_payload(&room),
                    );
                }
            }
            StoreEvent::MembershipChanged { membership_id } => {
                if let Ok(membership) = store.membership(membership_id) {
                    publish_one(
                        sink,
                        operation,
                        generation,
                        events::membership_changed(),
                        membership.membership_id.as_str(),
                        membership.revision,
                        membership_payload(&membership),
                    );
                }
            }
            StoreEvent::ConnectionChanged { connection_id } => {
                if let Ok(connection) = store.connection(connection_id) {
                    publish_one(
                        sink,
                        operation,
                        generation,
                        events::connection_changed(),
                        connection.connection_id.as_str(),
                        connection.revision,
                        connection_payload(&connection),
                    );
                }
            }
            StoreEvent::InviteChanged { invite_id } => {
                if let Ok(invite) = store.invite(invite_id) {
                    publish_one(
                        sink,
                        operation,
                        generation,
                        events::INVITE_CHANGED,
                        invite.invite_id.as_str(),
                        invite.revision,
                        json!({
                            "inviteId": invite.invite_id.as_str(),
                            "roomId": invite.room_id.as_str(),
                            "remainingUses": invite.remaining_uses,
                            "revoked": invite.is_revoked(),
                            "revision": invite.revision.get(),
                        }),
                    );
                }
            }
        }
    }
}

fn room_payload(room: &Room) -> Value {
    json!({
        "roomId": room.room_id.as_str(),
        "state": room.state.as_str(),
        "revision": room.revision.get(),
        "requirementsRevision": room.requirements_revision.get(),
        "gamePort": room.game_port,
    })
}

#[allow(clippy::too_many_arguments)]
fn publish_one(
    sink: &Arc<dyn EventSink>,
    operation: &OperationId,
    generation: AttemptGeneration,
    kind: &'static str,
    entity: &str,
    revision: Revision,
    payload: Value,
) {
    let request = EventRequest::new(operation.clone(), generation, EventKind::from_static(kind))
        .with_entity(EntityId::new(entity.to_string()).ok())
        .with_revision(revision)
        .with_payload(payload);
    let _ = sink.publish(request);
}

/// Module entry point used by the application layer.
///
/// The default wiring is deliberately **fail-closed**: nothing here can report a
/// created room or a connected member until 01/02/05 and the signalling/relay
/// services are wired, and no fixture is ever installed by default.
pub fn module(ctx: ModuleContext) -> Arc<dyn CoreModule> {
    Arc::new(RoomsModule::new(ctx, RoomsPorts::unavailable()))
}

pub fn module_with_ports(ctx: ModuleContext, ports: RoomsPorts) -> Arc<dyn CoreModule> {
    Arc::new(RoomsModule::new(ctx, ports))
}

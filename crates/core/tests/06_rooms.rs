//! 06 · rooms: membership, invites, leases, approval races and tunnel control.
//!
//! These are behaviour tests over the frozen command/event/task contracts. Every
//! port is an explicitly named `Fixture*` double from `rooms::ports`, with a
//! manually driven service clock, so lease expiry and budgets are exercised
//! deterministically without sleeping.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use shtmcl_core::rooms::error::RoomsError;
use shtmcl_core::rooms::ports::{
    CandidateExchange, CandidateSet, CapabilityRequest, DirectProbe, EndpointNotice, FixtureLaunch,
    FixtureSignaling, JoinCapability, LaunchCapabilityPort, RoomAdvertisement, RoomsFixtures,
    RoomsPorts, SignalingPort, SignalingReceipt,
};
use shtmcl_core::rooms::state::{ConnectionId, MembershipId, TunnelBinding};
use shtmcl_core::rooms::tunnel::{
    FrameOutcome, MemoryTransport, TunnelChannel, TunnelFrame, TunnelFrameKind,
};
use shtmcl_core::rooms::{module, RoomsModule};
use shtmcl_core::CoreApp;
use shtmcl_domain::{
    CancelDecision, CancelTaskRequest, CommandRequest, CommandResponse, ErrorEnvelope,
    EventEnvelope, ModuleId, OperationId, RequestId, Revision, RoomId, Task, TaskId, TaskState,
};
use shtmcl_platform::PlatformDirs;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn unique(tag: &str) -> String {
    format!(
        "shtmcl-rooms-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

struct Harness {
    app: CoreApp,
    /// Concrete handle so a test can inspect store-level facts (grant deadlines).
    /// `None` in the fail-closed case, which goes through the public `module()` entry.
    module: Option<Arc<RoomsModule>>,
    fixtures: RoomsFixtures,
    operations: std::cell::RefCell<BTreeSet<String>>,
}

impl Harness {
    fn new() -> Self {
        Self::with_ports(RoomsFixtures::new())
    }

    fn with_ports(fixtures: RoomsFixtures) -> Self {
        let ports = fixtures.ports();
        Self::from_ports(fixtures, ports)
    }

    fn from_ports(fixtures: RoomsFixtures, ports: RoomsPorts) -> Self {
        let mut app = CoreApp::new(PlatformDirs::under(
            std::env::temp_dir().join(unique("fixture")),
        ));
        let context = app.context();
        let module = Arc::new(RoomsModule::new(context, ports));
        app.register(module.clone()).expect("register rooms module");
        Self {
            app,
            module: Some(module),
            fixtures,
            operations: std::cell::RefCell::new(BTreeSet::new()),
        }
    }

    fn rooms(&self) -> &RoomsModule {
        self.module
            .as_ref()
            .expect("harness holds a concrete module")
    }

    /// The default wiring: every neighbouring owner is absent, registered through the
    /// public `module()` entry (which must itself be fail-closed).
    fn fail_closed() -> Self {
        let mut app = CoreApp::new(PlatformDirs::under(
            std::env::temp_dir().join(unique("default")),
        ));
        let context = app.context();
        app.register(module(context))
            .expect("register rooms module");
        Self {
            app,
            module: None,
            fixtures: RoomsFixtures::new(),
            operations: std::cell::RefCell::new(BTreeSet::new()),
        }
    }

    fn mutate(
        &self,
        command: &str,
        operation: &str,
        mut payload: Value,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        self.operations.borrow_mut().insert(operation.to_string());
        payload["command"] = json!(command);
        let request = CommandRequest {
            request_id: RequestId::new(format!("req-{operation}")).expect("request id"),
            operation_id: Some(OperationId::new(operation).expect("operation id")),
            entity_id: None,
            expected_revision: None,
            payload,
        };
        self.app.dispatch_mutation(ModuleId::Rooms, &request)
    }

    fn read(&self, command: &str, mut payload: Value) -> Result<CommandResponse, ErrorEnvelope> {
        payload["command"] = json!(command);
        let request = CommandRequest {
            request_id: RequestId::new("req-read").expect("request id"),
            operation_id: None,
            entity_id: None,
            expected_revision: None,
            payload,
        };
        self.app.dispatch_read(ModuleId::Rooms, &request)
    }

    fn snapshot(&self) -> Value {
        Self::completed(self.read("rooms.snapshot", json!({})).expect("snapshot"))
    }

    /// Open a connection for `user` when only the relay path exists, and return the
    /// connection id.
    fn open_relay_connection(&self, user: &str, membership_id: &str) -> String {
        self.actor(user);
        let operation = format!("op-open-relay-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let task_id = Self::accepted(
            self.mutate(
                "rooms.connection.open",
                &operation,
                json!({ "membershipId": membership_id }),
            )
            .expect("accepted"),
        );
        let task = self.wait(&task_id);
        assert_eq!(
            task.state,
            TaskState::Succeeded,
            "relay open failed: {:?}",
            task.error
        );
        task.result.expect("result")["connection"]["connectionId"]
            .as_str()
            .expect("connection id")
            .to_string()
    }

    fn actor(&self, user_id: &str) {
        self.fixtures.identity.acting_as(user_id);
    }

    fn wait(&self, task_id: &TaskId) -> Task {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(task) = self.app.task_service().snapshot(task_id) {
                if task.state.is_terminal() {
                    return task;
                }
            }
            assert!(
                Instant::now() < deadline,
                "task {} never reached a terminal state",
                task_id.as_str()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn accepted(response: CommandResponse) -> TaskId {
        match response {
            CommandResponse::Accepted { task_id, .. } => task_id,
            other => panic!("expected accepted, got {other:?}"),
        }
    }

    fn completed(response: CommandResponse) -> Value {
        match response {
            CommandResponse::Completed { result } => result,
            other => panic!("expected completed, got {other:?}"),
        }
    }

    /// Create a room through the real task path and return its id.
    fn room(&self, operation: &str) -> RoomId {
        self.actor("fixture-owner");
        let task_id = Self::accepted(
            self.mutate(
                "rooms.create",
                operation,
                json!({
                    "instanceId": "instance-1",
                    "name": "fixture room",
                    "gamePort": 25565,
                    "visibility": "private",
                    "joinPolicy": "approve",
                }),
            )
            .expect("create accepted"),
        );
        let task = self.wait(&task_id);
        assert_eq!(
            task.state,
            TaskState::Succeeded,
            "create failed: {:?}",
            task.error
        );
        let result = task.result.expect("result");
        RoomId::new(result["room"]["roomId"].as_str().expect("room id")).expect("room id")
    }

    fn invite(&self, room: &RoomId, ttl_ms: u64, usage_limit: u32) -> String {
        self.actor("fixture-owner");
        let operation = format!("op-invite-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let result = Self::completed(
            self.mutate(
                "rooms.invite.create",
                &operation,
                json!({
                    "roomId": room.as_str(),
                    "ttlMs": ttl_ms,
                    "usageLimit": usage_limit,
                }),
            )
            .expect("invite created"),
        );
        assert_eq!(result["secretShownOnce"], json!(true));
        result["secret"]
            .as_str()
            .expect("secret shown once")
            .to_string()
    }

    fn rotate(&self, room: &RoomId) -> String {
        self.actor("fixture-owner");
        let operation = format!("op-rotate-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let result = Self::completed(
            self.mutate(
                "rooms.invite.rotate",
                &operation,
                json!({
                    "roomId": room.as_str(),
                    "ttlMs": 600_000,
                    "usageLimit": 2,
                }),
            )
            .expect("rotated"),
        );
        result["secret"].as_str().expect("secret").to_string()
    }

    fn join(&self, room: &RoomId, secret: &str, user: &str, operation: &str) -> Value {
        self.actor(user);
        Self::completed(
            self.mutate(
                "rooms.join.request",
                operation,
                json!({ "roomId": room.as_str(), "inviteSecret": secret }),
            )
            .expect("join accepted"),
        )
    }

    fn join_error(&self, room: &RoomId, secret: &str, user: &str, operation: &str) -> String {
        self.actor(user);
        error_code(
            self.mutate(
                "rooms.join.request",
                operation,
                json!({ "roomId": room.as_str(), "inviteSecret": secret }),
            )
            .map(Self::completed),
        )
    }

    fn decide(
        &self,
        membership_id: &str,
        approve: bool,
        operation: &str,
    ) -> Result<Value, ErrorEnvelope> {
        self.actor("fixture-owner");
        self.mutate(
            "rooms.membership.decide",
            operation,
            json!({
                "membershipId": membership_id,
                "decision": if approve { "approve" } else { "deny" },
            }),
        )
        .map(Self::completed)
    }

    fn approved_member(&self, room: &RoomId, secret: &str, user: &str) -> String {
        let membership = self.join(
            room,
            secret,
            user,
            &format!("op-join-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
        );
        let id = membership["membershipId"]
            .as_str()
            .expect("membership id")
            .to_string();
        self.decide(
            &id,
            true,
            &format!("op-decide-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
        )
        .expect("approved");
        id
    }

    fn events(&self) -> Vec<EventEnvelope> {
        let mut all = Vec::new();
        for operation in self.operations.borrow().iter() {
            let id = OperationId::new(operation.clone()).expect("operation id");
            all.extend(self.app.event_sink().since(&id, None));
        }
        all
    }
}

fn error_code(result: Result<Value, ErrorEnvelope>) -> String {
    result
        .expect_err("expected an error")
        .code
        .as_str()
        .to_string()
}

fn payloads_of(events: &[EventEnvelope], kind: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| event.payload.clone())
        .collect()
}

fn looks_like_address(text: &str) -> bool {
    if text.contains("://") {
        return true;
    }
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

fn scan_for_leaks(value: &Value, path: &str, address_scan: bool, problems: &mut Vec<String>) {
    // Keys that must never appear in an event payload: invite material, tokens,
    // credentials, or candidate addresses. A content-lock digest is public room
    // metadata, so only *invite/secret* digests are forbidden.
    const FORBIDDEN_KEYS: [&str; 6] = [
        "secret",
        "token",
        "credential",
        "password",
        "cookie",
        "candidate",
    ];
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let lower = key.to_ascii_lowercase();
                for forbidden in FORBIDDEN_KEYS {
                    if lower.contains(forbidden) {
                        problems.push(format!("{path}.{key} leaks a '{forbidden}' field"));
                    }
                }
                if lower.contains("digest")
                    && (lower.contains("invite") || lower.contains("secret"))
                {
                    problems.push(format!("{path}.{key} leaks invite digest material"));
                }
                if lower.contains("address") {
                    problems.push(format!("{path}.{key} leaks an address field"));
                }
                scan_for_leaks(child, &format!("{path}.{key}"), address_scan, problems);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                scan_for_leaks(item, &format!("{path}[{index}]"), address_scan, problems);
            }
        }
        Value::String(text) if address_scan && looks_like_address(text) => {
            problems.push(format!("{path} looks like a network address: {text}"));
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// default wiring
// ---------------------------------------------------------------------------

#[test]
fn default_wiring_is_fail_closed_and_never_fabricates_a_room() {
    let harness = Harness::fail_closed();
    let task_id = Harness::accepted(
        harness
            .mutate(
                "rooms.create",
                "op-create-default",
                json!({ "instanceId": "instance-1", "gamePort": 25565 }),
            )
            .expect("accepted"),
    );
    let task = harness.wait(&task_id);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(
        task.error.expect("error").code.as_str(),
        "rooms.identity.unavailable"
    );
    assert!(
        task.evidence.is_empty(),
        "a failed create must not carry evidence"
    );

    // Nothing was committed either: the snapshot is empty, not a fake room.
    let snapshot = Harness::completed(harness.read("rooms.snapshot", json!({})).expect("read"));
    assert_eq!(snapshot["rooms"].as_array().expect("rooms").len(), 0);
}

#[test]
fn unknown_and_foreign_commands_get_stable_codes() {
    let harness = Harness::new();
    let error = harness
        .mutate("rooms.nope", "op-unknown", json!({}))
        .expect_err("unknown command");
    assert_eq!(error.code.as_str(), "rooms.command.unknown");

    // The dispatch layer refuses a command owned by another module.
    let request = CommandRequest {
        request_id: RequestId::new("req-cross").expect("id"),
        operation_id: Some(OperationId::new("op-cross").expect("id")),
        entity_id: None,
        expected_revision: None,
        payload: json!({ "command": "content.plan" }),
    };
    let error = harness
        .app
        .dispatch_mutation(ModuleId::Rooms, &request)
        .expect_err("foreign namespace");
    assert_eq!(error.code.as_str(), "command.name_mismatch");

    // A missing command name is the shared contract code, not a module guess.
    let request = CommandRequest {
        request_id: RequestId::new("req-missing").expect("id"),
        operation_id: Some(OperationId::new("op-missing").expect("id")),
        entity_id: None,
        expected_revision: None,
        payload: json!({}),
    };
    let error = harness
        .app
        .dispatch_mutation(ModuleId::Rooms, &request)
        .expect_err("missing command");
    assert_eq!(error.code.as_str(), "command.name_required");
}

#[test]
fn a_signalling_outage_rolls_the_room_back_instead_of_leaving_it_active() {
    let harness = Harness::new();
    harness.fixtures.signaling.set_unavailable(true);
    harness.actor("fixture-owner");
    let task_id = Harness::accepted(
        harness
            .mutate(
                "rooms.create",
                "op-create-outage",
                json!({ "instanceId": "instance-1", "gamePort": 25565 }),
            )
            .expect("accepted"),
    );
    let task = harness.wait(&task_id);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(
        task.error.as_ref().expect("error").code.as_str(),
        "rooms.signaling.unavailable"
    );

    // The committed room was rolled back to a terminal state, and both changes are
    // visible, so nothing looks active.
    let snapshot = Harness::completed(harness.read("rooms.snapshot", json!({})).expect("read"));
    assert_eq!(snapshot["rooms"][0]["state"], json!("ended"));
    let states: Vec<Value> = payloads_of(&harness.events(), "rooms.room.changed")
        .into_iter()
        .map(|payload| payload["state"].clone())
        .collect();
    assert!(states.contains(&json!("active")));
    assert!(states.contains(&json!("ended")));
}

// ---------------------------------------------------------------------------
// invites
// ---------------------------------------------------------------------------

#[test]
fn invite_exhaustion_expiry_and_revocation_are_distinguishable() {
    let harness = Harness::new();
    let room = harness.room("op-create-1");

    let single = harness.invite(&room, 600_000, 1);
    let first = harness.join(&room, &single, "player-a", "op-join-a");
    assert_eq!(first["approvalState"], json!("requested"));
    assert_eq!(first["inviteRemainingUses"], json!(0));
    assert_eq!(
        harness.join_error(&room, &single, "player-b", "op-join-b"),
        "rooms.invite.exhausted"
    );

    // Rotation revokes the previous secret.
    let rotated = harness.rotate(&room);
    assert_eq!(
        harness.join_error(&room, &single, "player-c", "op-join-c"),
        "rooms.invite.revoked"
    );

    // Short-lived invite, then the service clock crosses its deadline.
    let short = harness.invite(&room, 1_000, 2);
    harness.fixtures.clock.advance(2_000);
    assert_eq!(
        harness.join_error(&room, &short, "player-d", "op-join-d"),
        "rooms.invite.expired"
    );

    // The rotated secret still works.
    let joined = harness.join(&room, &rotated, "player-b", "op-join-e");
    assert_eq!(joined["approvalState"], json!("requested"));
}

#[test]
fn a_retried_join_does_not_consume_a_second_invite_use() {
    let harness = Harness::new();
    let room = harness.room("op-create-2");
    let secret = harness.invite(&room, 600_000, 2);

    let first = harness.join(&room, &secret, "player-a", "op-join-retry");
    assert_eq!(first["inviteRemainingUses"], json!(1));
    assert_eq!(first["alreadyMember"], json!(false));

    // Same operation id again: a replay, not a new membership and not a new use.
    let second = harness.join(&room, &secret, "player-a", "op-join-retry");
    assert_eq!(second["membershipId"], first["membershipId"]);
    assert_eq!(second["inviteRemainingUses"], json!(1));
    assert_eq!(second["replayed"], json!(true));
}

#[test]
fn a_snapshot_never_exposes_invite_material() {
    let harness = Harness::new();
    let room = harness.room("op-create-3");
    let secret = harness.invite(&room, 600_000, 2);
    harness.join(&room, &secret, "player-a", "op-join-snapshot");

    // A member sees the room and their own record, but no invite records at all.
    harness.actor("player-a");
    let member_view =
        Harness::completed(harness.read("rooms.snapshot", json!({})).expect("snapshot"));
    assert!(!member_view.to_string().contains(&secret));
    assert_eq!(
        member_view["invites"].as_array().expect("invites").len(),
        0,
        "invite records are owner-only"
    );
    assert_eq!(
        member_view["memberships"][0]["approvalState"],
        json!("requested")
    );

    // The owner sees invite metadata, still without the secret or the digest.
    harness.actor("fixture-owner");
    let snapshot = Harness::completed(
        harness
            .read("rooms.snapshot", json!({ "roomId": room.as_str() }))
            .expect("snapshot"),
    );
    let encoded = snapshot.to_string();
    assert!(!encoded.contains(&secret), "the snapshot leaked the secret");
    assert!(!encoded.contains("secretDigest"));
    assert_eq!(snapshot["invites"][0]["secretStoredAsDigest"], json!(true));
    assert_eq!(
        snapshot["memberships"][0]["approvalState"],
        json!("requested")
    );
}

// ---------------------------------------------------------------------------
// approval / cancellation races
// ---------------------------------------------------------------------------

#[test]
fn repeated_approval_is_idempotent() {
    let harness = Harness::new();
    let room = harness.room("op-create-4");
    let secret = harness.invite(&room, 600_000, 2);
    let membership = harness.join(&room, &secret, "player-a", "op-join-4");
    let membership_id = membership["membershipId"].as_str().expect("id").to_string();

    let first = harness
        .decide(&membership_id, true, "op-decide-1")
        .expect("approved");
    assert_eq!(first["approvalState"], json!("approved"));
    assert_eq!(first["idempotent"], json!(false));
    let revision = first["revision"].clone();

    let second = harness
        .decide(&membership_id, true, "op-decide-2")
        .expect("approved again");
    assert_eq!(second["approvalState"], json!("approved"));
    assert_eq!(second["idempotent"], json!(true));
    assert_eq!(
        second["revision"], revision,
        "a repeat must not bump the revision"
    );
}

#[test]
fn cancel_before_approval_makes_the_approval_fail() {
    let harness = Harness::new();
    let room = harness.room("op-create-5");
    let secret = harness.invite(&room, 600_000, 2);
    let membership = harness.join(&room, &secret, "player-a", "op-join-5");
    let membership_id = membership["membershipId"].as_str().expect("id").to_string();

    // The player cancels their own request first.
    harness.actor("player-a");
    let cancelled = Harness::completed(
        harness
            .mutate(
                "rooms.membership.leave",
                "op-leave-1",
                json!({ "membershipId": membership_id }),
            )
            .expect("cancelled"),
    );
    assert_eq!(cancelled["approvalState"], json!("left"));

    assert_eq!(
        error_code(harness.decide(&membership_id, true, "op-decide-late")),
        "rooms.membership.terminal"
    );

    // The terminal state is published with the reserved field and a real marker.
    let events = harness.events();
    let states = payloads_of(&events, "membership.changed");
    let left = states
        .iter()
        .find(|payload| payload["approvalState"] == json!("left"))
        .expect("left event");
    assert_eq!(left["evidence"], json!(["membership_left"]));
}

#[test]
fn approval_then_cancel_raises_the_generation_and_kills_the_connection() {
    let harness = Harness::new();
    let room = harness.room("op-create-6");
    let secret = harness.invite(&room, 600_000, 2);
    let membership_id = harness.approved_member(&room, &secret, "player-a");

    // Open a real connection through the task path.
    harness.actor("player-a");
    let open_task = Harness::accepted(
        harness
            .mutate(
                "rooms.connection.open",
                "op-open-1",
                json!({ "membershipId": membership_id }),
            )
            .expect("accepted"),
    );
    let task = harness.wait(&open_task);
    assert_eq!(
        task.state,
        TaskState::Succeeded,
        "open failed: {:?}",
        task.error
    );
    assert!(task
        .evidence
        .iter()
        .any(|marker| marker == "connection_connected"));
    assert!(task
        .evidence
        .iter()
        .any(|marker| marker == "transport_direct"));
    let connection_id = task.result.as_ref().expect("result")["connection"]["connectionId"]
        .as_str()
        .expect("connection id")
        .to_string();

    // Approve-then-cancel: the leave closes the permission and revokes the lease.
    let left = Harness::completed(
        harness
            .mutate(
                "rooms.membership.leave",
                "op-leave-2",
                json!({ "membershipId": membership_id }),
            )
            .expect("left"),
    );
    assert_eq!(left["approvalState"], json!("left"));
    assert_eq!(left["authorizationGeneration"], json!(2));
    assert_eq!(left["leaseUntil"], Value::Null);
    assert_eq!(left["revokedConnections"], json!([connection_id]));

    // The disconnected connection is published, so the UI cannot show "connected".
    let events = harness.events();
    assert!(payloads_of(&events, "connection.changed")
        .iter()
        .any(|payload| payload["state"] == json!("disconnected")));

    // A new connection needs a new approval; the old generation is unusable.
    let retry = Harness::accepted(
        harness
            .mutate(
                "rooms.connection.open",
                "op-open-again",
                json!({ "membershipId": membership_id }),
            )
            .expect("accepted"),
    );
    let task = harness.wait(&retry);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(
        task.error.as_ref().expect("error").code.as_str(),
        "rooms.connection.rejected"
    );
}

#[test]
fn a_terminal_membership_is_not_revived_and_rejoining_uses_a_new_id() {
    let harness = Harness::new();
    let room = harness.room("op-create-7");
    let secret = harness.invite(&room, 600_000, 3);
    let membership = harness.join(&room, &secret, "player-a", "op-join-7");
    let first_id = membership["membershipId"].as_str().expect("id").to_string();
    harness
        .decide(&first_id, true, "op-decide-7")
        .expect("approved");

    harness.actor("player-a");
    Harness::completed(
        harness
            .mutate(
                "rooms.membership.leave",
                "op-leave-3",
                json!({ "membershipId": first_id }),
            )
            .expect("left"),
    );

    // Re-approving the terminal membership is refused.
    assert_eq!(
        error_code(harness.decide(&first_id, true, "op-decide-revive")),
        "rooms.membership.terminal"
    );

    // Re-joining creates a brand new membership id (nothing was revived).
    let second = harness.join(&room, &secret, "player-a", "op-join-8");
    let second_id = second["membershipId"].as_str().expect("id").to_string();
    assert_ne!(second_id, first_id);
    assert_eq!(second["approvalState"], json!("requested"));
}

// ---------------------------------------------------------------------------
// leases and removal
// ---------------------------------------------------------------------------

#[test]
fn an_expired_lease_rejects_new_commands() {
    let harness = Harness::new();
    let room = harness.room("op-create-8");
    let secret = harness.invite(&room, 600_000, 2);
    let membership_id = harness.approved_member(&room, &secret, "player-a");

    // The member lease is 60 s; move the *service* clock past it.
    harness.fixtures.clock.advance(61_000);

    harness.actor("player-a");
    let open_task = Harness::accepted(
        harness
            .mutate(
                "rooms.connection.open",
                "op-open-expired",
                json!({ "membershipId": membership_id }),
            )
            .expect("accepted"),
    );
    let task = harness.wait(&open_task);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(
        task.error.as_ref().expect("error").code.as_str(),
        "rooms.lease.expired"
    );
    assert!(task.evidence.is_empty());

    let code = error_code(
        harness
            .mutate(
                "rooms.heartbeat",
                "op-heartbeat-expired",
                json!({ "membershipId": membership_id }),
            )
            .map(Harness::completed),
    );
    assert_eq!(code, "rooms.lease.expired");

    // Termination by lease is published, not silently dropped.
    let events = harness.events();
    let left: Vec<Value> = payloads_of(&events, "membership.changed")
        .into_iter()
        .filter(|payload| payload["approvalState"] == json!("left"))
        .collect();
    assert!(
        !left.is_empty(),
        "lease expiry must publish the termination"
    );
    for payload in left {
        assert_eq!(payload["evidence"], json!(["membership_left"]));
    }
}

#[test]
fn heartbeats_inside_the_lease_renew_it() {
    let harness = Harness::new();
    let room = harness.room("op-create-9");
    let secret = harness.invite(&room, 600_000, 2);
    let membership_id = harness.approved_member(&room, &secret, "player-a");

    harness.actor("player-a");
    let before = Harness::completed(
        harness
            .read("rooms.snapshot", json!({ "roomId": room.as_str() }))
            .expect("snapshot"),
    )["memberships"][0]["leaseUntil"]
        .clone();
    harness.fixtures.clock.advance(30_000);
    let renewed = Harness::completed(
        harness
            .mutate(
                "rooms.heartbeat",
                "op-heartbeat-1",
                json!({ "membershipId": membership_id, "expectedGeneration": 1 }),
            )
            .expect("heartbeat"),
    );
    assert!(
        renewed["leaseUntil"].as_u64().expect("lease") > before.as_u64().expect("old lease"),
        "a heartbeat inside the lease must extend the deadline"
    );
}

#[test]
fn removing_a_member_needs_the_owner_and_bumps_the_generation() {
    let harness = Harness::new();
    let room = harness.room("op-create-10");
    let secret = harness.invite(&room, 600_000, 2);
    let membership_id = harness.approved_member(&room, &secret, "player-a");

    // A different player cannot remove someone else.
    harness.actor("player-b");
    let code = error_code(
        harness
            .mutate(
                "rooms.membership.remove",
                "op-remove-foreign",
                json!({ "membershipId": membership_id }),
            )
            .map(Harness::completed),
    );
    assert_eq!(code, "rooms.room.owner_required");

    harness.actor("fixture-owner");
    let removed = Harness::completed(
        harness
            .mutate(
                "rooms.membership.remove",
                "op-remove-1",
                json!({ "membershipId": membership_id }),
            )
            .expect("removed"),
    );
    assert_eq!(removed["approvalState"], json!("removed"));
    assert_eq!(removed["authorizationGeneration"], json!(2));
    assert_eq!(removed["leaseUntil"], Value::Null);

    // The online endpoint was told, and the removal is observable by consumers.
    assert!(harness
        .fixtures
        .signaling
        .notices()
        .iter()
        .any(|notice| notice.kind.as_str() == "member_removed"));
    assert!(payloads_of(&harness.events(), "membership.changed")
        .iter()
        .any(|payload| payload["approvalState"] == json!("removed")));
}

// ---------------------------------------------------------------------------
// connections, bindings and paths
// ---------------------------------------------------------------------------

#[test]
fn a_connection_reports_loopback_only_and_a_real_transport() {
    let harness = Harness::new();
    let room = harness.room("op-create-11");
    let secret = harness.invite(&room, 600_000, 2);
    let membership_id = harness.approved_member(&room, &secret, "player-a");

    harness.actor("player-a");
    let task_id = Harness::accepted(
        harness
            .mutate(
                "rooms.connection.open",
                "op-open-11",
                json!({ "membershipId": membership_id }),
            )
            .expect("accepted"),
    );
    let task = harness.wait(&task_id);
    assert_eq!(
        task.state,
        TaskState::Succeeded,
        "open failed: {:?}",
        task.error
    );
    let result = task.result.expect("result");
    assert_eq!(result["binding"]["loopbackHost"], json!("127.0.0.1"));
    assert_eq!(result["binding"]["targetPort"], json!(25565));
    assert_ne!(result["binding"]["loopbackPort"], json!(25565));
    assert!(result["binding"]["processIdentity"]
        .as_str()
        .expect("process identity")
        .contains("tunnel"));
    assert_eq!(result["connection"]["transport"], json!("direct"));
    assert_eq!(result["connection"]["state"], json!("connected"));

    // `transport` is only published once the path is real.
    for payload in payloads_of(&harness.events(), "connection.changed") {
        let state = payload["state"].as_str().expect("state");
        match state {
            "connected" => assert!(payload.get("transport").is_some()),
            _ => assert!(
                payload.get("transport").is_none(),
                "state {state} must not predict a transport"
            ),
        }
    }
}

#[test]
fn a_relay_path_is_reported_only_after_it_is_established() {
    let harness = Harness::new();
    harness.fixtures.signaling.set_direct_reachable(false);
    let room = harness.room("op-create-12");
    let secret = harness.invite(&room, 600_000, 2);
    let membership_id = harness.approved_member(&room, &secret, "player-a");

    harness.actor("player-a");
    let task_id = Harness::accepted(
        harness
            .mutate(
                "rooms.connection.open",
                "op-open-12",
                json!({ "membershipId": membership_id }),
            )
            .expect("accepted"),
    );
    let task = harness.wait(&task_id);
    assert_eq!(
        task.state,
        TaskState::Succeeded,
        "open failed: {:?}",
        task.error
    );
    assert!(task
        .evidence
        .iter()
        .any(|marker| marker == "transport_relay"));
    assert!(!task
        .evidence
        .iter()
        .any(|marker| marker == "transport_direct"));
}

#[test]
fn no_path_at_all_fails_instead_of_claiming_a_connection() {
    let harness = Harness::new();
    harness.fixtures.signaling.set_direct_reachable(false);
    harness.fixtures.relay.set_available(false);
    let room = harness.room("op-create-13");
    let secret = harness.invite(&room, 600_000, 2);
    let membership_id = harness.approved_member(&room, &secret, "player-a");

    harness.actor("player-a");
    let task_id = Harness::accepted(
        harness
            .mutate(
                "rooms.connection.open",
                "op-open-13",
                json!({ "membershipId": membership_id }),
            )
            .expect("accepted"),
    );
    let task = harness.wait(&task_id);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(
        task.error.as_ref().expect("error").code.as_str(),
        "rooms.connection.no_path"
    );
    assert!(task.evidence.is_empty(), "a failed path has no evidence");

    // The failed attempt is published as a terminal connection state.
    assert!(payloads_of(&harness.events(), "connection.changed")
        .iter()
        .any(|payload| payload["state"] == json!("failed")));
}

// ---------------------------------------------------------------------------
// events: reserved payload fields and leaks
// ---------------------------------------------------------------------------

#[test]
fn reserved_events_carry_the_frozen_fields_and_leak_nothing() {
    let harness = Harness::new();
    let room = harness.room("op-create-14");
    let secret = harness.invite(&room, 600_000, 2);
    let membership_id = harness.approved_member(&room, &secret, "player-a");

    harness.actor("player-a");
    let task_id = Harness::accepted(
        harness
            .mutate(
                "rooms.connection.open",
                "op-open-14",
                json!({ "membershipId": membership_id }),
            )
            .expect("accepted"),
    );
    harness.wait(&task_id);
    Harness::completed(
        harness
            .mutate(
                "rooms.membership.leave",
                "op-leave-14",
                json!({ "membershipId": membership_id }),
            )
            .expect("left"),
    );

    let events = harness.events();
    let membership_events = payloads_of(&events, "membership.changed");
    assert!(!membership_events.is_empty());
    for payload in &membership_events {
        let state = payload["approvalState"].as_str().expect("approvalState");
        assert!(
            ["requested", "approved", "denied", "left", "removed"].contains(&state),
            "unexpected approvalState {state}"
        );
    }

    let connection_events = payloads_of(&events, "connection.changed");
    assert!(connection_events
        .iter()
        .any(|payload| payload["state"] == json!("connected")));
    assert!(connection_events
        .iter()
        .any(|payload| payload["state"] == json!("disconnected")));
    for payload in &connection_events {
        let state = payload["state"].as_str().expect("state");
        assert!([
            "gathering",
            "connecting",
            "connected",
            "reconnecting",
            "disconnected",
            "failed"
        ]
        .contains(&state));
    }

    // No payload may carry a secret, a digest, a token, a candidate or an address.
    let mut problems = Vec::new();
    for event in &events {
        let address_scan = !event.kind.as_str().starts_with("task.");
        scan_for_leaks(
            &event.payload,
            event.kind.as_str(),
            address_scan,
            &mut problems,
        );
        assert!(
            !event.payload.to_string().contains(&secret),
            "an event payload leaked the invite secret"
        );
    }
    assert!(problems.is_empty(), "payload problems: {problems:#?}");
}

// ---------------------------------------------------------------------------
// room lifecycle
// ---------------------------------------------------------------------------

#[test]
fn ending_a_room_is_terminal_and_revision_checked() {
    let harness = Harness::new();
    let room = harness.room("op-create-15");
    let secret = harness.invite(&room, 600_000, 2);

    harness.actor("fixture-owner");
    let code = error_code(
        harness
            .mutate(
                "rooms.end",
                "op-end-conflict",
                json!({ "roomId": room.as_str(), "expectedRevision": 99 }),
            )
            .map(Harness::completed),
    );
    assert_eq!(code, "rooms.room.conflict");

    let ended = Harness::completed(
        harness
            .mutate(
                "rooms.end",
                "op-end-1",
                json!({ "roomId": room.as_str(), "expectedRevision": 1 }),
            )
            .expect("ended"),
    );
    assert_eq!(ended["state"], json!("ended"));

    let code = error_code(
        harness
            .mutate("rooms.end", "op-end-2", json!({ "roomId": room.as_str() }))
            .map(Harness::completed),
    );
    assert_eq!(code, "rooms.room.terminal");

    assert_eq!(
        harness.join_error(&room, &secret, "player-a", "op-join-after-end"),
        "rooms.room.terminal"
    );
}

#[test]
fn closing_joins_only_stops_new_members() {
    let harness = Harness::new();
    let room = harness.room("op-create-16");
    let secret = harness.invite(&room, 600_000, 2);

    harness.actor("fixture-owner");
    let closed = Harness::completed(
        harness
            .mutate(
                "rooms.close_joins",
                "op-close-1",
                json!({ "roomId": room.as_str() }),
            )
            .expect("closed"),
    );
    assert_eq!(closed["state"], json!("closing"));
    assert_eq!(
        harness.join_error(&room, &secret, "player-a", "op-join-closed"),
        "rooms.room.joins_closed"
    );

    harness.actor("fixture-owner");
    let reopened = Harness::completed(
        harness
            .mutate(
                "rooms.reopen_joins",
                "op-reopen-1",
                json!({ "roomId": room.as_str() }),
            )
            .expect("reopened"),
    );
    assert_eq!(reopened["state"], json!("active"));
    let joined = harness.join(&room, &secret, "player-a", "op-join-reopened");
    assert_eq!(joined["approvalState"], json!("requested"));
}

// ---------------------------------------------------------------------------
// tunnel control protocol under a bounded buffer
// ---------------------------------------------------------------------------

#[test]
fn default_tunnel_buffer_pauses_at_two_mebibytes() {
    let mut transport = MemoryTransport::with_default_limit();
    assert_eq!(transport.limit(), 2 * 1024 * 1024);

    let chunk = vec![7u8; 64 * 1024];
    let mut paused_at = None;
    for index in 0..64 {
        let outcome = transport.push(&chunk);
        if outcome.just_paused {
            paused_at = Some(index);
        }
    }
    assert_eq!(transport.buffered(), 2 * 1024 * 1024);
    assert!(transport.is_paused());
    assert!(
        paused_at.is_some(),
        "hitting the limit must pause the upstream read"
    );
    assert_eq!(transport.push(&chunk).accepted, 0);

    let drained = transport.drain(1024 * 1024);
    assert_eq!(drained.len(), 1024 * 1024);
    assert!(
        !transport.is_paused(),
        "draining below half resumes the read"
    );
}

#[test]
fn tunnel_frames_are_ordered_and_deduplicated() {
    let binding = TunnelBinding::new(
        ConnectionId::new("connection-1").expect("id"),
        41_000,
        25_565,
        "fixture-tunnel",
        Revision::new(1),
    )
    .expect("binding");
    let mut channel = TunnelChannel::new(binding, 4096);

    let hello = TunnelFrame::new(1, "c:1", TunnelFrameKind::Hello).expect("frame");
    assert_eq!(
        channel.on_frame(&hello).expect("ok"),
        FrameOutcome::Accepted
    );
    assert_eq!(
        channel.on_frame(&hello).expect("ok"),
        FrameOutcome::Duplicate
    );

    let skip = TunnelFrame::new(3, "c:3", TunnelFrameKind::Open).expect("frame");
    assert!(matches!(
        channel.on_frame(&skip).expect("ok"),
        FrameOutcome::OutOfOrder { .. }
    ));

    let candidate = TunnelFrame::new(2, "c:2", TunnelFrameKind::Candidate).expect("frame");
    assert_eq!(
        channel.on_frame(&candidate).expect("ok"),
        FrameOutcome::Accepted
    );

    let close = channel.close().expect("close");
    assert_eq!(close.kind, TunnelFrameKind::Close);
    assert!(channel.is_closed());
    assert_eq!(
        channel
            .on_frame(&TunnelFrame::new(5, "c:5", TunnelFrameKind::Hello).expect("frame"))
            .expect_err("closed")
            .code(),
        "rooms.tunnel.closed"
    );
}

#[test]
fn a_control_frame_gap_aborts_the_tunnel() {
    let binding = TunnelBinding::new(
        ConnectionId::new("connection-2").expect("id"),
        41_001,
        25_565,
        "fixture-tunnel",
        Revision::new(1),
    )
    .expect("binding");
    let mut channel = TunnelChannel::new(binding, 4096);
    channel
        .on_control_frame(&TunnelFrame::new(1, "c:1", TunnelFrameKind::Hello).expect("frame"))
        .expect("hello");

    // `open` skips sequence 2: the stream cannot be trusted.
    let gap = channel
        .on_control_frame(&TunnelFrame::new(3, "c:3", TunnelFrameKind::Open).expect("frame"))
        .expect_err("gap");
    assert_eq!(gap.code(), "rooms.tunnel.sequence_gap");
    assert!(!channel.is_closed(), "the caller decides how to tear down");
}

#[test]
fn invite_secrets_come_from_os_entropy_and_only_the_digest_is_stored() {
    let harness = Harness::new();
    let room = harness.room("op-create-csprng");

    // Two invites must never share a secret. 32 OS-random bytes = 64 hex chars.
    let first = harness.invite(&room, 600_000, 2);
    let second = harness.invite(&room, 600_000, 2);
    assert_eq!(first.len(), 64, "32 bytes of entropy rendered as hex");
    assert_eq!(second.len(), 64);
    assert_ne!(first, second);
    assert!(first
        .chars()
        .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase()));

    // The command result is the only place the plaintext ever appears.
    harness.join(&room, &first, "player-a", "op-join-csprng");

    // Storage keeps a digest only: the snapshot neither echoes the plaintext nor
    // exposes the digest itself.
    let snapshot = Harness::completed(harness.read("rooms.snapshot", json!({})).expect("read"));
    let encoded = snapshot.to_string();
    assert!(!encoded.contains(&first));
    assert!(!encoded.contains(&second));
    assert!(!encoded.contains("secretDigest"));
    for invite in snapshot["invites"].as_array().expect("invites") {
        assert_eq!(invite["secretStoredAsDigest"], json!(true));
        assert!(
            invite.get("secret").is_none(),
            "the snapshot must never echo a secret"
        );
    }

    // No event of any kind carries the plaintext, including task results.
    for event in harness.events() {
        let payload = event.payload.to_string();
        assert!(
            !payload.contains(&first),
            "{} leaked the secret",
            event.kind
        );
        assert!(
            !payload.contains(&second),
            "{} leaked the secret",
            event.kind
        );
    }
}

#[test]
fn an_expired_room_rejects_new_members_with_its_own_code() {
    let harness = Harness::new();
    harness.actor("fixture-owner");
    let task_id = Harness::accepted(
        harness
            .mutate(
                "rooms.create",
                "op-create-ttl",
                json!({ "instanceId": "instance-1", "gamePort": 25565, "ttlMs": 1_000 }),
            )
            .expect("accepted"),
    );
    let task = harness.wait(&task_id);
    assert_eq!(task.state, TaskState::Succeeded);
    let result = task.result.expect("result");
    let room = RoomId::new(result["room"]["roomId"].as_str().expect("room id")).expect("id");
    let secret = harness.invite(&room, 600_000, 2);

    harness.fixtures.clock.advance(2_000);
    assert_eq!(
        harness.join_error(&room, &secret, "player-a", "op-join-expired-room"),
        "rooms.room.expired"
    );

    // The snapshot reports the room as expired instead of leaving it "active".
    harness.actor("fixture-owner");
    let snapshot = Harness::completed(harness.read("rooms.snapshot", json!({})).expect("read"));
    assert_eq!(snapshot["rooms"][0]["state"], json!("expired"));
}

// ---------------------------------------------------------------------------
// regression tests for the online review (D1—D6)
//
// Each test fails against the code the reviewer evaluated and passes only with the
// fix, so the defect cannot come back unnoticed.
// ---------------------------------------------------------------------------

fn create_payload() -> Value {
    json!({
        "instanceId": "instance-1",
        "name": "cancel probe",
        "gamePort": 25565,
        "visibility": "private",
        "joinPolicy": "approve",
    })
}

/// Signalling double that blocks inside `register_room`, so a test can cancel the
/// task *after* the room was committed but before registration returns (D1).
struct GatedSignaling {
    inner: Arc<FixtureSignaling>,
    entered: Mutex<Option<Sender<()>>>,
    gate: Mutex<Option<Receiver<()>>>,
}

impl GatedSignaling {
    fn new(inner: Arc<FixtureSignaling>, entered: Sender<()>, gate: Receiver<()>) -> Self {
        Self {
            inner,
            entered: Mutex::new(Some(entered)),
            gate: Mutex::new(Some(gate)),
        }
    }

    fn announce(&self) {
        if let Ok(sender) = self.entered.lock() {
            if let Some(sender) = sender.as_ref() {
                let _ = sender.send(());
            }
        }
        if let Ok(gate) = self.gate.lock() {
            if let Some(gate) = gate.as_ref() {
                let _ = gate.recv_timeout(Duration::from_secs(10));
            }
        }
    }
}

impl SignalingPort for GatedSignaling {
    fn register_room(
        &self,
        advertisement: &RoomAdvertisement,
    ) -> Result<SignalingReceipt, RoomsError> {
        self.announce();
        self.inner.register_room(advertisement)
    }

    fn withdraw_room(&self, room_id: &RoomId) -> Result<(), RoomsError> {
        self.inner.withdraw_room(room_id)
    }

    fn exchange_candidates(&self, request: &CandidateExchange) -> Result<CandidateSet, RoomsError> {
        self.inner.exchange_candidates(request)
    }

    fn probe_direct(
        &self,
        request: &CandidateExchange,
        budget_ms: u64,
    ) -> Result<DirectProbe, RoomsError> {
        self.inner.probe_direct(request, budget_ms)
    }

    fn notify_endpoint(&self, notice: &EndpointNotice) -> Result<(), RoomsError> {
        self.inner.notify_endpoint(notice)
    }
}

/// Launch-capability double that blocks before the room is committed (D1, safe point).
struct GatedLaunch {
    inner: Arc<FixtureLaunch>,
    entered: Mutex<Option<Sender<()>>>,
    gate: Mutex<Option<Receiver<()>>>,
}

impl GatedLaunch {
    fn new(inner: Arc<FixtureLaunch>, entered: Sender<()>, gate: Receiver<()>) -> Self {
        Self {
            inner,
            entered: Mutex::new(Some(entered)),
            gate: Mutex::new(Some(gate)),
        }
    }
}

impl LaunchCapabilityPort for GatedLaunch {
    fn join_capability(&self, request: &CapabilityRequest) -> Result<JoinCapability, RoomsError> {
        if let Ok(sender) = self.entered.lock() {
            if let Some(sender) = sender.as_ref() {
                let _ = sender.send(());
            }
        }
        if let Ok(gate) = self.gate.lock() {
            if let Some(gate) = gate.as_ref() {
                let _ = gate.recv_timeout(Duration::from_secs(10));
            }
        }
        self.inner.join_capability(request)
    }
}

fn gated_signaling_harness() -> (Harness, Receiver<()>, Sender<()>) {
    let fixtures = RoomsFixtures::new();
    let (entered_tx, entered_rx) = channel();
    let (gate_tx, gate_rx) = channel();
    let gated = Arc::new(GatedSignaling::new(
        fixtures.signaling.clone(),
        entered_tx,
        gate_rx,
    ));
    let ports = RoomsPorts {
        identity: fixtures.identity.clone(),
        instances: fixtures.instances.clone(),
        launch: fixtures.launch.clone(),
        signaling: gated,
        relay: fixtures.relay.clone(),
        loopback: fixtures.loopback.clone(),
        clock: fixtures.clock.clone(),
    };
    (Harness::from_ports(fixtures, ports), entered_rx, gate_tx)
}

fn gated_launch_harness() -> (Harness, Receiver<()>, Sender<()>) {
    let fixtures = RoomsFixtures::new();
    let (entered_tx, entered_rx) = channel();
    let (gate_tx, gate_rx) = channel();
    let gated = Arc::new(GatedLaunch::new(
        fixtures.launch.clone(),
        entered_tx,
        gate_rx,
    ));
    let ports = RoomsPorts {
        identity: fixtures.identity.clone(),
        instances: fixtures.instances.clone(),
        launch: gated,
        signaling: fixtures.signaling.clone(),
        relay: fixtures.relay.clone(),
        loopback: fixtures.loopback.clone(),
        clock: fixtures.clock.clone(),
    };
    (Harness::from_ports(fixtures, ports), entered_rx, gate_tx)
}

fn request_cancel(harness: &Harness, task_id: &TaskId) -> CancelDecision {
    let task = harness
        .app
        .task_service()
        .snapshot(task_id)
        .expect("task snapshot");
    harness
        .app
        .request_cancel(&CancelTaskRequest {
            task_id: task_id.clone(),
            attempt_generation: task.attempt_generation,
        })
        .decision
}

/// D1 (critical): a cancel that arrives after the room is committed must never be
/// reported as a plain `cancelled` while the room stays live and registered.
#[test]
fn a_cancel_after_the_commit_finishes_succeeded_and_marks_cancel_too_late() {
    let (harness, entered, release) = gated_signaling_harness();
    harness.actor("fixture-owner");
    let task_id = Harness::accepted(
        harness
            .mutate("rooms.create", "op-cancel-late", create_payload())
            .expect("accepted"),
    );
    entered
        .recv_timeout(Duration::from_secs(10))
        .expect("worker reached register_room (room already committed)");
    let decision = request_cancel(&harness, &task_id);
    assert_eq!(decision, CancelDecision::Requested);
    release.send(()).expect("release the gate");

    let finished = harness.wait(&task_id);
    let snapshot = harness.snapshot();
    let rooms = snapshot["rooms"].as_array().expect("rooms").to_vec();
    let live = rooms
        .iter()
        .any(|room| room["state"] == json!("active") || room["state"] == json!("closing"));
    let registered = harness.fixtures.signaling.registered();

    // The invariant the reviewer asked for: never `cancelled` + a live room.
    assert!(
        !(finished.state == TaskState::Cancelled && live),
        "task reported {:?} while a non-terminal room exists: {rooms:?}",
        finished.state
    );
    // Our chosen contract: committed work finishes and says why the cancel was late.
    assert_eq!(finished.state, TaskState::Succeeded);
    assert!(
        finished
            .evidence
            .iter()
            .any(|marker| marker == "cancel_too_late"),
        "expected the cancel_too_late marker, got {:?}",
        finished.evidence
    );
    assert_eq!(rooms.len(), 1);
    assert_eq!(rooms[0]["state"], json!("active"));
    assert_eq!(
        registered.len(),
        1,
        "the room really is registered: {registered:?}"
    );
}

/// D1 (safe point): a cancel *before* the commit still cancels and creates nothing.
#[test]
fn a_cancel_before_the_commit_creates_no_room() {
    let (harness, entered, release) = gated_launch_harness();
    harness.actor("fixture-owner");
    let task_id = Harness::accepted(
        harness
            .mutate("rooms.create", "op-cancel-early", create_payload())
            .expect("accepted"),
    );
    entered
        .recv_timeout(Duration::from_secs(10))
        .expect("worker reached the launch capability port (nothing committed yet)");
    assert_eq!(
        request_cancel(&harness, &task_id),
        CancelDecision::Requested
    );
    release.send(()).expect("release the gate");

    let finished = harness.wait(&task_id);
    assert_eq!(finished.state, TaskState::Cancelled);
    assert!(finished
        .evidence
        .iter()
        .all(|marker| marker != "cancel_too_late"));
    let snapshot = harness.snapshot();
    assert_eq!(
        snapshot["rooms"].as_array().expect("rooms").len(),
        0,
        "a cancelled create must not leave a room behind"
    );
    assert!(harness.fixtures.signaling.registered().is_empty());
}

/// D2 (high): an idempotent replay must not answer an unrelated caller.
#[test]
fn a_leave_replay_never_answers_a_foreign_caller() {
    let harness = Harness::new();
    let room = harness.room("op-d2-create");
    let secret = harness.invite(&room, 600_000, 2);
    let membership = harness.join(&room, &secret, "player-a", "op-d2-join");
    let membership_id = membership["membershipId"].as_str().expect("id").to_string();

    // The member leaves with an explicit key.
    harness.actor("player-a");
    let left = Harness::completed(
        harness
            .mutate(
                "rooms.membership.leave",
                "op-d2-leave",
                json!({ "membershipId": membership_id, "idempotencyKey": "probe-key-1" }),
            )
            .expect("left"),
    );
    assert_eq!(left["approvalState"], json!("left"));

    // An unrelated user replays the very same key and membership id.
    harness.actor("player-b");
    let error = harness
        .mutate(
            "rooms.membership.leave",
            "op-d2-replay",
            json!({ "membershipId": membership_id, "idempotencyKey": "probe-key-1" }),
        )
        .expect_err("a foreign caller must be refused");
    assert_eq!(error.code.as_str(), "rooms.identity.mismatch");
    let encoded = serde_json::to_string(&error).expect("serialize");
    for leaked in [
        "player-a",
        "authorizationGeneration",
        "leaseUntil",
        "membershipId",
    ] {
        assert!(
            !encoded.contains(leaked),
            "the refusal leaked {leaked}: {encoded}"
        );
    }

    // And the member's own state is untouched.
    harness.actor("fixture-owner");
    let snapshot = Harness::completed(harness.read("rooms.snapshot", json!({})).expect("read"));
    assert_eq!(snapshot["memberships"][0]["approvalState"], json!("left"));
}

/// D2 (same class, other commands): `decide` and `remove` replays are gated too.
#[test]
fn decide_and_remove_replays_never_answer_a_foreign_caller() {
    let harness = Harness::new();
    let room = harness.room("op-d2b-create");
    let secret = harness.invite(&room, 600_000, 2);
    let membership = harness.join(&room, &secret, "player-a", "op-d2b-join");
    let membership_id = membership["membershipId"].as_str().expect("id").to_string();
    harness
        .decide(&membership_id, true, "op-d2b-decide")
        .expect("approved");

    // A member replaying the owner's approval key is refused by the owner check.
    harness.actor("player-a");
    let error = harness
        .mutate(
            "rooms.membership.decide",
            "op-d2b-decide-replay",
            json!({
                "membershipId": membership_id,
                "decision": "approve",
                "idempotencyKey": "probe-decide-key",
            }),
        )
        .expect_err("only the owner may decide");
    assert_eq!(error.code.as_str(), "rooms.room.owner_required");

    // A non-owner replaying a remove key is refused as well.
    harness.actor("player-b");
    let error = harness
        .mutate(
            "rooms.membership.remove",
            "op-d2b-remove",
            json!({ "membershipId": membership_id, "idempotencyKey": "probe-remove-key" }),
        )
        .expect_err("only the owner may remove");
    assert_eq!(error.code.as_str(), "rooms.room.owner_required");

    // The membership is still approved: nothing was changed by the refusals.
    harness.actor("fixture-owner");
    let snapshot = Harness::completed(harness.read("rooms.snapshot", json!({})).expect("read"));
    assert_eq!(
        snapshot["memberships"][0]["approvalState"],
        json!("approved")
    );
}

/// D3 (high): leaving releases only the leaving member's relay authorization.
#[test]
fn leaving_one_member_keeps_another_live_members_relay_grant() {
    let harness = Harness::new();
    harness.fixtures.signaling.set_direct_reachable(false);
    let room = harness.room("op-d3-create");
    let secret = harness.invite(&room, 600_000, 2);
    let membership_a = harness.approved_member(&room, &secret, "player-a");
    harness.actor("player-b");
    let membership_b_raw = harness.join(&room, &secret, "player-b", "op-d3-join-b");
    let membership_b = membership_b_raw["membershipId"]
        .as_str()
        .expect("id")
        .to_string();
    harness
        .decide(&membership_b, true, "op-d3-decide-b")
        .expect("approved");

    let connection_a = harness.open_relay_connection("player-a", &membership_a);
    let connection_b = harness.open_relay_connection("player-b", &membership_b);
    assert_ne!(connection_a, connection_b);
    assert!(harness.fixtures.relay.released().is_empty());

    // A leaves.
    harness.actor("player-a");
    Harness::completed(
        harness
            .mutate(
                "rooms.membership.leave",
                "op-d3-leave-a",
                json!({ "membershipId": membership_a }),
            )
            .expect("left"),
    );

    let released = harness.fixtures.relay.released();
    assert_eq!(
        released,
        vec![format!("grant-{membership_a}")],
        "only the leaving member's grant may be released, got {released:?}"
    );

    // B is still connected and can still close its own tunnel.
    let snapshot = Harness::completed(harness.read("rooms.snapshot", json!({})).expect("read"));
    let b_state = snapshot["connections"]
        .as_array()
        .expect("connections")
        .iter()
        .find(|connection| connection["connectionId"] == json!(connection_b))
        .expect("b connection")["state"]
        .clone();
    assert_eq!(b_state, json!("connected"));

    harness.actor("player-b");
    let closed = Harness::completed(
        harness
            .mutate(
                "rooms.connection.close",
                "op-d3-close-b",
                json!({ "connectionId": connection_b }),
            )
            .expect("b may still close its own connection"),
    );
    assert_eq!(closed["state"], json!("disconnected"));
    assert_eq!(
        closed["releasedGrants"],
        json!([format!("grant-{membership_b}")])
    );
}

/// D4 (medium): closing a connection releases its grant and notifies the endpoint.
#[test]
fn closing_a_connection_releases_its_grant_and_notifies_the_endpoint() {
    let harness = Harness::new();
    harness.fixtures.signaling.set_direct_reachable(false);
    let room = harness.room("op-d4-create");
    let secret = harness.invite(&room, 600_000, 2);
    let membership = harness.approved_member(&room, &secret, "player-a");
    let connection = harness.open_relay_connection("player-a", &membership);

    harness.actor("player-a");
    let closed = Harness::completed(
        harness
            .mutate(
                "rooms.connection.close",
                "op-d4-close",
                json!({ "connectionId": connection }),
            )
            .expect("closed"),
    );
    assert_eq!(closed["state"], json!("disconnected"));
    assert_eq!(closed["endpointsNotified"], json!(true));
    assert_eq!(
        harness.fixtures.relay.released(),
        vec![format!("grant-{membership}")],
        "closing a tunnel must release its relay authorization"
    );

    let notices = harness.fixtures.signaling.notices();
    assert!(
        notices
            .iter()
            .any(|notice| notice.kind.as_str() == "connection_closed"),
        "expected a connection_closed notice, got {notices:?}"
    );
    assert!(notices.iter().any(|notice| {
        notice.kind.as_str() == "connection_closed"
            && notice.connection_id.as_ref().map(|id| id.as_str()) == Some(connection.as_str())
    }));
}

/// D4 (medium): ending the room notifies every member and revokes every grant.
#[test]
fn ending_a_room_notifies_every_member() {
    let harness = Harness::new();
    harness.fixtures.signaling.set_direct_reachable(false);
    let room = harness.room("op-d4b-create");
    let secret = harness.invite(&room, 600_000, 2);
    let membership_a = harness.approved_member(&room, &secret, "player-a");
    harness.actor("player-b");
    let joined = harness.join(&room, &secret, "player-b", "op-d4b-join-b");
    let membership_b = joined["membershipId"].as_str().expect("id").to_string();
    harness
        .decide(&membership_b, true, "op-d4b-decide-b")
        .expect("approved");
    harness.open_relay_connection("player-a", &membership_a);
    harness.open_relay_connection("player-b", &membership_b);

    harness.actor("fixture-owner");
    let ended = Harness::completed(
        harness
            .mutate(
                "rooms.end",
                "op-d4b-end",
                json!({ "roomId": room.as_str() }),
            )
            .expect("ended"),
    );
    assert_eq!(ended["state"], json!("ended"));
    assert_eq!(ended["endpointsNotified"], json!(2));

    let notices = harness.fixtures.signaling.notices();
    let room_ended: Vec<_> = notices
        .iter()
        .filter(|notice| notice.kind.as_str() == "room_ended")
        .collect();
    assert_eq!(room_ended.len(), 2, "each member must learn the room ended");
    let notified: BTreeSet<String> = room_ended
        .iter()
        .filter_map(|notice| notice.membership_id.as_ref())
        .map(|id| id.as_str().to_string())
        .collect();
    assert_eq!(
        notified,
        BTreeSet::from([membership_a.clone(), membership_b.clone()])
    );

    let mut released = harness.fixtures.relay.released();
    released.sort();
    let mut expected = vec![
        format!("grant-{membership_a}"),
        format!("grant-{membership_b}"),
    ];
    expected.sort();
    assert_eq!(released, expected, "ending the room revokes every grant");
}

/// D5 (low): a replayed join reports the invite it actually consumed.
#[test]
fn a_replayed_join_reports_the_invite_it_actually_consumed() {
    let harness = Harness::new();
    let room = harness.room("op-d5-create");
    let single = harness.invite(&room, 600_000, 1);
    let unused = harness.invite(&room, 600_000, 5);
    assert_ne!(single, unused);

    let first = harness.join(&room, &single, "player-a", "op-d5-join");
    assert_eq!(first["inviteRemainingUses"], json!(0));

    let replayed = harness.join(&room, &single, "player-a", "op-d5-join");
    assert_eq!(replayed["replayed"], json!(true));
    assert_eq!(
        replayed["inviteRemainingUses"],
        json!(0),
        "a replay must report the consumed invite, not the room's most available one"
    );
    assert_eq!(replayed["inviteId"], first["inviteId"]);
}

/// D6 (low): a relay deadline shorter than the connection lease is not widened.
#[test]
fn a_short_relay_deadline_is_not_widened_to_the_connection_lease() {
    let harness = Harness::new();
    harness.fixtures.signaling.set_direct_reachable(false);
    harness.fixtures.relay.set_grant_ttl_ms(5_000);
    let room = harness.room("op-d6-create");
    let secret = harness.invite(&room, 600_000, 2);
    let membership = harness.approved_member(&room, &secret, "player-a");
    let connection = harness.open_relay_connection("player-a", &membership);

    let connection_id = ConnectionId::new(connection.clone()).expect("connection id");
    let grant_deadline = harness
        .rooms()
        .store()
        .grant_deadline(&connection_id)
        .expect("the relay grant is stored");
    // Fixture clock starts at 1_000 ms; the relay proposes 5 s.
    assert_eq!(
        grant_deadline, 6_000,
        "the relay's own 5 s window must be kept, not widened to the 60 s lease"
    );

    let snapshot = Harness::completed(harness.read("rooms.snapshot", json!({})).expect("read"));
    let lease = snapshot["connections"]
        .as_array()
        .expect("connections")
        .iter()
        .find(|entry| entry["connectionId"] == json!(connection))
        .expect("connection")["leaseUntil"]
        .as_u64()
        .expect("lease");
    assert!(
        grant_deadline < lease,
        "grant {grant_deadline} must end before the connection lease {lease}"
    );
}

// ---------------------------------------------------------------------------
// 00 裁决后的三项收尾（membership_removed 标记 / snapshot 角色裁剪 / 证据成对）
// ---------------------------------------------------------------------------

/// Leaving and being removed are different facts and must publish different markers,
/// never each other's.
#[test]
fn membership_left_and_membership_removed_are_distinct_markers() {
    let harness = Harness::new();
    let room = harness.room("op-marker-create");
    let secret = harness.invite(&room, 600_000, 2);

    // player-a leaves voluntarily.
    let left_membership = harness.join(&room, &secret, "player-a", "op-marker-join-a");
    let left_id = left_membership["membershipId"]
        .as_str()
        .expect("id")
        .to_string();
    harness
        .decide(&left_id, true, "op-marker-decide-a")
        .expect("approved");
    harness.actor("player-a");
    Harness::completed(
        harness
            .mutate(
                "rooms.membership.leave",
                "op-marker-leave-a",
                json!({ "membershipId": left_id }),
            )
            .expect("left"),
    );

    // player-b is removed by the owner.
    let removed_membership = harness.join(&room, &secret, "player-b", "op-marker-join-b");
    let removed_id = removed_membership["membershipId"]
        .as_str()
        .expect("id")
        .to_string();
    harness
        .decide(&removed_id, true, "op-marker-decide-b")
        .expect("approved");
    harness.actor("fixture-owner");
    Harness::completed(
        harness
            .mutate(
                "rooms.membership.remove",
                "op-marker-remove-b",
                json!({ "membershipId": removed_id }),
            )
            .expect("removed"),
    );

    let events = harness.events();
    let payloads = payloads_of(&events, "membership.changed");
    let left_payload = payloads
        .iter()
        .find(|payload| {
            payload["approvalState"] == json!("left") && payload["membershipId"] == json!(left_id)
        })
        .expect("left event");
    let removed_payload = payloads
        .iter()
        .find(|payload| {
            payload["approvalState"] == json!("removed")
                && payload["membershipId"] == json!(removed_id)
        })
        .expect("removed event");

    assert_eq!(left_payload["evidence"], json!(["membership_left"]));
    assert_eq!(removed_payload["evidence"], json!(["membership_removed"]));
    assert!(
        !left_payload.to_string().contains("membership_removed"),
        "a voluntary leave must not claim removal"
    );
    assert!(
        !removed_payload.to_string().contains("membership_left"),
        "a removal must not claim a voluntary leave"
    );
}

/// `rooms.snapshot` is trimmed by the caller's role: the owner sees everything, a
/// member sees the room and their own record but not other members' administrative
/// fields, and an outsider sees nothing.
#[test]
fn a_member_snapshot_hides_other_members_administrative_fields() {
    let harness = Harness::new();
    harness.fixtures.signaling.set_direct_reachable(false);
    let room = harness.room("op-trim-create");
    let secret = harness.invite(&room, 600_000, 3);
    let membership_a = harness.approved_member(&room, &secret, "player-a");
    harness.actor("player-b");
    let joined = harness.join(&room, &secret, "player-b", "op-trim-join-b");
    let membership_b = joined["membershipId"].as_str().expect("id").to_string();
    harness
        .decide(&membership_b, true, "op-trim-decide-b")
        .expect("approved");
    harness.open_relay_connection("player-a", &membership_a);
    harness.open_relay_connection("player-b", &membership_b);

    // The owner sees both memberships in full, with lease/generation fields.
    harness.actor("fixture-owner");
    let owner = harness.snapshot();
    let owner_memberships = owner["memberships"].as_array().expect("memberships");
    assert_eq!(owner_memberships.len(), 2);
    for membership in owner_memberships {
        assert!(membership.get("authorizationGeneration").is_some());
        assert!(membership.get("leaseUntil").is_some());
    }
    assert_eq!(
        owner["connections"].as_array().expect("connections").len(),
        2
    );
    assert!(!owner["invites"].as_array().expect("invites").is_empty());

    // A member sees their own record in full and the other one reduced.
    harness.actor("player-a");
    let member = harness.snapshot();
    let encoded = member.to_string();
    for leaked in [
        "authorizationGeneration",
        "leaseUntil",
        "requestedAt",
        "decidedAt",
        "revision",
    ] {
        // Only the caller's own record may carry these keys.
        let others: Vec<&Value> = member["memberships"]
            .as_array()
            .expect("memberships")
            .iter()
            .filter(|entry| entry["membershipId"] != json!(membership_a))
            .collect();
        for other in others {
            assert!(
                other.get(leaked).is_none(),
                "another member's {leaked} leaked: {}",
                other
            );
        }
    }
    assert!(
        encoded.contains(&membership_b),
        "the member list is still visible"
    );
    assert_eq!(
        member["invites"].as_array().expect("invites").len(),
        0,
        "invite records are owner-only"
    );
    // The other member's connection is visible as state only, without lease/generation.
    let others: Vec<&Value> = member["connections"]
        .as_array()
        .expect("connections")
        .iter()
        .filter(|entry| entry["leaseUntil"].is_null())
        .collect();
    assert!(
        !others.is_empty(),
        "the other connection must still be listed"
    );
    for other in others {
        assert!(other.get("generation").is_none());
        assert!(other.get("leaseUntil").is_none());
    }

    // An outsider gets an empty projection.
    harness.actor("player-outsider");
    let outsider = harness.snapshot();
    assert_eq!(outsider["rooms"].as_array().expect("rooms").len(), 0);
    assert_eq!(
        outsider["memberships"]
            .as_array()
            .expect("memberships")
            .len(),
        0
    );
    assert!(!outsider.to_string().contains("player-a"));
}

/// The stale-authorization numbers follow the shared convention: `expected` is the
/// system's current truth, `actual` is what the caller presented.
#[test]
fn a_stale_authorization_reports_the_current_truth_in_expected() {
    let harness = Harness::new();
    let room = harness.room("op-stale-create");
    let secret = harness.invite(&room, 600_000, 2);
    let membership = harness.join(&room, &secret, "player-a", "op-stale-join");
    let membership_id = membership["membershipId"].as_str().expect("id").to_string();
    harness
        .decide(&membership_id, true, "op-stale-decide")
        .expect("approved");

    // Bump the permission generation to 2 by removing the approved member.
    harness.actor("fixture-owner");
    Harness::completed(
        harness
            .mutate(
                "rooms.membership.remove",
                "op-stale-remove",
                json!({ "membershipId": membership_id }),
            )
            .expect("removed"),
    );

    let id = MembershipId::new(membership_id).expect("membership id");
    let stored = harness.rooms().store().membership(&id).expect("membership");
    assert_eq!(stored.authorization_generation, 2);

    // Presenting the old generation must report current=2 / presented=1.
    let error = harness
        .rooms()
        .store()
        .require_generation(&id, 1)
        .expect_err("stale generation");
    match error {
        RoomsError::StaleAuthorization { expected, actual } => {
            assert_eq!(expected, 2, "expected must be the system's current truth");
            assert_eq!(actual, 1, "actual must be what the caller presented");
        }
        other => panic!("expected StaleAuthorization, got {other:?}"),
    }
    // A matching generation is accepted, so the gate is not simply always failing.
    harness
        .rooms()
        .store()
        .require_generation(&id, 2)
        .expect("current generation accepted");
    let encoded = serde_json::to_string(
        &RoomsError::StaleAuthorization {
            expected: 2,
            actual: 1,
        }
        .to_envelope(),
    )
    .expect("serialize");
    assert!(
        encoded.contains("current=2") && encoded.contains("presented=1"),
        "the detail reference must state the direction explicitly: {encoded}"
    );
}

/// `connection_connected` never appears alone: it always ships with the transport that
/// was really established, and never before the path exists.
#[test]
fn connection_evidence_is_always_paired_with_a_transport() {
    let harness = Harness::new();
    harness.fixtures.signaling.set_direct_reachable(false);
    let room = harness.room("op-evidence-create");
    let secret = harness.invite(&room, 600_000, 2);
    let membership = harness.approved_member(&room, &secret, "player-a");

    harness.actor("player-a");
    let task_id = Harness::accepted(
        harness
            .mutate(
                "rooms.connection.open",
                "op-evidence-open",
                json!({ "membershipId": membership }),
            )
            .expect("accepted"),
    );
    let task = harness.wait(&task_id);
    assert_eq!(task.state, TaskState::Succeeded);

    let connected = task
        .evidence
        .iter()
        .any(|marker| marker == "connection_connected");
    let transports: Vec<&String> = task
        .evidence
        .iter()
        .filter(|marker| *marker == "transport_direct" || *marker == "transport_relay")
        .collect();
    assert!(connected, "a connected task carries connection_connected");
    assert_eq!(
        transports.len(),
        1,
        "exactly one transport marker accompanies it: {:?}",
        task.evidence
    );
    assert_eq!(transports[0], "transport_relay");

    // The event stream only reaches `connected` after gathering/connecting, and only
    // the connected payload names a transport.
    let events = payloads_of(&harness.events(), "connection.changed");
    let states: Vec<String> = events
        .iter()
        .map(|payload| payload["state"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        states,
        vec!["gathering", "connecting", "connected"],
        "unexpected connection event sequence: {states:?}"
    );
    assert!(events[0].get("transport").is_none());
    assert!(events[1].get("transport").is_none());
    assert_eq!(events[2]["transport"], json!("relay"));

    // A failed attempt never claims `connection_connected`.
    harness.fixtures.relay.set_available(false);
    let retry = Harness::accepted(
        harness
            .mutate(
                "rooms.connection.open",
                "op-evidence-open-2",
                json!({ "membershipId": membership }),
            )
            .expect("accepted"),
    );
    let failed = harness.wait(&retry);
    assert_eq!(failed.state, TaskState::Failed);
    assert!(
        failed
            .evidence
            .iter()
            .all(|marker| marker != "connection_connected"),
        "a failed path must not claim a connection: {:?}",
        failed.evidence
    );
}

// ---------------------------------------------------------------------------
// D7：四条变更命令的幂等重试顺序必须一致（授权 → 重放 → 版本围栏）
// ---------------------------------------------------------------------------

/// A retry of `rooms.membership.leave` with the same operation id and an
/// `expectedRevision` must replay the committed result instead of failing the version
/// fence with the revision the operation itself produced.
#[test]
fn a_leave_retry_with_expected_revision_replays_instead_of_conflicting() {
    let harness = Harness::new();
    let room = harness.room("op-d7-create");
    let secret = harness.invite(&room, 600_000, 2);
    let membership = harness.join(&room, &secret, "player-a", "op-d7-join");
    let membership_id = membership["membershipId"].as_str().expect("id").to_string();
    let approved = harness
        .decide(&membership_id, true, "op-d7-decide")
        .expect("approved");
    let approved_revision = approved["revision"].as_u64().expect("revision");
    assert_eq!(approved_revision, 2);

    // First call commits and moves the revision to 4 (generation bump + transition).
    harness.actor("player-a");
    let first = Harness::completed(
        harness
            .mutate(
                "rooms.membership.leave",
                "op-d7-leave",
                json!({ "membershipId": membership_id, "expectedRevision": 2 }),
            )
            .expect("left"),
    );
    assert_eq!(first["approvalState"], json!("left"));
    assert_eq!(first["idempotent"], json!(false));
    let committed_revision = first["revision"].clone();
    let committed_generation = first["authorizationGeneration"].clone();

    // Same operation id + same body: the version fence must not reject the retry.
    let replay = Harness::completed(
        harness
            .mutate(
                "rooms.membership.leave",
                "op-d7-leave",
                json!({ "membershipId": membership_id, "expectedRevision": 2 }),
            )
            .expect("a committed operation must be replayable"),
    );
    assert_eq!(replay["idempotent"], json!(true));
    assert_eq!(replay["approvalState"], json!("left"));
    assert_eq!(
        replay["revision"], committed_revision,
        "no further revision bump"
    );
    assert_eq!(
        replay["authorizationGeneration"], committed_generation,
        "no further generation bump"
    );

    // A *different* request with a stale revision is still rejected by the fence.
    let error = harness
        .mutate(
            "rooms.membership.leave",
            "op-d7-leave-other",
            json!({ "membershipId": membership_id, "expectedRevision": 2 }),
        )
        .expect_err("a fresh request with a stale revision must conflict");
    assert_eq!(error.code.as_str(), "rooms.membership.conflict");
}

/// The same rule for `rooms.membership.remove`, which shares the leave transaction.
#[test]
fn a_remove_retry_with_expected_revision_replays_instead_of_conflicting() {
    let harness = Harness::new();
    let room = harness.room("op-d7b-create");
    let secret = harness.invite(&room, 600_000, 2);
    let membership = harness.join(&room, &secret, "player-a", "op-d7b-join");
    let membership_id = membership["membershipId"].as_str().expect("id").to_string();
    harness
        .decide(&membership_id, true, "op-d7b-decide")
        .expect("approved");

    harness.actor("fixture-owner");
    let first = Harness::completed(
        harness
            .mutate(
                "rooms.membership.remove",
                "op-d7b-remove",
                json!({ "membershipId": membership_id, "expectedRevision": 2 }),
            )
            .expect("removed"),
    );
    assert_eq!(first["approvalState"], json!("removed"));

    let replay = Harness::completed(
        harness
            .mutate(
                "rooms.membership.remove",
                "op-d7b-remove",
                json!({ "membershipId": membership_id, "expectedRevision": 2 }),
            )
            .expect("a committed removal must be replayable"),
    );
    assert_eq!(replay["idempotent"], json!(true));
    assert_eq!(replay["approvalState"], json!("removed"));
    assert_eq!(replay["revision"], first["revision"]);

    // The event stream did not gain a second terminal event for the replay.
    let terminal_events = payloads_of(&harness.events(), "membership.changed")
        .into_iter()
        .filter(|payload| {
            payload["membershipId"] == json!(membership_id)
                && payload["approvalState"] == json!("removed")
        })
        .count();
    assert_eq!(
        terminal_events, 1,
        "a replay must not publish a second removal"
    );
}

/// `decide`, `end` and `close_joins` follow the same order, so a retry with the
/// revision the operation itself produced still replays. This pins the four-command
/// consistency the review asked for.
#[test]
fn decide_end_and_close_joins_retries_also_replay_over_the_version_fence() {
    let harness = Harness::new();
    let room = harness.room("op-d7c-create");
    let secret = harness.invite(&room, 600_000, 2);
    let membership = harness.join(&room, &secret, "player-a", "op-d7c-join");
    let membership_id = membership["membershipId"].as_str().expect("id").to_string();
    let membership_revision = membership["revision"].as_u64().expect("revision");
    assert_eq!(membership_revision, 1);

    // decide: request revision 1, the approval moves it to 2, the retry must replay.
    harness.actor("fixture-owner");
    let approved = Harness::completed(
        harness
            .mutate(
                "rooms.membership.decide",
                "op-d7c-decide",
                json!({
                    "membershipId": membership_id,
                    "decision": "approve",
                    "expectedRevision": 1,
                }),
            )
            .expect("approved"),
    );
    let decide_replay = Harness::completed(
        harness
            .mutate(
                "rooms.membership.decide",
                "op-d7c-decide",
                json!({
                    "membershipId": membership_id,
                    "decision": "approve",
                    "expectedRevision": 1,
                }),
            )
            .expect("a committed decision must be replayable"),
    );
    assert_eq!(decide_replay["idempotent"], json!(true));
    assert_eq!(decide_replay["revision"], approved["revision"]);

    // close_joins: request revision 1, the close moves it to 2, the retry must replay.
    let closed = Harness::completed(
        harness
            .mutate(
                "rooms.close_joins",
                "op-d7c-close",
                json!({ "roomId": room.as_str(), "expectedRevision": 1 }),
            )
            .expect("closed"),
    );
    let close_replay = Harness::completed(
        harness
            .mutate(
                "rooms.close_joins",
                "op-d7c-close",
                json!({ "roomId": room.as_str(), "expectedRevision": 1 }),
            )
            .expect("a committed close must be replayable"),
    );
    assert_eq!(close_replay["state"], closed["state"]);
    assert_eq!(close_replay["state"], json!("closing"));

    // end: same shape, on the revision the close produced.
    let revision_before_end = closed["revision"].as_u64().expect("revision");
    let ended = Harness::completed(
        harness
            .mutate(
                "rooms.end",
                "op-d7c-end",
                json!({ "roomId": room.as_str(), "expectedRevision": revision_before_end }),
            )
            .expect("ended"),
    );
    assert_eq!(ended["state"], json!("ended"));
    let end_replay = Harness::completed(
        harness
            .mutate(
                "rooms.end",
                "op-d7c-end",
                json!({ "roomId": room.as_str(), "expectedRevision": revision_before_end }),
            )
            .expect("a committed end must be replayable"),
    );
    assert_eq!(end_replay["state"], json!("ended"));
    assert_eq!(end_replay["revision"], ended["revision"]);

    // And a foreign caller still cannot use the owner's end key (D2 kept).
    harness.actor("player-a");
    let error = harness
        .mutate(
            "rooms.end",
            "op-d7c-end-foreign",
            json!({ "roomId": room.as_str() }),
        )
        .expect_err("only the owner may end the room");
    assert_eq!(error.code.as_str(), "rooms.room.owner_required");
}

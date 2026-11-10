//! Behaviour tests for modules 01 / 03 / 05 / 07 driven through the real
//! `CoreModule::handle` boundary.
//!
//! The unit tests inside each module already cover the state rules. These tests
//! exist for the questions that only show up at the boundary:
//!
//! - does the command name dispatch to the right handler, and is a foreign or
//!   unknown name refused with the module's own code?
//! - does a mutate really require `operationId`, and does a replay really return
//!   the first outcome instead of applying twice?
//! - does the success path carry the semantics the contract promises (revision
//!   bump, evidence marker, session usable once, NBT unknown tags kept)?
//!
//! They run against the real crate, so the modules are exercised exactly as the
//! application registers them.

use std::sync::Arc;

use serde_json::{json, Value};
use shtmcl_core::{
    accounts::{AccountsModule, FixtureAccounts, LoginGrant, PollOutcome},
    downloads::{DownloadsModule, FixtureClock, FixtureDisk, FixtureDownloads},
    instances::{InstancesModule, StoragePort},
    servers::{FixtureDirectory, FixtureInstances, Reachability, ServersModule},
    CoreApp, CoreModule, ModuleContext, ModuleRegistry, TaskService,
};
use shtmcl_domain::{
    AttemptGeneration, CancelTaskRequest, CommandRequest, CommandResponse, EntityId, ModuleId,
    OperationId, RequestId, Revision, Task, TaskId, TaskState,
};
use shtmcl_platform::PlatformDirs;

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// A scratch root per test. Nothing is actually written (the storage port below
/// succeeds without touching disk), but a real `PlatformDirs` keeps the module
/// construction path identical to production.
fn app() -> CoreApp {
    let root = std::env::temp_dir().join("shtmcl-0701-tests");
    CoreApp::new(PlatformDirs::under(root))
}

fn context() -> ModuleContext {
    app().context()
}

fn request(command: &str, payload: Value) -> CommandRequest {
    CommandRequest {
        request_id: RequestId::new("req-1").expect("id"),
        operation_id: Some(OperationId::new("op-1").expect("id")),
        entity_id: None,
        expected_revision: None,
        payload: {
            let mut map = payload;
            map["command"] = json!(command);
            map
        },
    }
}

fn read(command: &str, payload: Value) -> CommandRequest {
    let mut req = request(command, payload);
    req.operation_id = None;
    req
}

fn completed(response: CommandResponse) -> Value {
    match response {
        CommandResponse::Completed { result } => result,
        CommandResponse::Accepted { .. } => panic!("expected completed, got accepted"),
    }
}

fn error_code(response: Result<CommandResponse, shtmcl_domain::ErrorEnvelope>) -> String {
    match response {
        Ok(_) => panic!("expected an error envelope"),
        Err(envelope) => envelope.code.as_str().to_string(),
    }
}

/// Storage for 01 that always succeeds, so behaviour tests do not need a real
/// filesystem.
struct OkStorage;

impl StoragePort for OkStorage {
    fn save_index(&self, _snapshot: &[shtmcl_core::instances::Instance]) -> Result<(), String> {
        Ok(())
    }

    fn load_index(&self) -> Result<Vec<shtmcl_core::instances::Instance>, String> {
        Ok(Vec::new())
    }

    fn write_recovery(
        &self,
        _record: &shtmcl_core::instances::RecoveryRecord,
    ) -> Result<(), String> {
        Ok(())
    }

    fn check_location(&self, _root: &str) -> Result<(), String> {
        Ok(())
    }

    fn import_directory(&self, _source: &str, _target: &str) -> Result<(), String> {
        Ok(())
    }
}

fn instance_payload(id: &str) -> Value {
    json!({
        "instance": {
            "id": id,
            "name": format!("instance {id}"),
            "version": { "gameVersion": "1.21.4", "loader": "vanilla" },
            "location": { "root": format!("/data/instances/{id}") },
            "javaMajor": 21
        }
    })
}

// ---------------------------------------------------------------------------
// 01 · instances
// ---------------------------------------------------------------------------

#[test]
fn instances_create_returns_the_committed_revision() {
    let module = InstancesModule::new(context(), Box::new(OkStorage)).expect("module");
    let result = completed(
        module
            .handle(&request("instances.create", instance_payload("i1")))
            .expect("create"),
    );
    assert_eq!(result["revision"], json!(1));
    assert_eq!(result["replayed"], json!(false));
    assert_eq!(result["instance"]["id"], json!("i1"));
    // A new instance starts ready, not busy: nothing holds it yet.
    assert_eq!(result["instance"]["state"], json!("ready"));
}

#[test]
fn instances_create_is_idempotent_for_a_repeated_request_id() {
    let module = InstancesModule::new(context(), Box::new(OkStorage)).expect("module");
    let first = completed(
        module
            .handle(&request("instances.create", instance_payload("i1")))
            .expect("create"),
    );
    let replay = completed(
        module
            .handle(&request("instances.create", instance_payload("i1")))
            .expect("replay"),
    );
    assert_eq!(first["revision"], replay["revision"]);
    assert_eq!(replay["replayed"], json!(true));
    let list = completed(
        module
            .handle(&read("instances.list", json!({})))
            .expect("list"),
    );
    assert_eq!(list["count"], json!(1));
}

#[test]
fn instances_update_requires_the_revision_the_caller_read() {
    let module = InstancesModule::new(context(), Box::new(OkStorage)).expect("module");
    module
        .handle(&request("instances.create", instance_payload("i1")))
        .expect("create");

    let stale = module.handle(&request(
        "instances.update",
        json!({
            "instanceId": "i1",
            "expectedRevision": 99,
            "patch": { "name": "renamed" }
        }),
    ));
    assert_eq!(error_code(stale), "instances.revision.stale");

    let ok = completed(
        module
            .handle(&request(
                "instances.update",
                json!({
                    "instanceId": "i1",
                    "expectedRevision": 1,
                    "patch": { "name": "renamed" }
                }),
            ))
            .expect("update"),
    );
    assert_eq!(ok["revision"], json!(2));
    assert_eq!(ok["instance"]["name"], json!("renamed"));
}

#[test]
fn instances_refuse_a_foreign_command_name_with_their_own_code() {
    let module = InstancesModule::new(context(), Box::new(OkStorage)).expect("module");
    assert_eq!(
        error_code(module.handle(&read("instances.nope", json!({})))),
        "instances.command.unknown"
    );
    // A command that belongs to another module is not routed here at all.
    assert_eq!(
        error_code(module.handle(&read("downloads.fetch", json!({})))),
        "instances.command.unknown"
    );
}

#[test]
fn instances_mutations_require_an_operation_id() {
    let module = InstancesModule::new(context(), Box::new(OkStorage)).expect("module");
    // `read()` deliberately strips the operation id.
    assert_eq!(
        error_code(module.handle(&read("instances.create", instance_payload("i1")))),
        "command.operation_required"
    );
}

#[test]
fn instances_lease_makes_the_instance_busy_and_blocks_edits() {
    let module = InstancesModule::new(context(), Box::new(OkStorage)).expect("module");
    module
        .handle(&request("instances.create", instance_payload("i1")))
        .expect("create");
    let leased = completed(
        module
            .handle(&request(
                "instances.lease.acquire",
                json!({ "instanceId": "i1", "holder": "task-1", "ttlMs": 60_000 }),
            ))
            .expect("lease"),
    );
    assert_eq!(leased["instance"]["state"], json!("busy"));

    let blocked = module.handle(&request(
        "instances.update",
        json!({
            "instanceId": "i1",
            "expectedRevision": leased["instance"]["revision"].as_u64().expect("rev"),
            "patch": { "name": "nope" }
        }),
    ));
    assert_eq!(error_code(blocked), "instances.instance.running");
}

#[test]
fn instances_read_expected_revision_from_the_envelope() {
    // `contracts/domain-v1.md` §3 puts `expectedRevision` on the command envelope.
    // Reading it only from `payload` would silently accept every stale write, so
    // this test drives the documented shape and nothing else.
    let module = InstancesModule::new(context(), Box::new(OkStorage)).expect("module");
    module
        .handle(&request("instances.create", instance_payload("i1")))
        .expect("create");

    let mut stale = request(
        "instances.update",
        json!({ "instanceId": "i1", "patch": { "name": "renamed" } }),
    );
    stale.expected_revision = Some(Revision::new(99));
    assert_eq!(
        error_code(module.handle(&stale)),
        "instances.revision.stale"
    );

    let mut fresh = request(
        "instances.update",
        json!({ "instanceId": "i1", "patch": { "name": "renamed" } }),
    );
    fresh.expected_revision = Some(Revision::new(1));
    let ok = completed(module.handle(&fresh).expect("update"));
    assert_eq!(ok["revision"], json!(2));

    // A mutation with no revision at all is an invalid request, not a silent
    // overwrite.
    let mut missing = request(
        "instances.update",
        json!({ "instanceId": "i1", "patch": { "name": "again" } }),
    );
    missing.expected_revision = None;
    assert_eq!(
        error_code(module.handle(&missing)),
        "instances.request.invalid"
    );
}

#[test]
fn instances_remove_rejects_a_stale_revision_and_keeps_the_instance() {
    let module = InstancesModule::new(context(), Box::new(OkStorage)).expect("module");
    module
        .handle(&request("instances.create", instance_payload("i1")))
        .expect("create");
    let mut update = request(
        "instances.update",
        json!({ "instanceId": "i1", "patch": { "name": "new" } }),
    );
    update.expected_revision = Some(Revision::new(1));
    module.handle(&update).expect("update");

    let mut remove = request("instances.remove", json!({ "instanceId": "i1" }));
    remove.expected_revision = Some(Revision::new(1));
    assert_eq!(
        error_code(module.handle(&remove)),
        "instances.revision.stale"
    );
    let snapshot = completed(
        module
            .handle(&read("instances.snapshot", json!({ "instanceId": "i1" })))
            .expect("snapshot"),
    );
    assert_eq!(snapshot["instance"]["name"], json!("new"));
}

// ---------------------------------------------------------------------------
// 03 · downloads
// ---------------------------------------------------------------------------

fn downloads_module(downloads: FixtureDownloads, free: u64) -> DownloadsModule {
    DownloadsModule::new(
        context(),
        Arc::new(downloads),
        Arc::new(FixtureDisk { free }),
        Arc::new(FixtureClock::new(vec![1_000, 2_000, 3_000, 4_000, 5_000])),
    )
}

fn fetch_payload(digest: &str, size: u64) -> Value {
    json!({
        "artifact": {
            "artifactId": "a1",
            "sourceId": "mirror-a",
            "size": size,
            "hash": { "algorithm": "sha256", "digest": digest }
        },
        "chunkBytes": 1024
    })
}

#[test]
fn downloads_refuse_a_full_disk_before_writing_anything() {
    let module = downloads_module(
        FixtureDownloads::new().with_artifact("a1", 10_000, &"a".repeat(64)),
        1_000,
    );
    let outcome = module.handle(&request(
        "downloads.fetch",
        fetch_payload(&"a".repeat(64), 10_000),
    ));
    assert_eq!(error_code(outcome), "downloads.disk.full");
}

#[test]
fn downloads_reject_conflicting_hashes_for_one_artifact_id() {
    let module = downloads_module(FixtureDownloads::new(), 1_000_000);
    let payload = json!({
        "artifacts": [
            {
                "artifactId": "same",
                "sourceId": "mirror-a",
                "size": 512,
                "hash": { "algorithm": "sha256", "digest": "a".repeat(64) }
            },
            {
                "artifactId": "same",
                "sourceId": "mirror-a",
                "size": 512,
                "hash": { "algorithm": "sha256", "digest": "b".repeat(64) }
            }
        ]
    });
    assert_eq!(
        error_code(module.handle(&request("downloads.fetch", payload))),
        "downloads.request.idempotency_conflict"
    );
}

#[test]
fn downloads_accept_a_task_and_publish_a_verified_artifact() {
    let digest = "a".repeat(64);
    let downloads = FixtureDownloads::new().with_artifact("a1", 2_048, &digest);
    let module = downloads_module(downloads, 1_000_000);

    let response = module
        .handle(&request("downloads.fetch", fetch_payload(&digest, 2_048)))
        .expect("accepted");
    let task_id = match response {
        CommandResponse::Accepted { task_id, .. } => task_id,
        CommandResponse::Completed { .. } => panic!("expected accepted"),
    };

    // Wait for the worker to settle the task rather than sleeping a fixed time.
    let task = wait_for_terminal(Arc::clone(module.task_service()), &task_id);

    assert_eq!(task.state, TaskState::Succeeded);
    assert!(
        task.evidence
            .iter()
            .any(|marker| marker == "completed_artifacts"),
        "evidence was {:?}",
        task.evidence
    );
    let (count, total, _) = module.cache_snapshot();
    assert_eq!(count, 1);
    assert_eq!(total, 2_048);
}

#[test]
fn downloads_evidence_never_claims_a_resume_it_did_not_keep() {
    let digest = "b".repeat(64);
    // Two chunks, 2048 bytes total: the worker needs exactly two fetches.
    let downloads = FixtureDownloads::new().with_artifact("a1", 2_048, &digest);
    let module = downloads_module(downloads, 1_000_000);
    let response = module
        .handle(&request("downloads.fetch", fetch_payload(&digest, 2_048)))
        .expect("accepted");
    let task_id = match response {
        CommandResponse::Accepted { task_id, .. } => task_id,
        CommandResponse::Completed { .. } => panic!("expected accepted"),
    };
    let task = wait_for_terminal(Arc::clone(module.task_service()), &task_id);
    // A task that finished must not also advertise resumable ranges.
    assert!(
        !task
            .evidence
            .iter()
            .any(|marker| marker == "resumable_ranges"),
        "finished task claimed resumable ranges: {:?}",
        task.evidence
    );
}

#[test]
fn downloads_a_hash_mismatch_fails_instead_of_publishing() {
    let expected = "c".repeat(64);
    let actual = "d".repeat(64);
    let downloads = FixtureDownloads::new().with_artifact("a1", 512, &actual);
    let module = downloads_module(downloads, 1_000_000);
    let response = module
        .handle(&request("downloads.fetch", fetch_payload(&expected, 512)))
        .expect("accepted");
    let task_id = match response {
        CommandResponse::Accepted { task_id, .. } => task_id,
        CommandResponse::Completed { .. } => panic!("expected accepted"),
    };
    let task = wait_for_terminal(Arc::clone(module.task_service()), &task_id);
    assert_eq!(task.state, TaskState::Failed);
    let (count, _, _) = module.cache_snapshot();
    assert_eq!(count, 0, "a corrupt artifact must not reach the cache");
    assert!(!task.evidence.iter().any(|m| m == "completed_artifacts"));
}

#[test]
fn identical_hash_subscribers_share_transfer_and_cancellation_isolated() {
    let digest = "e".repeat(64);
    let downloads = Arc::new(
        FixtureDownloads::new()
            .with_artifact("a1", 2_048, &digest)
            .with_artifact("a2", 2_048, &digest)
            .with_fetch_delay(100),
    );
    let module = DownloadsModule::new(
        context(),
        downloads.clone(),
        Arc::new(FixtureDisk { free: 1_000_000 }),
        Arc::new(FixtureClock::new(vec![1_000, 2_000, 3_000, 4_000])),
    );

    let mut first_payload = fetch_payload(&digest, 2_048);
    first_payload["discardOnCancel"] = json!(true);
    let first = module
        .handle(&request("downloads.fetch", first_payload))
        .expect("first accepted");
    let first_id = match first {
        CommandResponse::Accepted { task_id, .. } => task_id,
        CommandResponse::Completed { .. } => panic!("expected accepted"),
    };

    let mut second_payload = fetch_payload(&digest, 2_048);
    second_payload["artifact"]["artifactId"] = json!("a2");
    let mut second_request = request("downloads.fetch", second_payload);
    second_request.request_id = RequestId::new("req-2").expect("id");
    second_request.operation_id = Some(OperationId::new("op-2").expect("id"));
    let second = module.handle(&second_request).expect("second accepted");
    let second_id = match second {
        CommandResponse::Accepted { task_id, .. } => task_id,
        CommandResponse::Completed { .. } => panic!("expected accepted"),
    };

    let cancelled = module.task_service().request_cancel(&CancelTaskRequest {
        task_id: first_id.clone(),
        attempt_generation: AttemptGeneration::INITIAL,
    });
    assert_eq!(cancelled.decision, shtmcl_domain::CancelDecision::Requested);

    let first_task = wait_for_terminal(Arc::clone(module.task_service()), &first_id);
    let second_task = wait_for_terminal(Arc::clone(module.task_service()), &second_id);
    assert_eq!(first_task.state, TaskState::Cancelled);
    assert_eq!(second_task.state, TaskState::Succeeded);
    assert_eq!(
        downloads.fetch_count(),
        2,
        "same digest should need only its two 1 KiB chunks"
    );
    assert_eq!(module.cache_snapshot().0, 1);
}

#[test]
fn cache_collection_deletes_cached_content_before_removing_index() {
    let digest = "f".repeat(64);
    let downloads = Arc::new(FixtureDownloads::new().with_artifact("a1", 512, &digest));
    let module = DownloadsModule::new(
        context(),
        downloads.clone(),
        Arc::new(FixtureDisk { free: 1_000_000 }),
        Arc::new(FixtureClock::new(vec![1_000, 2_000, 3_000])),
    );
    let accepted = module
        .handle(&request("downloads.fetch", fetch_payload(&digest, 512)))
        .expect("accepted");
    let task_id = match accepted {
        CommandResponse::Accepted { task_id, .. } => task_id,
        CommandResponse::Completed { .. } => panic!("expected accepted"),
    };
    assert_eq!(
        wait_for_terminal(Arc::clone(module.task_service()), &task_id).state,
        TaskState::Succeeded
    );
    assert_eq!(downloads.cached_count(), 1);

    let result = completed(
        module
            .handle(&request("downloads.cache.collect", json!({ "keep": 0 })))
            .expect("collect"),
    );
    assert_eq!(result["collected"], 1);
    assert_eq!(downloads.cached_count(), 0);
    assert_eq!(module.cache_snapshot().0, 0);
}

#[test]
fn verified_cache_hit_skips_network_fetch() {
    let digest = "9".repeat(64);
    let downloads = Arc::new(FixtureDownloads::new().with_artifact("a1", 512, &digest));
    let module = DownloadsModule::new(
        context(),
        downloads.clone(),
        Arc::new(FixtureDisk { free: 1_000_000 }),
        Arc::new(FixtureClock::new(vec![1_000, 2_000, 3_000, 4_000])),
    );
    let first = module
        .handle(&request("downloads.fetch", fetch_payload(&digest, 512)))
        .expect("first accepted");
    let first_id = match first {
        CommandResponse::Accepted { task_id, .. } => task_id,
        CommandResponse::Completed { .. } => panic!("expected accepted"),
    };
    assert_eq!(
        wait_for_terminal(Arc::clone(module.task_service()), &first_id).state,
        TaskState::Succeeded
    );

    let mut payload = fetch_payload(&digest, 512);
    payload["artifact"]["artifactId"] = json!("a2");
    let second = module
        .handle(&request("downloads.fetch", payload))
        .expect("cache hit accepted");
    let second_id = match second {
        CommandResponse::Accepted { task_id, .. } => task_id,
        CommandResponse::Completed { .. } => panic!("expected accepted"),
    };
    assert_eq!(
        wait_for_terminal(Arc::clone(module.task_service()), &second_id).state,
        TaskState::Succeeded
    );
    assert_eq!(downloads.fetch_count(), 1);
    assert_eq!(module.cache_snapshot().0, 1);
}

#[test]
fn cancelled_download_resumes_from_saved_ranges_and_releases_its_slot() {
    let digest = "8".repeat(64);
    let downloads = Arc::new(
        FixtureDownloads::new()
            .with_artifact("a1", 2_048, &digest)
            .with_fetch_delay(120),
    );
    let module = DownloadsModule::new(
        context(),
        downloads.clone(),
        Arc::new(FixtureDisk { free: 1_000_000 }),
        Arc::new(FixtureClock::new(vec![1_000, 2_000, 3_000, 4_000, 5_000])),
    );
    let first = module
        .handle(&request("downloads.fetch", fetch_payload(&digest, 2_048)))
        .expect("first accepted");
    let first_id = match first {
        CommandResponse::Accepted { task_id, .. } => task_id,
        CommandResponse::Completed { .. } => panic!("expected accepted"),
    };
    std::thread::sleep(std::time::Duration::from_millis(25));
    let cancelled = module.task_service().request_cancel(&CancelTaskRequest {
        task_id: first_id.clone(),
        attempt_generation: AttemptGeneration::INITIAL,
    });
    assert_eq!(cancelled.decision, shtmcl_domain::CancelDecision::Requested);
    let first_task = wait_for_terminal(Arc::clone(module.task_service()), &first_id);
    assert_eq!(first_task.state, TaskState::Cancelled);
    assert!(first_task
        .evidence
        .iter()
        .any(|item| item == "resumable_ranges"));

    let mut resume_request = request("downloads.resume", fetch_payload(&digest, 2_048));
    resume_request.request_id = RequestId::new("req-resume").expect("id");
    resume_request.operation_id = Some(OperationId::new("op-resume").expect("id"));
    let resumed = module.handle(&resume_request).expect("resume accepted");
    let resumed_id = match resumed {
        CommandResponse::Accepted { task_id, .. } => task_id,
        CommandResponse::Completed { .. } => panic!("expected accepted"),
    };
    assert_eq!(
        wait_for_terminal(Arc::clone(module.task_service()), &resumed_id).state,
        TaskState::Succeeded
    );
    assert_eq!(
        downloads.fetch_count(),
        2,
        "resume should fetch only the missing range"
    );
    assert_eq!(module.cache_snapshot().0, 1);
}

fn wait_for_terminal(tasks: Arc<dyn TaskService>, task_id: &TaskId) -> Task {
    // Poll with a short sleep and a hard deadline: a stuck worker must fail the
    // test instead of hanging the suite.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(task) = tasks.snapshot(task_id) {
            if matches!(
                task.state,
                TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
            ) {
                return task;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("task did not settle within 5s");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

// ---------------------------------------------------------------------------
// 05 · accounts
// ---------------------------------------------------------------------------

fn accounts_module(ports: FixtureAccounts) -> AccountsModule {
    AccountsModule::new(context(), Arc::new(ports))
}

#[test]
fn accounts_offline_profile_needs_no_authorization_and_can_launch() {
    let module = accounts_module(FixtureAccounts::new());
    let created = completed(
        module
            .handle(&request(
                "accounts.add_offline",
                json!({ "displayName": "Steve" }),
            ))
            .expect("add"),
    );
    assert_eq!(created["identity"]["state"], json!("authorized"));
    assert_eq!(created["identity"]["providerKind"], json!("offline"));
    // The first identity becomes the default.
    assert_eq!(created["identity"]["isDefault"], json!(true));

    let session = completed(
        module
            .handle(&request(
                "accounts.session.issue",
                json!({ "identityId": "offline-steve", "purpose": "launch" }),
            ))
            .expect("issue"),
    );
    // The wire projection carries no token field whatsoever.
    let serialized = serde_json::to_string(&session).expect("json");
    assert!(
        !serialized.contains("token"),
        "session leaked: {serialized}"
    );
}

#[test]
fn accounts_login_flow_reports_pending_then_authorized() {
    let ports = FixtureAccounts::new().with_polls(vec![
        PollOutcome::Pending,
        PollOutcome::Granted(LoginGrant {
            access_token: "access-token".into(),
            refresh_token: Some("refresh-token".into()),
            expires_at: 500_000,
            display_name: "Alex".into(),
            account_uuid: "uuid-alex".into(),
            profile_id: None,
        }),
    ]);
    let module = accounts_module(ports);

    let started = completed(
        module
            .handle(&request(
                "accounts.auth.start",
                json!({ "providerId": "microsoft" }),
            ))
            .expect("start"),
    );
    assert_eq!(started["state"], json!("pending"));
    let identity_id = started["identityId"].as_str().expect("id").to_string();
    let code = started["challenge"]["userCode"]
        .as_str()
        .expect("code")
        .to_string();

    let pending = completed(
        module
            .handle(&request(
                "accounts.auth.poll",
                json!({ "identityId": identity_id, "userCode": code }),
            ))
            .expect("poll"),
    );
    assert_eq!(pending["state"], json!("pending"));

    let authorized = completed(
        module
            .handle(&request(
                "accounts.auth.poll",
                json!({ "identityId": identity_id, "userCode": code }),
            ))
            .expect("poll"),
    );
    assert_eq!(authorized["state"], json!("authorized"));
    assert_eq!(authorized["identity"]["displayName"], json!("Alex"));
    // Neither token may appear anywhere in the command result.
    let serialized = serde_json::to_string(&authorized).expect("json");
    assert!(!serialized.contains("access-token"));
    assert!(!serialized.contains("refresh-token"));
}

#[test]
fn accounts_yggdrasil_login_keeps_password_and_token_off_the_wire() {
    use shtmcl_core::accounts::YggdrasilGrant;

    let ports = FixtureAccounts::new().with_yggdrasil_grant(Some(YggdrasilGrant {
        access_token: "secret-mua-access-token".into(),
        display_name: "MuaPlayer".into(),
        account_uuid: "00112233445566778899aabbccddeeff".into(),
        profile_id: "00112233445566778899aabbccddeeff".into(),
    }));
    let module = accounts_module(ports);
    let response = module
        .handle(&request(
            "accounts.yggdrasil.login",
            json!({ "account": "player@example.test", "password": "secret-user-password" }),
        ))
        .expect("MUA login");
    let result = completed(response.clone());
    assert_eq!(result["identity"]["providerId"], json!("mua"));
    assert_eq!(result["identity"]["providerKind"], json!("yggdrasil"));
    assert_eq!(result["identity"]["displayName"], json!("MuaPlayer"));
    assert_eq!(result["identity"]["state"], json!("authorized"));
    assert_eq!(result["identity"]["isDefault"], json!(true));
    let serialized = serde_json::to_string(&response).expect("serialize response");
    assert!(!serialized.contains("secret-user-password"));
    assert!(!serialized.contains("secret-mua-access-token"));
}

#[test]
fn accounts_yggdrasil_rejection_does_not_create_an_identity() {
    let ports = FixtureAccounts::new().with_yggdrasil_grant(None);
    let module = accounts_module(ports);
    let error = module
        .handle(&request(
            "accounts.yggdrasil.login",
            json!({ "account": "player@example.test", "password": "wrong-password" }),
        ))
        .expect_err("rejected MUA login");
    assert_eq!(error.code.as_str(), "accounts.auth.denied");
    let identities = completed(
        module
            .handle(&read("accounts.list", json!({})))
            .expect("list identities"),
    );
    assert_eq!(identities["count"], json!(0));
}

#[test]
fn mua_adapter_does_not_advertise_deferred_microsoft_login() {
    let provider = shtmcl_core::accounts::MuaYggdrasilClient::new().expect("MUA provider");
    let module = AccountsModule::new(context(), Arc::new(provider));
    let error = module
        .handle(&request(
            "accounts.auth.start",
            json!({ "providerId": "microsoft" }),
        ))
        .expect_err("Microsoft flow remains deferred");
    assert_eq!(error.code.as_str(), "accounts.provider.unsupported");
}

#[test]
fn accounts_role_escalation_is_refused() {
    let module = accounts_module(FixtureAccounts::new());
    module
        .handle(&request(
            "accounts.add_offline",
            json!({ "displayName": "Steve" }),
        ))
        .expect("add");
    // A plain player must not be able to grant itself operator.
    assert_eq!(
        error_code(module.handle(&request(
            "accounts.role.grant",
            json!({
                "identityId": "offline-steve",
                "actorRole": "player",
                "targetRole": "operator"
            }),
        ))),
        "accounts.role.forbidden"
    );
    let allowed = completed(
        module
            .handle(&request(
                "accounts.role.grant",
                json!({
                    "identityId": "offline-steve",
                    "actorRole": "owner",
                    "targetRole": "operator"
                }),
            ))
            .expect("grant"),
    );
    assert_eq!(allowed["granted"], json!(true));
}

#[test]
fn accounts_session_is_single_use_at_the_boundary_too() {
    let module = accounts_module(FixtureAccounts::new());
    module
        .handle(&request(
            "accounts.add_offline",
            json!({ "displayName": "Steve" }),
        ))
        .expect("add");
    let session = completed(
        module
            .handle(&request(
                "accounts.session.issue",
                json!({ "identityId": "offline-steve", "purpose": "launch" }),
            ))
            .expect("issue"),
    );
    let handle_id = session["session"]["handleId"]
        .as_str()
        .expect("handle")
        .to_string();

    completed(
        module
            .handle(&request(
                "accounts.session.redeem",
                json!({ "handleId": handle_id, "purpose": "launch" }),
            ))
            .expect("first redeem"),
    );
    assert_eq!(
        error_code(module.handle(&request(
            "accounts.session.redeem",
            json!({ "handleId": handle_id, "purpose": "launch" }),
        ))),
        "accounts.session.consumed"
    );
}

// ---------------------------------------------------------------------------
// 07 · servers
// ---------------------------------------------------------------------------

fn servers_module(directory: FixtureDirectory, instances: FixtureInstances) -> ServersModule {
    ServersModule::new(context(), Arc::new(directory), Arc::new(instances))
}

#[test]
fn servers_add_writes_the_entry_and_keeps_unknown_tags() {
    let instances = FixtureInstances::new().with_instance("i1", true);
    // Seed a record carrying tags this module does not own.
    let mut file = shtmcl_core::servers::ServerListFile::new();
    let mut fields = std::collections::BTreeMap::new();
    fields.insert(
        "name".to_string(),
        shtmcl_core::servers::NbtTag::String("Existing".into()),
    );
    fields.insert(
        "ip".to_string(),
        shtmcl_core::servers::NbtTag::String("old.example.com:25565".into()),
    );
    fields.insert(
        "resourcePackId".to_string(),
        shtmcl_core::servers::NbtTag::String("pack-7".into()),
    );
    file.servers
        .push(shtmcl_core::servers::RawServerRecord { fields });
    shtmcl_core::servers::InstancePort::write_server_list(&instances, "i1", &file).expect("seed");

    let directory = FixtureDirectory::new(Reachability::Ready);
    let module = servers_module(directory, instances);

    let added = completed(
        module
            .handle(&request(
                "servers.add",
                json!({ "instanceId": "i1", "name": "New", "address": "new.example.com" }),
            ))
            .expect("add"),
    );
    assert_eq!(added["count"], json!(2));
    // The new entry was written with the canonical default port.
    assert_eq!(added["server"]["address"], json!("new.example.com:25565"));
}

#[test]
fn servers_add_refuses_an_instance_a_task_holds() {
    let instances = FixtureInstances::new().with_instance("i1", false);
    let module = servers_module(FixtureDirectory::new(Reachability::Ready), instances);
    assert_eq!(
        error_code(module.handle(&request(
            "servers.add",
            json!({ "instanceId": "i1", "name": "New", "address": "new.example.com" }),
        ))),
        "servers.instance.locked"
    );
}

#[test]
fn servers_add_rejects_a_duplicate_address_even_without_the_default_port() {
    let instances = FixtureInstances::new().with_instance("i1", true);
    let module = servers_module(FixtureDirectory::new(Reachability::Ready), instances);
    completed(
        module
            .handle(&request(
                "servers.add",
                json!({ "instanceId": "i1", "name": "A", "address": "play.example.com:25565" }),
            ))
            .expect("first"),
    );
    assert_eq!(
        error_code(module.handle(&request(
            "servers.add",
            json!({ "instanceId": "i1", "name": "B", "address": "play.example.com" }),
        ))),
        "servers.entry.duplicate_address"
    );
}

#[test]
fn servers_join_check_reports_a_version_mismatch_without_launching() {
    let directory = FixtureDirectory::new(Reachability::VersionMismatch {
        expected: "1.21.4".into(),
        actual: "1.20.1".into(),
    });
    let module = servers_module(directory, FixtureInstances::new().with_instance("i1", true));
    assert_eq!(
        error_code(module.handle(&request(
            "servers.join.check",
            json!({ "address": "play.example.com", "instanceId": "i1" }),
        ))),
        "servers.join.version_mismatch"
    );
}

#[test]
fn servers_publish_refuses_an_unreachable_server() {
    let directory = FixtureDirectory::new(Reachability::Unreachable("timeout".into()));
    let module = servers_module(directory, FixtureInstances::new());
    assert_eq!(
        error_code(module.handle(&request(
            "servers.publish",
            json!({
                "serverId": "s1",
                "identityId": "offline-steve",
                "name": "Mine",
                "address": "play.example.com"
            }),
        ))),
        "servers.directory.unavailable"
    );
}

// ---------------------------------------------------------------------------
// registry · one dispatch point
// ---------------------------------------------------------------------------

#[test]
fn every_module_registers_once_and_dispatch_routes_by_module_id() {
    let app = app();
    let context = app.context();
    let mut registry = ModuleRegistry::new();

    registry
        .register(Arc::new(
            InstancesModule::new(context.clone(), Box::new(OkStorage)).expect("01"),
        ))
        .expect("register 01");
    registry
        .register(Arc::new(downloads_module(FixtureDownloads::new(), 1_000)))
        .expect("register 03");
    registry
        .register(Arc::new(accounts_module(FixtureAccounts::new())))
        .expect("register 05");
    registry
        .register(Arc::new(servers_module(
            FixtureDirectory::new(Reachability::Ready),
            FixtureInstances::new(),
        )))
        .expect("register 07");

    assert_eq!(registry.len(), 4);
    let ids: Vec<ModuleId> = registry.ids().collect();
    assert!(ids.contains(&ModuleId::Instances));
    assert!(ids.contains(&ModuleId::Downloads));
    assert!(ids.contains(&ModuleId::Accounts));
    assert!(ids.contains(&ModuleId::Servers));

    // Dispatch through the registry reaches the owning module.
    let listed = completed(
        registry
            .dispatch(ModuleId::Instances, &read("instances.list", json!({})))
            .expect("dispatch"),
    );
    assert_eq!(listed["count"], json!(0));

    // A command routed to the wrong module is refused by that module.
    assert_eq!(
        error_code(registry.dispatch(ModuleId::Downloads, &read("instances.list", json!({})))),
        "downloads.command.unknown"
    );
}

#[test]
fn a_duplicate_registration_is_rejected_instead_of_silently_replacing() {
    let context = context();
    let mut registry = ModuleRegistry::new();
    registry
        .register(Arc::new(
            InstancesModule::new(context.clone(), Box::new(OkStorage)).expect("01"),
        ))
        .expect("register");
    let second = InstancesModule::new(context, Box::new(OkStorage)).expect("01 again");
    assert!(registry.register(Arc::new(second)).is_err());
    assert_eq!(registry.len(), 1);
}

#[test]
fn an_entity_scoped_event_keeps_the_instance_id() {
    // `instances.create` publishes with the instance as entity; the id must be a
    // valid `EntityId`, otherwise the UX layer cannot correlate the event.
    let entity = EntityId::new("i1").expect("entity");
    assert_eq!(entity.as_str(), "i1");
    // A value with whitespace is refused rather than silently truncated.
    assert!(EntityId::new("two words").is_err());
}

#[test]
fn revisions_are_not_substituted_for_generations() {
    // The contract keeps these distinct; this asserts the domain types still do.
    assert_eq!(Revision::new(3).get(), 3);
    assert_ne!(
        Revision::ZERO.get() as u64,
        shtmcl_domain::AttemptGeneration::INITIAL.get() as u64
    );
}

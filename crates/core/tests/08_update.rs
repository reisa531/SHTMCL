//! 08 - update and release transaction through the command surface.
//!
//! The release feed, the download, the signature verifier, the startup probe and
//! the active-work probe are the named `Fixture*` doubles from
//! `diagnostics::update`; the transaction store, the artifact identity check, the
//! version pointer and the rollback are the shipped implementation writing real
//! files under a temporary data directory.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use shtmcl_core::content::ports::FixtureInstanceWriter;
use shtmcl_core::diagnostics::update::{
    artifact_digest, ApplyOutcome, Channel, FixtureActiveWork, FixtureArtifactFetch,
    FixtureReleaseSource, FixtureSignature, FixtureStartupProbe, StartupProbe, UpdatePorts,
    UpdateStore, Updater,
};
use shtmcl_core::diagnostics::{
    module_with_ports, AppVersion, DiagnosticsModule, DiagnosticsPorts, ReleaseRecord,
};
use shtmcl_core::tasks::{EventSink, InMemoryEvents, InMemoryTasks, TaskService};
use shtmcl_core::{CoreModule, ModuleContext};
use shtmcl_domain::{
    CommandRequest, CommandResponse, OperationId, RequestId, Revision, TaskId, TaskState,
};
use shtmcl_platform::PlatformDirs;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "shtmcl-08-update-cmd-{name}-{}-{}",
        std::process::id(),
        unique()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp root");
    root
}

const ARTIFACT_BYTES: &[u8] = b"artifact-bytes";

fn release(version: &str, channel: Channel, os: &str, arch: &str) -> ReleaseRecord {
    ReleaseRecord {
        version: AppVersion::parse(version).expect("version"),
        channel,
        platform: os.to_string(),
        arch: arch.to_string(),
        min_host_version: None,
        data_schema: 1,
        artifact: shtmcl_core::diagnostics::update::ReleaseArtifact {
            artifact_id: format!("artifact-{version}"),
            sha256: artifact_digest(ARTIFACT_BYTES),
            size: ARTIFACT_BYTES.len() as u64,
            url_ref: Some("opaque-release-reference".to_string()),
            signature: Some(shtmcl_core::diagnostics::update::ArtifactSignature {
                scheme: "fixture".to_string(),
                value: "sig".to_string(),
            }),
            file_name: "app.bin".to_string(),
        },
        notes_ref: None,
    }
}

#[allow(dead_code)]
struct Fixture {
    root: PathBuf,
    module: Arc<DiagnosticsModule>,
    tasks: Arc<dyn TaskService>,
    events: Arc<dyn EventSink>,
}

impl Fixture {
    fn new(name: &str, ports: UpdatePorts) -> Self {
        let root = temp_root(name);
        let dirs = PlatformDirs::under(&root);
        dirs.create_all().expect("dirs");
        let events: Arc<dyn EventSink> = Arc::new(InMemoryEvents::new());
        let tasks: Arc<dyn TaskService> = Arc::new(InMemoryTasks::new(events.clone()));
        let ctx = ModuleContext::new(tasks.clone(), events.clone(), dirs);
        let instances = Arc::new(FixtureInstanceWriter::new(Revision::new(1)));
        let ports = DiagnosticsPorts {
            instances,
            update: ports,
            log_limits: Default::default(),
        };
        let module = module_with_ports(ctx, ports);
        Self {
            root,
            module,
            tasks,
            events,
        }
    }

    fn handle(
        &self,
        request: CommandRequest,
    ) -> Result<CommandResponse, shtmcl_domain::ErrorEnvelope> {
        self.module.handle(&request)
    }

    fn updater(&self) -> &Arc<Updater> {
        self.module.updater()
    }

    fn finish(&self, task_id: &TaskId) -> shtmcl_domain::Task {
        self.module.join_jobs();
        self.tasks.snapshot(task_id).expect("task")
    }
}

fn request(command: &str, payload: Value) -> CommandRequest {
    let mut payload = payload;
    if let Some(object) = payload.as_object_mut() {
        object.insert("command".to_string(), Value::String(command.to_string()));
    }
    CommandRequest {
        request_id: RequestId::new(format!("req-{}-{}", command, unique())).expect("request id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload,
    }
}

fn mutation(command: &str, payload: Value) -> CommandRequest {
    let mut request = request(command, payload);
    request.operation_id =
        Some(OperationId::new(format!("op-{}-{}", command, unique())).expect("operation id"));
    request
}

fn completed(response: CommandResponse) -> Value {
    match response {
        CommandResponse::Completed { result } => result,
        other => panic!("expected completed, got {other:?}"),
    }
}

fn accepted(response: CommandResponse) -> TaskId {
    match response {
        CommandResponse::Accepted { task_id, .. } => task_id,
        other => panic!("expected accepted, got {other:?}"),
    }
}

fn ports_with(
    record: ReleaseRecord,
) -> (
    UpdatePorts,
    Arc<FixtureArtifactFetch>,
    Arc<FixtureSignature>,
    Arc<FixtureStartupProbe>,
    Arc<FixtureActiveWork>,
) {
    let fetch = Arc::new(FixtureArtifactFetch::new());
    let signature = Arc::new(FixtureSignature::verified());
    let probe = Arc::new(FixtureStartupProbe::new(StartupProbe::NotReady {
        reason: "no window in this test".to_string(),
    }));
    let work = Arc::new(FixtureActiveWork::idle());
    let ports = UpdatePorts {
        source: Arc::new(FixtureReleaseSource::new(record)),
        artifacts: fetch.clone(),
        signature: signature.clone(),
        startup: probe.clone(),
        active_work: work.clone(),
    };
    (ports, fetch, signature, probe, work)
}

#[test]
fn a_version_check_rejects_a_prerelease_platform_and_channel_mismatch() {
    let (ports, _, _, _, _) = ports_with(release("1.2.0", Channel::Stable, "windows", "x86_64"));
    let fixture = Fixture::new("check", ports);

    let newer = completed(
        fixture
            .handle(request(
                "diagnostics.update.check",
                json!({
                    "currentVersion": "1.1.9",
                    "channel": "stable",
                    "platform": "windows",
                    "arch": "x86_64",
                }),
            ))
            .expect("check"),
    );
    assert_eq!(newer["verdict"], json!("update_available"));
    assert_eq!(newer["candidate"]["version"], json!("1.2.0"));
    assert!(!newer["candidate"]["artifact"]["sha256"]
        .as_str()
        .unwrap_or_default()
        .is_empty());
    assert!(
        newer["candidate"]["artifact"].get("urlRef").is_none(),
        "a release payload never exposes the download reference"
    );

    let downgrade = completed(
        fixture
            .handle(request(
                "diagnostics.update.check",
                json!({ "currentVersion": "1.3.0", "platform": "windows", "arch": "x86_64" }),
            ))
            .expect("check"),
    );
    assert_eq!(downgrade["verdict"], json!("downgrade"));

    let platform = fixture
        .handle(request(
            "diagnostics.update.check",
            json!({ "currentVersion": "1.1.9", "platform": "macos", "arch": "aarch64" }),
        ))
        .expect_err("platform mismatch");
    assert_eq!(
        platform.code.as_str(),
        "diagnostics.update.platform_mismatch"
    );

    let invalid = fixture
        .handle(request(
            "diagnostics.update.check",
            json!({ "currentVersion": "1.2", "platform": "windows", "arch": "x86_64" }),
        ))
        .expect_err("version must be strict semver");
    assert_eq!(invalid.code.as_str(), "diagnostics.update.version_invalid");
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn a_channel_mismatch_is_a_conflict_not_a_silent_switch() {
    let (ports, _, _, _, _) = ports_with(release("1.2.0", Channel::Beta, "windows", "x86_64"));
    let fixture = Fixture::new("channel", ports);
    let error = fixture
        .handle(request(
            "diagnostics.update.check",
            json!({ "currentVersion": "1.1.0", "channel": "stable" }),
        ))
        .expect_err("beta feed cannot answer a stable request");
    assert_eq!(error.code.as_str(), "diagnostics.update.channel_mismatch");
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn the_default_wiring_cannot_check_or_verify_anything() {
    let fixture = Fixture::new("unavailable", UpdatePorts::unavailable());
    let error = fixture
        .handle(request(
            "diagnostics.update.check",
            json!({ "currentVersion": "1.0.0" }),
        ))
        .expect_err("no feed");
    assert_eq!(error.code.as_str(), "diagnostics.update.unavailable");

    // Even with a candidate handed in, an unconnected verifier blocks the switch.
    let candidate = release("1.2.0", Channel::Stable, "windows", "x86_64");
    let task_id = accepted(
        fixture
            .handle(mutation(
                "diagnostics.update.prepare",
                json!({ "currentVersion": "1.0.0", "candidate": candidate }),
            ))
            .expect("accepted"),
    );
    let task = fixture.finish(&task_id);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(
        task.error.map(|error| error.code.as_str().to_string()),
        Some("diagnostics.owner.unavailable".to_string()),
        "the download owner is not connected"
    );
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn a_corrupted_download_fails_before_the_switch() {
    let candidate = release("1.2.0", Channel::Stable, "windows", "x86_64");
    let (ports, fetch, _, _, _) = ports_with(candidate.clone());
    fetch.set_corrupt(true);
    let fixture = Fixture::new("corrupt", ports);

    let task_id = accepted(
        fixture
            .handle(mutation(
                "diagnostics.update.prepare",
                json!({ "currentVersion": "1.0.0", "candidate": candidate }),
            ))
            .expect("accepted"),
    );
    let task = fixture.finish(&task_id);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(
        task.error
            .as_ref()
            .map(|error| error.code.as_str().to_string()),
        Some("diagnostics.update.artifact_mismatch".to_string())
    );
    assert!(
        fixture.updater().installed().is_none(),
        "nothing was switched"
    );
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn a_rejected_signature_is_never_reported_as_verified() {
    let candidate = release("1.2.0", Channel::Stable, "windows", "x86_64");
    let (ports, _, signature, _, _) = ports_with(candidate.clone());
    signature.set_verified(false);
    let fixture = Fixture::new("signature", ports);
    let task_id = accepted(
        fixture
            .handle(mutation(
                "diagnostics.update.prepare",
                json!({ "currentVersion": "1.0.0", "candidate": candidate }),
            ))
            .expect("accepted"),
    );
    let task = fixture.finish(&task_id);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(
        task.error
            .as_ref()
            .map(|error| error.code.as_str().to_string()),
        Some("diagnostics.update.signature_unverified".to_string())
    );
    assert!(
        !task
            .evidence
            .iter()
            .any(|marker| marker == "signature_verified"),
        "an unverified artifact never earns the evidence marker"
    );
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn the_happy_path_prepares_switches_and_confirms_the_start() {
    let candidate = release("1.2.0", Channel::Stable, "windows", "x86_64");
    let (ports, _, _, probe, work) = ports_with(candidate.clone());
    let fixture = Fixture::new("happy", ports);

    let task_id = accepted(
        fixture
            .handle(mutation(
                "diagnostics.update.prepare",
                json!({ "currentVersion": "1.0.0", "candidate": candidate }),
            ))
            .expect("accepted"),
    );
    let task = fixture.finish(&task_id);
    assert_eq!(task.state, TaskState::Succeeded);
    assert!(task
        .evidence
        .iter()
        .any(|marker| marker == "artifact_verified"));
    assert!(task
        .evidence
        .iter()
        .any(|marker| marker == "signature_verified"));
    let transaction = task.result.expect("transaction");
    assert_eq!(transaction["stage"], json!("restart_ready"));
    let transaction_id = transaction["transactionId"]
        .as_str()
        .expect("transaction id")
        .to_string();

    // A transaction record exists on disk before any switch happens.
    let stored = fixture
        .updater()
        .store()
        .load(&transaction_id)
        .expect("load")
        .expect("record");
    assert_eq!(stored.stage.as_str(), "restart_ready");

    let switched = completed(
        fixture
            .handle(mutation(
                "diagnostics.update.apply",
                json!({ "transactionId": transaction_id }),
            ))
            .expect("switch"),
    );
    assert_eq!(switched["status"], json!("switched"));
    assert_eq!(
        fixture.updater().installed().map(|pointer| pointer.version),
        Some("1.2.0".to_string())
    );

    // A second plain `apply` is a stage error: the switch happens once.
    let again = fixture
        .handle(mutation(
            "diagnostics.update.apply",
            json!({ "transactionId": transaction_id }),
        ))
        .expect_err("already switched");
    assert_eq!(again.code.as_str(), "diagnostics.update.stage_invalid");

    // Confirmation comes from the startup probe, not from a payload claim.
    probe.set(StartupProbe::Ready {
        version: "1.2.0".to_string(),
    });
    let operation = OperationId::new("op-confirm-start").expect("id");
    let mut confirm = mutation(
        "diagnostics.update.apply",
        json!({ "transactionId": transaction_id, "confirmStart": true }),
    );
    confirm.operation_id = Some(operation.clone());
    let confirmed = completed(fixture.handle(confirm).expect("confirm"));
    assert_eq!(confirmed["status"], json!("start_confirmed"));
    assert_eq!(confirmed["transaction"]["stage"], json!("completed"));
    let published = fixture.events.since(&operation, None);
    assert!(
        published
            .iter()
            .any(|event| event.kind.as_str() == "diagnostics.update.changed"),
        "the UI hears about the stage change"
    );
    let changed = published
        .iter()
        .find(|event| event.kind.as_str() == "diagnostics.update.changed")
        .expect("event");
    assert_eq!(changed.payload["status"], json!("start_confirmed"));
    assert!(
        changed.payload.get("urlRef").is_none(),
        "an event never carries the download reference"
    );
    let _ = work;
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn an_unavailable_startup_probe_leaves_the_transaction_open() {
    let candidate = release("1.2.0", Channel::Stable, "windows", "x86_64");
    let (ports, _, _, _, _) = ports_with(candidate.clone());
    // The default probe reports `Unavailable`.
    let ports = UpdatePorts {
        startup: Arc::new(FixtureStartupProbe::new(StartupProbe::Unavailable)),
        ..ports
    };
    let fixture = Fixture::new("probe", ports);
    let task_id = accepted(
        fixture
            .handle(mutation(
                "diagnostics.update.prepare",
                json!({ "currentVersion": "1.0.0", "candidate": candidate }),
            ))
            .expect("accepted"),
    );
    let transaction = fixture.finish(&task_id).result.expect("transaction");
    let transaction_id = transaction["transactionId"]
        .as_str()
        .expect("id")
        .to_string();
    fixture
        .handle(mutation(
            "diagnostics.update.apply",
            json!({ "transactionId": transaction_id }),
        ))
        .expect("switch");
    let error = fixture
        .handle(mutation(
            "diagnostics.update.apply",
            json!({ "transactionId": transaction_id, "confirmStart": true }),
        ))
        .expect_err("cannot judge the start");
    assert_eq!(
        error.code.as_str(),
        "diagnostics.update.startup_unconfirmed"
    );
    let stored = fixture
        .updater()
        .store()
        .load(&transaction_id)
        .expect("load")
        .expect("record");
    assert_eq!(stored.stage.as_str(), "start_confirmation");
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn an_interrupted_switch_is_recovered_from_the_artifact_identity() {
    let candidate = release("1.2.0", Channel::Stable, "windows", "x86_64");
    let (ports, _, _, probe, work) = ports_with(candidate.clone());
    let fixture = Fixture::new("recover", ports);

    // Version 1.0.0 is installed and launchable before the update.
    let old_dir = fixture
        .root
        .join("data")
        .join("updates")
        .join("installed")
        .join("1.0.0");
    std::fs::create_dir_all(&old_dir).expect("old dir");
    std::fs::write(old_dir.join("app.bin"), b"old-bytes").expect("old bytes");
    fixture
        .updater()
        .store()
        .set_pointer(&shtmcl_core::diagnostics::update::VersionPointer {
            version: "1.0.0".to_string(),
            artifact_id: "artifact-old".to_string(),
            sha256: artifact_digest(b"old-bytes"),
            relative_path: "installed/1.0.0/app.bin".to_string(),
        })
        .expect("pointer");

    let task_id = accepted(
        fixture
            .handle(mutation(
                "diagnostics.update.prepare",
                json!({ "currentVersion": "1.0.0", "candidate": candidate }),
            ))
            .expect("accepted"),
    );
    let transaction = fixture.finish(&task_id).result.expect("transaction");
    let transaction_id = transaction["transactionId"]
        .as_str()
        .expect("id")
        .to_string();
    let deferred = completed(
        fixture
            .handle(mutation(
                "diagnostics.update.apply",
                json!({ "transactionId": transaction_id }),
            ))
            .expect("switch"),
    );
    assert_eq!(deferred["status"], json!("switched"));

    // Power cut: the record says `switching`, the pointer never moved and the
    // installed bytes were damaged. Recovery must notice and go back.
    let store = UpdateStore::new(fixture.root.join("data").join("updates"));
    let mut stored = store.load(&transaction_id).expect("load").expect("record");
    stored.stage = shtmcl_core::diagnostics::update::Stage::Switching;
    store.save(&stored).expect("save");
    store
        .set_pointer(&shtmcl_core::diagnostics::update::VersionPointer {
            version: "1.0.0".to_string(),
            artifact_id: "artifact-old".to_string(),
            sha256: artifact_digest(b"old-bytes"),
            relative_path: "installed/1.0.0/app.bin".to_string(),
        })
        .expect("restore pointer");
    std::fs::write(
        fixture
            .root
            .join("data")
            .join("updates")
            .join("installed")
            .join("1.2.0")
            .join("app.bin"),
        b"tampered",
    )
    .expect("tamper");

    work.set(&[]);
    probe.set(StartupProbe::NotReady {
        reason: "never started".to_string(),
    });
    let recovery = completed(
        fixture
            .handle(mutation(
                "diagnostics.update.recover",
                json!({ "transactionId": transaction_id }),
            ))
            .expect("recover"),
    );
    assert_eq!(recovery["action"], json!("resumed"));
    assert_eq!(
        recovery["transaction"]["stage"],
        json!("start_confirmation")
    );
    assert_eq!(recovery["oldVersionAvailable"], json!(true));

    // The confirmation then fails, and the pointer goes back to the old version.
    let rolled = completed(
        fixture
            .handle(mutation(
                "diagnostics.update.apply",
                json!({ "transactionId": transaction_id, "confirmStart": true }),
            ))
            .expect("rollback is a settled outcome, not an error"),
    );
    assert_eq!(rolled["status"], json!("rolled_back"));
    assert_eq!(rolled["transaction"]["stage"], json!("rolled_back"));
    assert_eq!(
        fixture.updater().installed().map(|pointer| pointer.version),
        Some("1.0.0".to_string()),
        "the old version is launchable again"
    );
    assert!(
        old_dir.join("app.bin").exists(),
        "the old installation was never deleted"
    );
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn active_work_defers_the_switch_and_keeps_the_old_version() {
    let candidate = release("1.2.0", Channel::Stable, "windows", "x86_64");
    let (ports, _, _, _, work) = ports_with(candidate.clone());
    work.set(&["game", "connection"]);
    let fixture = Fixture::new("active", ports);
    let task_id = accepted(
        fixture
            .handle(mutation(
                "diagnostics.update.prepare",
                json!({ "currentVersion": "1.0.0", "candidate": candidate }),
            ))
            .expect("accepted"),
    );
    let transaction = fixture.finish(&task_id).result.expect("transaction");
    let transaction_id = transaction["transactionId"]
        .as_str()
        .expect("id")
        .to_string();
    let outcome = completed(
        fixture
            .handle(mutation(
                "diagnostics.update.apply",
                json!({ "transactionId": transaction_id }),
            ))
            .expect("deferred"),
    );
    assert_eq!(outcome["status"], json!("waiting_active_work"));
    assert_eq!(outcome["activeWork"], json!(["game", "connection"]));
    assert_eq!(outcome["transaction"]["stage"], json!("restart_ready"));
    assert!(fixture.updater().installed().is_none(), "no hard restart");

    // Once the work ends the same command goes through.
    work.set(&[]);
    let switched = completed(
        fixture
            .handle(mutation(
                "diagnostics.update.apply",
                json!({ "transactionId": transaction_id }),
            ))
            .expect("switch"),
    );
    assert_eq!(switched["status"], json!("switched"));
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn an_unknown_transaction_and_a_wrong_stage_have_stable_codes() {
    let candidate = release("1.2.0", Channel::Stable, "windows", "x86_64");
    let (ports, _, _, _, _) = ports_with(candidate.clone());
    let fixture = Fixture::new("stages", ports);

    let missing = fixture
        .handle(mutation(
            "diagnostics.update.apply",
            json!({ "transactionId": "update-does-not-exist" }),
        ))
        .expect_err("unknown transaction");
    assert_eq!(
        missing.code.as_str(),
        "diagnostics.update.transaction_not_found"
    );

    let task_id = accepted(
        fixture
            .handle(mutation(
                "diagnostics.update.prepare",
                json!({ "currentVersion": "1.0.0", "candidate": candidate }),
            ))
            .expect("accepted"),
    );
    let transaction = fixture.finish(&task_id).result.expect("transaction");
    let transaction_id = transaction["transactionId"].as_str().expect("id");
    // Confirming before switching is a stage error, not a fake success.
    let wrong = fixture
        .handle(mutation(
            "diagnostics.update.apply",
            json!({ "transactionId": transaction_id, "confirmStart": true }),
        ))
        .expect_err("wrong stage");
    assert_eq!(wrong.code.as_str(), "diagnostics.update.stage_invalid");
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn the_apply_outcome_type_is_the_documented_status_shape() {
    let waiting = ApplyOutcome::WaitingForActiveWork {
        transaction: shtmcl_core::diagnostics::update::UpdateTransaction {
            schema: "diagnostics-update.v1".to_string(),
            transaction_id: "update-1".to_string(),
            stage: shtmcl_core::diagnostics::update::Stage::RestartReady,
            current_version: "1.0.0".to_string(),
            target: release("1.2.0", Channel::Stable, "windows", "x86_64"),
            previous: None,
            staged_relative_path: None,
            artifact_verified: true,
            signature_verified: true,
            attempts: 0,
            max_attempts: 3,
            evidence: Vec::new(),
            failure: None,
        },
        active: vec!["game".to_string()],
    };
    let value = waiting.to_value();
    assert_eq!(value["status"], json!("waiting_active_work"));
    assert_eq!(value["activeWork"], json!(["game"]));
    assert_eq!(value["transaction"]["rollbackAvailable"], json!(false));
}

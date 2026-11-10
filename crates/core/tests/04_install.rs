//! 04 路 install / update / rollback behaviour: safe-point cancellation, plan
//! determinism, recovery after an interruption, and the module command flow.
//!
//! Everything here runs against explicitly named doubles
//! (`FixtureInstanceWriter`, `FixtureArtifactStager`, `MemoryJournalStore`): 01 and
//! 03 are not delivered yet, so no real instance write or download is exercised.

use std::sync::Arc;
use std::time::{Duration, Instant};

use shtmcl_core::content::install::{
    evidence, phases, planned_content_digest, planned_files as planned_files_of, InstallOutcome,
    InstallProgress, InstallRunner, TaskProgressAdapter,
};
use shtmcl_core::content::loader::{GameTarget, LoaderKind};
use shtmcl_core::content::modpack::{Hashes, PackSide, SideSupport};
use shtmcl_core::content::plan::{
    plan_changes, ContentLock, InstanceContentView, InstanceFile, LocalIdentity, LockFile,
    LockSource,
};
use shtmcl_core::content::ports::{ContentPorts, FixtureArtifactStager, FixtureInstanceWriter};
use shtmcl_core::content::recovery::{
    JournalStore, MemoryJournalStore, RecoveryKind, RecoveryRecord, Stage,
};
use shtmcl_core::content::service::module_with_ports;
use shtmcl_core::tasks::{EventSink, InMemoryEvents, InMemoryTasks, TaskService};
use shtmcl_core::{CoreApp, ModuleContext};
use shtmcl_domain::{
    AttemptGeneration, CancelTaskRequest, CommandRequest, CommandResponse, InstanceId, ModuleId,
    RequestId, Revision, Task, TaskId, TaskState,
};
use shtmcl_platform::PlatformDirs;

fn instance_id() -> InstanceId {
    InstanceId::new("instance-1").expect("instance id")
}

fn lock(paths: &[(&str, &str)]) -> ContentLock {
    let files: Vec<LockFile> = paths
        .iter()
        .map(|(path, sha1)| LockFile {
            path: (*path).to_string(),
            identity: LocalIdentity::Digest {
                sha1: Some((*sha1).to_string()),
                sha512: None,
            },
            hashes: Hashes {
                sha1: Some((*sha1).to_string()),
                sha512: None,
                sha256: None,
            },
            size: Some(8),
            urls: vec!["https://example.invalid/mod.jar".to_string()],
            source: LockSource::Download,
            client: SideSupport::Required,
            server: SideSupport::Required,
            origin: "pack:files".to_string(),
        })
        .collect();
    ContentLock::new(
        GameTarget::new("1.20.1", LoaderKind::Fabric),
        PackSide::Client,
        files,
    )
    .expect("lock")
}

fn harness() -> (
    Arc<FixtureInstanceWriter>,
    Arc<FixtureArtifactStager>,
    Arc<MemoryJournalStore>,
    ContentPorts,
) {
    let writer = Arc::new(FixtureInstanceWriter::new(Revision::new(1)));
    let stager = Arc::new(FixtureArtifactStager::new());
    let journal = Arc::new(MemoryJournalStore::new());
    let ports = ContentPorts {
        instance: writer.clone(),
        artifacts: stager.clone(),
        journal: journal.clone(),
        ..ContentPorts::fixtures()
    };
    (writer, stager, journal, ports)
}

fn module_context(tag: &str) -> (ModuleContext, Arc<InMemoryTasks>, Arc<InMemoryEvents>) {
    let events = Arc::new(InMemoryEvents::new());
    let tasks: Arc<InMemoryTasks> = Arc::new(InMemoryTasks::new(events.clone()));
    let dirs = PlatformDirs::under(
        std::env::temp_dir().join(format!("shtmcl-04-install-{tag}-{}", std::process::id())),
    );
    (
        ModuleContext::new(tasks.clone(), events.clone(), dirs),
        tasks,
        events,
    )
}

fn wait_terminal(tasks: &Arc<InMemoryTasks>, task_id: &TaskId) -> Task {
    wait_terminal_service(&(tasks.clone() as Arc<dyn TaskService>), task_id)
}

fn wait_terminal_service(tasks: &Arc<dyn TaskService>, task_id: &TaskId) -> Task {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(task) = tasks.snapshot(task_id) {
            if task.state.is_terminal() {
                return task;
            }
        }
        assert!(
            Instant::now() < deadline,
            "task {task_id} did not reach a terminal state"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn accepted(response: CommandResponse) -> TaskId {
    match response {
        CommandResponse::Accepted { task_id, .. } => task_id,
        other => panic!("expected an accepted response, got {other:?}"),
    }
}

fn request(command: &str, payload: serde_json::Value) -> CommandRequest {
    CommandRequest {
        request_id: RequestId::new(format!("req-{command}")).expect("request id"),
        operation_id: Some(shtmcl_domain::OperationId::new(format!("op-{command}")).expect("op")),
        entity_id: None,
        expected_revision: None,
        payload,
    }
}

/// Requests cancellation exactly when the runner enters the target phase, using
/// the frozen `TaskService::request_cancel` entry point and nothing else.
struct CancelAtPhase {
    adapter: TaskProgressAdapter,
    tasks: Arc<InMemoryTasks>,
    task_id: TaskId,
    generation: AttemptGeneration,
    target: String,
}

impl InstallProgress for CancelAtPhase {
    fn enter_phase(&self, phase: &str) -> Result<(), shtmcl_core::content::error::ContentError> {
        let result = self.adapter.enter_phase(phase);
        if phase == self.target {
            self.tasks.request_cancel(&CancelTaskRequest {
                task_id: self.task_id.clone(),
                attempt_generation: self.generation,
            });
        }
        result
    }

    fn evidence(&self, marker: &str) {
        self.adapter.evidence(marker);
    }

    fn cancel_requested(&self) -> bool {
        self.adapter.cancel_requested()
    }

    fn advance(
        &self,
        done: u64,
        total: u64,
    ) -> Result<(), shtmcl_core::content::error::ContentError> {
        self.adapter.advance(done, total)
    }
}

#[test]
fn cancellation_at_a_safe_point_rolls_back_and_never_commits() {
    let (writer, stager, journal, ports) = harness();
    let events = Arc::new(InMemoryEvents::new());
    let tasks: Arc<InMemoryTasks> = Arc::new(InMemoryTasks::new(events));
    let task = tasks
        .accept(shtmcl_core::tasks::TaskSpec::new(
            shtmcl_domain::OperationId::new("op-cancel").expect("op"),
            shtmcl_domain::TaskPhase::new(phases::PLAN).expect("phase"),
        ))
        .expect("accept");

    let progress = CancelAtPhase {
        adapter: TaskProgressAdapter::new(tasks.clone(), task.id.clone(), task.attempt_generation),
        tasks: tasks.clone(),
        task_id: task.id.clone(),
        generation: task.attempt_generation,
        target: phases::VERIFY.to_string(),
    };
    let runner = InstallRunner::new(&ports, &progress);
    let view = writer.view_of(&instance_id()).expect("view");
    let plan = plan_changes("plan-1", &view, &lock(&[("mods/a.jar", "aaaa")])).expect("plan");

    let outcome = runner.apply(&plan, &view).expect("apply");
    assert!(outcome.is_cancelled(), "a pre-commit cancel is a rollback");
    match outcome {
        InstallOutcome::RolledBack { at_phase, .. } => assert_eq!(at_phase, phases::VERIFY),
        other => panic!("expected a rollback, got {other:?}"),
    }

    // The safe stop is confirmed only now, through the task port.
    let stopped = tasks
        .cancel_now(&task.id, task.attempt_generation)
        .expect("safe stop");
    assert_eq!(stopped.state, TaskState::Cancelled);
    for marker in [evidence::SAFE_STOP_REACHED, evidence::STAGING_DISCARDED] {
        assert!(
            stopped.evidence.iter().any(|value| value == marker),
            "missing evidence {marker}"
        );
    }
    assert!(!stopped
        .evidence
        .iter()
        .any(|value| value == evidence::COMMIT_CONFIRMED));

    assert!(
        writer.commits().is_empty(),
        "no instance write after a cancel"
    );
    assert_eq!(writer.revision(), Revision::new(1));
    assert_eq!(stager.discarded_plans(), vec!["plan-1".to_string()]);
    assert!(journal.read("instance-1").expect("read").is_none());
}

#[test]
fn plan_then_apply_then_rollback_restores_the_previous_state() {
    let (writer, _stager, journal, ports) = harness();
    writer.seed(vec![InstanceFile::new("mods/old.jar", "dddd", 8)]);
    let (context, tasks, events) = module_context("flow");
    let module = module_with_ports(context, ports);

    let before = writer.files();
    let current_lock = lock(&[("mods/a.jar", "aaaa")]);

    // 1. plan: a task, not a blocking call.
    let plan_request = CommandRequest {
        expected_revision: Some(Revision::new(1)),
        ..request(
            "content.plan",
            serde_json::json!({
                "command": "content.plan",
                "instanceId": "instance-1",
                "lock": current_lock.to_value(),
            }),
        )
    };
    let plan_response = module.handle(&plan_request).expect("plan accepted");
    let plan_operation = match &plan_response {
        CommandResponse::Accepted { operation_id, .. } => operation_id.clone(),
        other => panic!("expected an accepted response, got {other:?}"),
    };
    let plan_task = accepted(plan_response);
    let settled = wait_terminal(&tasks, &plan_task);
    assert_eq!(settled.state, TaskState::Succeeded, "{:?}", settled.error);
    let result = settled.result.clone().expect("plan result");
    assert_eq!(result["counts"]["add"], 1);
    assert_eq!(result["counts"]["delete"], 1);
    let plan_id = result["planId"].as_str().expect("plan id").to_string();
    let plan_digest = result["planDigest"].as_str().expect("digest").to_string();
    assert!(
        plan_digest.starts_with("sha256:"),
        "plan digest is a real SHA-256, got {plan_digest}"
    );
    assert!(writer.commits().is_empty(), "planning writes nothing");

    // The plan is announced through the module event stream, not through UI callbacks.
    let stream = events.since(&plan_operation, None);
    let ready = stream
        .iter()
        .find(|event| event.kind.as_str() == "content.plan_ready")
        .expect("content.plan_ready must be published");
    assert_eq!(ready.payload["planId"], plan_id.as_str());
    assert_eq!(ready.revision, Revision::new(1));

    // 2. apply: the change lands through the instance port only.
    let apply_response = module
        .handle(&request(
            "content.apply",
            serde_json::json!({
                "command": "content.apply",
                "instanceId": "instance-1",
                "planId": plan_id,
                "planDigest": plan_digest,
            }),
        ))
        .expect("apply accepted");
    let apply_operation = match &apply_response {
        CommandResponse::Accepted { operation_id, .. } => operation_id.clone(),
        other => panic!("expected an accepted response, got {other:?}"),
    };
    let apply_task = accepted(apply_response);
    let settled = wait_terminal(&tasks, &apply_task);
    assert_eq!(settled.state, TaskState::Succeeded, "{:?}", settled.error);
    for marker in [evidence::COMMIT_CONFIRMED] {
        assert!(
            settled.evidence.iter().any(|value| value == marker),
            "missing evidence {marker}"
        );
    }
    let changed = events
        .since(&apply_operation, None)
        .into_iter()
        .find(|event| event.kind.as_str() == "content.changed")
        .expect("content.changed must be published after a commit");
    assert_eq!(
        changed.revision,
        Revision::new(2),
        "an entity event carries the new revision"
    );
    let installed = writer.files();
    assert_eq!(
        installed
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["mods/a.jar"]
    );
    assert_eq!(writer.commits().len(), 1);
    assert!(journal.read("instance-1").expect("read").is_some());

    // 3. rollback: the recorded previous set comes back.
    let rollback_task = accepted(
        module
            .handle(&request(
                "content.rollback",
                serde_json::json!({ "command": "content.rollback", "instanceId": "instance-1" }),
            ))
            .expect("rollback accepted"),
    );
    let settled = wait_terminal(&tasks, &rollback_task);
    assert_eq!(settled.state, TaskState::Succeeded, "{:?}", settled.error);
    assert!(settled
        .evidence
        .iter()
        .any(|value| value == evidence::ROLLBACK_APPLIED));
    assert_eq!(
        writer.files(),
        before,
        "rollback restores the file identities"
    );
    assert!(journal.read("instance-1").expect("read").is_none());
}

#[test]
fn a_stale_expected_revision_fails_the_plan_task_without_writing() {
    let (writer, _stager, _journal, ports) = harness();
    let (context, tasks, _events) = module_context("stale");
    let module = module_with_ports(context, ports);

    let plan_request = CommandRequest {
        expected_revision: Some(Revision::new(7)),
        ..request(
            "content.plan",
            serde_json::json!({
                "command": "content.plan",
                "instanceId": "instance-1",
                "lock": lock(&[("mods/a.jar", "aaaa")]).to_value(),
            }),
        )
    };
    let task_id = accepted(module.handle(&plan_request).expect("accepted"));
    let settled = wait_terminal(&tasks, &task_id);
    assert_eq!(settled.state, TaskState::Failed);
    assert_eq!(
        settled.error.as_ref().expect("error").code.as_str(),
        "content.plan.conflict"
    );
    assert!(writer.commits().is_empty());
}

#[test]
fn a_restricted_file_stops_before_any_write() {
    let (writer, _stager, journal, ports) = harness();
    let (context, tasks, _events) = module_context("restricted");
    let module = module_with_ports(context, ports);

    let mut restricted = lock(&[("mods/a.jar", "aaaa")]);
    restricted = ContentLock::new(
        restricted.target.clone(),
        restricted.side,
        restricted
            .files
            .iter()
            .cloned()
            .map(|mut file| {
                file.urls.clear();
                file
            })
            .collect(),
    )
    .expect("restricted lock");

    let plan_task = accepted(
        module
            .handle(&request(
                "content.plan",
                serde_json::json!({
                    "command": "content.plan",
                    "instanceId": "instance-1",
                    "lock": restricted.to_value(),
                }),
            ))
            .expect("accepted"),
    );
    let settled = wait_terminal(&tasks, &plan_task);
    let result = settled.result.expect("plan result");
    let plan_id = result["planId"].as_str().expect("plan id").to_string();
    assert_eq!(result["requiredArtifacts"][0]["restricted"], true);

    let apply_task = accepted(
        module
            .handle(&request(
                "content.apply",
                serde_json::json!({
                    "command": "content.apply",
                    "instanceId": "instance-1",
                    "planId": plan_id,
                }),
            ))
            .expect("accepted"),
    );
    let settled = wait_terminal(&tasks, &apply_task);
    assert_eq!(settled.state, TaskState::Failed);
    assert_eq!(
        settled.error.as_ref().expect("error").code.as_str(),
        "content.pack.restricted_file"
    );
    assert!(settled
        .evidence
        .iter()
        .any(|value| value == evidence::RESTRICTED_ARTIFACTS_PENDING));
    assert!(writer.commits().is_empty());
    assert!(journal.read("instance-1").expect("read").is_none());
}

#[test]
fn resume_discards_staging_and_finalizes_a_landed_commit() {
    let (writer, _stager, journal, ports) = harness();
    let progress = shtmcl_core::content::install::FixtureProgress::new();
    let runner = InstallRunner::new(&ports, &progress);
    let view = writer.view_of(&instance_id()).expect("view");
    let plan = plan_changes("plan-1", &view, &lock(&[("mods/a.jar", "aaaa")])).expect("plan");

    // Interrupted while staging: nothing was committed, staging is dropped.
    journal
        .write(&RecoveryRecord::new(
            "instance-1",
            "plan-1",
            plan.plan_digest.clone(),
            RecoveryKind::Apply,
            view.revision,
            view.files.clone(),
            planned_content_digest(&view, &plan),
            planned_files_of(&view, &plan),
        ))
        .expect("write");
    let resumed = runner.resume(&view).expect("resume");
    assert!(matches!(
        resumed,
        InstallOutcome::Resumed {
            status: shtmcl_core::content::install::ResumeStatus::DiscardedStaging
        }
    ));
    assert!(journal.read("instance-1").expect("read").is_none());

    // Interrupted while committing, and the commit had landed: finalize instead of
    // re-downloading or re-resolving anything.
    let committed_plan =
        plan_changes("plan-2", &view, &lock(&[("mods/b.jar", "bbbb")])).expect("plan");
    let mut record = RecoveryRecord::new(
        "instance-1",
        "plan-2",
        committed_plan.plan_digest.clone(),
        RecoveryKind::Apply,
        view.revision,
        view.files.clone(),
        planned_content_digest(&view, &committed_plan),
        planned_files_of(&view, &committed_plan),
    );
    record.stage = Stage::CommitStarted;
    journal.write(&record).expect("write");

    // Pretend the commit landed by applying the same plan through the port.
    ports.instance.apply(&committed_plan).expect("commit");
    let view_after = writer.view_of(&instance_id()).expect("view");
    let resumed = runner.resume(&view_after).expect("resume");
    assert!(matches!(
        resumed,
        InstallOutcome::Resumed {
            status: shtmcl_core::content::install::ResumeStatus::FinalizedCommit
        }
    ));
    assert_eq!(
        journal
            .read("instance-1")
            .expect("read")
            .map(|record| record.stage),
        Some(Stage::Committed),
        "a committed record stays as the rollback point"
    );

    // A second resume has nothing to do.
    let again = runner.resume(&view_after).expect("resume");
    assert!(matches!(
        again,
        InstallOutcome::Resumed {
            status: shtmcl_core::content::install::ResumeStatus::AlreadyCommitted
        }
    ));
}

#[test]
fn the_resume_command_reports_the_recovery_status() {
    let (writer, _stager, journal, ports) = harness();
    let (context, tasks, _events) = module_context("resume");
    let module = module_with_ports(context, ports);

    let view = writer.view_of(&instance_id()).expect("view");
    let plan = plan_changes("plan-9", &view, &lock(&[("mods/a.jar", "aaaa")])).expect("plan");
    journal
        .write(&RecoveryRecord::new(
            "instance-1",
            "plan-9",
            plan.plan_digest.clone(),
            RecoveryKind::Apply,
            view.revision,
            view.files.clone(),
            planned_content_digest(&view, &plan),
            planned_files_of(&view, &plan),
        ))
        .expect("write");

    let task_id = accepted(
        module
            .handle(&request(
                "content.recovery.resume",
                serde_json::json!({
                    "command": "content.recovery.resume",
                    "instanceId": "instance-1",
                }),
            ))
            .expect("accepted"),
    );
    let settled = wait_terminal(&tasks, &task_id);
    assert_eq!(settled.state, TaskState::Succeeded, "{:?}", settled.error);
    assert_eq!(
        settled.result.expect("result")["status"],
        "discarded_staging"
    );
    assert!(writer.commits().is_empty());
}

#[test]
fn the_change_plan_is_ordered_deduplicated_and_digest_stable() {
    let (_writer, _stager, _journal, ports) = harness();
    let view = InstanceContentView::new(
        instance_id(),
        Revision::new(2),
        vec![InstanceFile::new("mods/old.jar", "dddd", 1)],
    );

    let first = plan_changes(
        "plan-1",
        &view,
        &lock(&[("mods/b.jar", "bbbb"), ("mods/a.jar", "aaaa")]),
    )
    .expect("plan");
    let second = plan_changes(
        "plan-1",
        &view,
        &lock(&[("mods/a.jar", "aaaa"), ("mods/b.jar", "bbbb")]),
    )
    .expect("plan");

    assert_eq!(first.entries, second.entries);
    assert_eq!(first.plan_digest, second.plan_digest);
    assert_eq!(
        first
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["mods/a.jar", "mods/b.jar", "mods/old.jar"]
    );
    assert_eq!(first.counts.total, 3);
    assert_eq!(first.counts.bytes, 16);
    assert_eq!(first.required_artifacts.len(), 2);
    drop(ports);
}

/// The application layer dispatches through `CoreApp` only, which also enforces the
/// command namespace (`content.*` belongs to `ModuleId::Content`). This test exists
/// because a command that bypasses that entry point can hide a namespace bug.
#[test]
fn commands_are_dispatched_through_the_single_app_entry() {
    let (writer, _stager, _journal, ports) = harness();
    let dirs = PlatformDirs::under(
        std::env::temp_dir().join(format!("shtmcl-04-dispatch-{}", std::process::id())),
    );
    let mut app = CoreApp::new(dirs);
    let context = app.context();
    app.register(module_with_ports(context, ports))
        .expect("register content module");
    assert_eq!(app.module_ids(), vec![ModuleId::Content]);

    // 1. A read reaches the module through `dispatch_read`.
    let loader_read = CommandRequest {
        request_id: RequestId::new("req-loader").expect("id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload: serde_json::json!({
            "command": "content.loader.plan",
            "loader": "fabric",
            "gameVersion": "1.20.1",
        }),
    };
    match app
        .dispatch_read(ModuleId::Content, &loader_read)
        .expect("loader plan")
    {
        CommandResponse::Completed { result } => {
            assert_eq!(result["loader"], "fabric");
            assert!(result["libraries"].as_array().expect("libraries").len() >= 2);
        }
        other => panic!("expected a completed response, got {other:?}"),
    }

    // 2. A pack read inside the `content.packs.*` namespace passes the owner check.
    let manifest = serde_json::json!({
        "formatVersion": 1,
        "game": "minecraft",
        "name": "dispatch fixture",
        "versionId": "1.0.0",
        "files": [{
            "path": "mods/a.jar",
            "hashes": { "sha1": "a".repeat(40) },
            "downloads": ["https://example.invalid/a.jar"],
            "fileSize": 12
        }],
        "dependencies": { "minecraft": "1.20.1", "fabric-loader": "0.15.11" }
    });
    let inspect_read = CommandRequest {
        request_id: RequestId::new("req-inspect").expect("id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload: serde_json::json!({
            "command": "content.packs.inspect",
            "manifest": manifest,
        }),
    };
    match app
        .dispatch_read(ModuleId::Content, &inspect_read)
        .expect("inspect")
    {
        CommandResponse::Completed { result } => {
            assert_eq!(result["format"], "mrpack");
            assert_eq!(result["fileCount"], 1);
            assert_eq!(result["target"]["loader"], "fabric");
        }
        other => panic!("expected a completed response, got {other:?}"),
    }

    // 3. A mutation goes through `dispatch_mutation` and produces a real task.
    let plan_request = CommandRequest {
        expected_revision: Some(Revision::new(1)),
        ..request(
            "content.plan",
            serde_json::json!({
                "command": "content.plan",
                "instanceId": "instance-1",
                "lock": lock(&[("mods/a.jar", "aaaa")]).to_value(),
            }),
        )
    };
    let task_id = accepted(
        app.dispatch_mutation(ModuleId::Content, &plan_request)
            .expect("plan accepted"),
    );
    let settled = wait_terminal_service(app.task_service(), &task_id);
    assert_eq!(settled.state, TaskState::Succeeded, "{:?}", settled.error);
    assert!(writer.commits().is_empty(), "planning still writes nothing");

    // 4. A command from another module's namespace is refused at the entry point.
    let foreign = CommandRequest {
        request_id: RequestId::new("req-foreign").expect("id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload: serde_json::json!({ "command": "launch.prepare" }),
    };
    let error = app
        .dispatch_read(ModuleId::Content, &foreign)
        .expect_err("namespace mismatch");
    assert_eq!(error.code.as_str(), "command.name_mismatch");
}

/// `content::module(ctx)` must never wire a fake instance writer: with 01/03 absent
/// every command that needs them reports the missing owner.
#[test]
fn the_default_module_fails_closed() {
    let (context, tasks, _events) = module_context("unavailable");
    let module = shtmcl_core::content::module(context);

    // Vanilla needs no loader metadata, so it still answers.
    let vanilla = CommandRequest {
        request_id: RequestId::new("req-vanilla").expect("id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload: serde_json::json!({
            "command": "content.loader.plan",
            "loader": "vanilla",
            "gameVersion": "1.20.1",
        }),
    };
    assert!(matches!(
        module.handle(&vanilla).expect("vanilla plan"),
        CommandResponse::Completed { .. }
    ));

    // A loader that needs the platform catalog reports the missing owner.
    let fabric = CommandRequest {
        payload: serde_json::json!({
            "command": "content.loader.plan",
            "loader": "fabric",
            "gameVersion": "1.20.1",
        }),
        ..vanilla
    };
    let error = module.handle(&fabric).expect_err("no loader catalog");
    assert_eq!(error.code.as_str(), "content.owner.unavailable");

    // Planning a change needs the instance projection: the task fails and nothing is
    // written (no fixture writer is involved at all).
    let plan_request = CommandRequest {
        expected_revision: Some(Revision::new(1)),
        ..request(
            "content.plan",
            serde_json::json!({
                "command": "content.plan",
                "instanceId": "instance-1",
                "lock": lock(&[("mods/a.jar", "aaaa")]).to_value(),
            }),
        )
    };
    let task_id = accepted(module.handle(&plan_request).expect("accepted"));
    let settled = wait_terminal(&tasks, &task_id);
    assert_eq!(settled.state, TaskState::Failed);
    assert_eq!(
        settled.error.as_ref().expect("error").code.as_str(),
        "content.owner.unavailable"
    );
}

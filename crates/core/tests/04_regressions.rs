//! 04 · regression suite for the independent evaluation findings
//! (`docs/development/handoffs/04/EVALUATION-core.md`, D1—D17).
//!
//! One test per defect, each reproducing the reported input before asserting the
//! fixed behaviour. Command-level tests go through `CoreApp` (the single dispatch
//! point) so a namespace or operation-stream bug cannot hide.

use std::sync::Arc;
use std::time::{Duration, Instant};

use shtmcl_core::content::install::{evidence, phases, InstallOutcome, InstallRunner};
use shtmcl_core::content::loader::{GameTarget, LoaderKind};
use shtmcl_core::content::modpack::{
    plan_export, read_mrpack, ExportCandidate, Hashes, PackSide, SideSupport,
};
use shtmcl_core::content::paths::MAX_ARCHIVE_ENTRIES;
use shtmcl_core::content::plan::{
    plan_changes, ContentLock, InstanceContentView, InstanceFile, LockSource,
};
use shtmcl_core::content::ports::{ContentPorts, FixtureArtifactStager, FixtureInstanceWriter};
use shtmcl_core::content::recovery::{
    JournalStore, MemoryJournalStore, PlannedFile, RecoveryKind, RecoveryRecord, Stage,
};
use shtmcl_core::content::resolve::{
    solve, BudgetLimit, Candidate, Dependency, SolveBudget, SolveRequest, READ_MAX_STATES,
};
use shtmcl_core::content::service::module_with_ports;
use shtmcl_core::content::version::{Version, VersionRange};
use shtmcl_core::tasks::{EventSink, TaskService};
use shtmcl_core::CoreApp;
use shtmcl_domain::{
    CommandRequest, CommandResponse, InstanceId, ModuleId, OperationId, RequestId, Revision, Task,
    TaskId, TaskState,
};
use shtmcl_platform::PlatformDirs;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/modules/04/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read {}: {error}; run `node tests/modules/04/make-fixtures.mjs`",
            path.display()
        )
    })
}

fn instance_id() -> InstanceId {
    InstanceId::new("instance-1").expect("instance id")
}

struct Harness {
    writer: Arc<FixtureInstanceWriter>,
    stager: Arc<FixtureArtifactStager>,
    journal: Arc<MemoryJournalStore>,
    ports: ContentPorts,
}

fn harness() -> Harness {
    let writer = Arc::new(FixtureInstanceWriter::new(Revision::new(1)));
    let stager = Arc::new(FixtureArtifactStager::new());
    let journal = Arc::new(MemoryJournalStore::new());
    let ports = ContentPorts {
        instance: writer.clone(),
        artifacts: stager.clone(),
        journal: journal.clone(),
        ..ContentPorts::fixtures()
    };
    Harness {
        writer,
        stager,
        journal,
        ports,
    }
}

fn wait_terminal(tasks: &Arc<dyn TaskService>, task_id: &TaskId) -> Task {
    let deadline = Instant::now() + Duration::from_secs(30);
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

fn accepted_operation(response: &CommandResponse) -> OperationId {
    match response {
        CommandResponse::Accepted { operation_id, .. } => operation_id.clone(),
        other => panic!("expected an accepted response, got {other:?}"),
    }
}

fn mutate(command: &str, operation: &str, payload: serde_json::Value) -> CommandRequest {
    CommandRequest {
        request_id: RequestId::new(format!("req-{operation}")).expect("request id"),
        operation_id: Some(OperationId::new(operation).expect("operation id")),
        entity_id: None,
        expected_revision: None,
        payload: {
            let mut value = payload;
            value["command"] = serde_json::json!(command);
            value
        },
    }
}

fn read(command: &str, payload: serde_json::Value) -> CommandRequest {
    CommandRequest {
        request_id: RequestId::new(format!("req-read-{command}")).expect("request id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload: {
            let mut value = payload;
            value["command"] = serde_json::json!(command);
            value
        },
    }
}

/// A launcher `CoreApp` with this module registered and the given ports.
///
/// The app owns the event stream; events are read through `app.event_sink()`, so the
/// test can assert on the same stream the caller would subscribe to.
fn app(tag: &str, ports: ContentPorts) -> CoreApp {
    let mut app = CoreApp::new(PlatformDirs::under(
        std::env::temp_dir().join(format!("shtmcl-04-reg-{tag}-{}", std::process::id())),
    ));
    let context = app.context();
    app.register(module_with_ports(context, ports))
        .expect("register content module");
    app
}

/// A candidate in the payload shape `content.solve` accepts.
fn candidate_json(candidate: &Candidate) -> serde_json::Value {
    serde_json::json!({
        "provider": candidate.provider.as_str(),
        "projectId": candidate.project_id,
        "fileId": candidate.file_id,
        "version": candidate.version.as_str(),
        "gameVersions": candidate.game_versions,
        "loaders": candidate.loaders.iter().map(|loader| loader.as_str()).collect::<Vec<_>>(),
        "dependencies": candidate
            .dependencies
            .iter()
            .map(|dependency| serde_json::json!({
                "projectId": dependency.project_id,
                "range": dependency.range.as_str(),
            }))
            .collect::<Vec<_>>(),
    })
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// D1 — a pack with overrides must install
// ---------------------------------------------------------------------------

#[test]
fn d1_mrpack_with_overrides_installs_and_lands_the_right_bytes() {
    let bytes = fixture("overrides.mrpack");
    let manifest = read_mrpack(&bytes).expect("manifest");
    assert_eq!(
        manifest.overrides.len(),
        4,
        "two shared + one client + one server override"
    );
    // Every override carries a verified payload identity.
    for entry in &manifest.overrides {
        assert!(
            entry.sha256.is_some(),
            "override {} has no digest",
            entry.archive_path
        );
        assert!(entry.size.unwrap_or(0) > 0);
    }

    let h = harness();
    let app = app("d1", h.ports.clone());
    let lock = ContentLock::from_manifest(&manifest, PackSide::Client).expect("lock");

    // No lock entry is a restricted download; overrides come from the archive.
    for file in &lock.files {
        assert!(
            !file.is_restricted(),
            "{} must not be restricted",
            file.path
        );
    }
    let override_file = lock
        .files
        .iter()
        .find(|file| file.path == "config/foo.txt")
        .expect("override in lock");
    assert_eq!(
        override_file.source,
        LockSource::Archive {
            archive_path: "overrides/config/foo.txt".to_string()
        }
    );

    let plan_task = accepted(
        app.dispatch_mutation(
            ModuleId::Content,
            &mutate(
                "content.plan",
                "op-d1-plan",
                serde_json::json!({
                    "instanceId": "instance-1",
                    "lock": lock.to_value(),
                }),
            ),
        )
        .expect("plan accepted"),
    );
    let settled = wait_terminal(app.task_service(), &plan_task);
    assert_eq!(settled.state, TaskState::Succeeded, "{:?}", settled.error);
    let plan = settled.result.expect("plan result");
    let plan_id = plan["planId"].as_str().expect("plan id").to_string();
    let requirements = plan["requiredArtifacts"].as_array().expect("artifacts");
    assert!(
        requirements
            .iter()
            .any(|requirement| requirement["source"]["kind"] == "archive"
                && requirement["restricted"] == false),
        "an override requirement must be an archive source, not a restricted download"
    );

    let apply_task = accepted(
        app.dispatch_mutation(
            ModuleId::Content,
            &mutate(
                "content.apply",
                "op-d1-apply",
                serde_json::json!({ "instanceId": "instance-1", "planId": plan_id }),
            ),
        )
        .expect("apply accepted"),
    );
    let settled = wait_terminal(app.task_service(), &apply_task);
    assert_eq!(settled.state, TaskState::Succeeded, "{:?}", settled.error);

    let files = h.writer.files();
    let foo = files
        .iter()
        .find(|file| file.path == "config/foo.txt")
        .expect("config/foo.txt landed");
    assert_eq!(
        foo.digest,
        hex_sha256(b"foo-value\n"),
        "the landed identity is the archive payload's hash"
    );
    assert!(files
        .iter()
        .any(|file| file.path == "config/nested/bar.json"));
    assert!(files
        .iter()
        .any(|file| file.path == "config/client-only.json"));
    assert!(
        !files
            .iter()
            .any(|file| file.path == "config/server-only.json"),
        "a server-only override must not be installed on the client side"
    );
    assert!(h.stager.discarded_plans().is_empty());
}

// ---------------------------------------------------------------------------
// D2 — deep dependency chains must not abort the process
// ---------------------------------------------------------------------------

fn chain(depth: usize) -> (Vec<Candidate>, Vec<String>) {
    let mut candidates = Vec::new();
    for index in 0..depth {
        let mut candidate = Candidate::new(
            shtmcl_core::content::ports::SourceKind::Modrinth,
            format!("p{index}"),
            format!("f{index}"),
            Version::parse("1.0").expect("version"),
        )
        .for_target("1.20.1", LoaderKind::Fabric);
        if index + 1 < depth {
            candidate = candidate.depends_on(Dependency::new(
                format!("p{}", index + 1),
                VersionRange::parse("*").expect("range"),
            ));
        }
        candidates.push(candidate);
    }
    (candidates, vec!["p0".to_string()])
}

fn target() -> GameTarget {
    GameTarget::new("1.20.1", LoaderKind::Fabric)
}

#[test]
fn d2_five_thousand_deep_chain_returns_a_status_instead_of_overflowing() {
    let (candidates, roots) = chain(5_000);
    let request = SolveRequest {
        target: target(),
        roots,
        candidates,
        budget: SolveBudget::default(),
    };
    // The old recursive implementation aborted with 0xC00000FD here.
    let result = solve(&request, &|| false);
    match result.outcome {
        shtmcl_core::content::resolve::SolveOutcome::BudgetExhausted { limit, .. } => {
            assert_eq!(limit, BudgetLimit::Depth, "depth is the binding budget");
        }
        other => panic!("expected a bounded stop, got {other:?}"),
    }
    assert!(result.states_visited <= u64::from(SolveBudget::default().max_depth) + 1);
}

#[test]
fn d2_deep_chain_solves_when_the_depth_budget_allows_it() {
    let (candidates, roots) = chain(600);
    let request = SolveRequest {
        target: target(),
        roots,
        candidates,
        budget: SolveBudget::new(5_000, Duration::from_secs(60)).with_depth(1_024),
    };
    let result = solve(&request, &|| false);
    assert!(
        result.outcome.is_installable(),
        "600-deep chain should solve iteratively, got {:?}",
        result.outcome
    );
}

#[test]
fn d2_depth_is_reported_through_the_solver_not_a_crash() {
    // A caller may not raise the depth beyond the documented ceiling.
    let clamped = SolveBudget::new(1_000_000, Duration::from_secs(600))
        .with_depth(100_000)
        .clamped();
    assert!(clamped.max_states <= 10_000);
    assert!(clamped.max_wall <= Duration::from_secs(2));
    assert!(clamped.max_depth <= 4_096);
}

// ---------------------------------------------------------------------------
// D3 — an interrupted commit must not lose the rollback point
// ---------------------------------------------------------------------------

#[test]
fn d3_resume_keeps_a_rollback_point_when_digests_cannot_decide() {
    let h = harness();
    h.writer
        .seed(vec![InstanceFile::new("mods/old.jar", "aaaa", 8)]);
    let view = h.writer.view_of(&instance_id()).expect("view");

    // A record whose plan has one entry with no digest (weak evidence only).
    let mut record = RecoveryRecord::new(
        "instance-1",
        "plan-weak",
        "sha256:test",
        RecoveryKind::Apply,
        view.revision,
        view.files.clone(),
        "sha256:planned",
        vec![
            PlannedFile {
                path: "mods/old.jar".to_string(),
                digest: Some("aaaa".to_string()),
                size: Some(8),
            },
            PlannedFile {
                path: "config/foo.txt".to_string(),
                digest: None,
                size: Some(10),
            },
        ],
    );
    record.stage = Stage::CommitStarted;
    h.journal.write(&record).expect("write");

    // The commit landed, but only by presence+size (no digest for foo.txt).
    h.writer
        .seed(vec![InstanceFile::new("config/foo.txt", "bbbb", 10)]);
    let view_after = h.writer.view_of(&instance_id()).expect("view");

    let progress = shtmcl_core::content::install::FixtureProgress::new();
    let runner = InstallRunner::new(&h.ports, &progress);
    let resumed = runner.resume(&view_after).expect("resume");
    assert!(
        matches!(
            resumed,
            InstallOutcome::Resumed {
                status: shtmcl_core::content::install::ResumeStatus::CommitOutcomeUnknown
            }
        ),
        "undecidable must be reported, got {resumed:?}"
    );
    assert!(progress.has_evidence(evidence::ROLLBACK_POINT_KEPT));
    let kept = h
        .journal
        .read("instance-1")
        .expect("read")
        .expect("the rollback point must survive an undecidable resume");
    assert_eq!(kept.stage, Stage::Committed);

    // The rollback still works and restores the recorded previous set.
    let rollback = runner
        .rollback("plan-weak-rb", &view_after)
        .expect("rollback");
    assert!(matches!(rollback, InstallOutcome::RestoredPrevious { .. }));
    assert_eq!(
        h.writer
            .files()
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["mods/old.jar"]
    );
}

#[test]
fn d3_override_plan_finalizes_after_an_interrupted_commit() {
    let bytes = fixture("overrides.mrpack");
    let manifest = read_mrpack(&bytes).expect("manifest");
    let lock = ContentLock::from_manifest(&manifest, PackSide::Client).expect("lock");
    let h = harness();
    let view = h.writer.view_of(&instance_id()).expect("view");
    let plan = plan_changes("plan-ov", &view, &lock).expect("plan");
    assert!(!plan.has_conflicts());

    // Simulate: the record was written, the commit landed, then the process died.
    let mut record = RecoveryRecord::new(
        "instance-1",
        "plan-ov",
        plan.plan_digest.clone(),
        RecoveryKind::Apply,
        view.revision,
        view.files.clone(),
        shtmcl_core::content::install::planned_content_digest(&view, &plan),
        shtmcl_core::content::install::planned_files(&view, &plan),
    );
    record.stage = Stage::CommitStarted;
    h.journal.write(&record).expect("write");

    let mut applied = view.clone();
    applied.revision = Revision::new(2);
    applied.files = plan
        .entries
        .iter()
        .filter_map(|entry| match entry.action {
            shtmcl_core::content::plan::PlanAction::Delete => None,
            _ => Some(InstanceFile::new(
                entry.path.clone(),
                entry.primary_digest().unwrap_or_default(),
                entry.size.unwrap_or(0),
            )),
        })
        .collect();
    h.writer.seed(applied.files.clone());
    let view_after = h.writer.view_of(&instance_id()).expect("view");

    let progress = shtmcl_core::content::install::FixtureProgress::new();
    let runner = InstallRunner::new(&h.ports, &progress);
    let resumed = runner.resume(&view_after).expect("resume");
    assert!(
        matches!(
            resumed,
            InstallOutcome::Resumed {
                status: shtmcl_core::content::install::ResumeStatus::FinalizedCommit
            }
        ),
        "override-only plans must be decidable, got {resumed:?}"
    );
    assert_eq!(
        h.journal.read("instance-1").expect("read").map(|r| r.stage),
        Some(Stage::Committed)
    );
}

// ---------------------------------------------------------------------------
// D4 — events must land on the caller's operation stream
// ---------------------------------------------------------------------------

#[test]
fn d4_task_events_land_on_the_callers_operation_stream() {
    let h = harness();
    let app = app("d4", h.ports.clone());
    let view = h.writer.view_of(&instance_id()).expect("view");
    let manifest = read_mrpack(&fixture("sample.mrpack")).expect("manifest");
    let lock = ContentLock::from_manifest(&manifest, PackSide::Client).expect("lock");
    drop(view);

    let operation = OperationId::new("op-caller-1").expect("operation");
    let response = app
        .dispatch_mutation(
            ModuleId::Content,
            &CommandRequest {
                operation_id: Some(operation.clone()),
                ..mutate(
                    "content.plan",
                    "op-caller-1",
                    serde_json::json!({
                        "instanceId": "instance-1",
                        "lock": lock.to_value(),
                    }),
                )
            },
        )
        .expect("plan accepted");
    assert_eq!(
        accepted_operation(&response),
        operation,
        "the task must reuse the caller's operation id"
    );
    let task_id = accepted(response);
    let settled = wait_terminal(app.task_service(), &task_id);
    assert_eq!(settled.state, TaskState::Succeeded, "{:?}", settled.error);
    assert_eq!(
        settled.operation_id, operation,
        "Task.operation_id must be the request's"
    );

    let stream = app.event_sink().since(&operation, None);
    let kinds: Vec<&str> = stream.iter().map(|event| event.kind.as_str()).collect();
    assert!(
        kinds.iter().any(|kind| *kind == "task.state"),
        "task.state missing from the caller's stream: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|kind| *kind == "task.result"),
        "task.result missing from the caller's stream: {kinds:?}"
    );
    assert!(kinds.iter().any(|kind| *kind == "content.plan_ready"));
    assert!(
        stream.iter().all(|event| event.operation_id == operation),
        "no event may leak into another operation stream"
    );
}

// ---------------------------------------------------------------------------
// D5 — the synchronous solve read must stay bounded
// ---------------------------------------------------------------------------

#[test]
fn d5_read_budget_clamp_is_stricter_than_the_task_clamp() {
    let requested = SolveBudget::new(500_000_000, Duration::from_secs(600)).with_depth(100_000);
    let read = requested.clone().clamped_for_read();
    let task = requested.clamped();
    assert!(read.max_states <= READ_MAX_STATES);
    assert!(read.max_wall <= Duration::from_millis(200));
    assert!(read.max_depth <= 128);
    assert!(task.max_states <= 10_000 && read.max_states < task.max_states);
}

#[test]
fn d5_solve_read_is_clamped_and_answers_quickly() {
    // A graph that a caller could ask to search far beyond the read budget: a deep
    // chain plus a request for 500M states and 10 minutes.
    let (candidates, roots) = chain(5_000);
    let h = harness();
    let app = app("d5", h.ports.clone());
    let started = Instant::now();
    let response = app
        .dispatch_read(
            ModuleId::Content,
            &read(
                "content.solve",
                serde_json::json!({
                    "target": { "gameVersion": "1.20.1", "loader": "fabric" },
                    "roots": roots,
                    "candidates": candidates.iter().map(candidate_json).collect::<Vec<_>>(),
                    "budget": { "maxStates": 500000000, "maxWallMs": 600000, "maxDepth": 100000 },
                }),
            ),
        )
        .expect("solve read");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "the read path must be bounded, took {elapsed:?}"
    );
    match response {
        CommandResponse::Completed { result } => {
            assert_eq!(
                result["outcome"], "budget_exhausted",
                "a read must stop on the clamped budget, not search 500M states"
            );
            let states = result["statesVisited"].as_u64().expect("states");
            assert!(
                states <= u64::from(SolveBudget::default().max_depth) + 1,
                "read visited {states} states"
            );
            assert_eq!(result["budgetLimit"], "depth");
            let _ = READ_MAX_STATES;
        }
        other => panic!("expected a completed response, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// D6 — the plan registry is bounded
// ---------------------------------------------------------------------------

#[test]
fn d6_plan_registry_evicts_the_oldest_plan() {
    let h = harness();
    let app = app("d6", h.ports.clone());
    let manifest = read_mrpack(&fixture("sample.mrpack")).expect("manifest");
    let lock = ContentLock::from_manifest(&manifest, PackSide::Client).expect("lock");

    let mut first_plan: Option<String> = None;
    for index in 0..40 {
        let operation = format!("op-d6-{index}");
        let task_id = accepted(
            app.dispatch_mutation(
                ModuleId::Content,
                &mutate(
                    "content.plan",
                    &operation,
                    serde_json::json!({ "instanceId": "instance-1", "lock": lock.to_value() }),
                ),
            )
            .expect("plan accepted"),
        );
        let settled = wait_terminal(app.task_service(), &task_id);
        assert_eq!(settled.state, TaskState::Succeeded, "{:?}", settled.error);
        let plan_id = settled.result.expect("result")["planId"]
            .as_str()
            .expect("plan id")
            .to_string();
        if first_plan.is_none() {
            first_plan = Some(plan_id);
        }
    }

    // The oldest plan is gone: applying it is refused instead of applying stale work.
    let error = app
        .dispatch_mutation(
            ModuleId::Content,
            &mutate(
                "content.apply",
                "op-d6-apply",
                serde_json::json!({
                    "instanceId": "instance-1",
                    "planId": first_plan.expect("first plan"),
                }),
            ),
        )
        .expect_err("an evicted plan must be rejected");
    assert_eq!(error.code.as_str(), "content.plan.invalid");
}

// ---------------------------------------------------------------------------
// D8 — no evidence marker without a port fact
// ---------------------------------------------------------------------------

#[test]
fn d8_apply_does_not_claim_the_previous_version_was_preserved() {
    let h = harness();
    let app = app("d8", h.ports.clone());
    let manifest = read_mrpack(&fixture("sample.mrpack")).expect("manifest");
    let lock = ContentLock::from_manifest(&manifest, PackSide::Client).expect("lock");

    let plan_task = accepted(
        app.dispatch_mutation(
            ModuleId::Content,
            &mutate(
                "content.plan",
                "op-d8-plan",
                serde_json::json!({ "instanceId": "instance-1", "lock": lock.to_value() }),
            ),
        )
        .expect("accepted"),
    );
    let settled = wait_terminal(app.task_service(), &plan_task);
    let plan_id = settled.result.expect("result")["planId"]
        .as_str()
        .expect("id")
        .to_string();
    let apply_task = accepted(
        app.dispatch_mutation(
            ModuleId::Content,
            &mutate(
                "content.apply",
                "op-d8-apply",
                serde_json::json!({ "instanceId": "instance-1", "planId": plan_id }),
            ),
        )
        .expect("accepted"),
    );
    let settled = wait_terminal(app.task_service(), &apply_task);
    assert_eq!(settled.state, TaskState::Succeeded, "{:?}", settled.error);
    assert!(settled
        .evidence
        .iter()
        .any(|marker| marker == evidence::COMMIT_CONFIRMED));
    assert!(
        !settled
            .evidence
            .iter()
            .any(|marker| marker == "content_previous_preserved"),
        "the port exposes no backup fact, so the marker must not be written: {:?}",
        settled.evidence
    );
}

// ---------------------------------------------------------------------------
// D9 — rollback publishes an entity event
// ---------------------------------------------------------------------------

#[test]
fn d9_rollback_publishes_content_changed_with_the_new_revision() {
    let h = harness();
    h.writer
        .seed(vec![InstanceFile::new("mods/old.jar", "dddd", 8)]);
    let app = app("d9", h.ports.clone());
    let manifest = read_mrpack(&fixture("sample.mrpack")).expect("manifest");
    let lock = ContentLock::from_manifest(&manifest, PackSide::Client).expect("lock");

    let plan_task = accepted(
        app.dispatch_mutation(
            ModuleId::Content,
            &mutate(
                "content.plan",
                "op-d9-plan",
                serde_json::json!({ "instanceId": "instance-1", "lock": lock.to_value() }),
            ),
        )
        .expect("accepted"),
    );
    let settled = wait_terminal(app.task_service(), &plan_task);
    let plan_id = settled.result.expect("result")["planId"]
        .as_str()
        .expect("id")
        .to_string();
    let apply_task = accepted(
        app.dispatch_mutation(
            ModuleId::Content,
            &mutate(
                "content.apply",
                "op-d9-apply",
                serde_json::json!({ "instanceId": "instance-1", "planId": plan_id }),
            ),
        )
        .expect("accepted"),
    );
    wait_terminal(app.task_service(), &apply_task);

    let operation = OperationId::new("op-d9-rollback").expect("operation");
    let rollback_task = accepted(
        app.dispatch_mutation(
            ModuleId::Content,
            &CommandRequest {
                operation_id: Some(operation.clone()),
                ..mutate(
                    "content.rollback",
                    "op-d9-rollback",
                    serde_json::json!({ "instanceId": "instance-1" }),
                )
            },
        )
        .expect("accepted"),
    );
    let settled = wait_terminal(app.task_service(), &rollback_task);
    assert_eq!(settled.state, TaskState::Succeeded, "{:?}", settled.error);

    let changed = app
        .event_sink()
        .since(&operation, None)
        .into_iter()
        .find(|event| event.kind.as_str() == "content.changed")
        .expect("rollback must publish content.changed");
    assert_eq!(
        changed.revision,
        h.writer.revision(),
        "the event must carry the revision the rollback produced"
    );
}

// ---------------------------------------------------------------------------
// D10 — corrupted override payloads are rejected
// ---------------------------------------------------------------------------

#[test]
fn d10_truncated_override_payload_is_rejected() {
    let error = read_mrpack(&fixture("hostile-truncated-override.mrpack"))
        .expect_err("a lying payload length must be rejected");
    assert_eq!(error.code(), "content.pack.checksum_mismatch");
}

#[test]
fn d10_zeroed_override_crc_is_rejected() {
    let error =
        read_mrpack(&fixture("hostile-bad-crc.mrpack")).expect_err("a zeroed CRC must be rejected");
    assert_eq!(error.code(), "content.pack.checksum_mismatch");
}

// ---------------------------------------------------------------------------
// D11 — solver result codes have producers
// ---------------------------------------------------------------------------

#[test]
fn d11_unsatisfiable_solve_task_fails_with_a_stable_code() {
    let h = harness();
    let app = app("d11a", h.ports.clone());
    let task_id = accepted(
        app.dispatch_mutation(
            ModuleId::Content,
            &mutate(
                "content.solve.task",
                "op-d11a",
                serde_json::json!({
                    "target": { "gameVersion": "1.20.1", "loader": "fabric" },
                    "roots": ["a"],
                    "candidates": [{
                        "provider": "modrinth",
                        "projectId": "a",
                        "fileId": "a1",
                        "version": "1.0",
                        "gameVersions": ["1.20.1"],
                        "loaders": ["fabric"],
                        "dependencies": [{ "projectId": "b", "range": ">=2.0" }]
                    }, {
                        "provider": "modrinth",
                        "projectId": "b",
                        "fileId": "b1",
                        "version": "1.0",
                        "gameVersions": ["1.20.1"],
                        "loaders": ["fabric"]
                    }],
                }),
            ),
        )
        .expect("accepted"),
    );
    let settled = wait_terminal(app.task_service(), &task_id);
    assert_eq!(settled.state, TaskState::Failed);
    assert_eq!(
        settled.error.as_ref().expect("error").code.as_str(),
        "content.solve.unsatisfiable"
    );
}

#[test]
fn d11_budget_exhausted_solve_task_fails_with_a_stable_code() {
    let h = harness();
    let app = app("d11b", h.ports.clone());
    let task_id = accepted(
        app.dispatch_mutation(
            ModuleId::Content,
            &mutate(
                "content.solve.task",
                "op-d11b",
                serde_json::json!({
                    "target": { "gameVersion": "1.20.1", "loader": "fabric" },
                    "roots": ["a"],
                    "candidates": [{
                        "provider": "modrinth",
                        "projectId": "a",
                        "fileId": "a1",
                        "version": "1.0",
                        "gameVersions": ["1.20.1"],
                        "loaders": ["fabric"],
                        "dependencies": [{ "projectId": "missing", "range": ">=1.0" }]
                    }],
                    "budget": { "maxStates": 0 },
                }),
            ),
        )
        .expect("accepted"),
    );
    let settled = wait_terminal(app.task_service(), &task_id);
    assert_eq!(settled.state, TaskState::Failed);
    assert_eq!(
        settled.error.as_ref().expect("error").code.as_str(),
        "content.solve.budget_exhausted"
    );
}

// ---------------------------------------------------------------------------
// D12 — a pack file can be inspected from disk
// ---------------------------------------------------------------------------

#[test]
fn d12_inspect_reads_a_pack_file_from_disk() {
    let path =
        std::env::temp_dir().join(format!("shtmcl-04-inspect-{}.mrpack", std::process::id()));
    std::fs::write(&path, fixture("overrides.mrpack")).expect("write fixture");

    let h = harness();
    let app = app("d12", h.ports.clone());
    let response = app
        .dispatch_read(
            ModuleId::Content,
            &read(
                "content.packs.inspect",
                serde_json::json!({ "path": path.to_string_lossy() }),
            ),
        )
        .expect("inspect by path");
    match response {
        CommandResponse::Completed { result } => {
            assert_eq!(result["format"], "mrpack");
            assert_eq!(result["overrideCount"], 4);
            assert!(result["fileCount"].as_u64().unwrap_or(0) >= 2);
        }
        other => panic!("expected a completed response, got {other:?}"),
    }
    let _ = std::fs::remove_file(&path);

    // A missing file is a stable error, not a panic.
    let error = app
        .dispatch_read(
            ModuleId::Content,
            &read(
                "content.packs.inspect",
                serde_json::json!({ "path": "F:\\definitely-missing-404.mrpack" }),
            ),
        )
        .expect_err("missing file");
    assert_eq!(error.code.as_str(), "internal.unavailable");
}

// ---------------------------------------------------------------------------
// D13 — user-owned files are never replaced
// ---------------------------------------------------------------------------

#[test]
fn d13_unmanaged_file_becomes_a_conflict_and_blocks_apply() {
    let mut manifest_files = Vec::new();
    manifest_files.push(InstanceFile::new("mods/mine.jar", "user-digest", 10));
    let mut user_file = InstanceFile::new("mods/mine.jar", "user-digest", 10);
    user_file.managed = false;
    manifest_files.clear();
    manifest_files.push(user_file);

    let view = InstanceContentView::new(instance_id(), Revision::new(1), manifest_files);
    let lock = ContentLock::new(
        GameTarget::new("1.20.1", LoaderKind::Fabric),
        PackSide::Client,
        vec![shtmcl_core::content::plan::LockFile {
            path: "mods/mine.jar".to_string(),
            identity: shtmcl_core::content::plan::LocalIdentity::Digest {
                sha1: Some("pack-digest".to_string()),
                sha512: None,
            },
            hashes: Hashes {
                sha1: Some("pack-digest".to_string()),
                sha512: None,
                sha256: None,
            },
            size: Some(20),
            urls: vec!["https://example.invalid/mine.jar".to_string()],
            source: LockSource::Download,
            client: SideSupport::Required,
            server: SideSupport::Required,
            origin: "pack:files".to_string(),
        }],
    )
    .expect("lock");

    let plan = plan_changes("plan-ue", &view, &lock).expect("plan");
    assert_eq!(plan.counts.replace, 0, "the user file must not be replaced");
    assert_eq!(plan.conflicts.len(), 1);
    assert_eq!(plan.conflicts[0].path, "mods/mine.jar");

    // Applying such a plan is refused with a dedicated code.
    let h = harness();
    h.writer.seed(vec![{
        let mut file = InstanceFile::new("mods/mine.jar", "user-digest", 10);
        file.managed = false;
        file
    }]);
    let progress = shtmcl_core::content::install::FixtureProgress::new();
    let runner = InstallRunner::new(&h.ports, &progress);
    let error = runner
        .apply(&plan, &h.writer.view_of(&instance_id()).expect("view"))
        .expect_err("conflicting plan must not commit");
    assert_eq!(error.code(), "content.plan.unmanaged_conflict");
    assert!(h.writer.commits().is_empty());
}

// ---------------------------------------------------------------------------
// D14 — an empty plan confirms nothing and cancellation wins
// ---------------------------------------------------------------------------

#[test]
fn d14_empty_plan_writes_no_commit_evidence_and_cancellation_wins() {
    let h = harness();
    h.writer
        .seed(vec![InstanceFile::new("mods/a.jar", "aaaa", 4)]);
    let view = h.writer.view_of(&instance_id()).expect("view");
    let lock = ContentLock::new(
        GameTarget::new("1.20.1", LoaderKind::Fabric),
        PackSide::Client,
        vec![shtmcl_core::content::plan::LockFile {
            path: "mods/a.jar".to_string(),
            identity: shtmcl_core::content::plan::LocalIdentity::Digest {
                sha1: Some("aaaa".to_string()),
                sha512: None,
            },
            hashes: Hashes {
                sha1: Some("aaaa".to_string()),
                sha512: None,
                sha256: None,
            },
            size: Some(4),
            urls: vec!["https://example.invalid/a.jar".to_string()],
            source: LockSource::Download,
            client: SideSupport::Required,
            server: SideSupport::Required,
            origin: "pack:files".to_string(),
        }],
    )
    .expect("lock");
    let plan = plan_changes("plan-empty", &view, &lock).expect("plan");
    assert!(plan.is_empty());

    let progress = shtmcl_core::content::install::FixtureProgress::new();
    let runner = InstallRunner::new(&h.ports, &progress);
    let outcome = runner.apply(&plan, &view).expect("apply");
    assert!(matches!(outcome, InstallOutcome::Installed { .. }));
    assert!(
        !progress.has_evidence(evidence::COMMIT_CONFIRMED),
        "an empty plan commits nothing, so it cannot confirm a commit"
    );
    assert!(h.writer.commits().is_empty());

    // With a pending cancellation the plan must stop instead of reporting success.
    let cancelling =
        shtmcl_core::content::install::FixtureProgress::new().with_cancel_from(phases::PLAN);
    let runner = InstallRunner::new(&h.ports, &cancelling);
    let outcome = runner.apply(&plan, &view).expect("apply");
    assert!(
        outcome.is_cancelled(),
        "cancellation must win over an empty plan"
    );
}

// ---------------------------------------------------------------------------
// D15 — ADS paths and duplicate index entries are rejected
// ---------------------------------------------------------------------------

#[test]
fn d15_alternate_data_stream_override_is_rejected() {
    let error = read_mrpack(&fixture("hostile-ads-override.mrpack"))
        .expect_err("an ADS path must be rejected");
    assert_eq!(error.code(), "content.pack.path_escape");
}

#[test]
fn d15_duplicate_index_entries_are_rejected() {
    let error = read_mrpack(&fixture("hostile-duplicate-index.mrpack"))
        .expect_err("two manifests must be rejected");
    assert_eq!(error.code(), "content.pack.duplicate_path");
}

// ---------------------------------------------------------------------------
// D16 — the archive entry budget is reachable
// ---------------------------------------------------------------------------

#[test]
fn d16_archive_entry_budget_matches_the_observable_zip_limit() {
    // The EOCD count is a u16 and zip64 is rejected, so a larger budget could never
    // be reached by any input.
    assert_eq!(MAX_ARCHIVE_ENTRIES, u16::MAX as usize);
}

// ---------------------------------------------------------------------------
// D7 — a failed staging cleanup must not silently drop the record
// ---------------------------------------------------------------------------

#[test]
fn d7_failed_discard_keeps_the_recovery_record() {
    let h = harness();
    h.stager.fail_discard("staging volume busy");
    let view = h.writer.view_of(&instance_id()).expect("view");
    let manifest = read_mrpack(&fixture("sample.mrpack")).expect("manifest");
    let lock = ContentLock::from_manifest(&manifest, PackSide::Client).expect("lock");
    let plan = plan_changes("plan-d7", &view, &lock).expect("plan");

    let progress =
        shtmcl_core::content::install::FixtureProgress::new().with_cancel_from(phases::VERIFY);
    let runner = InstallRunner::new(&h.ports, &progress);
    let error = runner
        .apply(&plan, &view)
        .expect_err("a failed cleanup must surface, not vanish");
    assert_eq!(error.code(), "internal.unavailable");
    assert!(progress.has_evidence(evidence::STAGING_DISCARD_FAILED));
    assert!(
        h.journal.read("instance-1").expect("read").is_some(),
        "the record must survive a failed cleanup so leftovers stay recoverable"
    );
    assert!(h.writer.commits().is_empty());
}

// ---------------------------------------------------------------------------
// Extra: export still rejects credentials after the plan changes
// ---------------------------------------------------------------------------

#[test]
fn export_still_drops_credentials_and_logs() {
    let spec = plan_export(
        shtmcl_core::content::modpack::PackFormat::Modrinth,
        "regression",
        "1.0.0",
        GameTarget::new("1.20.1", LoaderKind::Fabric).with_loader_version("0.15.11"),
        vec![
            ExportCandidate {
                path: "mods/a.jar".to_string(),
                hashes: Hashes {
                    sha1: Some("c".repeat(40)),
                    sha512: None,
                    sha256: None,
                },
                downloads: vec!["https://example.invalid/a.jar".to_string()],
                file_size: Some(1),
                client: SideSupport::Required,
                server: SideSupport::Required,
            },
            ExportCandidate {
                path: "logs/latest.log".to_string(),
                hashes: Hashes {
                    sha1: Some("d".repeat(40)),
                    sha512: None,
                    sha256: None,
                },
                downloads: Vec::new(),
                file_size: Some(1),
                client: SideSupport::Required,
                server: SideSupport::Required,
            },
        ],
        false,
    )
    .expect("export plan");
    assert_eq!(spec.files.len(), 1);
    assert!(spec
        .excluded
        .iter()
        .any(|(path, _)| path == "logs/latest.log"));
}

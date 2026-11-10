//! 02 · Java discovery/matching and the Quick Play capability table.
//!
//! The Java candidate process is a controlled double; everything around it
//! (path enumeration, version parsing, caching, constraint matching, stable
//! reason codes) is the real implementation.

#[path = "02_support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::time::Duration;

use shtmcl_core::launch::java::{
    self, Arch, JavaProber, JavaRequirement, JavaRuntime, RuntimeConstraint,
};
use shtmcl_core::launch::quickplay::{CapabilityKey, QuickPlayCapability, QuickPlayTable};
use shtmcl_core::launch::session::LaunchSessionState;
use shtmcl_core::launch::{commands, LaunchPorts};
use shtmcl_core::tasks::{TaskService, TaskSpec};
use shtmcl_core::{CoreModule, ModuleContext};
use shtmcl_domain::{OperationId, TaskPhase, TaskState};
use shtmcl_platform::process::ProcessLauncher;

use support::*;

fn runtime_at(path: &str, major: u32, arch: Arch) -> JavaRuntime {
    JavaRuntime {
        id: format!("java-{major}"),
        path: PathBuf::from(path),
        platform: java::HostPlatform::current(),
        arch,
        vendor: java::JavaVendor::Temurin,
        version: java::JavaVersion {
            major,
            minor: 0,
            security: 2,
            update: None,
            raw: format!("{major}.0.2"),
        },
        verified_at_unix_ms: 0,
        file_identity: shtmcl_platform::FileIdentity {
            len: 4,
            modified_unix_ms: None,
        },
    }
}

#[test]
fn java_versions_are_compared_structurally_and_mismatches_are_structured() {
    // 1.8.0_402 is Java 8, not Java 1.
    let legacy = java::parse_version_output("java version \"1.8.0_402\"").expect("java 8");
    assert_eq!(legacy.major, 8);
    assert_eq!(legacy.update, Some(402));

    let (requirement, verified) = java::requirement_for_game_version("1.21.1");
    assert!(verified);
    assert_eq!(requirement, JavaRequirement::AtLeast(21));

    let (old, verified) = java::requirement_for_game_version("1.16.5");
    assert!(verified);
    assert_eq!(old, JavaRequirement::Exact(8));

    let runtimes = vec![
        runtime_at("C:/jdk8/bin/java.exe", 8, Arch::X64),
        runtime_at("C:/jdk17/bin/java.exe", 17, Arch::X64),
    ];
    let constraint = RuntimeConstraint::host(JavaRequirement::AtLeast(21));
    let selection = java::select(&runtimes, &constraint, true);
    assert!(selection.selected.is_none());
    assert_eq!(selection.rejected.len(), 2);
    let envelope = selection.failure_envelope(&constraint);
    assert_eq!(envelope.code.as_str(), "launch.java.version_mismatch");
    // The refusal names the requirement, never raw stderr.
    let detail = envelope.detail_ref.unwrap_or_default();
    assert!(detail.contains(">=21"), "detail: {detail}");
    assert!(!detail.contains("OpenJDK"), "no raw banner in the envelope");
}

#[test]
fn discovery_reuses_the_probe_cache_and_reports_a_hanging_runtime() {
    let fixture = Fixture::new("java-discovery", sample_snapshot("1.20.4"));
    fixture.process.set_java(&java_banner(21));

    let task = fixture
        .tasks
        .accept(TaskSpec::new(
            OperationId::new("op-discover").expect("op"),
            TaskPhase::new("runtime").expect("phase"),
        ))
        .expect("accept");
    let _ = task;

    let (_, task_id) = accepted(
        fixture
            .module
            .handle(&mutation(
                commands::RUNTIMES_DISCOVER,
                serde_json::json!({ "candidateRoots": [fixture.dirs.data.join("java")] }),
            ))
            .expect("discover accepted"),
    );
    fixture.module.join_jobs();

    let started = fixture.task(&task_id);
    assert_eq!(started.state, TaskState::Succeeded, "discovery finished");
    let result = started.result.expect("result");
    assert!(
        result["runtimes"].as_array().map(Vec::len).unwrap_or(0) >= 1,
        "at least the fixture JDK is discovered: {result}"
    );
    let first_probe_count = fixture.process.probe_spawns().len();
    assert!(first_probe_count >= 1);

    // Second pass: the cache answers, so no new probe process is created.
    let (_, second_task) = accepted(
        fixture
            .module
            .handle(&mutation(
                commands::RUNTIMES_DISCOVER,
                serde_json::json!({ "candidateRoots": [fixture.dirs.data.join("java")] }),
            ))
            .expect("discover accepted"),
    );
    fixture.module.join_jobs();
    assert_eq!(
        fixture.process.probe_spawns().len(),
        first_probe_count,
        "a cached runtime is not re-probed"
    );
    let second_result = fixture.task(&second_task).result.expect("result");
    assert!(
        second_result["cache"]["hits"].as_u64().unwrap_or(0) > 0,
        "cache hits are reported: {second_result}"
    );

    // A runtime that never answers becomes a structured timeout, not a hang.
    let fresh = Fixture::new("java-timeout", sample_snapshot("1.20.4"));
    fresh
        .process
        .probe_hangs
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (_, timeout_task) = accepted(
        fresh
            .module
            .handle(&mutation(
                commands::RUNTIMES_DISCOVER,
                serde_json::json!({ "candidateRoots": [fresh.dirs.data.join("java")] }),
            ))
            .expect("discover accepted"),
    );
    let start = std::time::Instant::now();
    fresh.module.join_jobs();
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "a hanging probe must time out, not block forever"
    );
    let timeout_result = fresh.task(&timeout_task).result.expect("result");
    let failed = timeout_result["failed"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        failed
            .iter()
            .any(|entry| entry["code"] == java::ProbeOutcome::TimedOut.code()),
        "timeout is reported with the stable probe code: {timeout_result}"
    );
    assert_eq!(
        timeout_result["runtimes"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        0,
        "an unresponsive runtime is not offered"
    );
}

#[test]
fn runtimes_list_explains_why_a_pinned_runtime_is_rejected() {
    let fixture = Fixture::new("java-list", sample_snapshot("1.20.4"));
    fixture.process.set_java(&java_banner(8));

    let (_, task_id) = accepted(
        fixture
            .module
            .handle(&mutation(
                commands::RUNTIMES_DISCOVER,
                serde_json::json!({ "candidateRoots": [fixture.dirs.data.join("java")] }),
            ))
            .expect("discover"),
    );
    fixture.module.join_jobs();
    assert_eq!(fixture.task(&task_id).state, TaskState::Succeeded);

    let mismatch = completed(
        fixture
            .module
            .handle(&request(
                commands::RUNTIMES_LIST,
                serde_json::json!({ "requirement": ">=17" }),
            ))
            .expect("list"),
    );
    assert!(mismatch["runtime"].is_null(), "java 8 cannot satisfy >=17");
    let rejected = mismatch["rejected"].as_array().cloned().unwrap_or_default();
    assert!(!rejected.is_empty());
    assert_eq!(rejected[0]["code"], "launch.java.version_mismatch");
    assert_eq!(rejected[0]["userAction"], "install_java");
    assert_eq!(mismatch["requirementVerified"], serde_json::json!(true));

    let matching = completed(
        fixture
            .module
            .handle(&request(
                commands::RUNTIMES_LIST,
                serde_json::json!({ "requirement": "8" }),
            ))
            .expect("list"),
    );
    assert!(!matching["runtime"].is_null(), "java 8 satisfies 8");
    assert_eq!(
        matching["runtime"]["version"]["major"],
        serde_json::json!(8)
    );
}

#[test]
fn quickplay_answers_unknown_instead_of_assuming_support() {
    let fixture = Fixture::new("quickplay", sample_snapshot("1.20.4"));

    let modern = completed(
        fixture
            .module
            .handle(&request(
                commands::QUICKPLAY_CAPABILITIES,
                serde_json::json!({
                    "gameVersion": "1.20.4",
                    "loaderFamily": "fabric",
                    "loaderVersion": "0.15.11",
                    "auth": "online",
                }),
            ))
            .expect("capability"),
    );
    assert_eq!(modern["result"]["capability"], "direct_join");
    assert_eq!(modern["result"]["available"], serde_json::json!(true));

    let unknown_loader = completed(
        fixture
            .module
            .handle(&request(
                commands::QUICKPLAY_CAPABILITIES,
                serde_json::json!({
                    "gameVersion": "1.20.4",
                    "loaderFamily": "forge",
                    "loaderVersion": "49.0.30",
                    "auth": "online",
                }),
            ))
            .expect("capability"),
    );
    assert_eq!(unknown_loader["result"]["capability"], "unknown");
    assert_eq!(
        unknown_loader["result"]["available"],
        serde_json::json!(false),
        "an unverified loader is never reported as usable"
    );
    assert_eq!(
        unknown_loader["result"]["reasonCode"],
        "launch.quickplay.unknown_loader"
    );

    let snapshot_version = completed(
        fixture
            .module
            .handle(&request(
                commands::QUICKPLAY_CAPABILITIES,
                serde_json::json!({
                    "gameVersion": "24w14a",
                    "loaderFamily": "vanilla",
                    "auth": "online",
                }),
            ))
            .expect("capability"),
    );
    assert_eq!(snapshot_version["result"]["capability"], "unknown");
    assert_eq!(
        snapshot_version["result"]["reasonCode"],
        "launch.quickplay.unknown_version"
    );

    // The table itself is directly consumable by 06/07/10.
    let entry = QuickPlayTable::lookup(&CapabilityKey::new(
        "1.19.4",
        shtmcl_core::launch::plan::LoaderKind::Vanilla,
        shtmcl_core::launch::ports::AuthRequirement::Online,
    ));
    assert_eq!(entry.capability, QuickPlayCapability::LaunchAndJoin);
    assert_eq!(
        QuickPlayTable::direct_join_argument(&entry),
        None,
        "an old version has no direct-join argument"
    );
}

#[test]
fn a_portless_module_fails_closed_instead_of_inventing_state() {
    // The default ports have no real instance source: nothing may be fabricated.
    let dirs = shtmcl_platform::PlatformDirs::under(temp_root("ports"));
    dirs.create_all().expect("dirs");
    let events = std::sync::Arc::new(shtmcl_core::tasks::InMemoryEvents::new());
    let tasks = std::sync::Arc::new(shtmcl_core::tasks::InMemoryTasks::new(events.clone()));
    let ctx = ModuleContext::new(tasks.clone(), events, dirs.clone());
    let bare = shtmcl_core::launch::module_with(ctx, LaunchPorts::unavailable());

    let (_, task_id) = accepted(
        bare.handle(&mutation(
            commands::LAUNCH_PREPARE,
            serde_json::json!({ "instanceId": "instance-1" }),
        ))
        .expect("accepted"),
    );
    bare.join_jobs();
    let task = tasks.snapshot(&task_id).expect("task");
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(
        task.error
            .as_ref()
            .map(|error| error.code.as_str().to_string()),
        Some("launch.instance.unavailable".to_string())
    );
    // The session reports the failure too; no process was ever created.
    let sessions = bare.sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].state, LaunchSessionState::Failed);
    let _ = std::fs::remove_dir_all(dirs.root);
}

#[test]
fn probing_uses_one_captured_version_banner() {
    // Direct prober test: `-version` output parsing is wired to the real prober.
    let root = temp_root("probe");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("bin dir");
    let program = bin.join(java_executable());
    std::fs::write(&program, b"fake").expect("write java");

    let process = FakeProcess::new();
    process.set_java(&java_banner(17));
    let launcher: std::sync::Arc<dyn ProcessLauncher> = process.clone();
    let supervisor: std::sync::Arc<dyn shtmcl_core::launch::ports::ProcessSupervisor> =
        process.clone();
    let prober = JavaProber::new(launcher, supervisor);
    let probe = prober.probe(&program);
    assert_eq!(probe.outcome, java::ProbeOutcome::Ok);
    assert_eq!(
        probe.version.as_ref().map(|version| version.major),
        Some(17)
    );
    assert_eq!(probe.arch, Arch::X64);
    assert_eq!(process.probe_spawns().len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

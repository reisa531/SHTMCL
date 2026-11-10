//! Opt-in production acceptance: repair a damaged, separately installed vanilla instance.
//! Network adapters remain enabled. This test does not claim offline repair or GUI acceptance.
//! The first 1.20.4 run installed successfully but failed this test's DLL-only
//! assumption: LWJGL3 keeps Windows natives in classpath JARs. That failed run's
//! evidence is retained; native_jar baselines below do not claim extracted-DLL repair.
#![cfg(windows)]

use crate::AppState;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use shtmcl_core::launch::{catalog::VersionMetadata, install::VanillaInstallPlan};
use shtmcl_domain::{
    CancelTaskRequest, CommandRequest, CommandResponse, ModuleId, OperationId, RequestId, Revision,
    TaskId, TaskState,
};
use shtmcl_platform::PlatformDirs;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn request(command: &str, mut payload: Value) -> CommandRequest {
    payload["command"] = json!(command);
    CommandRequest {
        request_id: RequestId::new("repair-acceptance").unwrap(),
        operation_id: Some(
            OperationId::new(format!(
                "repair-{}",
                shtmcl_platform::entropy::random_hex(12).unwrap()
            ))
            .unwrap(),
        ),
        entity_id: None,
        expected_revision: None,
        payload,
    }
}

fn read(state: &AppState, module: ModuleId, command: &str, payload: Value) -> Value {
    match state
        .core
        .dispatch_read(module, &request(command, payload))
        .expect("production read")
    {
        CommandResponse::Completed { result } => result,
        response => panic!("expected completed read: {response:?}"),
    }
}

fn wait_success(state: &AppState, id: &TaskId) -> Value {
    let deadline = Instant::now() + Duration::from_secs(1200);
    loop {
        let task = state.core.context().tasks().snapshot(id).expect("task");
        if task.state.is_terminal() {
            if task.state != TaskState::Succeeded {
                for child in state.core.snapshot() {
                    if child.error.is_some() {
                        eprintln!("repair_failed_child={child:?}");
                    }
                }
            }
            assert_eq!(task.state, TaskState::Succeeded, "task failed: {task:?}");
            return task.result.unwrap_or(Value::Null);
        }
        assert!(Instant::now() < deadline, "task deadline: {task:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn start_task(state: &AppState, command: &str, payload: Value) -> TaskId {
    match state
        .core
        .dispatch_mutation(ModuleId::Launch, &request(command, payload))
        .expect("production mutation accepted")
    {
        CommandResponse::Accepted { task_id, .. } => task_id,
        response => panic!("expected task: {response:?}"),
    }
}

fn evidence(dir: &Path, name: &str, value: &Value) {
    fs::write(dir.join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn sessions(state: &AppState, instance: &str) -> Value {
    read(
        state,
        ModuleId::Launch,
        "launch.sessions",
        json!({"instanceId":instance}),
    )
}

/// Cleanup is restricted to this test's unique instance and its known start task.
/// It also executes during unwinding; no global Java enumeration or termination.
struct StopOnDrop<'a> {
    state: &'a AppState,
    instance: &'a str,
    tasks: Vec<TaskId>,
    armed: bool,
}
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for id in &self.tasks {
            let Some(task) = self.state.core.context().tasks().snapshot(id) else {
                continue;
            };
            if !task.state.is_terminal() {
                self.state
                    .core
                    .context()
                    .tasks()
                    .request_cancel(&CancelTaskRequest {
                        task_id: task.id,
                        attempt_generation: task.attempt_generation,
                    });
            }
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let response = self.state.core.dispatch_read(
                ModuleId::Launch,
                &request("launch.sessions", json!({"instanceId":self.instance})),
            );
            let mut running = false;
            if let Ok(CommandResponse::Completed { result }) = response {
                if let Some(entries) = result["sessions"].as_array() {
                    for session in entries {
                        // Check again before issuing a command, even if the query is filtered.
                        if session["instanceId"] != self.instance || session["state"] == "exited" {
                            continue;
                        }
                        running = true;
                        let _ = self.state.core.dispatch_mutation(
                            ModuleId::Launch,
                            &request("launch.stop", json!({"sessionId":session["sessionId"]})),
                        );
                    }
                }
            }
            let tasks_terminal = self.tasks.iter().all(|id| {
                self.state
                    .core
                    .context()
                    .tasks()
                    .snapshot(id)
                    .is_some_and(|task| task.state.is_terminal())
            });
            if !running && tasks_terminal {
                break;
            }
            if Instant::now() >= deadline {
                eprintln!("repair cleanup deadline for {}", self.instance);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
#[ignore = "real production install, destructive damage to a unique test instance, and Java launch"]
fn real_damaged_vanilla_repairs_before_launch_and_preserves_user_content() {
    let root = PathBuf::from(
        std::env::var_os("SHTMCL_REPAIR_TEST_ROOT").expect("explicit retained test root required"),
    );
    assert!(root.is_dir(), "existing retained test root required");
    let version = std::env::var("SHTMCL_REPAIR_VERSION").unwrap_or_else(|_| "1.7.10".into());
    assert!(
        shtmcl_core::launch::install::safe_path(&version) && !version.contains('/'),
        "version must be a single safe path component"
    );
    let suffix = shtmcl_platform::entropy::random_hex(10).unwrap();
    let instance = format!("repair-acceptance-{suffix}");
    let target = root.join("instances").join(&instance);
    assert!(!target.exists(), "test must own a new instance");
    let output = std::env::var_os("SHTMCL_REPAIR_EVIDENCE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../work/validation/vanilla-repair/20261022-01/acceptance")
        })
        .join(&suffix);
    fs::create_dir_all(&output).unwrap();
    println!("repair_instance={instance} evidence={}", output.display());
    let state = AppState::new(PlatformDirs::under(&root));
    let installed = wait_success(
        &state,
        &start_task(
            &state,
            "launch.install",
            json!({"versionId":version,"instanceId":instance}),
        ),
    );
    evidence(&output, "installed.json", &installed);

    let metadata_path = target.join(format!("versions/{version}/{version}.json"));
    let metadata = VersionMetadata::parse(&fs::read(&metadata_path).unwrap(), Some(&version))
        .expect("installed version metadata");
    let plan = VanillaInstallPlan::parse(&metadata, "windows", "amd64").unwrap();
    let index = plan.asset_index_path.expect("selected version asset index");
    let index_path = target.join(&index);
    let object = shtmcl_core::launch::install::asset_artifacts(&fs::read(&index_path).unwrap())
        .unwrap()
        .into_iter()
        .find(|artifact| artifact.size > 0)
        .expect("nonempty resource object");
    let mut dlls: Vec<_> = fs::read_dir(target.join("natives"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("dll"))
        })
        .collect();
    dlls.sort();
    // Older versions extract native DLLs during installation. Modern LWJGL3
    // uses classpath native JARs, extracted by Java itself at runtime.
    let native = if let Some(dll) = dlls.into_iter().next() {
        ("native_dll", dll, false)
    } else {
        let artifact = plan
            .artifacts
            .iter()
            .find(|artifact| {
                artifact.path.contains("natives-windows") && artifact.path.ends_with(".jar")
            })
            .expect("trusted install plan must contain an extracted DLL or Windows native JAR");
        ("native_jar", target.join(&artifact.path), true)
    };
    let paths = [
        ("version_metadata", metadata_path, false),
        ("asset_index", index_path, true),
        (
            "client_jar",
            target.join(format!("versions/{version}/{version}.jar")),
            true,
        ),
        ("asset_object", target.join(&object.path), true),
        native,
    ];
    let originals: Vec<_> = paths
        .iter()
        .map(|(kind, path, corrupt)| {
            let bytes = fs::read(path).unwrap();
            assert!(!bytes.is_empty());
            (*kind, path.clone(), *corrupt, bytes)
        })
        .collect();
    let baseline: Vec<Value> = originals
        .iter()
        .map(|(kind, path, corrupt, bytes)| {
            json!({"kind":kind,"path":path.strip_prefix(&target).unwrap().to_string_lossy(),
                "bytes":bytes.len(),"sha1":format!("{:x}",Sha1::digest(bytes)),
                "damage":if *corrupt {"same_length_corruption"} else {"deleted"}})
        })
        .collect();
    evidence(&output, "baseline.json", &json!(baseline));
    let sentinels = [
        "mods/repair-sentinel.txt",
        "saves/repair-sentinel.txt",
        "config/repair-sentinel.txt",
    ];
    for relative in sentinels {
        let path = target.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, suffix.as_bytes()).unwrap();
    }
    for (_, path, corrupt, bytes) in &originals {
        // Unlink before corruption so an exported hard link cannot damage shared cache data.
        fs::remove_file(path).unwrap();
        if *corrupt {
            let mut damaged = bytes.clone();
            damaged[0] ^= 0xff;
            fs::write(path, &damaged).unwrap();
            assert_eq!(fs::metadata(path).unwrap().len(), bytes.len() as u64);
            assert_ne!(fs::read(path).unwrap(), *bytes);
        } else {
            assert!(!path.exists());
        }
    }
    evidence(
        &output,
        "damage-applied.json",
        &json!({"instanceId":instance,"fiveClassesDamaged":true}),
    );
    state
        .core
        .dispatch_mutation(
            ModuleId::Launch,
            &request(
                "launch.identity.select",
                json!({"kind":"offline","username":"SHTMCL_Test"}),
            ),
        )
        .unwrap();
    let mut cleanup = StopOnDrop {
        state: &state,
        instance: &instance,
        tasks: Vec::new(),
        armed: true,
    };
    let task = start_task(&state, "launch.game.start", json!({"instanceId":instance}));
    cleanup.tasks.push(task.clone());
    let result = wait_success(&state, &task);
    evidence(&output, "launch-result.json", &result);
    for (kind, path, _, bytes) in &originals {
        assert_eq!(
            fs::read(path).unwrap(),
            *bytes,
            "repair did not restore {kind}"
        );
    }
    for relative in sentinels {
        assert_eq!(fs::read(target.join(relative)).unwrap(), suffix.as_bytes());
    }
    let indexed = read(
        &state,
        ModuleId::Instances,
        "instances.snapshot",
        json!({"instanceId":instance}),
    );
    assert_eq!(indexed["instance"]["state"], "busy");
    let mut update = request(
        "instances.update",
        json!({"instanceId":instance,"patch":{"name":"Must remain protected"}}),
    );
    update.expected_revision = Some(Revision::new(
        indexed["instance"]["revision"].as_u64().unwrap(),
    ));
    let refusal = state
        .core
        .dispatch_mutation(ModuleId::Instances, &update)
        .unwrap_err();
    assert_eq!(refusal.code.as_str(), "instances.instance.running");
    std::thread::sleep(Duration::from_secs(5));
    let running = sessions(&state, &instance);
    let entries = running["sessions"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "unique instance must have one session");
    assert_eq!(entries[0]["state"], "process_running");
    evidence(&output, "running.json", &running);
    let second = start_task(&state, "launch.game.start", json!({"instanceId":instance}));
    cleanup.tasks.push(second.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    let refused = loop {
        let task = state.core.context().tasks().snapshot(&second).unwrap();
        if task.state.is_terminal() {
            assert_eq!(
                task.state,
                TaskState::Failed,
                "running instance must reject preparation"
            );
            assert_eq!(
                task.error
                    .as_ref()
                    .and_then(|error| error.detail_ref.as_deref()),
                Some("installed-instance:instance_busy"),
                "second start must fail specifically at busy preparation"
            );
            break task;
        }
        assert!(
            Instant::now() < deadline,
            "second start did not settle: {task:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    let after_refusal = sessions(&state, &instance);
    let after_entries = after_refusal["sessions"].as_array().unwrap();
    assert_eq!(
        after_entries.len(),
        1,
        "busy refusal must not create another session"
    );
    assert_eq!(after_entries[0]["sessionId"], entries[0]["sessionId"]);
    assert_eq!(after_entries[0]["state"], "process_running");
    for (kind, path, _, bytes) in &originals {
        assert_eq!(
            fs::read(path).unwrap(),
            *bytes,
            "busy refusal changed {kind}"
        );
    }
    for relative in sentinels {
        assert_eq!(fs::read(target.join(relative)).unwrap(), suffix.as_bytes());
    }
    evidence(
        &output,
        "running-repair-refused.json",
        &json!({
            "task":refused,"sessions":after_refusal,"fiveClassesUnchanged":true,
            "sentinelsUnchanged":true,"originalSessionStillRunning":true
        }),
    );
    let session = &entries[0]["sessionId"];
    let logs = read(
        &state,
        ModuleId::Launch,
        "launch.logs",
        json!({"sessionId":session,"limit":200}),
    );
    evidence(&output, "logs.json", &logs);
    state
        .core
        .dispatch_mutation(
            ModuleId::Launch,
            &request("launch.stop", json!({"sessionId":session})),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let exited = sessions(&state, &instance);
        let indexed = read(
            &state,
            ModuleId::Instances,
            "instances.snapshot",
            json!({"instanceId":instance}),
        );
        if exited["sessions"][0]["state"] == "exited" && indexed["instance"]["state"] == "ready" {
            evidence(&output, "exit.json", &exited);
            evidence(
                &output,
                "verified.json",
                &json!({"instanceId":instance,
                "fiveClassesRestored":true,"sentinelsPreserved":true,"runningMutationRefused":true,
                "instanceReadyAfterExit":true,"gameReached":null,"networkAdaptersEnabled":true}),
            );
            cleanup.armed = false;
            break;
        }
        assert!(
            Instant::now() < deadline,
            "game exit and lease release deadline"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("repair_verified_instance={instance}");
}

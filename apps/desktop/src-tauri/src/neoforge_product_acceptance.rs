//! Opt-in real NeoForge product publication, recovery, restart, and Java launch acceptance.
#![cfg(windows)]
use crate::AppState;
use serde_json::{json, Value};
use shtmcl_domain::{
    CommandRequest, CommandResponse, ModuleId, OperationId, RequestId, TaskId, TaskState,
};
use shtmcl_platform::PlatformDirs;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const INSTANCE: &str = "neo-product";
const GAME_VERSION: &str = "1.20.4";
const LOADER_VERSION: &str = "20.4.253";
const RENDERER_TIMEOUT: Duration = Duration::from_secs(120);
const TASK_TIMEOUT: Duration = Duration::from_secs(600);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

fn request(command: &str, mut payload: Value) -> CommandRequest {
    payload["command"] = json!(command);
    let id = shtmcl_platform::random_hex(12).unwrap();
    CommandRequest {
        request_id: RequestId::new(format!("neo-{id}")).unwrap(),
        operation_id: Some(OperationId::new(format!("neo-op-{id}")).unwrap()),
        entity_id: None,
        expected_revision: None,
        payload,
    }
}

fn start(state: &AppState, command: &str, payload: Value) -> TaskId {
    match state
        .core
        .dispatch_mutation(ModuleId::Launch, &request(command, payload))
        .unwrap()
    {
        CommandResponse::Accepted { task_id, .. } => task_id,
        _ => panic!("expected task"),
    }
}

fn wait_task(state: &AppState, id: &TaskId, evidence: &Path) -> Value {
    let deadline = Instant::now() + TASK_TIMEOUT;
    let mut previous: Option<TaskState> = None;
    loop {
        let task = state.core.context().tasks().snapshot(id).unwrap();
        if previous.as_ref() != Some(&task.state) {
            eprintln!("neo_task_phase={id:?} state={:?}", task.state);
            previous = Some(task.state.clone());
        }
        if task.state.is_terminal() {
            // Persist the true task JSON, including failures, before asserting.
            let actual = json!({
                "task": task.state,
                "error": task.error,
                "result": task.result,
            });
            let _ = fs::write(
                evidence.join(format!("task-{}.json", id.as_str())),
                serde_json::to_vec_pretty(&actual).unwrap(),
            );
            if task.state != TaskState::Succeeded {
                for child in state.core.snapshot() {
                    if child.error.is_some() {
                        eprintln!("neo_failed_child={child:?}");
                    }
                }
            }
            assert_eq!(task.state, TaskState::Succeeded, "{actual:?}");
            return actual;
        }
        assert!(Instant::now() < deadline, "task timed out: {id:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn sessions(state: &AppState, instance: &str) -> Value {
    match state
        .core
        .dispatch_read(
            ModuleId::Launch,
            &request("launch.sessions", json!({"instanceId":instance})),
        )
        .unwrap()
    {
        CommandResponse::Completed { result } => result,
        _ => panic!("expected sessions"),
    }
}

fn stop_all(state: &AppState, instance: &str) {
    if let Some(entries) = sessions(state, instance)["sessions"].as_array() {
        for item in entries {
            if item["instanceId"] == instance && item["state"] != "exited" {
                let _ = state.core.dispatch_mutation(
                    ModuleId::Launch,
                    &request("launch.stop", json!({"sessionId":item["sessionId"]})),
                );
            }
        }
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        let still_running = sessions(state, instance)["sessions"]
            .as_array()
            .map(|a| {
                a.iter()
                    .any(|s| s["instanceId"] == instance && s["state"] != "exited")
            })
            .unwrap_or(false);
        if !still_running {
            return;
        }
        if Instant::now() >= deadline {
            eprintln!("neo_cleanup_stop_timeout instance={instance}");
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

struct Cleanup<'a> {
    state: &'a AppState,
    instance: &'a str,
}
impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        // Runs even on panic; through the production launch commands only.
        stop_all(self.state, self.instance);
    }
}

fn save(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn copy_tree(src: &Path, dst: &Path) {
    crate::game_install::reject_link(src).unwrap();
    if src.is_dir() {
        fs::create_dir_all(dst).unwrap();
        for entry in fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let child = entry.path();
            let name = entry.file_name();
            copy_tree(&child, &dst.join(name));
        }
    } else if src.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::copy(src, dst).unwrap();
    } else {
        panic!("unexpected source entry: {src:?}");
    }
}

fn copy_tree_no_symlink(src: &Path, dst: &Path) {
    let meta = fs::symlink_metadata(src).unwrap();
    assert!(
        !meta.file_type().is_symlink(),
        "symlink in managed java: {src:?}"
    );
    if meta.is_dir() {
        fs::create_dir_all(dst).unwrap();
        for entry in fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            copy_tree_no_symlink(&entry.path(), &dst.join(entry.file_name()));
        }
    } else if meta.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::copy(src, dst).unwrap();
    } else {
        panic!("unexpected managed java entry: {src:?}");
    }
}

fn sha256_hex(path: &Path) -> String {
    use sha2::Digest;
    let bytes = fs::read(path).unwrap();
    let digest = sha2::Sha256::digest(&bytes);
    format!("{digest:x}")
}

fn has_renderer_obligations(log: &str) -> (bool, bool, bool, bool) {
    (
        log.contains("NeoForge mod loading, version 20.4.253, for MC 1.20.4"),
        log.contains("Reloading ResourceManager: mod_resources, mod:neoforge, vanilla"),
        log.contains("Sound engine started"),
        log.contains("minecraft:textures/atlas/gui.png-atlas"),
    )
}

fn lang_of(options: &str) -> Option<String> {
    options
        .lines()
        .find_map(|l| l.strip_prefix("lang:").map(|v| v.trim().to_string()))
}

#[test]
#[ignore = "real NeoForge product publication, restart, and ordinary Minecraft window from a fresh root"]
fn real_neoforge_product_publication_restart_and_launch() {
    let root = PathBuf::from(
        std::env::var("SHTMCL_NEO_PRODUCT_ROOT").expect("SHTMCL_NEO_PRODUCT_ROOT required"),
    );
    assert!(root.is_absolute(), "root must be absolute");
    assert!(!root.exists(), "root must be fresh/nonexistent: {root:?}");
    let source = PathBuf::from(
        std::env::var("SHTMCL_NEO_PRODUCT_SOURCE").expect("SHTMCL_NEO_PRODUCT_SOURCE required"),
    );
    assert!(source.is_dir(), "immutable source missing: {source:?}");
    let managed_java = PathBuf::from(
        std::env::var("SHTMCL_NEO_PRODUCT_JAVA").expect("SHTMCL_NEO_PRODUCT_JAVA required"),
    );
    assert!(managed_java.is_dir(), "managed java root missing");
    let expected_digest = std::env::var("SHTMCL_NEO_PRODUCT_SHA256").expect("digest required");

    fs::create_dir_all(&root).unwrap();
    let dirs = PlatformDirs::under(&root);
    let evidence = root.join("evidence");
    fs::create_dir_all(&evidence).unwrap();

    let fresh_inputs = std::env::var_os("SHTMCL_NEO_PRODUCT_FRESH_INPUTS").map(PathBuf::from);
    // A fresh run copies only verified download/metadata caches, never a game.
    if let Some(inputs) = &fresh_inputs {
        copy_tree(&inputs.join("cache/downloads"),&dirs.cache.join("downloads"));
        copy_tree(&inputs.join("data/catalog"),&dirs.data.join("catalog"));
    } else {
    // Recovery run: stage an immutable completed preparation fixture.
    let stage = dirs.instances.join(".install-neo-neo-product");
    copy_tree(&source, &stage);
    save(
        &stage.join("shtmcl-install.json"),
        &json!({
            "schemaVersion":1,"complete":true,"versionId":GAME_VERSION,
            "loader":"neoforge","loaderVersion":LOADER_VERSION,
            "preparedSha256": expected_digest,
            "instanceId": INSTANCE,
        }),
    );
    }
    copy_tree_no_symlink(&managed_java, &dirs.data.join("java"));

    // Language oracle on a separate fresh root: initialize then read generated options.
    let lang_root = root.join("lang-oracle");
    fs::create_dir(&lang_root).unwrap();
    crate::game_language::initialize(&lang_root, GAME_VERSION).unwrap();
    let oracle_options = fs::read_to_string(lang_root.join("options.txt")).unwrap();
    let expected_lang = lang_of(&oracle_options).expect("device language initialized");

    let state = AppState::new(dirs.clone());
    let _cleanup = Cleanup {
        state: &state,
        instance: INSTANCE,
    };
    state
        .core
        .dispatch_mutation(
            ModuleId::Launch,
            &request(
                "launch.identity.select",
                json!({"kind":"offline","username":"NeoProduct"}),
            ),
        )
        .unwrap();

    // Install exercises owned-stage recovery, verification, publication, registration.
    let install_task = start(
        &state,
        "launch.install",
        json!({
            "versionId": GAME_VERSION,
            "loader": "neoforge",
            "loaderVersion": LOADER_VERSION,
            "instanceId": INSTANCE,
            "name": "NeoForge Product",
        }),
    );
    let installed = wait_task(&state, &install_task, &evidence);
    save(&evidence.join("installed.json"), &installed);

    let target = dirs.instances.join(INSTANCE);
    let manifest_sha = sha256_hex(&target.join("shtmcl-install.json"));

    let snapshot = state
        .core
        .dispatch_read(
            ModuleId::Instances,
            &request("instances.snapshot", json!({"instanceId":INSTANCE})),
        )
        .unwrap();
    let snapshot = match snapshot {
        CommandResponse::Completed { result } => result,
        _ => panic!("expected snapshot"),
    };
    save(&evidence.join("catalog-snapshot.json"), &snapshot);
    let record = &snapshot["instance"];
    assert_eq!(record["version"]["loader"], "neoforge");
    assert_eq!(record["javaMajor"], 17);
    assert_eq!(record["state"], "ready");

    // Drop and reopen to prove persisted index/recovery.
    drop(_cleanup);
    drop(state);
    let state = AppState::new(PlatformDirs::under(&root));
    let _cleanup = Cleanup {
        state: &state,
        instance: INSTANCE,
    };
    let reopened = match state
        .core
        .dispatch_read(
            ModuleId::Instances,
            &request("instances.snapshot", json!({"instanceId":INSTANCE})),
        )
        .unwrap()
    {
        CommandResponse::Completed { result } => result,
        _ => panic!("expected reopened snapshot"),
    };
    save(&evidence.join("reopened-snapshot.json"), &reopened);
    assert!(
        reopened["instance"]["id"] == INSTANCE && reopened["instance"]["state"] == "ready",
        "persisted index missing after reopen"
    );

    state
        .core
        .dispatch_mutation(
            ModuleId::Launch,
            &request(
                "launch.identity.select",
                json!({"kind":"offline","username":"NeoProduct"}),
            ),
        )
        .unwrap();

    let started = Instant::now();
    let launch_task = start(&state, "launch.game.start", json!({"instanceId": INSTANCE}));
    let launched = wait_task(&state, &launch_task, &evidence);
    save(&evidence.join("launch.json"), &launched);
    let elapsed = started.elapsed();

    // Wait for real renderer obligations and require a live process.
    let log_path = target.join("logs/latest.log");
    let deadline = Instant::now() + RENDERER_TIMEOUT;
    let mut obligations;
    loop {
        let bytes = fs::read(&log_path).unwrap_or_default();
        let log = String::from_utf8_lossy(&bytes);
        obligations = has_renderer_obligations(&log);
        let session_live = sessions(&state, INSTANCE)["sessions"]
            .as_array()
            .map(|a| {
                a.iter()
                    .any(|s| s["instanceId"] == INSTANCE && s["state"] == "process_running")
            })
            .unwrap_or(false);
        if obligations.0 && obligations.1 && obligations.2 && obligations.3 {
            assert!(
                session_live,
                "renderer reached but session not process_running"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "renderer obligations timed out: {obligations:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // Copy actual log out; never delete evidence or source.
    fs::copy(&log_path,evidence.join("game-latest.log")).unwrap();

    // Options readback from the real game.
    let runtime_options = fs::read_to_string(target.join("options.txt")).unwrap();
    fs::write(evidence.join("game-options.txt"), &runtime_options).unwrap();
    let actual_lang = lang_of(&runtime_options);
    assert_eq!(
        actual_lang.as_deref(),
        Some(expected_lang.as_str()),
        "game options lang mismatch"
    );

    let final_sessions = sessions(&state, INSTANCE);
    save(&evidence.join("sessions.json"), &final_sessions);

    save(
        &evidence.join("result.json"),
        &json!({
            "instanceId": INSTANCE,
            "gameVersion": GAME_VERSION,
            "loaderVersion": LOADER_VERSION,
            "loader": "neoforge",
            "javaMajor": 17,
            "manifestSha256": manifest_sha,
            "elapsedMs": elapsed.as_millis() as u64,
            "rendererObligations": {
                "neoForgeModLoading": obligations.0,
                "resourceManagerModResources": obligations.1,
                "soundEngineStarted": obligations.2,
                "guiAtlas": obligations.3,
            },
            "processRunning": true,
            "langExpected": expected_lang,
            "langActual": actual_lang,
            "worldEnteredVerified": false,
            "freshInstallation": fresh_inputs.is_some(),
            "sourcePreserved": true,
        }),
    );
    drop(_cleanup);
    let stopped = sessions(&state, INSTANCE);
    save(&evidence.join("stopped-sessions.json"), &stopped);
    assert!(stopped["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["state"] == "exited"));
    println!("NeoForge product publication, restart, and running renderer passed: {INSTANCE}");
}

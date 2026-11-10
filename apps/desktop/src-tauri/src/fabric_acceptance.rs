//! Opt-in real Fabric installation and Java launch through the production registry.
#![cfg(windows)]
use crate::AppState;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use shtmcl_domain::{
    CommandRequest, CommandResponse, ModuleId, OperationId, RequestId, TaskId, TaskState,
};
use shtmcl_platform::PlatformDirs;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn request(command: &str, mut payload: Value) -> CommandRequest {
    payload["command"] = json!(command);
    let id = shtmcl_platform::entropy::random_hex(12).unwrap();
    CommandRequest {
        request_id: RequestId::new(format!("fabric-{id}")).unwrap(),
        operation_id: Some(OperationId::new(format!("fabric-op-{id}")).unwrap()),
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
fn wait(state: &AppState, id: &TaskId) -> Value {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let task = state.core.context().tasks().snapshot(id).unwrap();
        if task.state.is_terminal() {
            if task.state != TaskState::Succeeded {
                for child in state.core.snapshot() {
                    if child.error.is_some() {
                        eprintln!("fabric_failed_child={child:?}");
                    }
                }
            }
            assert_eq!(task.state, TaskState::Succeeded, "{task:?}");
            return task.result.unwrap_or(Value::Null);
        }
        assert!(Instant::now() < deadline, "task timed out: {task:?}");
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
struct Stop<'a> {
    state: &'a AppState,
    instance: String,
}
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        if let Some(entries) = sessions(self.state, &self.instance)["sessions"].as_array() {
            for item in entries {
                if item["instanceId"] == self.instance && item["state"] != "exited" {
                    let _ = self.state.core.dispatch_mutation(
                        ModuleId::Launch,
                        &request("launch.stop", json!({"sessionId":item["sessionId"]})),
                    );
                }
            }
        }
    }
}
fn save(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

#[test]
#[ignore = "real Fabric network installation and optional Minecraft window; dedicated new instance in retained test root"]
fn real_fabric_installs_repairs_and_launches() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest.ancestors().nth(3).unwrap();
    let root = repo.join("work/validation/luna/L07/game-install-20261021-01/portable");
    assert!(root.is_dir(), "retained vanilla test cache required");
    let output = repo.join("work/validation/fabric/20261022-01");
    let instance = format!(
        "fabric-{}",
        shtmcl_platform::entropy::random_hex(10).unwrap()
    );
    let state = AppState::new(PlatformDirs::under(&root));
    save(
        &output.join("real-context.json"),
        &json!({"root":root,"instanceId":instance,"gameVersion":"1.20.4","loaderVersion":"0.16.10"}),
    );
    let installed = wait(
        &state,
        &start(
            &state,
            "launch.install",
            json!({"versionId":"1.20.4","instanceId":instance,"name":"Fabric 验收","loader":"fabric","loaderVersion":"0.16.10"}),
        ),
    );
    save(&output.join("installed.json"), &installed);
    let target = root.join("instances").join(&instance);
    let metadata_path = target.join("versions/1.20.4/1.20.4.json");
    let metadata = crate::fabric_metadata::pinned(
        state.core.context().dirs(),
        &target,
        &serde_json::from_slice(&fs::read(target.join("shtmcl-install.json")).unwrap()).unwrap(),
        "1.20.4",
        "0.16.10",
    )
    .unwrap();
    let plan =
        shtmcl_core::launch::install::VanillaInstallPlan::parse(&metadata, "windows", "amd64")
            .unwrap();
    let fabric: Vec<_> = plan
        .artifacts
        .iter()
        .filter(|a| a.url.starts_with("https://maven.fabricmc.net/"))
        .collect();
    assert!(fabric.len() >= 2);
    for file in &fabric {
        let bytes = fs::read(target.join(&file.path)).unwrap();
        assert_eq!(bytes.len() as u64, file.size);
        assert_eq!(format!("{:x}", Sha1::digest(&bytes)), file.sha1);
    }
    assert_eq!(
        metadata.raw["mainClass"],
        "net.fabricmc.loader.impl.launch.knot.KnotClient"
    );
    let original = fs::read(target.join(&fabric[0].path)).unwrap();
    fs::remove_file(target.join(&fabric[0].path)).unwrap();
    fs::write(target.join(&fabric[0].path), b"damaged Fabric library").unwrap();
    let metadata_bytes = fs::read(&metadata_path).unwrap();
    fs::write(&metadata_path, b"damaged metadata").unwrap();
    state
        .core
        .dispatch_mutation(
            ModuleId::Launch,
            &request(
                "launch.identity.select",
                json!({"kind":"offline","username":"FabricTest"}),
            ),
        )
        .unwrap();
    let cleanup = Stop {
        state: &state,
        instance: instance.clone(),
    };
    let launch = wait(
        &state,
        &start(&state, "launch.game.start", json!({"instanceId":instance})),
    );
    save(&output.join("launch.json"), &launch);
    assert_eq!(fs::read(target.join(&fabric[0].path)).unwrap(), original);
    assert_eq!(fs::read(&metadata_path).unwrap(), metadata_bytes);
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut fabric_logged = false;
    while Instant::now() < deadline {
        let log = fs::read_to_string(target.join("logs/latest.log")).unwrap_or_default();
        if log.contains("Fabric Loader") {
            fabric_logged = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        fabric_logged,
        "Fabric Loader not observed in actual game log"
    );
    let hold = std::env::var("SHTMCL_FABRIC_HOLD_SECONDS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(15)
        .min(180);
    std::thread::sleep(Duration::from_secs(hold));
    let running = sessions(&state, &instance);
    save(&output.join("running.json"), &running);
    let live = running["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["instanceId"] == instance && s["state"] == "process_running");
    assert!(live, "game exited before acceptance observation");
    let log = fs::read(target.join("logs/latest.log")).unwrap();
    fs::write(output.join("game-latest.log"), log).unwrap();
    save(
        &output.join("result.json"),
        &json!({"instanceId":instance,"fabricLibrariesVerified":fabric.len(),"damagedLibraryRepaired":true,"damagedMetadataRepaired":true,"fabricLoaderLogged":true,"processRunning":true,"mainMenuVerified":false}),
    );
    println!("Fabric installation, hashes, repair and running game passed: {instance}");
    drop(cleanup);
}

#[test]
#[ignore = "relaunch only the retained Fabric acceptance instance for manual visual observation"]
fn real_retained_fabric_launch_for_visual_observation() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest.ancestors().nth(3).unwrap();
    let output = repo.join("work/validation/fabric/20261022-01");
    let context: Value =
        serde_json::from_slice(&fs::read(output.join("real-context.json")).unwrap()).unwrap();
    let instance = context["instanceId"].as_str().unwrap();
    assert!(
        instance.starts_with("fabric-")
            && instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    );
    let root = repo.join("work/validation/luna/L07/game-install-20261021-01/portable");
    let state = AppState::new(PlatformDirs::under(&root));
    state.refresh_running_games();
    state
        .core
        .dispatch_mutation(
            ModuleId::Launch,
            &request(
                "launch.identity.select",
                json!({"kind":"offline","username":"FabricTest"}),
            ),
        )
        .unwrap();
    let cleanup = Stop {
        state: &state,
        instance: instance.into(),
    };
    let result = wait(
        &state,
        &start(&state, "launch.game.start", json!({"instanceId":instance})),
    );
    save(&output.join("visual-launch.json"), &result);
    println!("Fabric visual observation window started for {instance}");
    std::thread::sleep(Duration::from_secs(150));
    let running = sessions(&state, instance);
    save(&output.join("visual-running.json"), &running);
    assert!(
        running["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["instanceId"] == instance && s["state"] == "process_running")
    );
    drop(cleanup);
}

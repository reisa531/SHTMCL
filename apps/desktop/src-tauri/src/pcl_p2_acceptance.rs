//! Opt-in cache-assisted installation through the production registry.
//! No installed game, identity or index is copied. Does not launch Minecraft.
#![cfg(windows)]
use crate::AppState;
use serde_json::{json, Value};
use shtmcl_domain::{CommandRequest, CommandResponse, ModuleId, OperationId, RequestId, TaskState};
use shtmcl_platform::PlatformDirs;
use std::{fs, path::{Path, PathBuf}, time::{Duration, Instant}};

fn request(command: &str, mut payload: Value) -> CommandRequest {
    let id = shtmcl_platform::random_hex(12).unwrap();
    payload["command"] = json!(command);
    CommandRequest {
        request_id: RequestId::new(format!("p2-{id}")).unwrap(),
        operation_id: Some(OperationId::new(format!("p2-op-{id}")).unwrap()),
        entity_id: None, expected_revision: None, payload,
    }
}
fn save(dir: &Path, name: &str, value: &Value) {
    fs::write(dir.join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
}
fn copy_tree(src: &Path, dst: &Path) -> u64 {
    crate::game_install::reject_link(src).unwrap();
    if src.is_dir() {
        fs::create_dir_all(dst).unwrap();
        fs::read_dir(src).unwrap().map(|e| {
            let e = e.unwrap(); copy_tree(&e.path(), &dst.join(e.file_name()))
        }).sum()
    } else {
        fs::copy(src, dst).unwrap(); 1
    }
}
fn snapshot(state: &AppState, id: &str) -> Value {
    match state.core.dispatch_read(ModuleId::Instances, &request("instances.snapshot", json!({"instanceId":id}))) {
        Ok(CommandResponse::Completed { result }) => result,
        other => json!({"error":format!("{other:?}")}),
    }
}

#[test]
#[ignore = "real HTTPS plus retained verified caches; fresh isolated root required"]
fn real_p2_supported_installers_and_restart() {
    let root = PathBuf::from(std::env::var("SHTMCL_P2_REAL_ROOT").expect("fresh root required"));
    assert!(root.is_absolute() && !root.exists());
    let sources: Vec<PathBuf> = serde_json::from_str(&std::env::var("SHTMCL_P2_REAL_SOURCES").expect("source roots JSON required")).unwrap();
    assert!(!sources.is_empty());
    fs::create_dir_all(&root).unwrap();
    let evidence = root.join("evidence"); fs::create_dir(&evidence).unwrap();
    let mut copied = Vec::new();
    for source in &sources {
        assert!(source.is_absolute() && source.is_dir());
        for child in ["cache/downloads", "data/catalog", "data/java"] {
            let from = source.join(child);
            if from.is_dir() { copied.push(json!({"source":from,"files":copy_tree(&from,&root.join(child))})); }
        }
    }
    save(&evidence, "inputs.json", &json!({"copied":copied,"networkDisabled":false,"copiesInstalledInstances":false}));
    let state = AppState::new(PlatformDirs::under(&root));
    let probe = state.core.dispatch_read(ModuleId::Launch, &request("launch.fabric.versions",json!({"gameVersion":"1.20.4"})));
    save(&evidence,"fabric-catalogue.json",&json!({"response":format!("{probe:?}")}));
    let cases = [
        ("p2-vanilla", "1.7.10", "vanilla", ""),
        ("p2-fabric", "1.20.4", "fabric", "0.16.10"),
        ("p2-neoforge", "1.20.4", "neoforge", "20.4.253"),
    ];
    let mut results = Vec::new();
    for (id, version, loader, loader_version) in cases {
        let mut payload = json!({"instanceId":id,"versionId":version,"name":id});
        if loader != "vanilla" { payload["loader"]=json!(loader); payload["loaderVersion"]=json!(loader_version); }
        let deadline = Instant::now()+Duration::from_secs(5);
        let accepted = loop {
            let response = state.core.dispatch_mutation(ModuleId::Launch,&request("launch.install",payload.clone()));
            if matches!(&response,Err(e) if e.code.as_str()=="launch.install.busy") && Instant::now()<deadline {
                std::thread::sleep(Duration::from_millis(100)); continue;
            }
            break response;
        };
        let task_id = match accepted {
            Ok(CommandResponse::Accepted { task_id, .. }) => task_id,
            other => { results.push(json!({"id":id,"ok":false,"error":format!("{other:?}")})); continue; }
        };
        let deadline = Instant::now()+Duration::from_secs(300);
        let task = loop {
            let task = state.core.context().tasks().snapshot(&task_id).unwrap();
            if task.state.is_terminal() || Instant::now()>=deadline { break task; }
            std::thread::sleep(Duration::from_millis(100));
        };
        let all = state.core.snapshot();
        save(&evidence,&format!("{id}-task.json"),&serde_json::to_value(&task).unwrap());
        save(&evidence,&format!("{id}-children.json"),&serde_json::to_value(all).unwrap());
        let installed = snapshot(&state,id);
        save(&evidence,&format!("{id}-snapshot.json"),&installed);
        results.push(json!({"id":id,"ok":task.state==TaskState::Succeeded,"state":task.state,"error":task.error}));
        eprintln!("p2-install={id} state={:?}",task.state);
    }
    save(&evidence,"results.json",&json!(results));
    drop(state);
    let reopened = AppState::new(PlatformDirs::under(&root));
    let mut persistence = Vec::new();
    for (id, version, loader, loader_version) in cases {
        let value = snapshot(&reopened,id);
        let receipt: Value = fs::read(root.join("instances").join(id).join("shtmcl-install.json"))
            .ok().and_then(|b|serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        let game = &value["instance"];
        let ok = game["state"]=="ready" && game["version"]["gameVersion"]==version
            && game["version"]["loader"]==loader && receipt["complete"]==true
            && (loader=="vanilla" || game["version"]["loaderVersion"]==loader_version);
        persistence.push(json!({"id":id,"ok":ok,"snapshot":value,"receipt":receipt}));
    }
    save(&evidence,"persistence.json",&json!(persistence));
    assert!(results.iter().all(|r|r["ok"]==true),"real installer failures: {results:?}");
    assert!(persistence.iter().all(|r|r["ok"]==true),"persistence failures: {persistence:?}");
}

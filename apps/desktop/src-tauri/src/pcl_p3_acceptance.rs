//! Actual registry and filesystem checks; jar contents are synthetic bytes.
use super::*;
use serde_json::{json, Value};
use shtmcl_domain::{OperationId, RequestId};
use std::{fs, path::Path};

fn request(payload: Value) -> CommandRequest {
    CommandRequest {
        request_id: RequestId::new(shtmcl_platform::entropy::random_hex(12).unwrap()).unwrap(),
        operation_id: None, entity_id: None, expected_revision: None, payload,
    }
}
fn save(name: &str, value: &Value) {
    if let Some(dir) = std::env::var_os("SHTMCL_P3_EVIDENCE_DIR") {
        let dir = std::path::PathBuf::from(dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }
}
fn snapshot(state: &AppState, root: &Path) -> Value {
    let req = request(json!({"command":"instances.snapshot","instanceId":"school"}));
    let CommandResponse::Completed { mut result } = state.core.dispatch_read(ModuleId::Instances, &req).unwrap() else { panic!("snapshot not completed") };
    crate::instance_presentation::enrich_snapshot(&mut result, &root.join("instances"));
    result
}
#[test]
fn production_registry_inventory_survives_reopen_and_unwired_source_fails_closed() {
    let root = std::env::temp_dir().join(format!("shtmcl-p3-{}", shtmcl_platform::entropy::random_hex(12).unwrap()));
    fs::create_dir_all(&root).unwrap();
    let state = AppState::new(PlatformDirs::under(&root));
    let req = request(json!({"command":"content.search","queryId":"p3-probe","provider":"curseforge","query":"sodium","limit":20,"target":{"gameVersion":"1.20.4","loader":"fabric"}}));
    let error = state.core.dispatch_read(ModuleId::Content, &req).unwrap_err();
    assert_eq!(error.code.as_str(), "content.owner.unavailable");
    save("content-search-probe.json", &json!({"code": error.code.as_str(), "network":false}));
    let game = root.join("instances/school");
    let mut create = request(json!({"command":"instances.create","instance":{"id":"school","name":"School","version":{"gameVersion":"1.20.4","loader":"fabric","loaderVersion":"0.16.10"},"location":{"root":game},"javaMajor":17}}));
    create.operation_id = Some(OperationId::new("p3-create").unwrap());
    assert!(matches!(state.core.dispatch_mutation(ModuleId::Instances, &create).unwrap(), CommandResponse::Completed { .. }));
    fs::create_dir_all(game.join("mods")).unwrap();
    fs::write(game.join("mods/a.jar"), b"AAAA").unwrap();
    fs::write(game.join("mods/b.jar.disabled"), b"BBBBBB").unwrap();
    fs::write(game.join("mods/notes.txt"), b"note").unwrap();
    let first = snapshot(&state, &root);
    assert_eq!(first["instance"]["id"], "school");
    assert_eq!(first["instance"]["presentation"]["modInventory"], json!({"status":"ready","files":[{"fileName":"a.jar","enabled":true,"bytes":4},{"fileName":"b.jar.disabled","enabled":false,"bytes":6}]}));
    drop(state);
    let reopened = AppState::new(PlatformDirs::under(&root));
    let second = snapshot(&reopened, &root);
    assert_eq!(first, second);
    save("inventory-reopen-probe.json", &json!({"snapshot":second,"reopenStable":true,"syntheticJarBytes":true}));
    let outside = root.join("outside");
    fs::create_dir_all(outside.join("mods")).unwrap();
    fs::write(outside.join("mods/private.jar"), b"private").unwrap();
    let mut wrong = json!({"instance":{"id":"school","location":{"root":outside}}});
    crate::instance_presentation::enrich_snapshot(&mut wrong, &root.join("instances"));
    assert!(wrong["instance"]["presentation"].is_null());
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn production_registry_mod_toggle_persists_and_publishes_the_committed_revision() {
    let root=std::env::temp_dir().join(format!("shtmcl-p3-toggle-{}",shtmcl_platform::entropy::random_hex(12).unwrap()));
    fs::create_dir_all(&root).unwrap();
    let state=AppState::new(PlatformDirs::under(&root));
    let game=root.join("instances/school");
    let mut create=request(json!({"command":"instances.create","instance":{"id":"school","name":"School","version":{"gameVersion":"1.20.4","loader":"vanilla"},"location":{"root":game},"javaMajor":17}}));
    create.operation_id=Some(OperationId::new("toggle-create").unwrap());
    state.core.dispatch_mutation(ModuleId::Instances,&create).unwrap();
    fs::create_dir_all(game.join("mods")).unwrap();
    let archive=include_bytes!("../../../../tests/modules/04/fixtures/mod-metadata/fabric-declaration.zip");
    fs::write(game.join("mods/example.jar"),archive).unwrap();
    let read=request(json!({"command":"instances.mod.snapshot","instanceId":"school","fileName":"example.jar"}));
    let CommandResponse::Completed {result:before}=state.core.dispatch_read(ModuleId::Instances,&read).unwrap() else {panic!("read not completed")};
    let mut toggle=request(json!({"command":"instances.mod.toggle","instanceId":"school","fileName":"example.jar","enabled":false,"sha256":before["file"]["sha256"]}));
    let op=OperationId::new("toggle-operation").unwrap();toggle.operation_id=Some(op.clone());toggle.expected_revision=Some(shtmcl_domain::Revision::new(1));
    let CommandResponse::Completed {result:written}=state.core.dispatch_mutation(ModuleId::Instances,&toggle).unwrap() else {panic!("toggle not completed")};
    assert_eq!(written["revision"],2);
    assert!(!game.join("mods/example.jar").exists());
    assert_eq!(fs::read(game.join("mods/example.jar.disabled")).unwrap(),archive);
    let events=state.core.event_sink().since(&op,None);
    assert_eq!(events.len(),1);assert_eq!(events[0].revision.get(),2);
    drop(state);
    let reopened=AppState::new(PlatformDirs::under(&root));
    let after=snapshot(&reopened,&root);
    assert_eq!(after["instance"]["revision"],2);
    assert_eq!(after["instance"]["presentation"]["modInventory"]["files"][0]["enabled"],false);
    let CommandResponse::Completed {result:replay}=reopened.core.dispatch_mutation(ModuleId::Instances,&toggle).unwrap() else {panic!("replay not completed")};
    assert_eq!(replay["replayed"],true);assert_eq!(replay["revision"],2);
    save("mod-toggle-reopen.json",&json!({"before":before,"after":after,"replayed":replay,"eventRevision":events[0].revision.get(),"syntheticArchive":true}));
    drop(reopened);fs::remove_dir_all(root).unwrap();
}

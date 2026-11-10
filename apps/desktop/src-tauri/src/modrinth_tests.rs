use super::*;
use serde_json::json;
use shtmcl_core::content::{
    loader::{GameTarget, LoaderKind},
    ports::{SourceKind, SourcePort, SourceQuery},
};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Http {
    calls: Mutex<Vec<reqwest::Url>>,
    responses: Mutex<VecDeque<Result<Value, &'static str>>>,
}
impl Http {
    fn new(responses: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(vec![]),
            responses: Mutex::new(responses.into_iter().map(Ok).collect()),
        })
    }
}
impl MetadataHttp for Http {
    fn get(&self, url: &reqwest::Url) -> Result<Vec<u8>, ErrorEnvelope> {
        self.calls.lock().unwrap().push(url.clone());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected HTTP call")
            .map(|v| serde_json::to_vec(&v).unwrap())
            .map_err(error)
    }
}
fn project() -> Value {
    json!({"id":"sodium","slug":"sodium","title":"Sodium","description":"Fast rendering","project_type":"mod","client_side":"required"})
}
fn version(id: &str) -> Value {
    json!({"id":id,"project_id":"sodium","name":"Sodium Test","version_number":"not-semver+mc1.20.4","version_type":"release","date_published":"2026-10-30T00:00:00Z",
    "game_versions":["1.20.4"],"loaders":["fabric"],"files":[{"filename":"sodium.jar","size":1024,"primary":true}],
    "dependencies":[{"dependency_type":"required","project_id":"fabric-api","version_id":"exactVersion","file_name":null},
        {"dependency_type":"optional","project_id":"optional"},{"dependency_type":"incompatible","project_id":"conflict"},{"dependency_type":"embedded","file_name":"inside.jar"}]})
}
fn target() -> GameTarget {
    GameTarget::new("1.20.4", LoaderKind::Fabric)
}
fn query() -> SourceQuery {
    SourceQuery {
        query_id: "q1".into(),
        provider: Some(SourceKind::Modrinth),
        text: "sodium & iris".into(),
        target: Some(target()),
        limit: 20,
    }
}

#[test]
fn search_uses_encoded_mod_client_game_loader_facets_and_real_identity() {
    let http = Http::new(vec![
        json!({"hits":[{"project_id":"sodium","slug":"mc1.16.5-sodium","title":"Sodium","project_type":"mod"}]}),
    ]);
    let source = Modrinth::with_http(http.clone());
    let result = source.search(&query()).unwrap();
    assert!(result.connected);
    assert_eq!(result.query_id, "q1");
    assert_eq!(result.projects[0].project_id, "sodium");
    assert_eq!(result.projects[0].slug, "mc1.16.5-sodium");
    let calls = http.calls.lock().unwrap();
    let params: std::collections::BTreeMap<_, _> = calls[0].query_pairs().into_owned().collect();
    assert_eq!(params["query"], "sodium & iris");
    assert_eq!(
        serde_json::from_str::<Value>(&params["facets"]).unwrap(),
        json!([
            ["project_type:mod"],
            ["client_side:required", "client_side:optional"],
            ["versions:1.20.4"],
            ["categories:fabric"]
        ])
    );
}

#[test]
fn unwired_sources_and_vanilla_do_not_issue_network_requests() {
    let http = Http::new(vec![]);
    let source = Modrinth::with_http(http.clone());
    let mut q = query();
    q.provider = Some(SourceKind::CurseForge);
    assert_eq!(
        source.search(&q).unwrap_err().code(),
        "content.owner.unavailable"
    );
    q.provider = Some(SourceKind::Modrinth);
    q.target = Some(GameTarget::new("1.20.4", LoaderKind::Vanilla));
    assert!(source.search(&q).unwrap().projects.is_empty());
    assert_eq!(
        source
            .versions("sodium", q.target.as_ref().unwrap(), 0)
            .unwrap()["total"],
        0
    );
    assert!(http.calls.lock().unwrap().is_empty());
}

#[test]
fn invalid_identifiers_queries_and_cross_origin_requests_are_rejected_before_http() {
    let http = Http::new(vec![]);
    let source = Modrinth::with_http(http.clone());
    for id in ["../secret", "x/y", "a?b", "a#b", "https://host", ""] {
        assert!(source.project(id).is_err());
    }
    assert!(source.get("https://other.invalid/", &[]).is_err());
    assert!(source.get("../project/x", &[]).is_err());
    let mut q = query();
    q.text = "x".repeat(257);
    assert!(source.search_mods(&q).is_err());
    q.text = "x".into();
    q.limit = 101;
    assert!(source.search_mods(&q).is_err());
    assert!(http.calls.lock().unwrap().is_empty());
}

#[test]
fn versions_refilter_target_preserve_exact_dependency_ids_and_page_without_truncating_total() {
    let mut versions: Vec<_> = (0..23).map(|i| version(&format!("v{i}"))).collect();
    let mut wrong = version("wrong");
    wrong["loaders"] = json!(["forge"]);
    versions.push(wrong);
    let http = Http::new(vec![project(), json!(versions)]);
    let source = Modrinth::with_http(http.clone());
    let first = source.versions("sodium", &target(), 0).unwrap();
    assert_eq!(first["total"], 23);
    assert_eq!(first["versions"].as_array().unwrap().len(), 20);
    assert_eq!(
        first["versions"][0]["dependencies"][0]["versionId"],
        "exactVersion"
    );
    assert_eq!(
        first["versions"][0]["dependencies"][2]["kind"],
        "incompatible"
    );
    assert_eq!(first["versions"][0]["versionNumber"], "not-semver+mc1.20.4");
    let last = source.versions("sodium", &target(), 20).unwrap();
    assert_eq!(last["versions"].as_array().unwrap().len(), 3);
    assert_eq!(last["versions"][0]["versionId"], "v20");
    assert_eq!(
        http.calls.lock().unwrap().len(),
        2,
        "cached metadata reused across pages"
    );
}

#[test]
fn identity_and_malformed_dependencies_fail_instead_of_empty_versions() {
    let mut bad = version("v1");
    bad["dependencies"][0]["dependency_type"] = json!("surprise");
    let source = Modrinth::with_http(Http::new(vec![project(), json!([bad])]));
    assert_eq!(
        source
            .versions("sodium", &target(), 0)
            .unwrap_err()
            .code
            .as_str(),
        "content.modrinth.invalid_metadata"
    );
    let mut wrong = project();
    wrong["id"] = json!("another");
    assert!(Modrinth::with_http(Http::new(vec![wrong]))
        .project("sodium")
        .is_err());
    let mut foreign = version("foreign");
    foreign["project_id"] = json!("another");
    assert!(
        Modrinth::with_http(Http::new(vec![project(), json!([foreign])]))
            .versions("sodium", &target(), 0)
            .is_err()
    );
}

#[test]
fn unsupported_client_project_has_no_client_versions() {
    let mut server = project();
    server["client_side"] = json!("unsupported");
    let source = Modrinth::with_http(Http::new(vec![server]));
    assert_eq!(source.versions("sodium", &target(), 0).unwrap()["total"], 0);
}

#[test]
fn failures_are_not_cached_and_success_cache_has_entry_budget() {
    let http = Http::new((0..26).map(|_| project()).collect());
    http.responses
        .lock()
        .unwrap()
        .push_front(Err("content.modrinth.rate_limited"));
    let source = Modrinth::with_http(http.clone());
    assert_eq!(
        source.project("sodium").unwrap_err().code.as_str(),
        "content.modrinth.rate_limited"
    );
    source.project("sodium").unwrap();
    source.project("sodium").unwrap();
    assert_eq!(http.calls.lock().unwrap().len(), 2);
    for i in 0..25 {
        source.get(&format!("project/project{i}"), &[]).unwrap();
    }
    assert_eq!(source.state.lock().unwrap().cache.len(), CACHE_ENTRIES);
    assert_eq!(source.state.lock().unwrap().active, 0);
}

#[test]
fn response_and_parallel_request_budgets_are_enforced() {
    struct Big;
    impl MetadataHttp for Big {
        fn get(&self, _: &reqwest::Url) -> Result<Vec<u8>, ErrorEnvelope> {
            Ok(vec![b' '; RESPONSE_LIMIT + 1])
        }
    }
    let source = Modrinth::with_http(Arc::new(Big));
    assert!(source.project("sodium").is_err());
    assert_eq!(source.state.lock().unwrap().active, 0);
    struct Blocking {
        started: AtomicUsize,
        gate: std::sync::Barrier,
    }
    impl MetadataHttp for Blocking {
        fn get(&self, _: &reqwest::Url) -> Result<Vec<u8>, ErrorEnvelope> {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.gate.wait();
            Ok(b"{}".to_vec())
        }
    }
    let http = Arc::new(Blocking {
        started: AtomicUsize::new(0),
        gate: std::sync::Barrier::new(5),
    });
    let source = Arc::new(Modrinth::with_http(http.clone()));
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let source = source.clone();
            scope.spawn(move || source.get("search", &[]).unwrap());
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while http.started.load(Ordering::SeqCst) < 4 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(
            source.get("search", &[]).unwrap_err().code.as_str(),
            "content.modrinth.busy"
        );
        http.gate.wait();
    });
    assert_eq!(source.state.lock().unwrap().active, 0);
}

#[test]
fn production_catalog_uses_instance_owner_and_ignores_forged_target() {
    use shtmcl_core::CoreModule;
    use shtmcl_domain::{CommandRequest, CommandResponse, ModuleId, OperationId, RequestId};
    let root = std::env::temp_dir().join(format!(
        "shtmcl-catalog-{}",
        shtmcl_platform::entropy::random_hex(8).unwrap()
    ));
    let state = crate::AppState::new(shtmcl_platform::PlatformDirs::under(&root));
    let ctx = state.core.context().clone();
    let instances: Arc<dyn CoreModule> = Arc::new(
        shtmcl_core::instances::InstancesModule::new(
            ctx.clone(),
            Box::new(shtmcl_core::instances::FileStorage::new(root.join("test-index")).unwrap()),
        )
        .unwrap(),
    );
    let mut req = CommandRequest {
        request_id: RequestId::new("create-test").unwrap(),
        operation_id: Some(OperationId::new("create-test").unwrap()),
        entity_id: None,
        expected_revision: None,
        payload: json!({"command":"instances.create","instance":{"id":"school","name":"School","version":{"gameVersion":"1.20.4","loader":"fabric","loaderVersion":"0.16.10"},"location":{"root":root.join("instances/school")},"javaMajor":17}}),
    };
    instances.handle(&req).unwrap();
    let source = Arc::new(Modrinth::with_http(Http::new(vec![
        project(),
        json!([version("v1")]),
    ])));
    let catalog = crate::content_catalog::ContentCatalog::with_source(ctx, Some(instances), source);
    req.operation_id = None;
    req.payload = json!({"command":"content.modrinth.versions","projectId":"sodium","instanceId":"school","target":{"gameVersion":"forged","loader":"forge"}});
    let CommandResponse::Completed { result } = catalog.handle(&req).unwrap() else {
        panic!("completed")
    };
    assert_eq!(result["target"]["gameVersion"], "1.20.4");
    assert_eq!(result["target"]["loader"], "fabric");
    req.payload["instanceId"] = json!("missing");
    assert!(catalog.handle(&req).is_err());
    assert_eq!(catalog.id(), ModuleId::Content);
    drop(catalog);
    drop(state);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "public Modrinth metadata network probe; no downloads or game installation"]
fn live_public_catalog_metadata() {
    use shtmcl_domain::{CommandRequest, CommandResponse, ModuleId, OperationId, RequestId};
    let root = std::env::temp_dir().join(format!(
        "shtmcl-live-catalog-{}",
        shtmcl_platform::entropy::random_hex(8).unwrap()
    ));
    let state = crate::AppState::new(shtmcl_platform::PlatformDirs::under(&root));
    let request = |payload| CommandRequest {
        request_id: RequestId::new(shtmcl_platform::entropy::random_hex(8).unwrap()).unwrap(),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload,
    };
    let mut create = request(
        json!({"command":"instances.create","instance":{"id":"metadata-probe","name":"Metadata probe","version":{"gameVersion":"1.20.4","loader":"fabric","loaderVersion":"0.16.10"},"location":{"root":root.join("instances/metadata-probe")},"javaMajor":17}}),
    );
    create.operation_id = Some(OperationId::new("metadata-probe-create").unwrap());
    state
        .core
        .dispatch_mutation(ModuleId::Instances, &create)
        .unwrap();
    let read = |payload| {
        let CommandResponse::Completed { result } = state
            .core
            .dispatch_read(ModuleId::Content, &request(payload))
            .unwrap()
        else {
            panic!("completed read")
        };
        result
    };
    let mut records = vec![];
    for slug in ["sodium", "iris"] {
        let search = read(
            json!({"command":"content.search","queryId":slug,"provider":"modrinth","query":slug,"limit":20,"target":{"gameVersion":"1.20.4","loader":"fabric"}}),
        );
        assert_eq!(search["connected"], true);
        let hit = search["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["slug"] == slug)
            .unwrap();
        let project_id = hit["projectId"].as_str().unwrap();
        let project = read(json!({"command":"content.modrinth.project","projectId":project_id}));
        let versions = read(
            json!({"command":"content.modrinth.versions","projectId":project_id,"instanceId":"metadata-probe"}),
        );
        assert!(versions["total"].as_u64().unwrap() > 0);
        if slug == "iris" {
            assert!(versions["versions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["dependencies"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|d| d["kind"] == "required")));
        }
        records.push(json!({"project":project,"page":versions}));
    }
    let evidence = json!({"provider":"modrinth","route":"production CoreApp registry","registeredTarget":"synthetic metadata-only instance","projects":records,"installed":false});
    if let Some(dir) = std::env::var_os("SHTMCL_CATALOG_EVIDENCE") {
        std::fs::write(
            std::path::PathBuf::from(dir).join("live-registry-metadata.json"),
            serde_json::to_vec_pretty(&evidence).unwrap(),
        )
        .unwrap();
    }
    println!("production registry: Sodium + Iris details, compatible versions and required dependencies passed; no game installed");
    drop(state);
    std::fs::remove_dir_all(root).unwrap();
}

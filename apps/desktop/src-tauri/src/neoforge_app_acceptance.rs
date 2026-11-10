//! Opt-in application preparation with retained real caches and actual Java.
use crate::{
    neoforge_commands::NeoForgeCommands, neoforge_sources, runtime_install::RuntimeInstaller,
    version_metadata_source::CachedVersionMetadataSource,
};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use shtmcl_core::{
    downloads::{DownloadsModule, FileDisk, HttpDownloads, SystemClock},
    launch::catalog::{CatalogCache, ManifestFetcher, VersionCatalog},
    CoreApp, CoreModule,
};
use shtmcl_domain::{
    CancelTaskRequest, CommandRequest, CommandResponse, ErrorEnvelope, ModuleId, OperationId,
    RequestId, Task, TaskState,
};
use shtmcl_platform::{PlatformDirs, StdProcesses};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

struct Fixture(Vec<u8>);
impl ManifestFetcher for Fixture {
    fn fetch(&self, url: &str, limit: usize) -> Result<Vec<u8>, String> {
        if url == "https://piston-meta.mojang.com/v1/packages/pilot/1.20.4.json"
            && self.0.len() <= limit
        {
            Ok(self.0.clone())
        } else {
            Err("metadata networking disabled for acceptance".into())
        }
    }
}
struct Delegate;
impl CoreModule for Delegate {
    fn id(&self) -> ModuleId {
        ModuleId::Launch
    }
    fn handle(&self, _: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        Err(ErrorEnvelope::internal_unavailable())
    }
}
fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).expect(name))
}
fn copy_tree(source: &Path, target: &Path) {
    let metadata = fs::symlink_metadata(source).unwrap();
    assert!(!metadata.file_type().is_symlink());
    if metadata.is_dir() {
        fs::create_dir_all(target).unwrap();
        for child in fs::read_dir(source).unwrap() {
            let child = child.unwrap();
            copy_tree(&child.path(), &target.join(child.file_name()));
        }
    } else {
        assert!(metadata.is_file());
        fs::copy(source, target).unwrap();
    }
}
fn request(operation: &str) -> CommandRequest {
    CommandRequest {
        request_id: RequestId::new(operation).unwrap(),
        operation_id: Some(OperationId::new(operation).unwrap()),
        entity_id: None,
        expected_revision: None,
        payload: json!({"command":"launch.neoforge.prepare","versionId":"1.20.4","loaderVersion":"20.4.253"}),
    }
}
fn wait(app: &CoreApp, response: CommandResponse, cancel: bool) -> Task {
    let CommandResponse::Accepted { task_id, .. } = response else {
        panic!("expected parent task");
    };
    let deadline = Instant::now() + Duration::from_secs(1200);
    let mut phase = String::new();
    let mut requested = false;
    loop {
        let task = app.context().tasks().snapshot(&task_id).unwrap();
        if phase != task.phase.as_str() {
            phase = task.phase.as_str().into();
            println!("{} phase={phase}", task.id);
        }
        if task.state.is_terminal() {
            return task;
        }
        if cancel && !requested && phase == "neoforge_processor-6" {
            app.context().tasks().request_cancel(&CancelTaskRequest {
                task_id: task.id.clone(),
                attempt_generation: task.attempt_generation,
            });
            requested = true;
        }
        if Instant::now() >= deadline {
            app.context().tasks().request_cancel(&CancelTaskRequest {
                task_id: task.id.clone(),
                attempt_generation: task.attempt_generation,
            });
            panic!("prepare timeout; cancellation requested");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "requires new root, captured vanilla cache/metadata and managed Java; permits HTTPS artifacts"]
fn full_app_prepare_cancel_then_complete() {
    let root = env_path("SHTMCL_NEOFORGE_APP_ROOT");
    assert!(root.is_absolute());
    fs::create_dir(&root).expect("fresh test root");
    let dirs = PlatformDirs::under(&root);
    dirs.create_all().unwrap();
    copy_tree(
        &env_path("SHTMCL_NEOFORGE_VANILLA_CACHE"),
        &dirs.cache.join("downloads"),
    );
    copy_tree(
        &env_path("SHTMCL_NEOFORGE_MANAGED_JAVA"),
        &dirs.data.join("java"),
    );
    let mut bytes = Vec::new();
    fs::File::open(env_path("SHTMCL_NEOFORGE_BASE_METADATA"))
        .unwrap()
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() <= 8 * 1024 * 1024);
    let digest = format!("{:x}", Sha1::digest(&bytes));
    assert_eq!(digest, "ee2bc046cef978a91a7494e760a32d4f7f3e835c");
    let fetcher = Arc::new(Fixture(bytes));
    let catalog_path = dirs.data.join("catalog");
    let cache = CatalogCache::new(&catalog_path);
    cache
        .store(
            &serde_json::to_vec(&json!({"versions":[{"id":"1.20.4","type":"release",
        "url":"https://piston-meta.mojang.com/v1/packages/pilot/1.20.4.json","sha1":digest}]}))
            .unwrap(),
        )
        .unwrap();
    let metadata = Arc::new(CachedVersionMetadataSource::new(cache, fetcher.clone()));
    let catalog = Arc::new(VersionCatalog::new(&catalog_path, fetcher.clone()));
    let mut app = CoreApp::new(dirs.clone());
    let ctx = app.context();
    let download_root = dirs.cache.join("downloads");
    let downloads: Arc<dyn CoreModule> = Arc::new(DownloadsModule::new(
        ctx.clone(),
        Arc::new(HttpDownloads::new(&download_root, neoforge_sources::sources()).unwrap()),
        Arc::new(FileDisk::new(&download_root)),
        Arc::new(SystemClock),
    ));
    let delegate: Arc<dyn CoreModule> = Arc::new(Delegate);
    let runtime = Arc::new(RuntimeInstaller::new(
        ctx.clone(),
        delegate.clone(),
        downloads.clone(),
        fetcher,
    ));
    let commands = NeoForgeCommands::new(
        ctx,
        delegate,
        runtime,
        metadata,
        catalog,
        downloads.clone(),
        Arc::new(StdProcesses::capturing()),
        None,
    );
    app.register(downloads).unwrap();
    app.register(Arc::new(commands)).unwrap();
    let cancelled = wait(
        &app,
        app.dispatch_mutation(ModuleId::Launch, &request("neo-app-cancel"))
            .unwrap(),
        true,
    );
    fs::write(
        root.join("cancel-result.json"),
        serde_json::to_vec_pretty(&cancelled).unwrap(),
    )
    .unwrap();
    assert_eq!(
        cancelled.state,
        TaskState::Cancelled,
        "{:?}",
        cancelled.error
    );
    for attempt in fs::read_dir(&dirs.instances).unwrap() {
        assert!(!attempt
            .unwrap()
            .path()
            .join("game/neoforge-prepared.json")
            .exists());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let response = loop {
        match app.dispatch_mutation(ModuleId::Launch, &request("neo-app-complete")) {
            Ok(response) => break response,
            Err(error)
                if error.code.as_str() == "launch.install.busy" && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("retry rejected: {error:?}"),
        }
    };
    let task = wait(&app, response, false);
    fs::write(
        root.join("success-task.json"),
        serde_json::to_vec_pretty(&task).unwrap(),
    )
    .unwrap();
    assert_eq!(task.state, TaskState::Succeeded, "{:?}", task.error);
    let result = task.result.unwrap();
    assert_eq!(result["installed"], false);
    assert_eq!(result["launchReady"], false);
    assert!(result["verifiedFiles"].as_u64().unwrap() > 3800);
    assert_eq!(
        result["mainClass"],
        "cpw.mods.bootstraplauncher.BootstrapLauncher"
    );
    let stage = PathBuf::from(result["path"].as_str().unwrap());
    use sha2::Sha256;
    assert_eq!(
        result["preparedSha256"],
        format!(
            "{:x}",
            Sha256::digest(fs::read(stage.join("neoforge-prepared.json")).unwrap())
        )
    );
    let marker: Value =
        serde_json::from_slice(&fs::read(stage.join("neoforge-prepared.json")).unwrap()).unwrap();
    assert!(marker["baseArtifacts"].as_array().unwrap().len() > 1000);
    assert!(!stage.join("shtmcl-install.json").exists());
    assert!(app.snapshot().iter().all(|t| t.state.is_terminal()));
    fs::write(
        root.join("result.json"),
        serde_json::to_vec_pretty(&json!({
            "stage":stage,"baseArtifacts":marker["baseArtifacts"].as_array().unwrap().len(),
            "metadataSource":"captured_official_fixture","javaSource":"retained_managed_runtime",
            "downloads":"retained_cache_plus_live_https","cancelledParent":cancelled.id,
            "succeededParent":task.id,"preparedSha256":result["preparedSha256"],
            "verifiedFiles":result["verifiedFiles"],"instanceRegistered":false,"gameLaunched":false
        }))
        .unwrap(),
    )
    .unwrap();
}

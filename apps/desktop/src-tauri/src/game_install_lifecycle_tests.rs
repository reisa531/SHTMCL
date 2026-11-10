//! Offline lifecycle acceptance: production installer, task service, disk and
//! download module; only metadata and byte transport are fixtures.
use super::*;
use sha1::{Digest, Sha1};
use shtmcl_core::downloads::{ArtifactHash, ByteRange, DownloadPort, FetchOutcome, FileStore};
use std::io::Write;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

struct Metadata {
    manifest: Vec<u8>,
    version: Vec<u8>,
}
impl shtmcl_core::launch::catalog::ManifestFetcher for Metadata {
    fn fetch(&self, url: &str, _: usize) -> Result<Vec<u8>, String> {
        Ok(if url.ends_with("fixture.json") {
            &self.version
        } else {
            &self.manifest
        }
        .clone())
    }
}

struct Transport {
    store: FileStore,
    stop_first: AtomicBool,
    entered: Mutex<mpsc::Sender<String>>,
}
impl DownloadPort for Transport {
    fn probe(&self, _: &str, _: &str) -> Result<Option<u64>, String> {
        Ok(Some(4))
    }
    fn fetch(&self, _: &str, id: &str, range: ByteRange, _: Option<&str>) -> FetchOutcome {
        self.store
            .open_partial(id, 0)
            .unwrap()
            .write_all(b"good")
            .unwrap();
        FetchOutcome::Bytes { range, etag: None }
    }
    fn fetch_url_cancellable(
        &self,
        source: &str,
        id: &str,
        _: &str,
        range: ByteRange,
        etag: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> FetchOutcome {
        if !self.stop_first.load(Ordering::Acquire) {
            return self.fetch(source, id, range, etag);
        }
        self.store
            .open_partial(id, 0)
            .unwrap()
            .write_all(b"go")
            .unwrap();
        self.entered.lock().unwrap().send(id.into()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if cancelled() {
            FetchOutcome::Cancelled
        } else {
            FetchOutcome::TransportFailed("fixture deadline".into())
        }
    }
    fn hash_bytes(&self, id: &str, algorithm: &str) -> Result<String, String> {
        self.store
            .hash_partial(id, algorithm)
            .map_err(|e| e.to_string())
    }
    fn commit_range(&self, id: &str, _: ByteRange) -> Result<(), String> {
        self.store.commit_partial(id).map_err(|e| e.to_string())
    }
    fn publish(&self, hash: &ArtifactHash, id: &str) -> Result<u64, String> {
        self.store.publish(hash, id).map_err(|e| e.to_string())
    }
    fn materialize_cached(&self, hash: &ArtifactHash, id: &str) -> Result<Option<u64>, String> {
        self.store.materialize(hash, id).map_err(|e| e.to_string())
    }
    fn discard(&self, id: &str) -> Result<(), String> {
        self.store.discard(id).map_err(|e| e.to_string())
    }
}
struct UnusedLaunch;
impl CoreModule for UnusedLaunch {
    fn id(&self) -> ModuleId {
        ModuleId::Launch
    }
    fn handle(&self, _: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        Err(ErrorEnvelope::module_unavailable())
    }
}
fn request(command: &str, operation: Option<&str>) -> CommandRequest {
    CommandRequest {
        request_id: RequestId::new(operation.unwrap_or("snapshot")).unwrap(),
        operation_id: operation.map(|s| OperationId::new(s).unwrap()),
        entity_id: None,
        expected_revision: None,
        payload: json!({"command":command,"versionId":"fixture","instanceId":"same-id"}),
    }
}
fn accepted(installer: &GameInstaller, op: &str) -> TaskId {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match installer.handle(&request("launch.install", Some(op))) {
            Ok(CommandResponse::Accepted { task_id, .. }) => return task_id,
            Err(e) if e.code.as_str() == "launch.install.busy" && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => panic!("installation not accepted: {other:?}"),
        }
    }
}
fn terminal(ctx: &ModuleContext, id: &TaskId) -> shtmcl_domain::TaskState {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let task = ctx.tasks().snapshot(id).unwrap();
        if matches!(
            task.state,
            TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
        ) {
            println!("terminal: {task:?}");
            return task.state;
        }
        assert!(Instant::now() < deadline, "task deadline: {task:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[cfg(all(windows, target_arch = "x86_64"))]
fn cancel_cleans_before_terminal_and_same_instance_can_be_recreated() {
    let root = std::env::var_os("SHTMCL_LIFECYCLE_TEST_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "shtmcl-install-lifecycle-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
        });
    assert!(!root.exists(), "use a fresh fixture root");
    let app = shtmcl_core::CoreApp::new(shtmcl_platform::PlatformDirs::under(&root));
    let ctx = app.context();
    let cache = ctx.dirs().cache.join("downloads");
    let store = FileStore::new(&cache).unwrap();
    let digest = format!("{:x}", Sha1::digest(b"good"));
    let version = serde_json::to_vec(&json!({"id":"fixture","mainClass":"net.minecraft.client.main.Main","libraries":[],
        "downloads":{"client":{"url":"https://piston-data.mojang.com/fixture.jar","sha1":digest,"size":4}}})).unwrap();
    let fetcher = Arc::new(Metadata {
        manifest: serde_json::to_vec(&json!({"latest":{"release":"fixture"},"versions":[{
        "id":"fixture","type":"release","url":"https://piston-meta.mojang.com/fixture.json",
        "sha1":format!("{:x}",Sha1::digest(&version))}]}))
        .unwrap(),
        version,
    });
    let catalog_dir = ctx.dirs().data.join("catalog");
    let catalog = Arc::new(VersionCatalog::new(&catalog_dir, fetcher.clone()));
    let metadata = Arc::new(CachedVersionMetadataSource::new(
        shtmcl_core::launch::catalog::CatalogCache::new(&catalog_dir),
        fetcher,
    ));
    let instances: Arc<dyn CoreModule> = Arc::new(
        shtmcl_core::instances::InstancesModule::new(
            ctx.clone(),
            Box::new(
                shtmcl_core::instances::FileStorage::new(ctx.dirs().data.join("instance-store"))
                    .unwrap(),
            ),
        )
        .unwrap(),
    );
    let (sender, receiver) = mpsc::channel();
    let transport = Arc::new(Transport {
        store: store.clone(),
        stop_first: AtomicBool::new(true),
        entered: Mutex::new(sender),
    });
    let downloads: Arc<dyn CoreModule> = Arc::new(shtmcl_core::downloads::DownloadsModule::new(
        ctx.clone(),
        transport.clone(),
        Arc::new(shtmcl_core::downloads::FileDisk::new(&cache)),
        Arc::new(shtmcl_core::downloads::SystemClock),
    ));
    let installer = GameInstaller::new(
        ctx.clone(),
        Arc::new(UnusedLaunch),
        catalog,
        metadata,
        downloads,
        Some(instances.clone()),
    );
    let sibling = ctx.dirs().instances.join("keep/options.txt");
    std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
    std::fs::write(&sibling, b"lang:zh_cn").unwrap();
    // Publish an unrelated verified object, rather than a fake cache marker.
    let keep_hash = ArtifactHash::new("sha1", &format!("{:x}", Sha1::digest(b"keep"))).unwrap();
    store
        .open_partial("keep", 0)
        .unwrap()
        .write_all(b"keep")
        .unwrap();
    store.commit_partial("keep").unwrap();
    store.publish(&keep_hash, "keep").unwrap();
    let first = accepted(&installer, "cancel-first");
    let artifact_id = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("active transfer");
    let stage = ctx.dirs().instances.join(".install-same-id");
    assert!(stage.is_dir());
    let snapshot = ctx.tasks().snapshot(&first).unwrap();
    ctx.tasks().request_cancel(&CancelTaskRequest {
        task_id: first.clone(),
        attempt_generation: snapshot.attempt_generation,
    });
    assert_eq!(terminal(&ctx, &first), TaskState::Cancelled);
    assert!(!stage.exists(), "cleanup must precede cancelled terminal");
    assert!(!ctx.dirs().instances.join("same-id").exists());
    assert!(instances
        .handle(&request("instances.snapshot", None))
        .is_err());
    assert!(
        store.hash_partial(&artifact_id, "sha1").is_err(),
        "owned partial removed"
    );
    assert!(ctx
        .tasks()
        .snapshots()
        .iter()
        .all(|t| matches!(t.state, TaskState::Cancelled | TaskState::Succeeded)));
    std::thread::sleep(Duration::from_millis(100));
    assert!(!stage.exists(), "no late writer recreates staging");
    transport.stop_first.store(false, Ordering::Release);
    let second = accepted(&installer, "recreate-second");
    assert_ne!(first, second);
    assert_eq!(terminal(&ctx, &second), TaskState::Succeeded);
    assert!(instances
        .handle(&request("instances.snapshot", None))
        .is_ok());
    let installed = std::fs::read(
        ctx.dirs()
            .instances
            .join("same-id/versions/fixture/fixture.jar"),
    )
    .unwrap();
    assert_eq!(format!("{:x}", Sha1::digest(&installed)), digest);
    assert!(!stage.exists());
    assert_eq!(std::fs::read(&sibling).unwrap(), b"lang:zh_cn");
    assert_eq!(store.verify_cached(&keep_hash).unwrap(), Some(4));
    println!("offline_lifecycle_root={} cleanup_before_terminal=true same_id_recreated=true preserved_shared_cache=true", root.display());
}

#[test]
#[cfg(all(windows, target_arch = "x86_64"))]
fn cancelled_cache_download_retries_with_verified_objects_and_retains_history() {
    let root = std::env::var_os("SHTMCL_CACHE_RETRY_TEST_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "shtmcl-cache-retry-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
        });
    assert!(!root.exists(), "use a fresh fixture root");
    let app = shtmcl_core::CoreApp::new(shtmcl_platform::PlatformDirs::under(&root));
    let ctx = app.context();
    let cache = ctx.dirs().cache.join("downloads");
    let store = FileStore::new(&cache).unwrap();
    let keep = ArtifactHash::new("sha1", &format!("{:x}", Sha1::digest(b"keep"))).unwrap();
    let good = ArtifactHash::new("sha1", &format!("{:x}", Sha1::digest(b"good"))).unwrap();
    store
        .open_partial("seed-client", 0)
        .unwrap()
        .write_all(b"keep")
        .unwrap();
    store.commit_partial("seed-client").unwrap();
    store.publish(&keep, "seed-client").unwrap();
    let version = serde_json::to_vec(&json!({"id":"fixture","mainClass":"net.minecraft.client.main.Main",
        "downloads":{"client":{"url":"https://piston-data.mojang.com/fixture.jar","sha1":keep.digest,"size":4}},
        "libraries":[{"name":"fixture:library:1","downloads":{"artifact":{"path":"fixture/library/1/library-1.jar",
            "url":"https://libraries.minecraft.net/fixture/library/1/library-1.jar","sha1":good.digest,"size":4}}}]})).unwrap();
    let fetcher = Arc::new(Metadata {
        manifest: serde_json::to_vec(&json!({"latest":{"release":"fixture"},"versions":[{
        "id":"fixture","type":"release","url":"https://piston-meta.mojang.com/fixture.json",
        "sha1":format!("{:x}",Sha1::digest(&version))}]}))
        .unwrap(),
        version,
    });
    let catalog_dir = ctx.dirs().data.join("catalog");
    let catalog = Arc::new(VersionCatalog::new(&catalog_dir, fetcher.clone()));
    let metadata = Arc::new(CachedVersionMetadataSource::new(
        shtmcl_core::launch::catalog::CatalogCache::new(&catalog_dir),
        fetcher,
    ));
    let (sender, receiver) = mpsc::channel();
    let transport = Arc::new(Transport {
        store: store.clone(),
        stop_first: AtomicBool::new(true),
        entered: Mutex::new(sender),
    });
    let downloads: Arc<dyn CoreModule> = Arc::new(shtmcl_core::downloads::DownloadsModule::new(
        ctx.clone(),
        transport.clone(),
        Arc::new(shtmcl_core::downloads::FileDisk::new(&cache)),
        Arc::new(shtmcl_core::downloads::SystemClock),
    ));
    let installer = GameInstaller::new(
        ctx.clone(),
        Arc::new(UnusedLaunch),
        catalog,
        metadata,
        downloads,
        None,
    );
    let start = |op: &str| {
        let mut command = request("launch.environments.download", Some(op));
        command
            .payload
            .as_object_mut()
            .unwrap()
            .remove("instanceId");
        installer.handle(&command)
    };
    let first = match start("cache-cancel-first").unwrap() {
        CommandResponse::Accepted { task_id, .. } => task_id,
        other => panic!("expected cache task: {other:?}"),
    };
    let partial_id = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("missing library transfer");
    assert_eq!(
        partial_id,
        format!("install/{}", good.digest),
        "verified client must not be fetched"
    );
    assert_eq!(
        start("cache-duplicate").unwrap_err().code.as_str(),
        "launch.install.busy"
    );
    let first_snapshot = ctx.tasks().snapshot(&first).unwrap();
    ctx.tasks().request_cancel(&CancelTaskRequest {
        task_id: first.clone(),
        attempt_generation: first_snapshot.attempt_generation,
    });
    assert_eq!(terminal(&ctx, &first), TaskState::Cancelled);
    assert!(store.hash_partial(&partial_id, "sha1").is_err());
    assert_eq!(store.verify_cached(&keep).unwrap(), Some(4));
    let list = || match installer
        .handle(&request("launch.environments.list", None))
        .unwrap()
    {
        CommandResponse::Completed { result } => result,
        other => panic!("expected catalogue: {other:?}"),
    };
    assert!(
        list()["entries"].as_array().unwrap().is_empty(),
        "cancelled recipe not published"
    );
    transport.stop_first.store(false, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(5);
    let second = loop {
        match start("cache-retry-second") {
            Ok(CommandResponse::Accepted { task_id, .. }) => break task_id,
            Err(error)
                if error.code.as_str() == "launch.install.busy" && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => panic!("retry not accepted: {other:?}"),
        }
    };
    assert_ne!(first, second);
    assert_eq!(terminal(&ctx, &second), TaskState::Succeeded);
    assert_eq!(
        ctx.tasks().snapshot(&first).unwrap().state,
        TaskState::Cancelled
    );
    let catalogue = list();
    let entries = catalogue["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["versionId"], "fixture");
    assert_eq!(entries[0]["resourceCount"], 2);
    assert_eq!(entries[0]["bytes"], 8);
    assert_eq!(store.verify_cached(&keep).unwrap(), Some(4));
    assert_eq!(store.verify_cached(&good).unwrap(), Some(4));
    assert!(
        !ctx.dirs().instances.exists(),
        "cache retry must not create any instance"
    );
    // Terminal task publication and durable history are separate writes.
    let history = crate::operation_history::OperationHistory::new(ctx.dirs());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let read = |operation: &str, task: &str| match history.lookup(operation, task) {
            Ok(snapshot) => snapshot,
            Err(error) if error == "history_busy" => None,
            Err(error) => panic!("durable history lookup: {error}"),
        };
        let old = read("cache-cancel-first", first.as_str());
        let new = read("cache-retry-second", second.as_str());
        if old
            .as_ref()
            .is_some_and(|s| s.state == TaskState::Cancelled)
            && new
                .as_ref()
                .is_some_and(|s| s.state == TaskState::Succeeded)
        {
            break;
        }
        assert!(Instant::now() < deadline, "durable history deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
    println!("cache_retry_root={} duplicate_rejected=true verified_client_reused=true recipes=1 history_retained=true", root.display());
}

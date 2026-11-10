//! Disk-cache recovery must work with a new module and no registered network source.
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use sha1::{Digest, Sha1};
use shtmcl_core::downloads::{
    ArtifactHash, DownloadPort, DownloadsModule, FileDisk, FileStore, HttpDownloads, SystemClock,
    UnavailableDownloads,
};
use shtmcl_core::CoreApp;
use shtmcl_domain::{
    CommandRequest, CommandResponse, ModuleId, OperationId, RequestId, Task, TaskState,
};
use shtmcl_platform::PlatformDirs;

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "shtmcl-cache-recovery-{}",
            shtmcl_platform::random_hex(12).unwrap()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn seed(&self) -> ArtifactHash {
        let bytes = b"real cached file bytes";
        let hash = ArtifactHash::new("sha1", &format!("{:x}", Sha1::digest(bytes))).unwrap();
        let store = FileStore::new(self.0.join("cache")).unwrap();
        store
            .reset_partial("seed")
            .unwrap()
            .write_all(bytes)
            .unwrap();
        store.commit_partial("seed").unwrap();
        store.publish(&hash, "seed").unwrap();
        hash
    }
    fn module(&self) -> (CoreApp, Arc<DownloadsModule>) {
        let mut app = CoreApp::new(PlatformDirs::under(&self.0));
        // No sources: this transport cannot fetch a network byte.
        let port = HttpDownloads::new(self.0.join("cache"), Vec::new()).unwrap();
        let module = Arc::new(DownloadsModule::new(
            app.context(),
            Arc::new(port),
            Arc::new(FileDisk::new(&self.0)),
            Arc::new(SystemClock),
        ));
        app.register(module.clone()).unwrap();
        (app, module)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fetch(app: &CoreApp, hash: &ArtifactHash, size: u64) -> Task {
    let response = app.dispatch_mutation(ModuleId::Downloads, &CommandRequest {
        request_id: RequestId::new("recover-request").unwrap(),
        operation_id: Some(OperationId::new("recover-operation").unwrap()),
        entity_id: None, expected_revision: None,
        payload: json!({"command":"downloads.fetch","artifacts":[{"artifactId":"mapping","sourceId":"absent","size":size,"hash":{"algorithm":hash.algorithm,"digest":hash.digest}}]}),
    }).unwrap();
    let CommandResponse::Accepted { task_id, .. } = response else {
        panic!("expected task");
    };
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let task = app
            .snapshot()
            .into_iter()
            .find(|task| task.id == task_id)
            .unwrap();
        if matches!(
            task.state,
            TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
        ) {
            return task;
        }
        assert!(Instant::now() < end, "cache recovery task did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn new_module_recovers_verified_disk_object_without_sources() {
    let root = Root::new();
    let hash = root.seed();
    for _ in 0..2 {
        let (app, module) = root.module();
        assert_eq!(module.cache_snapshot().0, 0);
        let task = fetch(&app, &hash, 22);
        assert_eq!(task.state, TaskState::Succeeded, "{:?}", task.error);
        assert_eq!(module.cache_snapshot().0, 1);
        assert_eq!(module.cache_snapshot().1, 22);
    }
}
#[test]
fn recovered_size_mismatch_never_publishes_ready() {
    let root = Root::new();
    let hash = root.seed();
    let (app, module) = root.module();
    let task = fetch(&app, &hash, 23);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(task.error.unwrap().code.as_str(), "downloads.cache.corrupt");
    assert_eq!(module.cache_snapshot().0, 0);
}
#[test]
fn corrupt_disk_object_is_rejected_before_recovery() {
    let root = Root::new();
    let hash = root.seed();
    let object = root
        .0
        .join("cache/objects/sha1")
        .join(&hash.digest[..2])
        .join(&hash.digest);
    fs::write(object, b"corrupt file").unwrap();
    let (app, module) = root.module();
    let task = fetch(&app, &hash, 22);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(task.error.unwrap().code.as_str(), "downloads.io.failed");
    assert_eq!(module.cache_snapshot().0, 0);
}
#[test]
fn missing_disk_object_and_legacy_port_do_not_invent_cache_hits() {
    let root = Root::new();
    let hash = ArtifactHash::new("sha1", &"00".repeat(20)).unwrap();
    let http = HttpDownloads::new(root.0.join("cache"), Vec::new()).unwrap();
    assert_eq!(http.recover_cached(&hash, "missing").unwrap(), None);
    assert_eq!(
        UnavailableDownloads
            .recover_cached(&hash, "unsupported")
            .unwrap(),
        None
    );
}

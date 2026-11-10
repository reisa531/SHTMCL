//! Real installer acquisition plus captured official metadata identity.
//! Does not register an instance or execute Java. Evidence is retained.
use crate::{neoforge_sources, version_metadata_source::CachedVersionMetadataSource};
use serde_json::json;
use sha1::{Digest, Sha1};
use shtmcl_core::{
    downloads::{ArtifactHash, DownloadsModule, FileDisk, FileStore, HttpDownloads, SystemClock},
    launch::catalog::{CatalogCache, ManifestFetcher},
    CoreApp,
};
use shtmcl_domain::{
    CancelTaskRequest, CommandRequest, CommandResponse, ModuleId, OperationId, RequestId, TaskState,
};
use shtmcl_platform::PlatformDirs;
use std::{
    io::Read,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

const METADATA_SHA1: &str = "ee2bc046cef978a91a7494e760a32d4f7f3e835c";
const INSTALLER_SHA256: &str = "8f1413379067059acc0151444b17536cfad97756d99fc70e45587bf229c986fa";
const FIXTURE_URL: &str = "https://piston-meta.mojang.com/v1/packages/pilot/1.20.4.json";

struct CapturedMetadata(Option<Vec<u8>>);
impl ManifestFetcher for CapturedMetadata {
    fn fetch(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
        if url != FIXTURE_URL {
            return Err("fixture_url".into());
        }
        self.0
            .as_ref()
            .filter(|bytes| bytes.len() <= max_bytes)
            .cloned()
            .ok_or_else(|| "offline".into())
    }
}

#[test]
#[ignore = "requires fresh evidence root, captured official metadata and real HTTPS"]
fn official_installer_and_raw_metadata() {
    let root =
        PathBuf::from(std::env::var_os("SHTMCL_NEOFORGE_ACQUISITION_ROOT").expect("fresh root"));
    assert!(root.is_absolute());
    std::fs::create_dir(&root).expect("root must not already exist");
    let metadata_path = PathBuf::from(
        std::env::var_os("SHTMCL_NEOFORGE_BASE_METADATA").expect("captured metadata"),
    );
    let mut bytes = Vec::new();
    std::fs::File::open(metadata_path)
        .unwrap()
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() <= 8 * 1024 * 1024);
    assert_eq!(format!("{:x}", Sha1::digest(&bytes)), METADATA_SHA1);
    let cache = root.join("cache/downloads");
    let mut app = CoreApp::new(PlatformDirs::under(&root));
    let downloads = DownloadsModule::new(
        app.context(),
        Arc::new(HttpDownloads::new(&cache, neoforge_sources::sources()).unwrap()),
        Arc::new(FileDisk::new(&cache)),
        Arc::new(SystemClock),
    );
    app.register(Arc::new(downloads)).unwrap();
    let payload = neoforge_sources::installer_payload("1.20.4", "20.4.253").unwrap();
    let response = app
        .dispatch_mutation(
            ModuleId::Downloads,
            &CommandRequest {
                request_id: RequestId::new("neo-app-installer").unwrap(),
                operation_id: Some(OperationId::new("neo-app-installer").unwrap()),
                entity_id: None,
                expected_revision: None,
                payload,
            },
        )
        .unwrap();
    let CommandResponse::Accepted { task_id, .. } = response else {
        panic!("download not accepted");
    };
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let snapshot = app.context().tasks().snapshot(&task_id).unwrap();
        if matches!(
            snapshot.state,
            TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
        ) {
            assert_eq!(snapshot.state, TaskState::Succeeded, "{:?}", snapshot.error);
            break;
        }
        if Instant::now() >= deadline {
            app.context().tasks().request_cancel(&CancelTaskRequest {
                task_id: task_id.clone(),
                attempt_generation: snapshot.attempt_generation,
            });
            panic!("installer task timed out; cancellation requested");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let store = FileStore::new(&cache).unwrap();
    let hash = ArtifactHash::new("sha256", INSTALLER_SHA256).unwrap();
    assert_eq!(store.verify_cached(&hash).unwrap(), Some(6_540_143));

    let catalog_path = root.join("catalog");
    let catalog = CatalogCache::new(&catalog_path);
    catalog
        .store(
            &serde_json::to_vec(&json!({"versions":[{
                "id":"1.20.4","type":"release","url":FIXTURE_URL,"sha1":METADATA_SHA1
            }]}))
            .unwrap(),
        )
        .unwrap();
    let source =
        CachedVersionMetadataSource::new(catalog, Arc::new(CapturedMetadata(Some(bytes.clone()))));
    let detail = source.load_verified("1.20.4").unwrap().unwrap();
    assert_eq!(detail.bytes(), bytes);
    assert_eq!(detail.sha1(), METADATA_SHA1);
    assert_eq!(detail.metadata()["id"], "1.20.4");
    let offline = CachedVersionMetadataSource::new(
        CatalogCache::new(&catalog_path),
        Arc::new(CapturedMetadata(None)),
    );
    let cached = offline.load_verified("1.20.4").unwrap().unwrap();
    assert_eq!(cached.bytes(), bytes);
    assert_eq!(cached.sha1(), METADATA_SHA1);
    let result = json!({
        "installerNetwork":true,"metadataSource":"captured_official_fixture",
        "installerSize":6_540_143,"installerSha256":INSTALLER_SHA256,
        "metadataSize":bytes.len(),"metadataSha1":METADATA_SHA1,
        "rawMetadataOfflineVerified":true,"taskId":task_id,"state":"succeeded",
        "instanceRegistered":false,"gameLaunched":false
    });
    std::fs::write(
        root.join("result.json"),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
    println!("{}", result);
}

//! Processor artifact acquisition through the existing download task and cache.
//! Application adapters register the sources below on their shared transport.
//! Acquisition does not install a loader or authorize processor execution.

use serde_json::{json, Value};
use shtmcl_domain::{
    CommandRequest, CommandResponse, ErrorEnvelope, ModuleId, OperationId, RequestId,
};

use super::error::ContentError;
use super::neoforge_profile::{assess_processors, NeoForgeInstallerProfile};

pub const PROCESSOR_SOURCES: &[(&str, &str)] = &[
    ("neoforge-maven", "https://maven.neoforged.net/releases/"),
    ("mojang-libraries", "https://libraries.minecraft.net/"),
];

/// Build a download command only after the complete profile passes preflight.
/// Expected hashes and URLs always come from the embedded pins.
pub fn processor_download_payload(
    profile: &NeoForgeInstallerProfile,
) -> Result<Value, ContentError> {
    assess_processors(profile).map_err(|detail| ContentError::InvalidLoaderMetadata {
        detail: detail.to_string(),
    })?;
    let artifacts: Vec<Value> = super::neoforge_processor_allowlist::ARTIFACTS
        .iter()
        .map(|pin| {
            let source = if pin.url.starts_with("https://libraries.minecraft.net/") {
                "mojang-libraries"
            } else {
                "neoforge-maven"
            };
            json!({
                "artifactId": format!("neo-{}", pin.sha256),
                "sourceId": source,
                "url": pin.url,
                "size": pin.size,
                "hash": {"algorithm": "sha256", "digest": pin.sha256}
            })
        })
        .collect();
    Ok(json!({"command": crate::downloads::actions::FETCH, "artifacts": artifacts}))
}

/// Submit to the application's single downloads dispatcher. Cancellation,
/// verification, cache publication and retries remain owned by module 03.
pub fn start_processor_downloads(
    profile: &NeoForgeInstallerProfile,
    app: &crate::CoreApp,
    request_id: RequestId,
    operation_id: OperationId,
) -> Result<CommandResponse, ErrorEnvelope> {
    let payload = processor_download_payload(profile).map_err(|error| error.to_envelope())?;
    app.dispatch_mutation(
        ModuleId::Downloads,
        &CommandRequest {
            request_id,
            operation_id: Some(operation_id),
            entity_id: None,
            expected_revision: None,
            payload,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloads::{
        ArtifactHash, ByteRange, DownloadPort, DownloadsModule, FetchOutcome, FileDisk, FileStore,
        HttpDownloads, HttpSource, SystemClock,
    };
    use shtmcl_domain::TaskState;
    use shtmcl_platform::PlatformDirs;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};

    fn profile() -> NeoForgeInstallerProfile {
        let bytes = std::fs::read(
            std::env::var_os("SHTMCL_NEOFORGE_PROFILE").expect("set captured profile path"),
        )
        .unwrap();
        super::super::neoforge_profile::parse_install_profile("1.20.4", "20.4.253", &bytes).unwrap()
    }

    fn root(label: &str) -> PathBuf {
        let base = std::env::var_os("SHTMCL_NEOFORGE_DOWNLOAD_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        base.join(format!("neo-{label}-{}-{stamp}", std::process::id()))
    }

    fn app(root: &Path, port: Arc<dyn DownloadPort>) -> crate::CoreApp {
        let mut app = crate::CoreApp::new(PlatformDirs::under(root));
        let module = DownloadsModule::new(
            app.context(),
            port,
            Arc::new(FileDisk::new(root)),
            Arc::new(SystemClock),
        );
        app.register(Arc::new(module)).unwrap();
        app
    }

    fn acquire(
        app: &crate::CoreApp,
        profile: &NeoForgeInstallerProfile,
        operation: &str,
    ) -> shtmcl_domain::Task {
        let response = start_processor_downloads(
            profile,
            app,
            RequestId::new(format!("req-{operation}")).unwrap(),
            OperationId::new(operation).unwrap(),
        )
        .unwrap();
        let CommandResponse::Accepted { task_id, .. } = response else {
            panic!("expected download task")
        };
        let deadline = Instant::now() + Duration::from_secs(240);
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
            assert!(
                Instant::now() < deadline,
                "download task timeout: {:?}",
                task.state
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn check_cache(root: &Path) {
        let store = FileStore::new(root.join("cache")).unwrap();
        for pin in super::super::neoforge_processor_allowlist::ARTIFACTS {
            let hash = ArtifactHash::new("sha256", pin.sha256).unwrap();
            assert_eq!(
                store.verify_cached(&hash).unwrap(),
                Some(pin.size),
                "{}",
                pin.coordinate
            );
        }
    }

    // Real file bytes and FileStore hashing/publication, with only the network
    // replaced by a deterministic source. Never supplies a synthetic digest.
    struct CapturedDownloads {
        store: FileStore,
        artifacts: PathBuf,
        corrupt: bool,
        fetches: AtomicUsize,
    }

    impl CapturedDownloads {
        fn new(root: &Path, corrupt: bool) -> Self {
            Self {
                store: FileStore::new(root.join("cache")).unwrap(),
                artifacts: PathBuf::from(
                    std::env::var_os("SHTMCL_NEOFORGE_ARTIFACTS")
                        .expect("set captured artifact directory"),
                ),
                corrupt,
                fetches: AtomicUsize::new(0),
            }
        }
    }

    impl DownloadPort for CapturedDownloads {
        fn probe(&self, _: &str, _: &str) -> Result<Option<u64>, String> {
            Err("unexpected probe: pinned sizes required".into())
        }
        fn fetch(&self, _: &str, id: &str, range: ByteRange, _: Option<&str>) -> FetchOutcome {
            self.fetches.fetch_add(1, Ordering::Relaxed);
            let Some(pin) = super::super::neoforge_processor_allowlist::ARTIFACTS
                .iter()
                .find(|pin| id == format!("neo-{}", pin.sha256))
            else {
                return FetchOutcome::TransportFailed("unknown captured artifact".into());
            };
            let result = (|| -> std::io::Result<()> {
                assert!(range.end <= pin.size && range.len() <= 8 * 1024 * 1024);
                let mut source = std::fs::File::open(self.artifacts.join(pin.path))?;
                source.seek(SeekFrom::Start(range.start))?;
                let mut bytes = vec![0; range.len() as usize];
                source.read_exact(&mut bytes)?;
                if self.corrupt && range.start == 0 {
                    bytes[0] ^= 1;
                }
                self.store
                    .open_partial(id, range.start)?
                    .write_all(&bytes)?;
                Ok(())
            })();
            match result {
                Ok(()) => FetchOutcome::Bytes { range, etag: None },
                Err(error) => FetchOutcome::TransportFailed(error.to_string()),
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

    #[test]
    fn unsupported_profile_never_queues_a_download() {
        let profile = NeoForgeInstallerProfile {
            game_version: "1.20.4".into(),
            loader_version: "unknown".into(),
            version_json_path: "version.json".into(),
            libraries: Vec::new(),
            processors: Vec::new(),
            data: Default::default(),
        };
        let app = crate::CoreApp::new(PlatformDirs::under(root("rejected")));
        let error = start_processor_downloads(
            &profile,
            &app,
            RequestId::new("reject").unwrap(),
            OperationId::new("reject").unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            error.code.as_str(),
            super::super::error::codes::LOADER_METADATA_INVALID
        );
        assert!(app.snapshot().is_empty());
    }

    #[test]
    #[ignore = "requires captured profile and processor artifacts"]
    fn captured_bytes_download_verify_and_reuse_cache() {
        let root = root("fixture");
        let port = Arc::new(CapturedDownloads::new(&root, false));
        let app = app(&root, port.clone());
        let profile = profile();
        let task = acquire(&app, &profile, "first");
        assert_eq!(task.state, TaskState::Succeeded, "{:?}", task.error);
        check_cache(&root);
        let fetches = port.fetches.load(Ordering::Relaxed);
        assert!(fetches >= 33);
        let task = acquire(&app, &profile, "cached");
        assert_eq!(task.state, TaskState::Succeeded, "{:?}", task.error);
        assert_eq!(port.fetches.load(Ordering::Relaxed), fetches);
        let mut modified = profile.clone();
        let pinned_path = super::super::neoforge_processor_allowlist::ARTIFACTS[0].path;
        modified
            .libraries
            .iter_mut()
            .find(|library| library.path == pinned_path)
            .unwrap()
            .sha1 = "0".repeat(40);
        let before = app.snapshot().len();
        assert!(start_processor_downloads(
            &modified,
            &app,
            RequestId::new("modified").unwrap(),
            OperationId::new("modified").unwrap()
        )
        .is_err());
        assert_eq!(app.snapshot().len(), before);
        println!("33 files verified; repeat made no source fetches; modified profile not queued; root={}", root.display());
    }

    #[test]
    #[ignore = "requires captured profile and processor artifacts"]
    fn changed_source_bytes_are_not_published() {
        let root = root("tamper");
        let app = app(&root, Arc::new(CapturedDownloads::new(&root, true)));
        let task = acquire(&app, &profile(), "tamper");
        assert_eq!(task.state, TaskState::Failed);
        assert_eq!(
            task.error.as_ref().unwrap().code.as_str(),
            crate::downloads::codes::HASH_MISMATCH
        );
        let store = FileStore::new(root.join("cache")).unwrap();
        for pin in super::super::neoforge_processor_allowlist::ARTIFACTS {
            assert_eq!(
                store
                    .verify_cached(&ArtifactHash::new("sha256", pin.sha256).unwrap())
                    .unwrap(),
                None
            );
        }
    }

    #[test]
    #[ignore = "requires captured profile and external official HTTPS endpoints"]
    fn official_https_processor_acquisition() {
        let sources: Vec<_> = PROCESSOR_SOURCES
            .iter()
            .map(|(id, url)| HttpSource::new(*id, url).unwrap())
            .collect();
        let root = root("official");
        let app = app(
            &root,
            Arc::new(HttpDownloads::new(root.join("cache"), sources).unwrap()),
        );
        let task = acquire(&app, &profile(), "official");
        assert_eq!(task.state, TaskState::Succeeded, "{:?}", task.error);
        check_cache(&root);
        println!(
            "official HTTPS: 33 SHA256 cache objects verified at {}",
            root.display()
        );
    }
}

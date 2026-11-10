//! Verified Mojang client-mapping acquisition for the fixed 1.20.4 pilot.
//! Metadata identity is trusted from the caller (already verified against Mojang
//! version JSON); this module validates the mappings shape, hash, size and URL.

use crate::downloads::ArtifactHash;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use shtmcl_domain::{
    CommandRequest, CommandResponse, ErrorEnvelope, ModuleId, OperationId, RequestId,
};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::neoforge_runner::{bind, regular_file, RunnerError};

pub const MAPPINGS_SOURCE: (&str, &str) = ("mojang-data", "https://piston-data.mojang.com/");
pub const MAPPINGS_PATH: &str =
    "libraries/net/minecraft/client/1.20.4-20240627.114801/client-1.20.4-20240627.114801-mappings.txt";
const PILOT_GAME: &str = "1.20.4";
const DIR_ROOT: &str = "https://piston-data.mojang.com/v1/objects/";
const MAX_JSON: usize = 8 * 1024 * 1024;
const MAX_SIZE: u64 = 64 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;

#[derive(Debug)]
pub enum MappingsError {
    Io(io::Error),
    Invalid(String),
    Cancelled,
    Path(RunnerError),
}

impl fmt::Display for MappingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MappingsError::Io(e) => write!(f, "io error: {e}"),
            MappingsError::Invalid(m) => write!(f, "invalid mappings metadata: {m}"),
            MappingsError::Cancelled => write!(f, "mappings copy cancelled"),
            MappingsError::Path(e) => write!(f, "path error: {e}"),
        }
    }
}

impl std::error::Error for MappingsError {}
impl From<io::Error> for MappingsError {
    fn from(e: io::Error) -> Self {
        MappingsError::Io(e)
    }
}
impl From<RunnerError> for MappingsError {
    fn from(e: RunnerError) -> Self {
        MappingsError::Path(e)
    }
}

/// Verified client-mappings metadata bound to an immutable source URL.
#[derive(Debug, Clone)]
pub struct ClientMappings {
    size: u64,
    sha1: String,
    url: String,
}

fn invalid<T>(message: &str) -> Result<T, MappingsError> {
    Err(MappingsError::Invalid(message.into()))
}

fn canonical_sha1(value: &str) -> Result<String, MappingsError> {
    if value.len() != 40 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return invalid("sha1 must be 40 hex characters");
    }
    Ok(value.to_ascii_lowercase())
}

fn canonical_url(sha1: &str) -> String {
    format!("{DIR_ROOT}{sha1}/client.txt")
}

impl ClientMappings {
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn sha1(&self) -> &str {
        &self.sha1
    }
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn artifact_hash(&self) -> ArtifactHash {
        ArtifactHash::new("sha1", &self.sha1).expect("validated sha1 is a legal artifact digest")
    }

    pub fn download_payload(&self) -> Value {
        json!({
            "command": crate::downloads::actions::FETCH,
            "artifacts": [{
                "artifactId": format!("mojmaps-{}", self.sha1),
                "sourceId": MAPPINGS_SOURCE.0,
                "url": self.url,
                "size": self.size,
                "hash": {"algorithm": "sha1", "digest": self.sha1}
            }]
        })
    }

    pub fn start_download(
        &self,
        app: &crate::CoreApp,
        request_id: RequestId,
        operation_id: OperationId,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        app.dispatch_mutation(
            ModuleId::Downloads,
            &CommandRequest {
                request_id,
                operation_id: Some(operation_id),
                entity_id: None,
                expected_revision: None,
                payload: self.download_payload(),
            },
        )
    }

    /// Copy a cached artifact into the pilot destination without overwriting.
    pub fn stage_cached(
        &self,
        root: &Path,
        cached: &Path,
        cancelled: impl Fn() -> bool,
    ) -> Result<PathBuf, MappingsError> {
        let meta = std::fs::symlink_metadata(root)?;
        if !meta.is_dir() {
            return invalid("root must be an existing directory");
        }
        let destination = bind(root, MAPPINGS_PATH)?;
        if regular_file(cached)? != self.size {
            return invalid("cached size does not match metadata");
        }
        if cancelled() {
            return Err(MappingsError::Cancelled);
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut source = File::open(cached)?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)?;
        let result = copy_verified(&mut source, &mut output, self.size, &self.sha1, &cancelled);
        drop(output);
        match result {
            Ok(()) => Ok(destination),
            Err(error) => {
                let _ = std::fs::remove_file(&destination);
                Err(error)
            }
        }
    }
}

fn copy_verified(
    source: &mut File,
    output: &mut File,
    expected: u64,
    expected_sha1: &str,
    cancelled: &impl Fn() -> bool,
) -> Result<(), MappingsError> {
    let mut buf = vec![0u8; CHUNK];
    let mut size = 0u64;
    let mut hash = Sha1::new();
    loop {
        if cancelled() {
            return Err(MappingsError::Cancelled);
        }
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        if size > expected {
            return invalid("cached artifact exceeds expected size");
        }
        hash.update(&buf[..n]);
        output.write_all(&buf[..n])?;
    }
    if size != expected {
        return invalid("cached artifact shorter than expected size");
    }
    let digest = format!("{:x}", hash.finalize());
    if digest != expected_sha1 {
        return invalid("cached artifact sha1 mismatch");
    }
    output.sync_all()?;
    Ok(())
}

/// Parse trusted, already-identity-verified version metadata into mappings.
pub fn parse_verified_metadata(
    game_version: &str,
    bytes: &[u8],
) -> Result<ClientMappings, MappingsError> {
    if game_version != PILOT_GAME {
        return invalid("unsupported game version");
    }
    if bytes.len() > MAX_JSON {
        return invalid("metadata exceeds size budget");
    }
    let root: Value =
        serde_json::from_slice(bytes).map_err(|e| MappingsError::Invalid(e.to_string()))?;
    if root.get("id").and_then(Value::as_str) != Some(game_version) {
        return invalid("root id does not match game version");
    }
    let entry = root
        .get("downloads")
        .and_then(|d| d.get("client_mappings"))
        .ok_or_else(|| MappingsError::Invalid("missing client_mappings".into()))?;
    let size = entry
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| MappingsError::Invalid("missing size".into()))?;
    if size == 0 || size > MAX_SIZE {
        return invalid("size out of bounds");
    }
    let raw_sha1 = entry
        .get("sha1")
        .and_then(Value::as_str)
        .ok_or_else(|| MappingsError::Invalid("missing sha1".into()))?;
    let sha1 = canonical_sha1(raw_sha1)?;
    let url = entry
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| MappingsError::Invalid("missing url".into()))?;
    if url != canonical_url(&sha1) {
        return invalid("url is not canonical Mojang object url");
    }
    Ok(ClientMappings {
        size,
        sha1,
        url: url.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "be76ecc174ea25580bdc9bf335481a5192d9f3b7";

    fn doc(sha1: &str, size: u64, url: &str, id: &str) -> Vec<u8> {
        json!({"id": id, "downloads": {"client_mappings": {"size": size, "sha1": sha1, "url": url}}}).to_string().into_bytes()
    }

    fn good() -> Vec<u8> {
        doc(
            &H.to_ascii_uppercase(),
            8897012,
            &canonical_url(H),
            "1.20.4",
        )
    }

    fn tempdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "neo-map-{label}-{}",
            shtmcl_platform::random_hex(12).unwrap()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha1::digest(bytes))
    }

    fn cached(dir: &Path, bytes: &[u8]) -> String {
        let path = dir.join("cache.txt");
        std::fs::write(&path, bytes).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn accepts_canonical_metadata_and_normalizes_sha1() {
        let m = parse_verified_metadata("1.20.4", &good()).unwrap();
        assert_eq!(m.sha1(), H);
        assert_eq!(m.size(), 8897012);
        assert_eq!(m.artifact_hash().algorithm, "sha1");
    }

    #[test]
    fn rejects_bad_versions_and_urls() {
        assert!(parse_verified_metadata("1.20.6", &good()).is_err());
        assert!(
            parse_verified_metadata("1.20.4", &doc(H, 8897012, &canonical_url(H), "1.20.6"))
                .is_err()
        );
        for bad in [
            format!("{DIR_ROOT}{H}/client.txt?x=1"),
            format!("{DIR_ROOT}{H}/client.txt#f"),
            format!("{DIR_ROOT}{H}/%2e%2e/client.txt"),
            format!("https://piston-data.mojang.com.evil/v1/objects/{H}/client.txt"),
            format!("https://piston-data.mojang.com:8443/v1/objects/{H}/client.txt"),
            format!("https://user:pass@piston-data.mojang.com/v1/objects/{H}/client.txt"),
            format!("http://piston-data.mojang.com/v1/objects/{H}/client.txt"),
        ] {
            assert!(
                parse_verified_metadata("1.20.4", &doc(H, 8897012, &bad, "1.20.4")).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn rejects_bad_hash_and_size() {
        assert!(
            parse_verified_metadata("1.20.4", &doc("abc", 1, &canonical_url(H), "1.20.4")).is_err()
        );
        assert!(parse_verified_metadata(
            "1.20.4",
            &doc(&"g".repeat(40), 1, &canonical_url(H), "1.20.4")
        )
        .is_err());
        assert!(
            parse_verified_metadata("1.20.4", &doc(H, 0, &canonical_url(H), "1.20.4")).is_err()
        );
        assert!(parse_verified_metadata(
            "1.20.4",
            &doc(H, MAX_SIZE + 1, &canonical_url(H), "1.20.4")
        )
        .is_err());
        let mut big = vec![b' '; MAX_JSON + 1];
        big[0] = b'{';
        assert!(parse_verified_metadata("1.20.4", &big).is_err());
    }

    fn mappings(bytes: &[u8]) -> ClientMappings {
        parse_verified_metadata(
            "1.20.4",
            &doc(
                &sha1_hex(bytes),
                bytes.len() as u64,
                &format!("{DIR_ROOT}{}/client.txt", sha1_hex(bytes)),
                "1.20.4",
            ),
        )
        .unwrap()
    }

    #[test]
    fn stages_matching_bytes() {
        let root = tempdir("ok");
        let data = b"hello mappings".to_vec();
        let src = cached(&root, &data);
        let m = mappings(&data);
        let out = m.stage_cached(&root, Path::new(&src), || false).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), data);
        assert_eq!(out, root.join(MAPPINGS_PATH));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hash_mismatch_cleans_up() {
        let root = tempdir("hash");
        let data = b"real".to_vec();
        let src = cached(&root, b"fake");
        let m = mappings(&data);
        assert!(m.stage_cached(&root, Path::new(&src), || false).is_err());
        assert!(!root.join(MAPPINGS_PATH).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_cleans_up() {
        let root = tempdir("cancel");
        let data = vec![7u8; CHUNK * 2];
        let src = cached(&root, &data);
        let m = mappings(&data);
        let checks = std::cell::Cell::new(0);
        let result = m.stage_cached(&root, Path::new(&src), || {
            checks.set(checks.get() + 1);
            // Cancel after at least one chunk has actually reached disk.
            checks.get() >= 3
        });
        assert!(matches!(result, Err(MappingsError::Cancelled)));
        assert!(!root.join(MAPPINGS_PATH).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn existing_destination_preserved() {
        let root = tempdir("exists");
        let data = b"new".to_vec();
        let src = cached(&root, &data);
        let dest = root.join(MAPPINGS_PATH);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"old").unwrap();
        let m = mappings(&data);
        assert!(m.stage_cached(&root, Path::new(&src), || false).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn download_request_uses_mapping_identity_and_shared_dispatcher() {
        let mappings = parse_verified_metadata("1.20.4", &good()).unwrap();
        let payload = mappings.download_payload();
        assert_eq!(payload["command"], crate::downloads::actions::FETCH);
        assert_eq!(payload["artifacts"][0]["sourceId"], MAPPINGS_SOURCE.0);
        assert_eq!(payload["artifacts"][0]["hash"]["digest"], H);
        // An unregistered application must report the missing download owner.
        let root = tempdir("dispatcher");
        let app = crate::CoreApp::new(shtmcl_platform::PlatformDirs::under(&root));
        assert!(mappings
            .start_download(
                &app,
                RequestId::new("mapping-request").unwrap(),
                OperationId::new("mapping-operation").unwrap()
            )
            .is_err());
        assert!(app.snapshot().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Acceptance harness: real shared HTTP download/cache, followed by the
    /// fixed client processor chain. Does not create or commit a game instance.
    #[test]
    #[ignore = "requires official HTTPS, a fresh copied stage and managed Java 17"]
    fn official_mappings_and_client_processor_chain() {
        use crate::downloads::{
            ByteRange, DownloadPort, DownloadsModule, FetchOutcome, FileDisk, FileStore,
            HttpDownloads, HttpSource, SystemClock,
        };
        use shtmcl_domain::TaskState;
        use shtmcl_platform::{PlatformDirs, StdProcesses};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use std::time::{Duration, Instant};

        // A second application uses only FileStore: any accidental fetch is a
        // recorded failure, so success proves reuse across application state.
        struct CacheOnly {
            store: FileStore,
            fetches: AtomicUsize,
        }
        impl DownloadPort for CacheOnly {
            fn probe(&self, _: &str, _: &str) -> Result<Option<u64>, String> {
                Err("network disabled".into())
            }
            fn fetch(&self, _: &str, _: &str, _: ByteRange, _: Option<&str>) -> FetchOutcome {
                self.fetches.fetch_add(1, Ordering::Relaxed);
                FetchOutcome::TransportFailed("network disabled".into())
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
            fn materialize_cached(
                &self,
                hash: &ArtifactHash,
                id: &str,
            ) -> Result<Option<u64>, String> {
                self.store.materialize(hash, id).map_err(|e| e.to_string())
            }
            fn recover_cached(&self, hash: &ArtifactHash, id: &str) -> Result<Option<u64>, String> {
                match self.store.verify_cached(hash).map_err(|e| e.to_string())? {
                    Some(_) => self.materialize_cached(hash, id),
                    None => Ok(None),
                }
            }
            fn discard(&self, id: &str) -> Result<(), String> {
                self.store.discard(id).map_err(|e| e.to_string())
            }
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
            mappings: &ClientMappings,
            operation: &str,
        ) -> shtmcl_domain::Task {
            let response = mappings
                .start_download(
                    app,
                    RequestId::new(format!("req-{operation}")).unwrap(),
                    OperationId::new(operation).unwrap(),
                )
                .unwrap();
            let CommandResponse::Accepted { task_id, .. } = response else {
                panic!("expected task");
            };
            let deadline = Instant::now() + Duration::from_secs(180);
            loop {
                let task = app
                    .snapshot()
                    .into_iter()
                    .find(|t| t.id == task_id)
                    .unwrap();
                if matches!(
                    task.state,
                    TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
                ) {
                    return task;
                }
                assert!(Instant::now() < deadline, "mapping download did not finish");
                std::thread::sleep(Duration::from_millis(25));
            }
        }

        let env_path = |key| PathBuf::from(std::env::var_os(key).expect(key));
        let root = env_path("SHTMCL_NEOFORGE_MAPPING_TEST_ROOT");
        let stage = root.join("pipeline-stage");
        let metadata = std::fs::read(env_path("SHTMCL_NEOFORGE_GAME_METADATA")).unwrap();
        let expected_metadata = std::env::var("SHTMCL_NEOFORGE_GAME_METADATA_SHA1").unwrap();
        assert_eq!(format!("{:x}", Sha1::digest(&metadata)), expected_metadata);
        let mappings = parse_verified_metadata("1.20.4", &metadata).unwrap();
        let cache = root.join("cache");
        let source = HttpSource::new(MAPPINGS_SOURCE.0, MAPPINGS_SOURCE.1).unwrap();
        let download_app = app(
            &root,
            Arc::new(HttpDownloads::new(&cache, vec![source]).unwrap()),
        );
        let first = acquire(&download_app, &mappings, "official-mappings");
        assert_eq!(first.state, TaskState::Succeeded, "{:?}", first.error);
        let store = FileStore::new(&cache).unwrap();
        assert_eq!(
            store.verify_cached(&mappings.artifact_hash()).unwrap(),
            Some(mappings.size())
        );
        std::fs::write(
            root.join("official-download-task.json"),
            serde_json::to_vec_pretty(&first).unwrap(),
        )
        .unwrap();
        drop(download_app);
        let cache_port = Arc::new(CacheOnly {
            store,
            fetches: AtomicUsize::new(0),
        });
        let cached_app = app(&root, cache_port.clone());
        let second = acquire(&cached_app, &mappings, "cached-mappings");
        assert_eq!(second.state, TaskState::Succeeded, "{:?}", second.error);
        assert_eq!(cache_port.fetches.load(Ordering::Relaxed), 0);
        std::fs::write(
            root.join("cached-download-task.json"),
            serde_json::to_vec_pretty(&second).unwrap(),
        )
        .unwrap();
        let object = cache
            .join("objects/sha1")
            .join(&mappings.sha1()[..2])
            .join(mappings.sha1());
        let java = env_path("SHTMCL_NEOFORGE_RUNNER_JAVA");
        let installer_digest = std::env::var("SHTMCL_NEOFORGE_INSTALLER_SHA256").unwrap();
        let processes = StdProcesses::capturing();
        let mut results = Vec::new();
        for index in [3, 4, 5, 6, 8, 9] {
            println!("starting client processor index={index}");
            if index == 4 {
                let path = mappings.stage_cached(&stage, &object, || false).unwrap();
                assert_eq!(
                    format!("{:x}", Sha1::digest(std::fs::read(&path).unwrap())),
                    mappings.sha1()
                );
                results.push(json!({"index":4,"kind":"shared-download-and-copy","path":path,"size":mappings.size(),"sha1":mappings.sha1(),"cacheReusedWithoutNetwork":true}));
            } else {
                let start = Instant::now();
                let result = super::super::neoforge_runner::execute_client_step(
                    &stage,
                    &installer_digest,
                    index,
                    &java,
                    &processes,
                    &processes,
                    Duration::from_secs(300),
                    || false,
                );
                match result {
                    Ok(result) => {
                        std::fs::write(
                            root.join(format!("step-{index}-stdout.log")),
                            &result.logs.stdout.bytes,
                        )
                        .unwrap();
                        std::fs::write(
                            root.join(format!("step-{index}-stderr.log")),
                            &result.logs.stderr.bytes,
                        )
                        .unwrap();
                        let outputs: Vec<_> = result
                            .outputs
                            .iter()
                            .map(|o| json!({"path":o.path,"size":o.size,"sha256":o.sha256}))
                            .collect();
                        results.push(json!({"index":index,"outputs":outputs,"elapsedMs":start.elapsed().as_millis(),"stdoutTruncated":result.logs.stdout.truncated,"stderrTruncated":result.logs.stderr.truncated}));
                    }
                    Err(error) => {
                        std::fs::write(
                            root.join(format!("step-{index}-error.txt")),
                            format!("{error:?}"),
                        )
                        .unwrap();
                        panic!("processor {index} failed: {error}");
                    }
                }
            }
            std::fs::write(
                root.join("pipeline-results.json"),
                serde_json::to_vec_pretty(&results).unwrap(),
            )
            .unwrap();
            println!("completed client processor index={index}");
        }
        assert_eq!(results.len(), 6);
    }
}

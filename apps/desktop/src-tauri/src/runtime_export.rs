//! Controlled export from the verified download cache to a Java staging tree.
//!
//! The cache owns its object paths. This adapter uses only `FileStore`'s public
//! content identity APIs and publishes each file atomically inside a caller-
//! supplied staging directory.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sha1::{Digest, Sha1};
use sha2::Sha256;
use shtmcl_core::downloads::{ArtifactHash, FileStore};

const COPY_BUFFER_BYTES: usize = 128 * 1024;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

struct PartialCleanup<'a> {
    store: &'a FileStore,
    artifact_id: &'a str,
}

impl Drop for PartialCleanup<'_> {
    fn drop(&mut self) {
        let _ = self.store.discard(self.artifact_id);
    }
}

#[derive(Debug)]
pub enum ExportError {
    InvalidPath,
    CacheMiss,
    SizeMismatch { expected: u64, actual: u64 },
    DigestMismatch,
    Cancelled,
    DestinationConflict,
    Io(io::Error),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPath => formatter.write_str("runtime export path is invalid"),
            Self::CacheMiss => formatter.write_str("verified runtime artifact is not cached"),
            Self::SizeMismatch { expected, actual } => {
                write!(
                    formatter,
                    "runtime artifact size mismatch: expected {expected}, got {actual}"
                )
            }
            Self::DigestMismatch => formatter.write_str("runtime artifact digest mismatch"),
            Self::Cancelled => formatter.write_str("runtime artifact export was cancelled"),
            Self::DestinationConflict => {
                formatter.write_str("runtime staging destination conflicts with verified content")
            }
            Self::Io(error) => write!(formatter, "runtime artifact export failed: {error}"),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<io::Error> for ExportError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Streaming digest selected from a validated [`ArtifactHash`] algorithm name.
///
/// Unknown algorithms are rejected before any cache or staging side effects.
enum StreamingDigest {
    Sha1(Sha1),
    Sha256(Sha256),
}

impl StreamingDigest {
    fn from_hash(hash: &ArtifactHash) -> Result<Self, ExportError> {
        match hash.algorithm.as_str() {
            "sha1" => Ok(Self::Sha1(Sha1::default())),
            "sha256" => Ok(Self::Sha256(Sha256::default())),
            _ => Err(ExportError::InvalidPath),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Sha1(digest) => digest.update(bytes),
            Self::Sha256(digest) => digest.update(bytes),
        }
    }

    fn finalize_hex(self) -> String {
        match self {
            Self::Sha1(digest) => format!("{:x}", digest.finalize()),
            Self::Sha256(digest) => format!("{:x}", digest.finalize()),
        }
    }
}

/// Copy one cache-verified Mojang runtime file into the staging tree.
///
/// A completed file is accepted on retry only if its size and declared hash match the
/// manifest. New files are written beside the destination, synced, rechecked,
/// and renamed into place. Cancellation removes only this attempt's temp file.
pub fn export_cached_artifact(
    store: &FileStore,
    hash: &ArtifactHash,
    artifact_id: &str,
    staging_root: &Path,
    relative_path: &Path,
    expected_size: u64,
    cancelled: &AtomicBool,
) -> Result<u64, ExportError> {
    let cancel = || cancelled.load(Ordering::Acquire);
    export_cached_artifact_with_cancel(
        store,
        hash,
        artifact_id,
        staging_root,
        relative_path,
        expected_size,
        &cancel,
    )
}

/// Copy one cache-verified runtime file into the staging tree, polling a live
/// cancellation callback throughout streamed and verification IO.
///
/// A completed file is accepted on retry only if its size and declared digest
/// (SHA-1 or SHA-256) match the manifest. New files are written beside the
/// destination, synced, rechecked, and renamed into place. Cancellation removes
/// only this attempt's temp file and the exported cache partial.
pub fn export_cached_artifact_with_cancel(
    store: &FileStore,
    hash: &ArtifactHash,
    artifact_id: &str,
    staging_root: &Path,
    relative_path: &Path,
    expected_size: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<u64, ExportError> {
    if !is_safe_relative_path(relative_path) {
        return Err(ExportError::InvalidPath);
    }
    // Validate both the algorithm and digest before touching cache or stage.
    ArtifactHash::new(&hash.algorithm, &hash.digest).map_err(|_| ExportError::InvalidPath)?;
    check_cancelled(cancelled)?;

    let Some(materialized_size) = store.materialize(hash, artifact_id)? else {
        return Err(ExportError::CacheMiss);
    };
    let _cleanup = PartialCleanup { store, artifact_id };
    if materialized_size != expected_size {
        return Err(ExportError::SizeMismatch {
            expected: expected_size,
            actual: materialized_size,
        });
    }
    if store.hash_partial(artifact_id, &hash.algorithm)? != hash.digest {
        return Err(ExportError::DigestMismatch);
    }
    check_cancelled(cancelled)?;

    let root = ensure_staging_root(staging_root)?;
    let destination = prepare_destination(&root, relative_path)?;
    if destination.exists() {
        return verify_existing(&destination, hash, expected_size, cancelled);
    }

    let temp_path = temporary_path(&destination);
    let result = write_verified_temp(
        store,
        artifact_id,
        hash,
        expected_size,
        &temp_path,
        cancelled,
    )
    .and_then(|size| {
        check_cancelled(cancelled)?;
        if destination.exists() {
            return Err(ExportError::DestinationConflict);
        }
        fs::rename(&temp_path, &destination)?;
        Ok(size)
    });
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn write_verified_temp(
    store: &FileStore,
    artifact_id: &str,
    hash: &ArtifactHash,
    expected_size: u64,
    temp_path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<u64, ExportError> {
    let mut source = store.open_partial(artifact_id, 0)?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)?;
    let mut digest = StreamingDigest::from_hash(hash)?;
    let mut buffer = [0u8; COPY_BUFFER_BYTES];
    let mut total = 0u64;

    loop {
        check_cancelled(cancelled)?;
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or(ExportError::SizeMismatch {
                expected: expected_size,
                actual: u64::MAX,
            })?;
        if total > expected_size {
            return Err(ExportError::SizeMismatch {
                expected: expected_size,
                actual: total,
            });
        }
        target.write_all(&buffer[..count])?;
        digest.update(&buffer[..count]);
        check_cancelled(cancelled)?;
    }
    if total != expected_size {
        return Err(ExportError::SizeMismatch {
            expected: expected_size,
            actual: total,
        });
    }
    if digest.finalize_hex() != hash.digest {
        return Err(ExportError::DigestMismatch);
    }
    check_cancelled(cancelled)?;
    target.sync_all()?;
    Ok(total)
}

fn verify_existing(
    path: &Path,
    hash: &ArtifactHash,
    expected_size: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<u64, ExportError> {
    reject_link(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ExportError::DestinationConflict);
    }
    if metadata.len() != expected_size {
        return Err(ExportError::DestinationConflict);
    }
    if hash_file(path, hash, expected_size, cancelled)? != hash.digest {
        return Err(ExportError::DestinationConflict);
    }
    Ok(metadata.len())
}

fn ensure_staging_root(path: &Path) -> Result<PathBuf, ExportError> {
    for ancestor in path.ancestors() {
        reject_link(ancestor)?;
    }
    fs::create_dir_all(path)?;
    let root = fs::canonicalize(path)?;
    if !fs::metadata(&root)?.is_dir() {
        return Err(ExportError::InvalidPath);
    }
    Ok(root)
}

fn prepare_destination(root: &Path, relative: &Path) -> Result<PathBuf, ExportError> {
    let mut parent = root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    let prefix_len = components.len().saturating_sub(1);
    for component in &components[..prefix_len] {
        let Component::Normal(segment) = component else {
            return Err(ExportError::InvalidPath);
        };
        parent.push(segment);
        reject_link(&parent)?;
        match fs::symlink_metadata(&parent) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(ExportError::InvalidPath);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&parent)?,
            Err(error) => return Err(error.into()),
        }
        if !fs::canonicalize(&parent)?.starts_with(root) {
            return Err(ExportError::InvalidPath);
        }
    }
    let Component::Normal(file_name) = components.last().ok_or(ExportError::InvalidPath)? else {
        return Err(ExportError::InvalidPath);
    };
    let destination = parent.join(file_name);
    reject_link(&destination)?;
    Ok(destination)
}

fn reject_link(path: &Path) -> Result<(), ExportError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ExportError::InvalidPath);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err(ExportError::InvalidPath);
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn temporary_path(destination: &Path) -> PathBuf {
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("runtime-file");
    destination.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), sequence))
}

fn is_safe_relative_path(path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }
    path.components().all(|component| match component {
        Component::Normal(segment) => {
            let text = segment.to_string_lossy();
            !text.is_empty() && !text.contains(['\\', '/', ':']) && text != "." && text != ".."
        }
        _ => false,
    })
}

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> Result<(), ExportError> {
    if cancelled() {
        Err(ExportError::Cancelled)
    } else {
        Ok(())
    }
}

fn hash_file(
    path: &Path,
    hash: &ArtifactHash,
    expected_size: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<String, ExportError> {
    let mut file = File::open(path)?;
    let mut digest = StreamingDigest::from_hash(hash)?;
    let mut buffer = [0u8; COPY_BUFFER_BYTES];
    let mut total = 0u64;
    loop {
        check_cancelled(cancelled)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or(ExportError::DestinationConflict)?;
        if total > expected_size {
            return Err(ExportError::DestinationConflict);
        }
        digest.update(&buffer[..count]);
    }
    if total != expected_size {
        return Err(ExportError::DestinationConflict);
    }
    Ok(digest.finalize_hex())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "shtmcl-runtime-export-{label}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            fs::create_dir_all(&root).expect("temp directory");
            Self(root)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn cache_fixture(root: &Path, artifact_id: &str, bytes: &[u8]) -> (FileStore, ArtifactHash) {
        cache_fixture_algo(root, artifact_id, bytes, "sha1")
    }

    fn cache_fixture_algo(
        root: &Path,
        artifact_id: &str,
        bytes: &[u8],
        algorithm: &str,
    ) -> (FileStore, ArtifactHash) {
        let store = FileStore::new(root.join("cache")).expect("file store");
        let digest = match algorithm {
            "sha1" => format!("{:x}", Sha1::default().chain_update(bytes).finalize()),
            "sha256" => format!("{:x}", Sha256::default().chain_update(bytes).finalize()),
            other => panic!("unsupported fixture algorithm {other}"),
        };
        let hash = ArtifactHash::new(algorithm, &digest).expect("hash");
        let mut partial = store.reset_partial(artifact_id).expect("partial");
        partial.write_all(bytes).expect("write fixture");
        store.commit_partial(artifact_id).expect("sync fixture");
        store.publish(&hash, artifact_id).expect("publish fixture");
        (store, hash)
    }

    fn no_cancel() -> bool {
        false
    }

    #[allow(clippy::redundant_closure)]
    fn never_cancelled() -> impl Fn() -> bool {
        || false
    }

    #[test]
    fn exports_verified_bytes_atomically_inside_staging_root() {
        let temp = TempDir::new("success");
        let bytes = b"verified runtime file";
        let (store, hash) = cache_fixture(&temp.0, "runtime/bin/java.exe", bytes);
        let destination = temp.0.join("staging");
        let cancelled = AtomicBool::new(false);

        let written = export_cached_artifact(
            &store,
            &hash,
            "export-java",
            &destination,
            Path::new("bin/java.exe"),
            bytes.len() as u64,
            &cancelled,
        )
        .expect("export verified runtime file");

        assert_eq!(written, bytes.len() as u64);
        assert_eq!(
            fs::read(destination.join("bin/java.exe")).expect("read output"),
            bytes
        );
        assert_eq!(
            export_cached_artifact(
                &store,
                &hash,
                "export-java-retry",
                &destination,
                Path::new("bin/java.exe"),
                bytes.len() as u64,
                &cancelled,
            )
            .expect("idempotent verified retry"),
            bytes.len() as u64
        );
    }

    #[test]
    fn exports_and_reuses_sha256_artifact() {
        let temp = TempDir::new("sha256");
        let bytes = b"sha256 verified runtime payload";
        let (store, hash) = cache_fixture_algo(&temp.0, "runtime/bin/java256.exe", bytes, "sha256");
        let destination = temp.0.join("staging");

        let written = export_cached_artifact_with_cancel(
            &store,
            &hash,
            "export-java256",
            &destination,
            Path::new("bin/java256.exe"),
            bytes.len() as u64,
            &never_cancelled(),
        )
        .expect("export sha256 runtime file");
        assert_eq!(written, bytes.len() as u64);
        assert_eq!(
            fs::read(destination.join("bin/java256.exe")).expect("read output"),
            bytes
        );

        // Reuse path verifies the existing destination against SHA-256.
        assert_eq!(
            export_cached_artifact_with_cancel(
                &store,
                &hash,
                "export-java256-retry",
                &destination,
                Path::new("bin/java256.exe"),
                bytes.len() as u64,
                &never_cancelled(),
            )
            .expect("idempotent sha256 retry"),
            bytes.len() as u64
        );
    }

    #[test]
    fn rejects_same_size_corruption_at_existing_destination() {
        let temp = TempDir::new("corrupt-dest");
        let bytes = b"verified runtime file";
        let (store, hash) = cache_fixture(&temp.0, "runtime/bin/java.exe", bytes);
        let destination = temp.0.join("staging");
        let cancelled = AtomicBool::new(false);

        export_cached_artifact(
            &store,
            &hash,
            "export-java",
            &destination,
            Path::new("bin/java.exe"),
            bytes.len() as u64,
            &cancelled,
        )
        .expect("initial export");

        let target = destination.join("bin/java.exe");
        let mut corrupt = bytes.to_vec();
        corrupt[0] ^= 0xff;
        fs::write(&target, &corrupt).expect("corrupt destination");
        assert_eq!(
            fs::metadata(&target).expect("metadata").len(),
            bytes.len() as u64
        );

        assert!(matches!(
            export_cached_artifact(
                &store,
                &hash,
                "export-java-retry",
                &destination,
                Path::new("bin/java.exe"),
                bytes.len() as u64,
                &cancelled,
            ),
            Err(ExportError::DestinationConflict)
        ));
        assert_eq!(fs::read(&target).expect("read preserved"), corrupt);
    }

    #[cfg(windows)]
    #[test]
    fn refuses_junction_as_staging_root() {
        use std::os::windows::process::CommandExt;
        let temp = TempDir::new("junction");
        let external = temp.0.join("external");
        let link = temp.0.join("linked");
        fs::create_dir(&external).unwrap();
        let status = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&external)
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(status.status.success(), "{:?}", status);
        let bytes = b"verified";
        let (store, hash) = cache_fixture(&temp.0, "junction-input", bytes);
        let result = export_cached_artifact_with_cancel(
            &store,
            &hash,
            "junction-export",
            &link,
            Path::new("file.jar"),
            bytes.len() as u64,
            &no_cancel,
        );
        assert!(matches!(result, Err(ExportError::InvalidPath)));
        assert_eq!(fs::read_dir(&external).unwrap().count(), 0);
        fs::remove_dir(&link).unwrap();
    }

    #[test]
    fn cancellation_after_copy_begins_removes_only_temp() {
        let temp = TempDir::new("cancel-live");
        let bytes = b"verified runtime file for cancellation";
        let (store, hash) = cache_fixture(&temp.0, "runtime/bin/java.exe", bytes);
        let destination = temp.0.join("staging");
        let cancel = std::sync::atomic::AtomicU32::new(0);

        let result = export_cached_artifact_with_cancel(
            &store,
            &hash,
            "export-cancelled-live",
            &destination,
            Path::new("bin/java.exe"),
            bytes.len() as u64,
            &|| {
                // Begin allowing progress, then trip cancellation once inside IO.
                let previous = cancel.fetch_add(1, Ordering::AcqRel);
                previous >= 3
            },
        );

        assert!(
            matches!(result, Err(ExportError::Cancelled)),
            "got {result:?}"
        );
        assert!(!destination.join("bin/java.exe").exists());
        let mut leftovers = Vec::new();
        if let Ok(entries) = fs::read_dir(destination.join("bin")) {
            for entry in entries {
                leftovers.push(entry.expect("dir entry").path());
            }
        }
        assert!(
            leftovers.iter().all(|path| !path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.ends_with(".tmp"))
                .unwrap_or(false)),
            "temp file left behind: {leftovers:?}"
        );
        assert_eq!(
            fs::read_dir(temp.0.join("cache/partial")).unwrap().count(),
            0
        );
        assert_eq!(
            store.verify_cached(&hash).unwrap(),
            Some(bytes.len() as u64)
        );
    }

    #[test]
    fn rejects_unknown_hash_algorithm_before_writes() {
        let temp = TempDir::new("unknown-algo");
        let bytes = b"verified runtime file";
        let (store, _hash) = cache_fixture(&temp.0, "runtime/bin/java.exe", bytes);
        let unknown = ArtifactHash {
            algorithm: "md5".into(),
            digest: "0123456789abcdef0123456789abcdef".into(),
        };
        let destination = temp.0.join("staging");

        assert!(matches!(
            export_cached_artifact_with_cancel(
                &store,
                &unknown,
                "export-unknown",
                &destination,
                Path::new("bin/java.exe"),
                bytes.len() as u64,
                &no_cancel,
            ),
            Err(ExportError::InvalidPath)
        ));
        assert!(!destination.exists());
        assert_eq!(
            fs::read_dir(temp.0.join("cache/partial")).unwrap().count(),
            0
        );
    }

    #[test]
    fn rejects_unsafe_paths_and_observes_cancellation_before_export() {
        let temp = TempDir::new("reject");
        let bytes = b"verified runtime file";
        let (store, hash) = cache_fixture(&temp.0, "runtime/bin/java.exe", bytes);
        let destination = temp.0.join("staging");
        let cancelled = AtomicBool::new(false);

        assert!(matches!(
            export_cached_artifact(
                &store,
                &hash,
                "export-traversal",
                &destination,
                Path::new("../outside.exe"),
                bytes.len() as u64,
                &cancelled,
            ),
            Err(ExportError::InvalidPath)
        ));

        cancelled.store(true, Ordering::Release);
        assert!(matches!(
            export_cached_artifact(
                &store,
                &hash,
                "export-cancelled",
                &destination,
                Path::new("bin/java.exe"),
                bytes.len() as u64,
                &cancelled,
            ),
            Err(ExportError::Cancelled)
        ));
        assert!(!destination.join("bin/java.exe").exists());
    }
}

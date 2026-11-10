use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::manifest::{ManifestError, ManifestIndex, MAX_MANIFEST_BYTES};

const CACHE_FILE: &str = "version_manifest_v2.json";
const BACKUP_FILE: &str = "version_manifest_v2.json.bak";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSnapshot {
    pub index: ManifestIndex,
    pub source: SnapshotSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotSource {
    Fresh,
    OfflineCache,
    RecoveredBackup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogCacheError {
    Io(String),
    InvalidCache(ManifestError),
    NoUsableCache,
}

/// Recoverable cache rooted under the caller's portable data directory.
/// The caller fetches and validates network bytes before calling `store`.
#[derive(Debug, Clone)]
pub struct CatalogCache {
    directory: PathBuf,
}

impl CatalogCache {
    pub fn new(data_directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: data_directory.into(),
        }
    }

    pub fn load_offline(&self) -> Result<CatalogSnapshot, CatalogCacheError> {
        let primary = self.directory.join(CACHE_FILE);
        match read_index(&primary) {
            Ok(index) => {
                return Ok(CatalogSnapshot {
                    index,
                    source: SnapshotSource::OfflineCache,
                })
            }
            Err(_) => {}
        }
        let backup = self.directory.join(BACKUP_FILE);
        match read_index(&backup) {
            Ok(index) => Ok(CatalogSnapshot {
                index,
                source: SnapshotSource::RecoveredBackup,
            }),
            Err(_) => Err(CatalogCacheError::NoUsableCache),
        }
    }

    pub fn store(&self, bytes: &[u8]) -> Result<CatalogSnapshot, CatalogCacheError> {
        let index = ManifestIndex::parse(bytes).map_err(CatalogCacheError::InvalidCache)?;
        fs::create_dir_all(&self.directory).map_err(io_error)?;
        let temp = self.directory.join(format!(
            ".manifest-{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(io_error)?;
        if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&temp);
            return Err(io_error(error));
        }
        drop(file);

        let primary = self.directory.join(CACHE_FILE);
        let backup = self.directory.join(BACKUP_FILE);
        if primary.exists() {
            let _ = fs::remove_file(&backup);
            fs::rename(&primary, &backup).map_err(|error| {
                let _ = fs::remove_file(&temp);
                io_error(error)
            })?;
        }
        if let Err(error) = fs::rename(&temp, &primary) {
            if backup.exists() && !primary.exists() {
                let _ = fs::rename(&backup, &primary);
            }
            let _ = fs::remove_file(&temp);
            return Err(io_error(error));
        }
        // Keep the last known-good snapshot until the replacement is committed.
        let _ = File::open(&self.directory).and_then(|directory| directory.sync_all());
        Ok(CatalogSnapshot {
            index,
            source: SnapshotSource::Fresh,
        })
    }

    pub fn cache_path(&self) -> PathBuf {
        self.directory.join(CACHE_FILE)
    }
}

fn read_index(path: &Path) -> Result<ManifestIndex, CatalogCacheError> {
    let file = File::open(path).map_err(io_error)?;
    let length = file.metadata().map_err(io_error)?.len();
    if length > MAX_MANIFEST_BYTES as u64 {
        return Err(CatalogCacheError::InvalidCache(ManifestError::TooLarge));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take((MAX_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    ManifestIndex::parse(&bytes).map_err(CatalogCacheError::InvalidCache)
}

fn io_error(error: std::io::Error) -> CatalogCacheError {
    CatalogCacheError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "shtmcl-catalog-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    const MANIFEST: &[u8] =
        br#"{"versions":[{"id":"1.20","type":"release","url":"https://example.invalid/v.json"}]}"#;

    #[test]
    fn cache_can_be_written_and_loaded_offline() {
        let root = temp_dir();
        let cache = CatalogCache::new(&root);
        assert_eq!(cache.store(MANIFEST).unwrap().source, SnapshotSource::Fresh);
        assert_eq!(
            cache.load_offline().unwrap().source,
            SnapshotSource::OfflineCache
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_replacement_recovers_previous_valid_snapshot() {
        let root = temp_dir();
        let cache = CatalogCache::new(&root);
        cache.store(MANIFEST).unwrap();
        fs::rename(cache.cache_path(), root.join(BACKUP_FILE)).unwrap();
        let recovered = cache.load_offline().unwrap();
        assert_eq!(recovered.source, SnapshotSource::RecoveredBackup);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_network_bytes_never_replace_the_good_cache() {
        let root = temp_dir();
        let cache = CatalogCache::new(&root);
        cache.store(MANIFEST).unwrap();
        assert!(matches!(
            cache.store(b"not json"),
            Err(CatalogCacheError::InvalidCache(_))
        ));
        assert_eq!(cache.load_offline().unwrap().index.entries[0].id, "1.20");
        fs::remove_dir_all(root).unwrap();
    }
}

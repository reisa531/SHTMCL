//! Official Minecraft version-detail fetcher with SHA-1-bound portable cache.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use sha1::{Digest as Sha1Digest, Sha1};
use sha2::Sha256;
use shtmcl_core::launch::catalog::{
    CatalogCache, ManifestFetcher, VersionMetadata, VersionMetadataSource,
};

const DETAILS_DIRECTORY: &str = "versions";
const MAX_VERSION_DETAIL_BYTES: usize = 8 * 1024 * 1024;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

pub(crate) struct CachedVersionMetadataSource {
    catalog: CatalogCache,
    fetcher: Arc<dyn ManifestFetcher>,
    refresh_lock: Mutex<()>,
}

/// A version-detail document whose raw bytes have been verified against the
/// catalog SHA-1 and whose parsed identity matches the requested id.
///
/// The original bytes are immutable through the accessors and are exactly those
/// present in the cache or returned by the fetcher, preserving whitespace and
/// object key order.
#[derive(Debug)]
pub(crate) struct VerifiedVersionDetail {
    bytes: Vec<u8>,
    sha1: String,
    metadata: Value,
}

impl VerifiedVersionDetail {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn sha1(&self) -> &str {
        &self.sha1
    }

    pub(crate) fn metadata(&self) -> &Value {
        &self.metadata
    }
}

impl CachedVersionMetadataSource {
    pub(crate) fn new(catalog: CatalogCache, fetcher: Arc<dyn ManifestFetcher>) -> Self {
        Self {
            catalog,
            fetcher,
            refresh_lock: Mutex::new(()),
        }
    }

    fn details_directory(&self) -> Result<PathBuf, String> {
        let manifest = self.catalog.cache_path();
        let parent = manifest
            .parent()
            .ok_or_else(|| "version.cache_path_invalid".to_string())?;
        Ok(parent.join(DETAILS_DIRECTORY))
    }

    fn cache_key(id: &str) -> String {
        format!("{:x}", Sha256::digest(id.as_bytes()))
    }

    fn read_cached(path: &Path, expected_sha1: &str, id: &str) -> Option<VerifiedVersionDetail> {
        let file = File::open(path).ok()?;
        if file.metadata().ok()?.len() > MAX_VERSION_DETAIL_BYTES as u64 {
            return None;
        }
        let mut bytes = Vec::new();
        file.take((MAX_VERSION_DETAIL_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        // Detect growth past the bound even if the initial metadata was small.
        if bytes.len() > MAX_VERSION_DETAIL_BYTES {
            return None;
        }
        Self::verify(bytes, expected_sha1, id)
    }

    fn verify(bytes: Vec<u8>, expected_sha1: &str, id: &str) -> Option<VerifiedVersionDetail> {
        if !is_sha1(expected_sha1) {
            return None;
        }
        if sha1_hex(&bytes) != expected_sha1.to_ascii_lowercase() {
            return None;
        }
        let metadata = VersionMetadata::parse(&bytes, Some(id)).ok()?.raw;
        Some(VerifiedVersionDetail {
            bytes,
            sha1: expected_sha1.to_ascii_lowercase(),
            metadata,
        })
    }

    fn persist(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
        let directory = self.details_directory()?;
        fs::create_dir_all(&directory).map_err(|_| "version.cache_create_failed")?;
        let key = Self::cache_key(id);
        let primary = directory.join(format!("{key}.json"));
        let backup = directory.join(format!("{key}.json.bak"));
        let temp = directory.join(format!(
            ".{key}.{}.{}.tmp",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|_| "version.cache_write_failed")?;
        if file.write_all(bytes).and_then(|_| file.sync_all()).is_err() {
            let _ = fs::remove_file(&temp);
            return Err("version.cache_write_failed".into());
        }
        drop(file);

        if primary.exists() {
            let _ = fs::remove_file(&backup);
            if fs::rename(&primary, &backup).is_err() {
                let _ = fs::remove_file(&temp);
                return Err("version.cache_replace_failed".into());
            }
        }
        if fs::rename(&temp, &primary).is_err() {
            if backup.exists() && !primary.exists() {
                let _ = fs::rename(&backup, &primary);
            }
            let _ = fs::remove_file(&temp);
            return Err("version.cache_replace_failed".into());
        }
        let _ = File::open(&directory).and_then(|directory| directory.sync_all());
        Ok(())
    }

    /// Resolve the SHA-1-pinned version detail for `id`, returning the raw
    /// verified bytes alongside the parsed metadata.
    ///
    /// The whole operation (catalog snapshot, primary/backup cache lookup,
    /// fetch, and persistence) runs exactly once under the refresh lock. Bounds
    /// and digest/id validation all precede any persistence.
    pub(crate) fn load_verified(&self, id: &str) -> Result<Option<VerifiedVersionDetail>, String> {
        let _refresh = self
            .refresh_lock
            .lock()
            .map_err(|_| "version.cache_lock_failed")?;
        let snapshot = self
            .catalog
            .load_offline()
            .map_err(|_| "version.catalog_cache_unavailable")?;
        let Some(entry) = snapshot.index.get(id) else {
            return Ok(None);
        };
        let expected_sha1 = entry
            .sha1
            .as_deref()
            .filter(|digest| is_sha1(digest))
            .ok_or_else(|| "version.manifest_digest_missing".to_string())?;
        let directory = self.details_directory()?;
        let key = Self::cache_key(id);
        let primary = directory.join(format!("{key}.json"));
        let backup = directory.join(format!("{key}.json.bak"));
        if let Some(verified) = Self::read_cached(&primary, expected_sha1, id)
            .or_else(|| Self::read_cached(&backup, expected_sha1, id))
        {
            return Ok(Some(verified));
        }

        let bytes = self.fetcher.fetch(&entry.url, MAX_VERSION_DETAIL_BYTES)?;
        // The fetcher is a trait object and may ignore `max_bytes`; enforce
        // the bound ourselves before validating or persisting anything.
        if bytes.len() > MAX_VERSION_DETAIL_BYTES {
            return Err("version.metadata_validation_failed".into());
        }
        let verified = Self::verify(bytes, expected_sha1, id)
            .ok_or_else(|| "version.metadata_validation_failed".to_string())?;
        self.persist(id, verified.bytes())?;
        Ok(Some(verified))
    }
}

impl VersionMetadataSource for CachedVersionMetadataSource {
    fn load(&self, id: &str) -> Result<Option<Value>, String> {
        Ok(self
            .load_verified(id)?
            .map(|detail| detail.metadata().clone()))
    }
}

fn is_sha1(digest: &str) -> bool {
    digest.len() == 40 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha1_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shtmcl_core::launch::catalog::{CatalogCache, ManifestIndex, MetadataError};
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicUsize;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Records fetch calls; `bytes` may exceed the requested `max_bytes` to
    /// simulate a fetcher that ignores the bound.
    struct RecordingFetcher {
        responses: BTreeMap<String, Vec<u8>>,
        calls: AtomicUsize,
    }

    impl RecordingFetcher {
        fn new(responses: BTreeMap<String, Vec<u8>>) -> Self {
            Self {
                responses,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl ManifestFetcher for RecordingFetcher {
        fn fetch(&self, url: &str, _max_bytes: usize) -> Result<Vec<u8>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| "fixture URL not found".into())
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "shtmcl-version-details-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            fs::create_dir_all(&root).expect("temp root");
            Self(root)
        }

        fn catalog(&self) -> CatalogCache {
            CatalogCache::new(self.0.join("catalog"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_catalog(temp: &TempDir, id: &str, url: &str, sha1: &str) {
        let manifest = serde_json::json!({
            "versions": [{"id": id, "type": "release", "url": url, "sha1": sha1}]
        });
        let bytes = serde_json::to_vec(&manifest).expect("serialize manifest");
        ManifestIndex::parse(&bytes).expect("fixture manifest valid");
        temp.catalog()
            .store(&bytes)
            .expect("persist fixture catalog");
    }

    fn detail_paths(temp: &TempDir, id: &str) -> (PathBuf, PathBuf) {
        let base = temp.0.join("catalog").join("versions");
        let key = CachedVersionMetadataSource::cache_key(id);
        (
            base.join(format!("{key}.json")),
            base.join(format!("{key}.json.bak")),
        )
    }

    #[test]
    fn detail_is_bound_to_manifest_sha1_and_cached_for_offline_resolution() {
        let temp = TempDir::new();
        let id = "1.20.1";
        let url = "https://piston-meta.mojang.com/v1/packages/pinned/1.20.1.json";
        let detail =
            br#"{"id":"1.20.1","mainClass":"net.minecraft.client.main.Main","libraries":[]}"#;
        write_catalog(&temp, id, url, &sha1_hex(detail));
        let source = CachedVersionMetadataSource::new(
            temp.catalog(),
            Arc::new(RecordingFetcher::new(BTreeMap::from([(
                url.into(),
                detail.to_vec(),
            )]))),
        );

        let resolved = VersionMetadata::resolve(&source, id).expect("resolve detail");
        assert_eq!(resolved.metadata.id, id);
        assert_eq!(
            resolved.metadata.main_class.as_deref(),
            Some("net.minecraft.client.main.Main")
        );
        assert_eq!(resolved.inheritance_chain, vec![id]);

        let offline = CachedVersionMetadataSource::new(
            temp.catalog(),
            Arc::new(RecordingFetcher::new(BTreeMap::new())),
        );
        assert_eq!(
            offline.load(id).expect("offline cached detail"),
            Some(resolved.metadata.raw)
        );
    }

    #[test]
    fn raw_whitespace_and_key_order_preserved_on_fetch_and_offline_read() {
        let temp = TempDir::new();
        let id = "1.20.1";
        let url = "https://piston-meta.mojang.com/v1/packages/pinned/1.20.1.json";
        // Deliberately non-canonical: padded whitespace and reversed key order.
        let detail = b"{\n  \"libraries\" : [],\n  \"mainClass\" : \"net.minecraft.client.main.Main\",\n  \"id\"  : \"1.20.1\"\n}";
        write_catalog(&temp, id, url, &sha1_hex(detail));
        let fetcher = Arc::new(RecordingFetcher::new(BTreeMap::from([(
            url.into(),
            detail.to_vec(),
        )])));
        let source = CachedVersionMetadataSource::new(temp.catalog(), fetcher.clone());

        let fetched = source
            .load_verified(id)
            .expect("load fetched")
            .expect("present");
        assert_eq!(fetched.bytes(), detail);
        assert_eq!(fetched.sha1(), sha1_hex(detail));
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);

        let offline_fetcher = Arc::new(RecordingFetcher::new(BTreeMap::new()));
        let offline = CachedVersionMetadataSource::new(temp.catalog(), offline_fetcher.clone());
        let cached = offline
            .load_verified(id)
            .expect("load offline")
            .expect("present");
        assert_eq!(cached.bytes(), detail);
        assert_eq!(cached.sha1(), sha1_hex(detail));
        assert_eq!(cached.metadata(), fetched.metadata());
        assert_eq!(offline_fetcher.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn corrupt_primary_falls_back_to_valid_backup_without_network() {
        let temp = TempDir::new();
        let id = "1.20.1";
        let url = "https://piston-meta.mojang.com/v1/packages/pinned/1.20.1.json";
        let detail = br#"{"id":"1.20.1","mainClass":"ok","libraries":[]}"#;
        write_catalog(&temp, id, url, &sha1_hex(detail));
        let (primary, backup) = detail_paths(&temp, id);
        fs::create_dir_all(primary.parent().expect("parent")).expect("versions dir");
        fs::write(&primary, b"not-a-valid-detail").expect("corrupt primary");
        fs::write(&backup, detail).expect("valid backup");
        let fetcher = Arc::new(RecordingFetcher::new(BTreeMap::from([(
            url.into(),
            detail.to_vec(),
        )])));
        let source = CachedVersionMetadataSource::new(temp.catalog(), fetcher.clone());

        let verified = source.load_verified(id).expect("load").expect("present");
        assert_eq!(verified.bytes(), detail);
        assert_eq!(verified.sha1(), sha1_hex(detail));
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn oversized_fetch_is_rejected_and_not_persisted() {
        let temp = TempDir::new();
        let id = "1.20.1";
        let url = "https://piston-meta.mojang.com/v1/packages/pinned/1.20.1.json";
        let mut oversized = br#"{"id":"1.20.1","libraries":[],"pad":""#.to_vec();
        oversized.resize(MAX_VERSION_DETAIL_BYTES + 1, b'a');
        oversized.extend_from_slice(b"\"}");
        // Catalog hash matches the oversized body so only the length bound rejects it.
        write_catalog(&temp, id, url, &sha1_hex(&oversized));
        let fetcher = Arc::new(RecordingFetcher::new(BTreeMap::from([(
            url.into(),
            oversized,
        )])));
        let source = CachedVersionMetadataSource::new(temp.catalog(), fetcher.clone());

        assert_eq!(
            source.load_verified(id).unwrap_err(),
            "version.metadata_validation_failed"
        );
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
        assert!(!temp.0.join("catalog/versions").exists());
    }

    #[test]
    fn unknown_id_returns_none_without_fetch() {
        let temp = TempDir::new();
        let url = "https://piston-meta.mojang.com/v1/packages/pinned/1.20.1.json";
        let detail = br#"{"id":"1.20.1","libraries":[]}"#;
        write_catalog(&temp, "1.20.1", url, &sha1_hex(detail));
        let fetcher = Arc::new(RecordingFetcher::new(BTreeMap::from([(
            url.into(),
            detail.to_vec(),
        )])));
        let source = CachedVersionMetadataSource::new(temp.catalog(), fetcher.clone());

        assert!(source
            .load_verified("9.99.9")
            .expect("unknown id")
            .is_none());
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
        assert!(!temp.0.join("catalog/versions").exists());
    }

    #[test]
    fn mismatched_id_despite_matching_hash_is_rejected() {
        let temp = TempDir::new();
        let id = "1.20.1";
        let url = "https://piston-meta.mojang.com/v1/packages/pinned/1.20.1.json";
        // Hash matches catalog, but the embedded document id differs.
        let detail = br#"{"id":"1.20.4","mainClass":"wrong","libraries":[]}"#;
        write_catalog(&temp, id, url, &sha1_hex(detail));
        let fetcher = Arc::new(RecordingFetcher::new(BTreeMap::from([(
            url.into(),
            detail.to_vec(),
        )])));
        let source = CachedVersionMetadataSource::new(temp.catalog(), fetcher.clone());

        assert_eq!(
            source.load_verified(id).unwrap_err(),
            "version.metadata_validation_failed"
        );
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
        assert!(!temp.0.join("catalog/versions").exists());
    }

    #[test]
    fn mismatched_detail_digest_is_never_cached_or_returned() {
        let temp = TempDir::new();
        let id = "1.20.1";
        let url = "https://piston-meta.mojang.com/v1/packages/pinned/1.20.1.json";
        let good = br#"{"id":"1.20.1","mainClass":"ok","libraries":[]}"#;
        write_catalog(&temp, id, url, &sha1_hex(good));
        let source = CachedVersionMetadataSource::new(
            temp.catalog(),
            Arc::new(RecordingFetcher::new(BTreeMap::from([(
                url.into(),
                br#"{"id":"1.20.1","mainClass":"tampered","libraries":[]}"#.to_vec(),
            )]))),
        );

        assert_eq!(
            VersionMetadata::resolve(&source, id).unwrap_err(),
            MetadataError::SourceUnavailable {
                id: id.into(),
                code: "version.metadata_validation_failed".into(),
            }
        );
        assert!(!temp.0.join("catalog/versions").exists());
    }
}

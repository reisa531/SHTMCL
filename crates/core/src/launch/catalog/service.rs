use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::{CatalogCache, CatalogCacheError, ManifestEntry, ManifestIndex, SnapshotSource};

pub const OFFICIAL_MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
pub const MAX_CATALOG_PAGE_SIZE: usize = 200;
const MAX_OFFSET: usize = 100_000;

pub trait ManifestFetcher: Send + Sync {
    fn fetch(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, String>;
}

/// Runtime catalog operations shared by command handlers and tests. Network
/// access remains an injected application adapter; only validated manifests
/// can replace the recoverable portable cache.
pub struct VersionCatalog {
    cache: CatalogCache,
    fetcher: Arc<dyn ManifestFetcher>,
    refresh_lock: Mutex<()>,
}

impl VersionCatalog {
    pub fn new(
        cache_directory: impl Into<std::path::PathBuf>,
        fetcher: Arc<dyn ManifestFetcher>,
    ) -> Self {
        Self {
            cache: CatalogCache::new(cache_directory),
            fetcher,
            refresh_lock: Mutex::new(()),
        }
    }

    pub fn refresh(&self) -> Result<Value, VersionCatalogError> {
        let _refresh = self
            .refresh_lock
            .try_lock()
            .map_err(|_| VersionCatalogError::Busy)?;
        let bytes = self
            .fetcher
            .fetch(OFFICIAL_MANIFEST_URL, super::manifest::MAX_MANIFEST_BYTES)
            .map_err(VersionCatalogError::Fetch)?;
        let snapshot = self
            .cache
            .store(&bytes)
            .map_err(VersionCatalogError::Cache)?;
        Ok(snapshot_summary(&snapshot.index, snapshot.source))
    }

    pub fn list(
        &self,
        kind_filter: Option<&str>,
        query: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Value, VersionCatalogError> {
        if limit == 0 || limit > MAX_CATALOG_PAGE_SIZE || offset > MAX_OFFSET {
            return Err(VersionCatalogError::InvalidPage);
        }
        let snapshot = self
            .cache
            .load_offline()
            .map_err(VersionCatalogError::Cache)?;
        let index = snapshot.index;
        let normalized_query = query.map(str::to_lowercase);
        let entries = index
            .entries
            .iter()
            .filter(|entry| kind_filter.is_none_or(|kind| entry.kind.as_str() == kind))
            .filter(|entry| {
                normalized_query
                    .as_ref()
                    .is_none_or(|query| entry.id.to_lowercase().contains(query))
            })
            .collect::<Vec<_>>();
        let total = entries.len();
        let page = entries
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(entry_json)
            .collect::<Vec<_>>();

        Ok(json!({
            "latestRelease": index.latest_release,
            "latestSnapshot": index.latest_snapshot,
            "counts": index.counts(),
            "source": snapshot_source(snapshot.source),
            "offset": offset,
            "limit": limit,
            "total": total,
            "entries": page,
        }))
    }
}

#[derive(Debug)]
pub enum VersionCatalogError {
    Fetch(String),
    Cache(CatalogCacheError),
    InvalidPage,
    Busy,
}

impl VersionCatalogError {
    pub fn detail_ref(&self) -> String {
        match self {
            Self::Fetch(_) => "catalog:fetch_failed".into(),
            Self::Cache(CatalogCacheError::NoUsableCache) => "catalog:cache_missing".into(),
            Self::Cache(CatalogCacheError::InvalidCache(_)) => "catalog:cache_invalid".into(),
            Self::Cache(CatalogCacheError::Io(_)) => "catalog:cache_io_failed".into(),
            Self::InvalidPage => "catalog:page_invalid".into(),
            Self::Busy => "catalog:refresh_in_progress".into(),
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Fetch(_) | Self::Cache(CatalogCacheError::Io(_)) | Self::Busy
        )
    }
}

fn snapshot_summary(index: &ManifestIndex, source: SnapshotSource) -> Value {
    json!({
        "latestRelease": index.latest_release,
        "latestSnapshot": index.latest_snapshot,
        "counts": index.counts(),
        "source": snapshot_source(source),
    })
}

fn snapshot_source(source: SnapshotSource) -> &'static str {
    match source {
        SnapshotSource::Fresh => "fresh",
        SnapshotSource::OfflineCache => "offline_cache",
        SnapshotSource::RecoveredBackup => "recovered_backup",
    }
}

fn entry_json(entry: &ManifestEntry) -> Value {
    json!({
        "id": entry.id,
        "type": entry.kind.as_str(),
        "preRelease": entry.kind.is_prerelease(&entry.id),
        "releaseTime": entry.release_time,
        "complianceLevel": entry.compliance_level,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct FixtureFetcher(Vec<u8>);

    impl ManifestFetcher for FixtureFetcher {
        fn fetch(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
            assert_eq!(url, OFFICIAL_MANIFEST_URL);
            assert!(self.0.len() <= max_bytes);
            Ok(self.0.clone())
        }
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "shtmcl-version-catalog-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    const MANIFEST: &[u8] = br#"{"latest":{"release":"1.20","snapshot":"24w14a"},"versions":[
      {"id":"1.20","type":"release","url":"https://example.invalid/release.json"},
      {"id":"24w14a","type":"snapshot","url":"https://example.invalid/snapshot.json"},
      {"id":"1.20-pre1","type":"snapshot","url":"https://example.invalid/pre.json"},
      {"id":"c0.0.11a","type":"old_alpha","url":"https://example.invalid/alpha.json"},
      {"id":"b1.0","type":"old_beta","url":"https://example.invalid/beta.json"}] }"#;

    #[test]
    fn refresh_caches_and_lists_all_version_families_with_bounded_pages() {
        let root = temp_dir();
        let catalog = VersionCatalog::new(&root, Arc::new(FixtureFetcher(MANIFEST.to_vec())));
        let refreshed = catalog.refresh().expect("manifest refresh");
        assert_eq!(refreshed["source"], "fresh");

        let first = catalog.list(None, None, 0, 3).expect("first page");
        assert_eq!(first["total"], 5);
        assert_eq!(first["entries"].as_array().expect("entries").len(), 3);
        assert_eq!(first["counts"]["old_alpha"], 1);
        assert_eq!(first["counts"]["old_beta"], 1);
        assert_eq!(first["entries"][2]["preRelease"], true);

        let second = catalog.list(Some("old_beta"), None, 0, 10).expect("filter");
        assert_eq!(second["total"], 1);
        assert_eq!(second["entries"][0]["id"], "b1.0");
        assert!(matches!(
            catalog.list(None, None, 0, MAX_CATALOG_PAGE_SIZE + 1),
            Err(VersionCatalogError::InvalidPage)
        ));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn invalid_refresh_does_not_destroy_offline_catalog() {
        let root = temp_dir();
        let good = VersionCatalog::new(&root, Arc::new(FixtureFetcher(MANIFEST.to_vec())));
        good.refresh().expect("initial cache");

        let bad = VersionCatalog::new(&root, Arc::new(FixtureFetcher(b"not-json".to_vec())));
        assert!(bad.refresh().is_err());
        let page = good
            .list(None, None, 0, 10)
            .expect("good prior cache remains");
        assert_eq!(page["total"], 5);
        fs::remove_dir_all(root).expect("cleanup");
    }
}

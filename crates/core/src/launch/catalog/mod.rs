//! Official Minecraft version catalog, recoverable cache and metadata resolver.
//!
//! The catalog deliberately accepts unknown version types and metadata fields:
//! Mojang can add entries without forcing an application update. Network access
//! is supplied by the caller; this module owns validation, caching and merging.

mod cache;
mod manifest;
mod metadata;
mod service;

pub use cache::{CatalogCache, CatalogCacheError, CatalogSnapshot, SnapshotSource};
pub use manifest::{ManifestEntry, ManifestError, ManifestIndex, VersionKind};
pub use metadata::{
    evaluate_rules, LibraryRuleDecision, MetadataError, ResolvedVersion, VersionMetadata,
    VersionMetadataSource, MAX_INHERITANCE_DEPTH,
};
pub use service::{
    ManifestFetcher, VersionCatalog, VersionCatalogError, MAX_CATALOG_PAGE_SIZE,
    OFFICIAL_MANIFEST_URL,
};

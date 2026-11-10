use std::collections::BTreeMap;

use serde_json::Value;

pub const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_MANIFEST_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionKind {
    Release,
    Snapshot,
    OldAlpha,
    OldBeta,
    Unknown(String),
}

impl VersionKind {
    pub fn parse(value: &str) -> Self {
        match value {
            "release" => Self::Release,
            "snapshot" => Self::Snapshot,
            "old_alpha" => Self::OldAlpha,
            "old_beta" => Self::OldBeta,
            other => Self::Unknown(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Release => "release",
            Self::Snapshot => "snapshot",
            Self::OldAlpha => "old_alpha",
            Self::OldBeta => "old_beta",
            Self::Unknown(value) => value,
        }
    }

    /// Mojang marks pre-releases and release candidates as snapshot entries.
    /// Keep the source type intact while exposing a display grouping.
    pub fn is_prerelease(&self, id: &str) -> bool {
        matches!(self, Self::Snapshot) && (id.contains("-pre") || id.contains("-rc"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub id: String,
    pub kind: VersionKind,
    pub url: String,
    pub sha1: Option<String>,
    pub release_time: Option<String>,
    pub compliance_level: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestIndex {
    pub latest_release: Option<String>,
    pub latest_snapshot: Option<String>,
    pub entries: Vec<ManifestEntry>,
    by_id: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    TooLarge,
    InvalidJson,
    MissingVersions,
    TooManyEntries,
    InvalidEntry(usize),
    DuplicateId(String),
}

impl ManifestIndex {
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge);
        }
        let root: Value = serde_json::from_slice(bytes).map_err(|_| ManifestError::InvalidJson)?;
        let versions = root
            .get("versions")
            .and_then(Value::as_array)
            .ok_or(ManifestError::MissingVersions)?;
        if versions.len() > MAX_MANIFEST_ENTRIES {
            return Err(ManifestError::TooManyEntries);
        }

        let latest_release = root
            .pointer("/latest/release")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let latest_snapshot = root
            .pointer("/latest/snapshot")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut entries = Vec::with_capacity(versions.len());
        let mut by_id = BTreeMap::new();
        for (index, item) in versions.iter().enumerate() {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or(ManifestError::InvalidEntry(index))?;
            let kind = item
                .get("type")
                .and_then(Value::as_str)
                .ok_or(ManifestError::InvalidEntry(index))?;
            let url = item
                .get("url")
                .and_then(Value::as_str)
                .filter(|value| value.starts_with("https://"))
                .ok_or(ManifestError::InvalidEntry(index))?;
            let sha1 = item.get("sha1").and_then(Value::as_str).map(str::to_owned);
            if sha1.as_deref().is_some_and(|value| {
                value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                return Err(ManifestError::InvalidEntry(index));
            }
            let entry = ManifestEntry {
                id: id.to_owned(),
                kind: VersionKind::parse(kind),
                url: url.to_owned(),
                sha1,
                release_time: item
                    .get("releaseTime")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                compliance_level: item
                    .get("complianceLevel")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok()),
            };
            if by_id.insert(entry.id.clone(), entries.len()).is_some() {
                return Err(ManifestError::DuplicateId(entry.id));
            }
            entries.push(entry);
        }
        Ok(Self {
            latest_release,
            latest_snapshot,
            entries,
            by_id,
        })
    }

    pub fn get(&self, id: &str) -> Option<&ManifestEntry> {
        self.by_id.get(id).map(|index| &self.entries[*index])
    }

    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::from([
            ("release", 0),
            ("snapshot", 0),
            ("old_alpha", 0),
            ("old_beta", 0),
            ("unknown", 0),
        ]);
        for entry in &self.entries {
            let key = match entry.kind {
                VersionKind::Release => "release",
                VersionKind::Snapshot => "snapshot",
                VersionKind::OldAlpha => "old_alpha",
                VersionKind::OldBeta => "old_beta",
                VersionKind::Unknown(_) => "unknown",
            };
            *counts.get_mut(key).expect("count keys are fixed") += 1;
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_official_types_and_keeps_unknown_types() {
        let data = br#"{"latest":{"release":"1.20","snapshot":"23w01a"},"versions":[
          {"id":"1.20","type":"release","url":"https://a"},
          {"id":"23w01a","type":"snapshot","url":"https://b"},
          {"id":"c0.0.11a","type":"old_alpha","url":"https://c"},
          {"id":"b1.0","type":"old_beta","url":"https://d"},
          {"id":"future","type":"future_kind","url":"https://e"}] }"#;
        let index = ManifestIndex::parse(data).unwrap();
        assert_eq!(index.entries.len(), 5);
        assert_eq!(index.counts()["old_alpha"], 1);
        assert_eq!(index.counts()["old_beta"], 1);
        assert_eq!(
            index.get("future").unwrap().kind,
            VersionKind::Unknown("future_kind".into())
        );
    }

    #[test]
    fn duplicate_ids_and_malformed_entries_fail_closed() {
        let duplicate = br#"{"versions":[{"id":"x","type":"release","url":"https://example.invalid/u"},{"id":"x","type":"snapshot","url":"https://example.invalid/v"}]}"#;
        assert_eq!(
            ManifestIndex::parse(duplicate),
            Err(ManifestError::DuplicateId("x".into()))
        );
        assert_eq!(
            ManifestIndex::parse(br#"{"versions":[{"id":"x"}]}"#),
            Err(ManifestError::InvalidEntry(0))
        );
        assert_eq!(
            ManifestIndex::parse(
                br#"{"versions":[{"id":"x","type":"release","url":"http://unsafe"}]}"#
            ),
            Err(ManifestError::InvalidEntry(0))
        );
    }

    #[test]
    fn pre_release_group_is_derived_without_changing_source_type() {
        assert!(VersionKind::Snapshot.is_prerelease("1.20-pre1"));
        assert!(VersionKind::Snapshot.is_prerelease("1.20-rc2"));
        assert!(!VersionKind::Snapshot.is_prerelease("24w01a"));
        assert!(!VersionKind::Release.is_prerelease("1.20"));
    }
}

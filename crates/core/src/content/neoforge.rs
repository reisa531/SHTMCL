//! Bounded NeoForge Maven catalog parsing and installer artifact identities.
//!
//! This module does not download or execute installers. It gives the application
//! adapter a validated game-version mapping and immutable artifact URLs.

use std::collections::BTreeSet;

const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_VERSIONS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeoForgeRelease {
    pub game_version: String,
    pub loader_version: String,
    pub artifact_id: &'static str,
    pub repository_path: String,
    pub stable: bool,
}

impl NeoForgeRelease {
    pub fn installer_urls(&self) -> [String; 2] {
        let filename = format!("{}-{}-installer.jar", self.artifact_id, self.loader_version);
        [
            format!(
                "https://maven.neoforged.net/releases/{}/{}/{}",
                self.repository_path, self.loader_version, filename
            ),
            format!(
                "https://bmclapi2.bangbang93.com/maven/{}/{}/{}",
                self.repository_path, self.loader_version, filename
            ),
        ]
    }
}

pub fn catalog_urls(game_version: &str) -> Result<[String; 2], &'static str> {
    let (_, repository_path, _) = catalog_identity(game_version)?;
    Ok([
        format!("https://maven.neoforged.net/releases/{repository_path}/maven-metadata.xml"),
        format!("https://bmclapi2.bangbang93.com/maven/{repository_path}/maven-metadata.xml"),
    ])
}

/// Selects the Maven artifact and version prefix for one Minecraft release.
/// NeoForge's 1.20.1 compatibility artifact is kept distinct from current
/// NeoForge releases; later versions use the NeoForged versioning scheme.
pub fn catalog_identity(
    game_version: &str,
) -> Result<(&'static str, &'static str, String), &'static str> {
    let parts = parse_numeric_game_version(game_version)?;
    if game_version == "1.20.1" {
        return Ok(("forge", "net/neoforge/forge", "1.20.1-".to_string()));
    }

    let (major, minor) = match parts.as_slice() {
        [1, minor] => (*minor, 0),
        [1, minor, patch] => (*minor, *patch),
        [major, minor] => (*major, *minor),
        [major, minor, _patch] => (*major, *minor),
        _ => return Err("neoforge_game_version_unsupported"),
    };
    Ok((
        "neoforge",
        "net/neoforged/neoforge",
        format!("{major}.{minor}."),
    ))
}

/// Parses Maven metadata without resolving entities or allocating unbounded
/// strings. Only simple version elements are accepted because artifact versions
/// must be safe path components.
pub fn parse_maven_versions(
    game_version: &str,
    xml: &[u8],
) -> Result<Vec<NeoForgeRelease>, &'static str> {
    let (artifact_id, repository_path, prefix) = catalog_identity(game_version)?;
    if xml.len() > MAX_METADATA_BYTES {
        return Err("neoforge_metadata_too_large");
    }
    let source = std::str::from_utf8(xml).map_err(|_| "neoforge_metadata_encoding")?;
    if !source.contains("<metadata") || !source.contains("<versioning>") {
        return Err("neoforge_metadata_invalid");
    }

    let mut seen = BTreeSet::new();
    let mut releases = Vec::new();
    for tail in source.split("<version>").skip(1) {
        let (candidate, _) = tail
            .split_once("</version>")
            .ok_or("neoforge_metadata_invalid")?;
        let version = candidate.trim();
        if !version.starts_with(&prefix) {
            continue;
        }
        if !valid_artifact_version(version) {
            return Err("neoforge_version_invalid");
        }
        if !seen.insert(version.to_string()) {
            continue;
        }
        if releases.len() == MAX_VERSIONS {
            return Err("neoforge_version_count_exceeded");
        }
        releases.push(NeoForgeRelease {
            game_version: game_version.to_string(),
            loader_version: version.to_string(),
            artifact_id,
            repository_path: repository_path.to_string(),
            stable: !is_prerelease(version),
        });
    }
    if releases.is_empty() {
        return Err("neoforge_version_unavailable");
    }
    releases.sort_by(|left, right| compare_versions(&left.loader_version, &right.loader_version));
    releases.reverse();
    Ok(releases)
}

fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    fn split(value: &str) -> (Vec<u64>, Option<&str>) {
        let value = value.strip_prefix("1.20.1-").unwrap_or(value);
        let (numeric, suffix) = value
            .split_once('-')
            .map_or((value, None), |(numeric, suffix)| (numeric, Some(suffix)));
        let parts = numeric
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect();
        (parts, suffix)
    }
    let (left_parts, left_suffix) = split(left);
    let (right_parts, right_suffix) = split(right);
    let width = left_parts.len().max(right_parts.len());
    for index in 0..width {
        let ordering = left_parts
            .get(index)
            .copied()
            .unwrap_or(0)
            .cmp(&right_parts.get(index).copied().unwrap_or(0));
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    match (left_suffix, right_suffix) {
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(left), Some(right)) => left.cmp(right),
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn parse_numeric_game_version(value: &str) -> Result<Vec<u64>, &'static str> {
    if value.is_empty()
        || value.len() > 32
        || !value.bytes().all(|c| c.is_ascii_digit() || c == b'.')
    {
        return Err("neoforge_game_version_invalid");
    }
    let parts = value
        .split('.')
        .map(|part| {
            if part.is_empty() || part.len() > 6 || (part.len() > 1 && part.starts_with('0')) {
                return Err("neoforge_game_version_invalid");
            }
            part.parse::<u64>()
                .map_err(|_| "neoforge_game_version_invalid")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !matches!(parts.len(), 2 | 3) || parts.iter().any(|part| *part > 999_999) {
        return Err("neoforge_game_version_invalid");
    }
    Ok(parts)
}

fn valid_artifact_version(value: &str) -> bool {
    let (numeric, suffix) = value
        .split_once('-')
        .map_or((value, None), |(numeric, suffix)| (numeric, Some(suffix)));
    let numeric_components = numeric.split('.');
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('.')
        && !value.ends_with('.')
        && !value.contains("..")
        && numeric_components
            .clone()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && suffix.is_none_or(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
        })
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c))
}

fn is_prerelease(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    ["alpha", "beta", "rc", "snapshot", "pre"]
        .iter()
        .any(|tag| lower.contains(tag))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(values: &[&str]) -> Vec<u8> {
        format!(
            "<metadata><versioning><versions>{}</versions></versioning></metadata>",
            values
                .iter()
                .map(|value| format!("<version>{value}</version>"))
                .collect::<String>()
        )
        .into_bytes()
    }

    #[test]
    fn maps_supported_game_lines_to_official_maven_artifacts() {
        assert_eq!(
            catalog_identity("1.20.1").unwrap(),
            ("forge", "net/neoforge/forge", "1.20.1-".to_string())
        );
        assert_eq!(catalog_identity("1.20.4").unwrap().2, "20.4.");
        assert_eq!(catalog_identity("1.21").unwrap().2, "21.0.");
        assert_eq!(catalog_identity("1.21.1").unwrap().2, "21.1.");
        assert_eq!(catalog_identity("26.1").unwrap().2, "26.1.");
        assert_eq!(catalog_identity("26.1.1").unwrap().2, "26.1.");
        assert_eq!(
            catalog_urls("1.20.4").unwrap()[0],
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml"
        );
        for invalid in ["", "snapshot", "1.21.1-pre", "1..1", "1.21.1/../x"] {
            assert!(catalog_identity(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn filters_versions_marks_prereleases_and_builds_fallback_urls() {
        let xml = metadata(&["20.4.230", "20.4.231-beta", "20.5.1", "20.4.231-beta"]);
        let releases = parse_maven_versions("1.20.4", &xml).unwrap();
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].loader_version, "20.4.231-beta");
        assert!(!releases[0].stable);
        assert_eq!(releases[1].loader_version, "20.4.230");
        assert!(releases[1].stable);
        assert_eq!(
            releases[1].installer_urls(),
            [
                "https://maven.neoforged.net/releases/net/neoforged/neoforge/20.4.230/neoforge-20.4.230-installer.jar",
                "https://bmclapi2.bangbang93.com/maven/net/neoforged/neoforge/20.4.230/neoforge-20.4.230-installer.jar"
            ]
        );
        let legacy = parse_maven_versions("1.20.1", &metadata(&["1.20.1-47.1.0"])).unwrap();
        assert_eq!(
            legacy[0].installer_urls()[0],
            "https://maven.neoforged.net/releases/net/neoforge/forge/1.20.1-47.1.0/forge-1.20.1-47.1.0-installer.jar"
        );
        let legacy =
            parse_maven_versions("1.20.1", &metadata(&["1.20.1-47.9.0", "1.20.1-47.10.0"]))
                .unwrap();
        assert_eq!(legacy[0].loader_version, "1.20.1-47.10.0");
    }

    #[test]
    fn rejects_unsafe_or_unbounded_metadata() {
        assert_eq!(
            parse_maven_versions("1.20.4", b"<metadata><versioning><versions><version>20.4.0/evil</version></versions></versioning></metadata>"),
            Err("neoforge_version_invalid")
        );
        assert_eq!(
            parse_maven_versions("1.20.4", &vec![b'a'; MAX_METADATA_BYTES + 1]),
            Err("neoforge_metadata_too_large")
        );
        assert_eq!(
            parse_maven_versions("1.20.4", &metadata(&["20.5.1"])),
            Err("neoforge_version_unavailable")
        );
    }
}

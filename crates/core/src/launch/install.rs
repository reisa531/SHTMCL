//! Pure, bounded vanilla installation planning. Transfers remain owned by 03.
use super::catalog::{evaluate_rules, LibraryRuleDecision, VersionMetadata};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallArtifact {
    pub path: String,
    pub url: String,
    pub sha1: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct VanillaInstallPlan {
    pub artifacts: Vec<InstallArtifact>,
    pub native_archives: Vec<String>,
    pub asset_index_path: Option<String>,
}

pub fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && !path.starts_with('/')
        && !path.contains(['\\', ':', '\0'])
        && path.split('/').all(|part| {
            let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
            !part.is_empty()
                && !matches!(
                    stem.as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                )
                && part != "."
                && part != ".."
                && !part.ends_with(['.', ' '])
                && !part
                    .chars()
                    .any(|c| c.is_control() || "<>\"|?*".contains(c))
        })
}

fn artifact(value: &Value, path: String) -> Result<InstallArtifact, String> {
    if !safe_path(&path) {
        return Err("unsafe artifact path".into());
    }
    let url = value["url"].as_str().ok_or("missing artifact URL")?;
    let parsed = reqwest::Url::parse(url).map_err(|_| "invalid artifact URL")?;
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || parsed.port_or_known_default() != Some(443)
        || !matches!(
            parsed.host_str(),
            Some(
                "piston-data.mojang.com"
                    | "launcher.mojang.com"
                    | "libraries.minecraft.net"
                    | "piston-meta.mojang.com"
                    | "launchermeta.mojang.com"
                    | "resources.download.minecraft.net"
                    | "maven.fabricmc.net"
            )
        )
    {
        return Err("unsupported artifact origin".into());
    }
    let sha1 = value["sha1"].as_str().ok_or("missing artifact SHA-1")?;
    if sha1.len() != 40 || !sha1.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid artifact SHA-1".into());
    }
    let size = value["size"]
        .as_u64()
        .filter(|s| *s <= 2 * 1024 * 1024 * 1024)
        .ok_or("invalid artifact size")?;
    Ok(InstallArtifact {
        path,
        url: url.into(),
        sha1: sha1.to_ascii_lowercase(),
        size,
    })
}

impl VanillaInstallPlan {
    pub fn parse(metadata: &VersionMetadata, os: &str, arch: &str) -> Result<Self, String> {
        if !safe_path(&metadata.id) || metadata.id.contains('/') {
            return Err("unsafe version id".into());
        }
        let mut plan = Self {
            artifacts: vec![artifact(
                &metadata.raw["downloads"]["client"],
                format!("versions/{0}/{0}.jar", metadata.id),
            )?],
            native_archives: vec![],
            asset_index_path: None,
        };
        if let Some(index) = &metadata.asset_index {
            let id = index["id"]
                .as_str()
                .filter(|id| safe_path(id) && !id.contains('/'))
                .ok_or("invalid asset index id")?;
            let path = format!("assets/indexes/{id}.json");
            plan.artifacts.push(artifact(index, path.clone())?);
            plan.asset_index_path = Some(path);
        }
        if let Some(logging) = metadata.raw.pointer("/logging/client/file") {
            let id = logging["id"]
                .as_str()
                .filter(|id| safe_path(id) && !id.contains('/'))
                .ok_or("invalid logging file id")?;
            plan.artifacts
                .push(artifact(logging, format!("assets/log_configs/{id}"))?);
        }
        for library in &metadata.libraries {
            match evaluate_rules(library.get("rules"), os, arch, &BTreeMap::new()) {
                LibraryRuleDecision::Excluded => continue,
                LibraryRuleDecision::Undetermined(reason) => {
                    return Err(format!("unsupported library rule: {reason}"))
                }
                LibraryRuleDecision::Included => {}
            }
            if let Some(download) = library.pointer("/downloads/artifact") {
                let path = download["path"].as_str().ok_or("library path missing")?;
                plan.artifacts
                    .push(artifact(download, format!("libraries/{path}"))?);
            } else if library.get("natives").is_none() {
                return Err(
                    "library metadata lacks pinned download; legacy resolver required".into(),
                );
            }
            if let Some(classifier) = library
                .get("natives")
                .and_then(|n| n.get(os))
                .and_then(Value::as_str)
            {
                let classifier =
                    classifier.replace("${arch}", if arch == "x86" { "32" } else { "64" });
                let download = library
                    .get("downloads")
                    .and_then(|d| d.get("classifiers"))
                    .and_then(|c| c.get(&classifier))
                    .ok_or("native classifier missing")?;
                let path = format!(
                    "libraries/{}",
                    download["path"].as_str().ok_or("native path missing")?
                );
                plan.artifacts.push(artifact(download, path.clone())?);
                plan.native_archives.push(path);
            }
        }
        // Same destinations must always refer to the same pinned resource.
        let mut paths = BTreeMap::new();
        for file in &plan.artifacts {
            if let Some(previous) = paths.insert(file.path.clone(), file.clone()) {
                if previous != *file {
                    return Err("conflicting artifact destination".into());
                }
            }
        }
        plan.artifacts = paths.into_values().collect();
        if plan.artifacts.len() > 10_000
            || plan.artifacts.iter().map(|a| a.size).sum::<u64>() > 8 * 1024 * 1024 * 1024
        {
            return Err("installation plan budget exceeded".into());
        }
        Ok(plan)
    }
}

pub fn asset_artifacts(bytes: &[u8]) -> Result<Vec<InstallArtifact>, String> {
    if bytes.len() > 32 * 1024 * 1024 {
        return Err("asset index exceeds limit".into());
    }
    let root: Value = serde_json::from_slice(bytes).map_err(|_| "invalid asset index")?;
    let objects = root["objects"]
        .as_object()
        .filter(|o| o.len() <= 100_000)
        .ok_or("invalid asset objects")?;
    let mut seen = BTreeMap::new();
    let mut total = 0u64;
    let mut files = vec![];
    for (name, object) in objects {
        if !safe_path(name) {
            return Err("unsafe asset name".into());
        }
        let hash = object["hash"].as_str().ok_or("asset hash missing")?;
        if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid asset hash".into());
        }
        let hash = hash.to_ascii_lowercase();
        let size = object["size"].as_u64().ok_or("asset size missing")?;
        if let Some(previous) = seen.insert(hash.clone(), size) {
            if previous != size {
                return Err("conflicting asset size".into());
            }
            continue;
        }
        total = total
            .checked_add(size)
            .filter(|total| *total <= 16 * 1024 * 1024 * 1024)
            .ok_or("asset budget exceeded")?;
        let location = format!("{}/{}", &hash[..2], hash);
        files.push(artifact(&serde_json::json!({"url":format!("https://resources.download.minecraft.net/{location}"),"sha1":hash,"size":object["size"]}), format!("assets/objects/{location}"))?);
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_traversal_and_unsafe_windows_paths() {
        for path in ["../x", "a/../x", "C:/x", "a\\x", "a/", "a/x.", "/x"] {
            assert!(!safe_path(path), "{path}");
        }
        assert!(safe_path("assets/objects/ab/abcdef"));
    }
    #[test]
    fn asset_objects_are_deduplicated() {
        let hash = "a".repeat(40);
        let bytes = serde_json::to_vec(&serde_json::json!({"objects":{"a":{"hash":hash,"size":10},"b":{"hash":hash,"size":10}}})).unwrap();
        assert_eq!(asset_artifacts(&bytes).unwrap().len(), 1);
    }
    #[test]
    fn rejects_bad_asset_hash() {
        assert!(asset_artifacts(br#"{"objects":{"x":{"hash":"bad","size":1}}}"#).is_err());
    }

    #[test]
    fn plans_client_libraries_index_and_windows_natives() {
        let file = serde_json::json!({"url":"https://libraries.minecraft.net/g/a.jar","sha1":"a".repeat(40),"size":10,"path":"g/a.jar"});
        let metadata=VersionMetadata::from_value(serde_json::json!({"id":"1.7.10","downloads":{"client":file},"assetIndex":{"id":"legacy","url":"https://piston-meta.mojang.com/index.json","sha1":"b".repeat(40),"size":20},"libraries":[{"downloads":{"artifact":file,"classifiers":{"natives-windows-64":file}},"natives":{"windows":"natives-windows-${arch}"}},{"rules":[{"action":"allow","os":{"name":"osx","version":"^10"}}]}]}),None).unwrap();
        let plan = VanillaInstallPlan::parse(&metadata, "windows", "amd64").unwrap();
        assert_eq!(plan.artifacts.len(), 3);
        assert_eq!(plan.native_archives, vec!["libraries/g/a.jar"]);
        assert_eq!(
            plan.asset_index_path.as_deref(),
            Some("assets/indexes/legacy.json")
        );
    }

    #[test]
    fn rejects_conflicting_sizes_for_shared_asset_hash() {
        let hash = "a".repeat(40);
        let bytes=serde_json::to_vec(&serde_json::json!({"objects":{"a":{"hash":hash,"size":10},"b":{"hash":hash,"size":20}}})).unwrap();
        assert!(asset_artifacts(&bytes).is_err());
    }

    #[test]
    fn official_logging_configuration_is_pinned_and_path_checked() {
        let file = serde_json::json!({"url":"https://piston-data.mojang.com/object/config.xml","sha1":"a".repeat(40),"size":966,"id":"client-1.7.xml"});
        let mut raw = serde_json::json!({"id":"1.7.10","downloads":{"client":file},"logging":{"client":{"file":file}}});
        let metadata = VersionMetadata::from_value(raw.clone(), None).unwrap();
        let plan = VanillaInstallPlan::parse(&metadata, "windows", "amd64").unwrap();
        assert!(plan
            .artifacts
            .iter()
            .any(|a| a.path == "assets/log_configs/client-1.7.xml" && a.size == 966));
        raw["logging"]["client"]["file"]["id"] = serde_json::json!("../config.xml");
        let metadata = VersionMetadata::from_value(raw, None).unwrap();
        assert!(VanillaInstallPlan::parse(&metadata, "windows", "amd64").is_err());
    }
}

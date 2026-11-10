//! Pure, deterministic NeoForge runtime planning for the piloted 1.20.4 /
//! loader 20.4.253 combination. This module performs no IO, no network work and
//! never invents download URLs. Generated artifacts and the universal jar are
//! recorded as install obligations only; FML discovers them on disk.

use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

use super::neoforge_profile::{
    maven_path, InstallerData, InstallerLibrary, NeoForgeInstallerProfile,
};

const PILOT_GAME: &str = "1.20.4";
const PILOT_LOADER: &str = "20.4.253";
const PILOT_CHILD_ID: &str = "neoforge-20.4.253";
const BASE_ID: &str = "1.20.4";
const NEOFORGE_MAIN_CLASS: &str = "cpw.mods.bootstraplauncher.BootstrapLauncher";
const MAX_CHILD_LIBRARIES: usize = 256;
const MAX_BASE_LIBRARIES: usize = 1024;
const MAX_ARGUMENTS: usize = 512;
const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_STRING: usize = 8192;
const LOADER_KIND: &str = "neoforge";
const LOADER_VERSION: &str = "20.4.253";
const UNIVERSAL_COORDINATE: &str = "net.neoforged:neoforge:20.4.253:universal";
const NEOFORGE_PREFIX: &str = "https://maven.neoforged.net/releases/";
const MC_PREFIX: &str = "https://libraries.minecraft.net/";

/// Result of planning the merged runtime metadata for one instance.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimePlan {
    pub metadata: Value,
    pub downloads: Vec<InstallerLibrary>,
    pub generated_paths: Vec<String>,
}

/// Plans the merged NeoForge runtime. Inputs must already be identity-verified
/// by the caller; this still re-checks the pilot pins because it is cheap.
pub fn plan_runtime(
    profile: &NeoForgeInstallerProfile,
    version: &Value,
    base: &Value,
) -> Result<RuntimePlan, String> {
    validate_pilot_profile(profile)?;
    let child = validate_child_metadata(version)?;
    validate_base_metadata(base)?;
    validate_profile_pins(profile)?;

    let base_object = base.as_object().ok_or("neoforge_base_invalid")?;
    let mut merged = base_object.clone();
    merged.insert(
        "mainClass".into(),
        Value::String(NEOFORGE_MAIN_CLASS.into()),
    );
    merged.remove("inheritsFrom");

    let merged_libraries = merge_libraries(base, &child)?;
    merged.insert("libraries".into(), merged_libraries);

    let merged_arguments = merge_arguments(base, &child)?;
    if let Some(arguments) = merged_arguments {
        merged.insert("arguments".into(), arguments);
    } else {
        merged.remove("arguments");
    }

    let mut loader = Map::new();
    loader.insert("kind".into(), Value::String(LOADER_KIND.into()));
    loader.insert("version".into(), Value::String(LOADER_VERSION.into()));
    merged.insert("shtmclLoader".into(), Value::Object(loader));

    let downloads = collect_child_downloads(&child, profile)?;
    let generated_paths = generated_paths();

    Ok(RuntimePlan {
        metadata: Value::Object(merged),
        downloads,
        generated_paths,
    })
}

fn validate_pilot_profile(profile: &NeoForgeInstallerProfile) -> Result<(), String> {
    if profile.game_version != PILOT_GAME || profile.loader_version != PILOT_LOADER {
        return Err("neoforge_runtime_profile_version_unsupported".into());
    }
    Ok(())
}

fn validate_child_metadata(version: &Value) -> Result<Value, String> {
    let object = version.as_object().ok_or("neoforge_child_invalid")?;
    if object.get("id").and_then(Value::as_str) != Some(PILOT_CHILD_ID) {
        return Err("neoforge_child_id".into());
    }
    if object.get("inheritsFrom").and_then(Value::as_str) != Some(BASE_ID) {
        return Err("neoforge_child_inherits".into());
    }
    if object.get("mainClass").and_then(Value::as_str) != Some(NEOFORGE_MAIN_CLASS) {
        return Err("neoforge_child_main_class".into());
    }
    let libraries = object
        .get("libraries")
        .and_then(Value::as_array)
        .ok_or("neoforge_child_libraries")?;
    if libraries.is_empty() || libraries.len() > MAX_CHILD_LIBRARIES {
        return Err("neoforge_child_libraries".into());
    }
    let arguments = object
        .get("arguments")
        .and_then(Value::as_object)
        .ok_or("neoforge_arguments")?;
    for key in ["game", "jvm"] {
        let group = arguments
            .get(key)
            .and_then(Value::as_array)
            .ok_or("neoforge_arguments")?;
        if group.is_empty() || group.len() > MAX_ARGUMENTS {
            return Err("neoforge_arguments_budget".into());
        }
    }
    Ok(version.clone())
}

fn validate_base_metadata(base: &Value) -> Result<(), String> {
    let object = base.as_object().ok_or("neoforge_base_invalid")?;
    if object.get("id").and_then(Value::as_str) != Some(BASE_ID) {
        return Err("neoforge_base_id".into());
    }
    if object.contains_key("inheritsFrom") {
        return Err("neoforge_base_inherited".into());
    }
    let libraries = object
        .get("libraries")
        .and_then(Value::as_array)
        .ok_or("neoforge_base_libraries")?;
    if libraries.len() > MAX_BASE_LIBRARIES {
        return Err("neoforge_base_libraries".into());
    }
    Ok(())
}

fn validate_profile_pins(profile: &NeoForgeInstallerProfile) -> Result<(), String> {
    expected_data(
        profile,
        "MC_SRG",
        "[net.minecraft:client:1.20.4-20240627.114801:srg]",
    )?;
    expected_data(
        profile,
        "MC_EXTRA",
        "[net.minecraft:client:1.20.4-20240627.114801:extra]",
    )?;
    expected_data(
        profile,
        "PATCHED",
        "[net.neoforged:neoforge:20.4.253:client]",
    )?;
    Ok(())
}

fn expected_data(
    profile: &NeoForgeInstallerProfile,
    key: &str,
    expected: &str,
) -> Result<(), String> {
    match profile.data.get(key) {
        Some(InstallerData::Shared(value)) if value == expected => Ok(()),
        Some(InstallerData::Sided {
            client: Some(value),
            ..
        }) if value == expected => Ok(()),
        _ => Err(format!("neoforge_runtime_profile_data_{key}")),
    }
}

fn merge_libraries(base: &Value, child: &Value) -> Result<Value, String> {
    let base_libraries = base
        .get("libraries")
        .and_then(Value::as_array)
        .ok_or("neoforge_base_libraries")?;
    let child_libraries = child
        .get("libraries")
        .and_then(Value::as_array)
        .ok_or("neoforge_child_libraries")?;
    if child_libraries.len() > MAX_CHILD_LIBRARIES {
        return Err("neoforge_child_libraries".into());
    }

    let mut ordered: Vec<Value> = Vec::new();
    let mut positions = BTreeMap::new();
    for library in base_libraries {
        let key = library_conflict_key(library)?;
        if positions.contains_key(&key) {
            return Err("neoforge_library_path_conflict".into());
        }
        positions.insert(key, ordered.len());
        ordered.push(library.clone());
    }

    let mut child_seen = BTreeSet::new();
    for library in child_libraries {
        let key = library_conflict_key(library)?;
        if !child_seen.insert(key.clone()) {
            return Err("neoforge_child_library_duplicate".into());
        }
        match positions.get(&key).copied() {
            Some(index) => ordered[index] = library.clone(),
            None => {
                positions.insert(key, ordered.len());
                ordered.push(library.clone());
            }
        }
    }
    Ok(Value::Array(ordered))
}

fn library_conflict_key(library: &Value) -> Result<String, String> {
    let name = library
        .get("name")
        .and_then(Value::as_str)
        .ok_or("neoforge_library_name")?;
    maven_conflict_key(name)
}

/// Conflict key: `group:artifact:classifier@extension`, dropping the version.
/// A bare `@jar` and an omitted extension must agree.
fn maven_conflict_key(coordinate: &str) -> Result<String, String> {
    maven_path(coordinate).map_err(str::to_owned)?;
    let (base, extension) = coordinate.split_once('@').unwrap_or((coordinate, "jar"));
    let parts: Vec<_> = base.split(':').collect();
    Ok(format!(
        "{}:{}:{}@{}",
        parts[0],
        parts[1],
        parts.get(3).unwrap_or(&""),
        extension
    ))
}

fn merge_arguments(base: &Value, child: &Value) -> Result<Option<Value>, String> {
    let base_args = base.get("arguments");
    let child_args = child.get("arguments");
    if base_args.is_none() && child_args.is_none() {
        return Ok(None);
    }
    let mut result = Map::new();
    for key in ["game", "jvm"] {
        let merged = merge_argument_list(base_args, child_args, key)?;
        if !merged.is_empty() {
            result.insert(key.into(), Value::Array(merged));
        }
    }
    Ok(Some(Value::Object(result)))
}

fn merge_argument_list(
    base_args: Option<&Value>,
    child_args: Option<&Value>,
    key: &str,
) -> Result<Vec<Value>, String> {
    let mut combined = Vec::new();
    for source in [base_args, child_args] {
        let Some(arguments) = source else { continue };
        let group = arguments
            .get(key)
            .and_then(Value::as_array)
            .ok_or("neoforge_arguments")?;
        if group.len() > MAX_ARGUMENTS {
            return Err("neoforge_arguments_budget".into());
        }
        for argument in group {
            validate_argument(argument)?;
            combined.push(argument.clone());
        }
    }
    Ok(combined)
}

fn validate_argument(argument: &Value) -> Result<(), String> {
    if let Some(text) = argument.as_str() {
        if text.is_empty() || text.len() > MAX_STRING || text.chars().any(char::is_control) {
            return Err("neoforge_arguments".into());
        }
        return Ok(());
    }
    let object = argument.as_object().ok_or("neoforge_arguments")?;
    if object.len() > 2
        || object
            .keys()
            .any(|k| !matches!(k.as_str(), "value" | "rules"))
    {
        return Err("neoforge_arguments".into());
    }
    let value = object.get("value").ok_or("neoforge_arguments")?;
    match value {
        Value::String(text) => {
            if text.is_empty() || text.len() > MAX_STRING || text.chars().any(char::is_control) {
                return Err("neoforge_arguments".into());
            }
        }
        Value::Array(items) if !items.is_empty() && items.len() <= 32 => {
            for item in items {
                if !item.is_string() {
                    return Err("neoforge_arguments".into());
                }
                validate_argument(item)?;
            }
        }
        _ => return Err("neoforge_arguments".into()),
    }
    if let Some(rules) = object.get("rules") {
        let rules = rules
            .as_array()
            .filter(|rules| !rules.is_empty() && rules.len() <= 32)
            .ok_or("neoforge_arguments")?;
        for rule in rules {
            let rule = rule.as_object().ok_or("neoforge_arguments")?;
            if rule.len() > 3
                || !matches!(
                    rule.get("action").and_then(Value::as_str),
                    Some("allow" | "disallow")
                )
                || rule
                    .keys()
                    .any(|k| !matches!(k.as_str(), "action" | "os" | "features"))
            {
                return Err("neoforge_arguments".into());
            }
        }
    }
    Ok(())
}

fn collect_child_downloads(
    child: &Value,
    profile: &NeoForgeInstallerProfile,
) -> Result<Vec<InstallerLibrary>, String> {
    let libraries = child
        .get("libraries")
        .and_then(Value::as_array)
        .ok_or("neoforge_child_libraries")?;
    let mut downloads = Vec::new();
    let mut paths = BTreeSet::new();
    for library in libraries {
        if library.get("rules").is_some() || library.get("natives").is_some() {
            return Err("neoforge_child_library_rules_unsupported".into());
        }
        let coordinate = library
            .get("name")
            .and_then(Value::as_str)
            .ok_or("neoforge_library_name")?;
        let entry = parse_download(coordinate, library.get("downloads"))?;
        if !paths.insert(entry.path.clone()) {
            return Err("neoforge_download_path_conflict".into());
        }
        downloads.push(entry);
    }

    let mut universal_matches = profile
        .libraries
        .iter()
        .filter(|library| library.coordinate == UNIVERSAL_COORDINATE);
    let universal = universal_matches
        .next()
        .ok_or("neoforge_universal_missing")?;
    if universal_matches.next().is_some() {
        return Err("neoforge_universal_duplicate".into());
    }
    validate_installer_library(universal)?;
    if !paths.insert(universal.path.clone()) {
        return Err("neoforge_download_path_conflict".into());
    }
    downloads.push(universal.clone());
    Ok(downloads)
}

fn parse_download(coordinate: &str, downloads: Option<&Value>) -> Result<InstallerLibrary, String> {
    let expected_path = maven_path(coordinate).map_err(str::to_owned)?;
    let artifact = downloads
        .and_then(|d| d.get("artifact"))
        .ok_or("neoforge_library_artifact")?;
    let path = artifact
        .get("path")
        .and_then(Value::as_str)
        .ok_or("neoforge_library_path")?;
    if path != expected_path || !crate::launch::install::safe_path(path) {
        return Err("neoforge_library_path".into());
    }
    let sha1 = artifact
        .get("sha1")
        .and_then(Value::as_str)
        .ok_or("neoforge_library_checksum")?;
    if sha1.len() != 40 || !sha1.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("neoforge_library_checksum".into());
    }
    let size = artifact
        .get("size")
        .and_then(Value::as_u64)
        .filter(|s| *s > 0 && *s <= MAX_ARTIFACT_BYTES)
        .ok_or("neoforge_library_size")?;
    let url = artifact
        .get("url")
        .and_then(Value::as_str)
        .ok_or("neoforge_library_url")?;
    validate_url(url, path)?;
    Ok(InstallerLibrary {
        coordinate: coordinate.to_string(),
        path: path.to_owned(),
        url: url.to_string(),
        sha1: sha1.to_ascii_lowercase(),
        size,
    })
}

fn validate_installer_library(library: &InstallerLibrary) -> Result<(), String> {
    let expected = maven_path(&library.coordinate).map_err(str::to_owned)?;
    if library.path != expected
        || !crate::launch::install::safe_path(&library.path)
        || library.sha1.len() != 40
        || !library.sha1.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("neoforge_universal_invalid".into());
    }
    if library.size == 0 || library.size > MAX_ARTIFACT_BYTES {
        return Err("neoforge_universal_invalid".into());
    }
    validate_url(&library.url, &library.path)
}

fn validate_url(url: &str, path: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix(NEOFORGE_PREFIX)
        .or_else(|| url.strip_prefix(MC_PREFIX))
        .ok_or("neoforge_library_url")?;
    if rest != path {
        return Err("neoforge_library_url".into());
    }
    Ok(())
}

fn generated_paths() -> Vec<String> {
    vec![
        "libraries/net/minecraft/client/1.20.4-20240627.114801/client-1.20.4-20240627.114801-srg.jar".into(),
        "libraries/net/minecraft/client/1.20.4-20240627.114801/client-1.20.4-20240627.114801-extra.jar".into(),
        "libraries/net/neoforged/neoforge/20.4.253/neoforge-20.4.253-client.jar".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PROFILE_BYTES: &[u8] = include_bytes!("fixtures/neoforge-plan-20.4.253.json");
    const VERSION_JSON: &str = include_str!("fixtures/neoforge-runtime-20.4.253.json");

    fn profile() -> NeoForgeInstallerProfile {
        super::super::neoforge_profile::parse_install_profile("1.20.4", "20.4.253", PROFILE_BYTES)
            .expect("profile fixture")
    }

    fn version() -> Value {
        serde_json::from_str(VERSION_JSON).expect("version fixture")
    }

    fn base() -> Value {
        json!({
            "id": "1.20.4",
            "mainClass": "net.minecraft.client.main.Main",
            "downloads": {"client": {"url": "https://piston-data.mojang.com/x"}},
            "assetIndex": {"id": "5"},
            "javaVersion": {"majorVersion": 17},
            "logging": {"client": {"file": {"id": "x"}}},
            "libraries": [
                {"name": "org.ow2.asm:asm:9.5"},
                {"name": "org.lwjgl:lwjgl:3.3.2", "natives": {"linux": "natives-linux"}},
                {"name": "org.lwjgl:lwjgl:3.3.2:natives-linux"}
            ],
            "arguments": {
                "game": [
                    "--game",
                    {"rules": [{"action": "allow", "features": {"is_demo_user": true}}], "value": "--demo"}
                ],
                "jvm": ["-Xmx2G"]
            }
        })
    }

    #[test]
    fn asm_override_preserves_classifier() {
        let plan = plan_runtime(&profile(), &version(), &base()).expect("plan");
        let libs = plan.metadata["libraries"].as_array().unwrap();
        assert!(libs.iter().any(|l| l["name"] == "org.ow2.asm:asm:9.8@jar"));
        assert!(!libs.iter().any(|l| l["name"] == "org.ow2.asm:asm:9.5"));
        assert!(libs
            .iter()
            .any(|l| l["name"] == "org.lwjgl:lwjgl:3.3.2:natives-linux"));
    }

    #[test]
    fn base_conditional_features_args_preserved() {
        let plan = plan_runtime(&profile(), &version(), &base()).expect("plan");
        let game = plan.metadata["arguments"]["game"].as_array().unwrap();
        assert_eq!(game[0], "--game");
        assert_eq!(game[1]["rules"][0]["features"]["is_demo_user"], true);
        assert_eq!(game[1]["value"], "--demo");
        assert_eq!(game[2], "--fml.neoForgeVersion");
    }

    #[test]
    fn base_fields_preserved() {
        let plan = plan_runtime(&profile(), &version(), &base()).expect("plan");
        let m = &plan.metadata;
        assert_eq!(m["id"], "1.20.4");
        assert_eq!(m["assetIndex"]["id"], "5");
        assert_eq!(m["javaVersion"]["majorVersion"], 17);
        assert_eq!(m["logging"]["client"]["file"]["id"], "x");
        assert_eq!(
            m["downloads"]["client"]["url"],
            "https://piston-data.mojang.com/x"
        );
        assert_eq!(m["mainClass"], NEOFORGE_MAIN_CLASS);
        assert!(m.get("inheritsFrom").is_none());
    }

    #[test]
    fn download_and_generated_counts() {
        let plan = plan_runtime(&profile(), &version(), &base()).expect("plan");
        assert_eq!(plan.downloads.len(), 50, "49 version libs + universal");
        assert_eq!(plan.generated_paths.len(), 3);
        assert!(plan
            .downloads
            .iter()
            .any(|d| d.coordinate == UNIVERSAL_COORDINATE));
        let payload = plan.download_payload();
        let artifacts = payload["artifacts"].as_array().unwrap();
        assert_eq!(artifacts.len(), 50);
        for artifact in artifacts {
            assert_eq!(artifact["hash"]["algorithm"], "sha1");
            let url = artifact["url"].as_str().unwrap();
            assert_eq!(
                artifact["sourceId"],
                if url.starts_with(NEOFORGE_PREFIX) {
                    "neo-runtime"
                } else {
                    "mojang-libraries"
                }
            );
            assert!(!url.contains("-client.jar"));
        }
        assert_eq!(plan, plan_runtime(&profile(), &version(), &base()).unwrap());
    }

    fn rejected(mutate: impl FnOnce(&mut Value, &mut Value, &mut NeoForgeInstallerProfile)) {
        let (mut v, mut b, mut p) = (version(), base(), profile());
        mutate(&mut v, &mut b, &mut p);
        assert!(plan_runtime(&p, &v, &b).is_err());
    }

    #[test]
    fn rejects_bad_inputs() {
        rejected(|v, _, _| v["id"] = json!("neoforge-20.4.254"));
        rejected(|_, b, _| b["id"] = json!("1.20.1"));
        rejected(|v, _, _| {
            v["libraries"][0]["downloads"]["artifact"]["url"] =
                json!("https://maven.neoforged.net/releases/x.jar?y=1")
        });
        rejected(|v, _, _| {
            v["libraries"][0]["downloads"]["artifact"]["path"] = json!("../../evil.jar")
        });
        rejected(|v, _, _| v["libraries"][0]["downloads"]["artifact"]["sha1"] = json!("zz"));
        rejected(|v, _, _| v["libraries"][0]["downloads"]["artifact"]["size"] = json!(0));
        rejected(|v, _, _| {
            let library = v["libraries"][0].clone();
            v["libraries"].as_array_mut().unwrap().push(library);
        });
        rejected(|v, _, _| {
            v["arguments"]["game"] = json!([{"value": [{"value": "nested"}]}]);
        });
        rejected(|_, _, p| {
            p.libraries.retain(|l| l.coordinate != UNIVERSAL_COORDINATE);
        });
        rejected(|_, _, p| {
            p.data
                .insert("MC_SRG".into(), InstallerData::Shared("[bad]".into()));
        });
    }
}

impl RuntimePlan {
    /// Build a downloads FETCH command from the already-planned library list.
    /// URLs/hashes/sizes originate from verified installer + child metadata; no
    /// synthetic values and no new network path are introduced here.
    pub fn download_payload(&self) -> Value {
        let artifacts: Vec<Value> = self
            .downloads
            .iter()
            .map(|library| {
                let source_id = if library.url.starts_with(NEOFORGE_PREFIX) {
                    "neo-runtime"
                } else {
                    "mojang-libraries"
                };
                serde_json::json!({
                    "artifactId": format!("neo-runtime-{}", library.sha1),
                    "sourceId": source_id,
                    "url": library.url,
                    "size": library.size,
                    "hash": {"algorithm": "sha1", "digest": library.sha1}
                })
            })
            .collect();
        serde_json::json!({"command": crate::downloads::actions::FETCH, "artifacts": artifacts})
    }

    /// Submit the planned artifacts to the shared CoreApp downloads dispatcher.
    /// Cancellation, verification, cache publication and retry policy stay with
    /// the downloads module; this only forwards an advisory payload.
    pub fn start_download(
        &self,
        app: &crate::CoreApp,
        request_id: shtmcl_domain::RequestId,
        operation_id: shtmcl_domain::OperationId,
    ) -> Result<shtmcl_domain::CommandResponse, shtmcl_domain::ErrorEnvelope> {
        let payload = self.download_payload();
        app.dispatch_mutation(
            shtmcl_domain::ModuleId::Downloads,
            &shtmcl_domain::CommandRequest {
                request_id,
                operation_id: Some(operation_id),
                entity_id: None,
                expected_revision: None,
                payload,
            },
        )
    }
}

#[cfg(test)]
#[path = "neoforge_runtime_acceptance.rs"]
mod acceptance;

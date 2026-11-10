//! Pure adapter that folds verified NeoForge metadata into a base vanilla
//! launch snapshot. No IO, download, process spawn or publishing happens here.
//!
//! The caller has already hash-verified every path in `VerifiedPrepared.files`
//! under exclusive directory ownership, including `base_metadata`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;
use shtmcl_core::launch::{
    install::safe_path,
    plan::{ArtifactRole, InstanceLaunchSnapshot, LoaderKind, RequiredFile},
};

const PILOT_GAME: &str = "1.20.4";
const PILOT_LOADER: &str = "20.4.253";
const PILOT_MAIN_CLASS: &str = "cpw.mods.bootstraplauncher.BootstrapLauncher";
const MAX_LIBRARIES: usize = 4096;
const LOGGING_PREFIX: &str = "-Dlog4j.configurationFile=";

/// One-use verification result shared by preparation and snapshot construction.
/// Keys include the launch lease holder and revision; never reused across launches.
#[derive(Default)]
pub(crate) struct LaunchChecks(std::sync::Mutex<std::collections::BTreeMap<String, Checked>>);
struct Checked {
    holder: String,
    revision: u64,
    root: PathBuf,
    digest: String,
    verified: crate::neoforge_integrity::VerifiedPrepared,
}
impl LaunchChecks {
    pub(crate) fn prepare(
        &self,
        instance: &shtmcl_core::instances::Instance,
        root: &Path,
        holder: &str,
        digest: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|_| "neoforge_check_lock")?
            .remove(&instance.id);
        let verified = crate::neoforge_integrity::verify(root, digest, cancelled)?;
        if cancelled() {
            return Err("cancelled".into());
        }
        let mut checks = self.0.lock().map_err(|_| "neoforge_check_lock")?;
        if checks.len() >= 16 {
            return Err("neoforge_check_capacity".into());
        }
        checks.insert(
            instance.id.clone(),
            Checked {
                holder: holder.into(),
                revision: instance.revision,
                root: root.into(),
                digest: digest.into(),
                verified,
            },
        );
        Ok(())
    }
    pub(crate) fn take(
        &self,
        instance: &shtmcl_core::instances::Instance,
        root: &Path,
        holder: &str,
        digest: &str,
    ) -> Result<crate::neoforge_integrity::VerifiedPrepared, String> {
        let checked = self
            .0
            .lock()
            .map_err(|_| "neoforge_check_lock")?
            .remove(&instance.id)
            .ok_or("neoforge_check_missing")?;
        if checked.holder != holder
            || checked.revision != instance.revision
            || checked.root != root
            || checked.digest != digest
        {
            return Err("neoforge_check_identity".into());
        }
        Ok(checked.verified)
    }
}

/// Metadata obligations produced alongside the prepared metadata set.
const PREPARED_OBLIGATIONS: [&str; 3] = [
    "neoforge-base.json",
    "neoforge-complete.json",
    "neoforge-prepared.json",
];

/// Fold verified NeoForge metadata onto `base` and return the merged snapshot.
///
/// `verified.metadata` is the final merged NeoForge JSON (libraries, arguments,
/// mainClass). `verified.base_metadata` is the raw verified base metadata.
/// `verified.files` are the fully hash-verified paths under the instance root.
pub(crate) fn apply(
    mut base: InstanceLaunchSnapshot,
    verified: &crate::neoforge_integrity::VerifiedPrepared,
) -> Result<InstanceLaunchSnapshot, String> {
    let merged = &verified.metadata;

    validate_identity(&base, merged)?;

    let root = base.game_dir.clone();
    if !root.is_absolute() {
        return Err(format!("instance root is not absolute: {}", root.display()));
    }

    // Casefold membership set of every verified file (Windows behaviour).
    let mut verified_keys: HashSet<String> = HashSet::with_capacity(verified.files.len());
    for file in &verified.files {
        let key = casefold(file_under_root(file, &root)?);
        verified_keys.insert(key);
    }

    // Classpath from merged libraries (ordered, deduped).
    let libraries = merged
        .get("libraries")
        .and_then(Value::as_array)
        .ok_or_else(|| "neoforge metadata missing libraries array".to_string())?;
    if libraries.len() > MAX_LIBRARIES {
        return Err(format!(
            "neoforge libraries exceeds limit {}: {}",
            MAX_LIBRARIES,
            libraries.len()
        ));
    }

    let mut classpath: Vec<PathBuf> = Vec::new();
    let mut seen_cp: HashSet<String> = HashSet::new();
    for lib in libraries {
        if !crate::launch_instances::decision(lib.get("rules"))? {
            continue;
        }
        let rel = match lib
            .get("downloads")
            .and_then(|d| d.get("artifact"))
            .and_then(|a| a.get("path"))
            .and_then(Value::as_str)
        {
            Some(p) => p,
            None => continue,
        };
        if !safe_path(rel) {
            return Err("neoforge_library_path".into());
        }
        let full = root.join("libraries").join(rel);
        let key = casefold(file_under_root(&full, &root)?);
        if !verified_keys.contains(&key) {
            return Err(format!(
                "merged library path not in verified files: {}",
                full.display()
            ));
        }
        if seen_cp.insert(key) {
            classpath.push(full);
        }
    }

    // Append the base client jar only when it is verified present.
    let base_jar = root
        .join("versions")
        .join(PILOT_GAME)
        .join(format!("{PILOT_GAME}.jar"));
    let base_jar_key = casefold(file_under_root(&base_jar, &root)?);
    if !verified_keys.contains(&base_jar_key) {
        return Err("neoforge_client_unverified".into());
    }
    if seen_cp.insert(base_jar_key) {
        classpath.push(base_jar);
    }

    if classpath.is_empty() {
        return Err("neoforge merged metadata produced empty classpath".to_string());
    }

    // JVM/game arguments from merged metadata; keep only base logging property.
    let arguments = merged.get("arguments");
    let mut jvm_args = parse_arguments(arguments.and_then(|a| a.get("jvm")))?;
    let game_args = parse_arguments(arguments.and_then(|a| a.get("game")))?;

    // Preserve the single base logging JVM property rather than all base JVM args.
    for arg in base
        .jvm_args
        .iter()
        .filter(|a| a.starts_with(LOGGING_PREFIX))
    {
        if !jvm_args.contains(arg) {
            jvm_args.push(arg.clone());
        }
    }

    // Required files: existing base obligations retained, then all verified.
    let mut required_files = base.required_files.clone();
    let mut seen_req: HashSet<String> = required_files
        .iter()
        .map(|r| casefold(r.path.clone()))
        .collect();

    for name in PREPARED_OBLIGATIONS {
        let p = root.join(name);
        let key = casefold(file_under_root(&p, &root)?);
        if seen_req.insert(key) {
            required_files.push(RequiredFile::new(p, ArtifactRole::Library));
        }
    }

    for file in &verified.files {
        let rel = file_under_root(file, &root)?;
        let key = casefold(rel.clone());
        if !seen_req.insert(key) {
            continue;
        }
        let relative = file.strip_prefix(&root).map_err(|_| "neoforge_path")?;
        let role = if relative.starts_with("assets") {
            ArtifactRole::Asset
        } else {
            ArtifactRole::Library
        };
        required_files.push(RequiredFile::new(file.clone(), role));
    }

    base.classpath = classpath;
    base.jvm_args = jvm_args;
    base.game_args = game_args;
    base.main_class = PILOT_MAIN_CLASS.to_string();
    base.loader = LoaderKind::NeoForge {
        version: Some(PILOT_LOADER.to_string()),
    };
    base.required_files = required_files;

    // Preserve existing explicit auth/offline identity/memory/runtime untouched.
    Ok(base)
}

fn validate_identity(base: &InstanceLaunchSnapshot, merged: &Value) -> Result<(), String> {
    if merged["id"] != PILOT_GAME {
        return Err("neoforge_metadata_id".into());
    }
    if base.game_version != PILOT_GAME {
        return Err(format!(
            "pilot requires game {}, found {}",
            PILOT_GAME, base.game_version
        ));
    }
    if merged.get("inheritsFrom").is_some() {
        return Err("neoforge metadata must not contain inheritsFrom".to_string());
    }
    let main = merged
        .get("mainClass")
        .and_then(Value::as_str)
        .ok_or_else(|| "neoforge metadata missing mainClass".to_string())?;
    if main != PILOT_MAIN_CLASS {
        return Err(format!("unexpected mainClass: {main}"));
    }
    let loader = merged
        .pointer("/shtmclLoader")
        .ok_or_else(|| "neoforge metadata missing shtmclLoader".to_string())?;
    let kind = loader.get("kind").and_then(Value::as_str);
    let version = loader.get("version").and_then(Value::as_str);
    if kind != Some("neoforge") || version != Some(PILOT_LOADER) {
        return Err(format!(
            "neoforge shtmclLoader mismatch: kind={:?} version={:?}",
            kind, version
        ));
    }
    Ok(())
}

fn parse_arguments(value: Option<&Value>) -> Result<Vec<String>, String> {
    match value {
        Some(v) => crate::launch_instances::argument_list(v),
        None => Err("neoforge_arguments_missing".into()),
    }
}

/// Ensure `path` is inside `root`; return it unchanged without normalization.
fn file_under_root(path: &Path, root: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("verified path not absolute: {}", path.display()));
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("verified path traversal: {}", path.display()));
    }
    if !path.starts_with(root) {
        return Err(format!(
            "verified path outside root {}: {}",
            root.display(),
            path.display()
        ));
    }
    Ok(path.to_path_buf())
}

fn casefold(path: PathBuf) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

/// Construct a NeoForge snapshot from the identity recorded at installation.
/// Published instances enter through InstalledInstances, which checks receipt and lease.
pub(crate) fn installed(
    root: &Path,
    expected: &str,
    instance: &shtmcl_core::instances::Instance,
    options: &crate::launch_instances::LaunchOptions,
    cancelled: &dyn Fn() -> bool,
) -> Result<InstanceLaunchSnapshot, String> {
    if Path::new(&instance.location.root) != root
        || instance.version.game_version != PILOT_GAME
        || instance.version.loader != shtmcl_core::instances::LoaderKind::NeoForge
        || instance.version.loader_version.as_deref() != Some(PILOT_LOADER)
    {
        return Err("neoforge_instance_identity".into());
    }
    let verified = crate::neoforge_integrity::verify(root, expected, cancelled)?;
    from_verified(root, instance, options, verified)
}

pub(crate) fn from_verified(
    root: &Path,
    instance: &shtmcl_core::instances::Instance,
    options: &crate::launch_instances::LaunchOptions,
    verified: crate::neoforge_integrity::VerifiedPrepared,
) -> Result<InstanceLaunchSnapshot, String> {
    let metadata = shtmcl_core::launch::catalog::VersionMetadata::from_value(
        verified.base_metadata.clone(),
        Some(PILOT_GAME),
    )
    .map_err(|_| "neoforge_base_metadata")?;
    let id = shtmcl_domain::InstanceId::new(instance.id.clone()).map_err(|_| "instance_id")?;
    let base = crate::launch_instances::build_snapshot(&id, instance, root, metadata, options)?;
    apply(base, &verified)
}

/// A copied, unpublished stage has no committed instance receipt. Test-only.
#[cfg(test)]
pub(crate) fn staged(
    root: &Path,
    expected: &str,
    instance: &shtmcl_core::instances::Instance,
    options: &crate::launch_instances::LaunchOptions,
    cancelled: &dyn Fn() -> bool,
) -> Result<InstanceLaunchSnapshot, String> {
    let mut snapshot = installed(root, expected, instance, options, cancelled)?;
    snapshot
        .required_files
        .retain(|file| file.path != root.join("shtmcl-install.json"));
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The exact VerifiedPrepared shape is defined by the root integrator; these
    // tests assume `metadata`, `base_metadata` and `files` fields.
    use crate::neoforge_integrity::VerifiedPrepared;
    use shtmcl_core::launch::{java::JavaRequirement, ports::AuthRequirement};
    use shtmcl_domain::{InstanceId, Revision};

    #[test]
    fn verification_result_is_single_use_and_bound_to_launch_identity() {
        let instance: shtmcl_core::instances::Instance = serde_json::from_value(json!({
            "id":"pilot","name":"Pilot","version":{"gameVersion":"1.20.4","loader":"neo_forge","loaderVersion":"20.4.253"},
            "location":{"root":root()},"javaMajor":17,"state":"busy","revision":2,"createdAt":1,"updatedAt":1
        })).unwrap();
        assert_eq!(serde_json::to_value(&instance).unwrap()["version"]["loader"],"neoforge");
        for mutation in ["none","holder","revision","root","digest"] {
            let cache = LaunchChecks::default();
            cache.0.lock().unwrap().insert(instance.id.clone(),Checked {
                holder:"launch-1".into(),revision:2,root:root(),digest:"a".repeat(64),
                verified:VerifiedPrepared {base_metadata:json!({}),metadata:json!({}),files:vec![]}
            });
            let mut current = instance.clone();
            if mutation == "revision" { current.revision += 1; }
            let path = if mutation == "root" { root().join("other") } else { root() };
            let holder = if mutation == "holder" { "launch-2" } else { "launch-1" };
            let digest = if mutation == "digest" { "b".repeat(64) } else { "a".repeat(64) };
            assert_eq!(cache.take(&current,&path,holder,&digest).is_ok(),mutation == "none");
            assert!(cache.take(&instance,&root(),"launch-1",&"a".repeat(64)).is_err());
        }
    }

    fn root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from("C:/shtmcl-test")
        } else {
            PathBuf::from("/shtmcl-test")
        }
    }

    fn base(root: &Path) -> InstanceLaunchSnapshot {
        InstanceLaunchSnapshot {
            instance_id: InstanceId::new("test").unwrap(),
            revision: Revision::new(1),
            content_revision: Some(Revision::new(2)),
            name: "NeoForge Pilot".to_string(),
            game_version: PILOT_GAME.to_string(),
            version_type: Some("release".to_string()),
            loader: LoaderKind::Vanilla,
            game_dir: root.to_path_buf(),
            main_class: "net.minecraft.client.main.Main".to_string(),
            classpath: vec![root.join("libraries/old/Old.jar")],
            assets_dir: Some(root.join("assets")),
            asset_index: Some("5".to_string()),
            natives_dir: Some(root.join("natives")),
            jvm_args: vec![
                "-Xmx2G".to_string(),
                format!("{LOGGING_PREFIX}{{game_directory}}//log4j.xml"),
            ],
            game_args: vec!["--old".to_string()],
            memory_mb: Some(2048),
            java_requirement: Some(JavaRequirement::Exact(17)),
            java_path: Some(root.join("runtime/bin/java.exe")),
            required_files: vec![RequiredFile::new(
                root.join("versions/1.20.4/1.20.4.json"),
                ArtifactRole::Client,
            )],
            auth: AuthRequirement::Offline,
            offline_username: Some("Steve".to_string()),
            multi_open: false,
            profile_ref: None,
        }
    }

    fn merged_metadata(libs: Vec<Value>) -> Value {
        json!({
            "id": PILOT_GAME,
            "mainClass": PILOT_MAIN_CLASS,
            "shtmclLoader": {"kind": "neoforge", "version": PILOT_LOADER},
            "libraries": libs,
            "arguments": {
                "jvm": ["-Dneoforge.test=1", {"rules": [], "value": "-Dextra=1"}],
                "game": ["--username", "${auth_player_name}"]
            }
        })
    }

    fn lib(path: &str) -> Value {
        json!({"downloads": {"artifact": {"path": path}}})
    }

    fn files(root: &Path, extra: &[&str]) -> Vec<PathBuf> {
        let mut v = vec![root.join("versions/1.20.4/1.20.4.jar")];
        for e in extra {
            v.push(root.join(e));
        }
        v
    }

    #[test]
    fn ordering_and_dedup() {
        let r = root();
        let merged = merged_metadata(vec![lib("a/A.jar"), lib("b/B.jar"), lib("a/a.jar")]);
        let v = VerifiedPrepared {
            metadata: merged,
            base_metadata: json!({}),
            files: files(&r, &["libraries/a/A.jar", "libraries/b/B.jar"]),
        };
        let out = apply(base(&r), &v).unwrap();
        assert_eq!(
            out.classpath,
            vec![
                r.join("libraries/a/A.jar"),
                r.join("libraries/b/B.jar"),
                r.join("versions/1.20.4/1.20.4.jar"),
            ]
        );
    }

    #[test]
    fn overrides_base_libs_preserves_options() {
        let r = root();
        let merged = merged_metadata(vec![lib("neoforge/nf.jar")]);
        let v = VerifiedPrepared {
            metadata: merged,
            base_metadata: json!({"shtmclReceipt": {"x": 1}}),
            files: files(&r, &["libraries/neoforge/nf.jar"]),
        };
        let out = apply(base(&r), &v).unwrap();
        assert!(!out
            .classpath
            .iter()
            .any(|p| p.to_string_lossy().contains("old")));
        assert_eq!(out.memory_mb, Some(2048));
        assert!(matches!(out.auth, AuthRequirement::Offline));
        assert_eq!(out.offline_username.as_deref(), Some("Steve"));
        assert_eq!(out.java_requirement, Some(JavaRequirement::Exact(17)));
        assert!(out.jvm_args.iter().any(|a| a.starts_with(LOGGING_PREFIX)));
        assert!(out.jvm_args.iter().any(|a| a == "-Dneoforge.test=1"));
        assert_eq!(out.game_args, vec!["--username", "${auth_player_name}"]);
        assert!(matches!(
            &out.loader,
            LoaderKind::NeoForge { version: Some(v) } if v == PILOT_LOADER
        ));
    }

    #[test]
    fn generated_files_required_not_classpath() {
        let r = root();
        let merged = merged_metadata(vec![lib("neoforge/nf.jar")]);
        let v = VerifiedPrepared {
            metadata: merged,
            base_metadata: json!({}),
            files: files(
                &r,
                &[
                    "libraries/neoforge/nf.jar",
                    "libraries/net/neoforged/universal.jar",
                    "neoforge-prepared.json",
                    "assets/indexes/5.json",
                ],
            ),
        };
        let out = apply(base(&r), &v).unwrap();
        assert!(!out
            .classpath
            .iter()
            .any(|p| p.to_string_lossy().contains("universal")));
        let has = |name: &str| {
            out.required_files
                .iter()
                .any(|r| r.path.to_string_lossy().contains(name))
        };
        assert!(has("universal.jar"));
        assert!(has("neoforge-prepared.json"));
        assert!(has("5.json"));
        let asset = out
            .required_files
            .iter()
            .find(|r| r.path.to_string_lossy().contains("assets"))
            .unwrap();
        assert!(matches!(asset.role, ArtifactRole::Asset));
    }

    #[test]
    fn unknown_rule_fails() {
        let r = root();
        let bad =
            json!({"downloads": {"artifact": {"path": "x/X.jar"}}, "rules": [{"action": "bogus"}]});
        let v = VerifiedPrepared {
            metadata: merged_metadata(vec![bad]),
            base_metadata: json!({}),
            files: files(&r, &["libraries/x/X.jar"]),
        };
        assert!(apply(base(&r), &v).is_err());
    }

    #[test]
    fn classpath_outside_verified_fails() {
        let r = root();
        let merged = merged_metadata(vec![lib("missing/M.jar")]);
        let v = VerifiedPrepared {
            metadata: merged,
            base_metadata: json!({}),
            files: files(&r, &[]),
        };
        assert!(apply(base(&r), &v).is_err());
    }

    #[test]
    fn bad_loader_identity_fails() {
        let r = root();
        let mut merged = merged_metadata(vec![lib("a/A.jar")]);
        merged["shtmclLoader"]["version"] = json!("9.9.9");
        let v = VerifiedPrepared {
            metadata: merged,
            base_metadata: json!({}),
            files: files(&r, &["libraries/a/A.jar"]),
        };
        assert!(apply(base(&r), &v).is_err());
    }
}

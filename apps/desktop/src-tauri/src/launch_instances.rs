//! Production bridge from committed vanilla instances to module 02 snapshots.
//! Metadata is local, bounded and pinned to the indexed instance and receipt;
//! this adapter neither installs missing files nor invents an account policy.
use crate::game_install::reject_link;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use shtmcl_core::{
    launch::{
        catalog::{evaluate_rules, LibraryRuleDecision, VersionMetadata},
        install::{asset_artifacts, safe_path, VanillaInstallPlan},
        java::JavaRequirement,
        plan::{ArtifactRole, InstanceLaunchSnapshot, LoaderKind, RequiredFile},
        ports::{AuthRequirement, InstancePreparation, InstanceSnapshotSource},
    },
    CoreModule,
};
use shtmcl_domain::{
    CommandRequest, CommandResponse, ErrorEnvelope, InstanceId, RequestId, Revision,
};
use shtmcl_platform::PlatformDirs;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{Arc, RwLock},
};

// Local read cap matches the metadata parser's independent 8 MiB limit.
const MAX_LOCAL_METADATA_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct LaunchOptions {
    pub auth: AuthRequirement,
    pub offline_username: Arc<RwLock<Option<String>>>,
    pub memory_mb: Option<u32>,
    pub java_path: Option<PathBuf>,
}

pub(crate) struct InstalledInstances {
    instances: Arc<dyn CoreModule>,
    dirs: PlatformDirs,
    options: LaunchOptions,
    leases: Option<Arc<crate::launch_leases::InstanceLeases>>,
    neoforge_checks: Option<Arc<crate::neoforge_snapshot::LaunchChecks>>,
}

impl InstalledInstances {
    pub(crate) fn new(
        instances: Arc<dyn CoreModule>,
        dirs: PlatformDirs,
        options: LaunchOptions,
    ) -> Self {
        Self {
            instances,
            dirs,
            options,
            leases: None,
            neoforge_checks: None,
        }
    }

    pub(crate) fn with_leases(mut self, leases: Arc<crate::launch_leases::InstanceLeases>) -> Self {
        self.leases = Some(leases);
        self
    }

    pub(crate) fn with_neoforge_checks(mut self, checks: Arc<crate::neoforge_snapshot::LaunchChecks>) -> Self {
        self.neoforge_checks = Some(checks); self
    }

    fn read_instance(
        &self,
        instance_id: &InstanceId,
        holder: Option<&str>,
    ) -> Result<(shtmcl_core::instances::Instance, PathBuf), String> {
        let id = instance_id.as_str();
        if id.len() > 64
            || !safe_path(id)
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("instance_id".into());
        }
        let response = self
            .instances
            .handle(&CommandRequest {
                request_id: RequestId::new("launch-instance-snapshot").unwrap(),
                operation_id: None,
                entity_id: None,
                expected_revision: None,
                payload: json!({"command":"instances.snapshot","instanceId":id}),
            })
            .map_err(|_| "instance_snapshot")?;
        let CommandResponse::Completed { result } = response else {
            return Err("instance_snapshot_pending".into());
        };
        let instance: shtmcl_core::instances::Instance =
            serde_json::from_value(result["instance"].clone())
                .map_err(|_| "instance_snapshot_invalid")?;
        if instance.state == shtmcl_core::instances::InstanceState::Busy {
            let authorized = match (&self.leases, holder) {
                (Some(leases), Some(holder)) => leases
                    .owns(instance_id, holder)
                    .map_err(|_| "lease_registry")?,
                _ => false,
            };
            if !authorized {
                return Err("instance_busy".into());
            }
        }
        if instance.id != id
            || !matches!(
                instance.version.loader,
                shtmcl_core::instances::LoaderKind::Vanilla
                    | shtmcl_core::instances::LoaderKind::Fabric
                    | shtmcl_core::instances::LoaderKind::NeoForge
            )
            || instance.state == shtmcl_core::instances::InstanceState::Broken
            || instance.revision == 0
        {
            return Err("instance_not_ready_vanilla".into());
        }
        let root = self.dirs.instances.join(id);
        if Path::new(&instance.location.root) != root {
            return Err("instance_root_mismatch".into());
        }
        check_path(&root)?;
        let version = &instance.version.game_version;
        if !safe_path(version) || version.contains('/') {
            return Err("version_id".into());
        }
        let receipt: Value =
            serde_json::from_slice(&read_bounded(&root.join("shtmcl-install.json"), 4096)?)
                .map_err(|_| "install_receipt")?;
        if receipt["schemaVersion"] != 1
            || receipt["complete"] != true
            || receipt["instanceId"] != id
            || receipt["versionId"] != *version
        {
            return Err("install_receipt_mismatch".into());
        }
        if instance.version.loader == shtmcl_core::instances::LoaderKind::Fabric
            && (receipt["loader"] != "fabric"
                || receipt["loaderVersion"].as_str() != instance.version.loader_version.as_deref())
        {
            return Err("fabric_receipt_identity".into());
        }
        if instance.version.loader == shtmcl_core::instances::LoaderKind::NeoForge
            && (version != "1.20.4"
                || instance.version.loader_version.as_deref() != Some("20.4.253")
                || instance.java_major.is_some_and(|major| major != 17)
                || receipt["loader"] != "neoforge"
                || receipt["loaderVersion"] != "20.4.253"
                || receipt["preparedSha256"].as_str().is_none_or(|digest| {
                    digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit())
                }))
        {
            return Err("neoforge_receipt_identity".into());
        }
        Ok((instance, root))
    }

    fn read_snapshot(
        &self,
        instance_id: &InstanceId,
        holder: Option<&str>,
    ) -> Result<InstanceLaunchSnapshot, String> {
        let (instance, root) = self.read_instance(instance_id, holder)?;
        let version = &instance.version.game_version;
        if instance.version.loader == shtmcl_core::instances::LoaderKind::NeoForge {
            let receipt: Value =
                serde_json::from_slice(&read_bounded(&root.join("shtmcl-install.json"), 4096)?)
                    .map_err(|_| "install_receipt")?;
            if let Some(holder) = holder {
                let verified = self.neoforge_checks.as_ref().ok_or("neoforge_checks_unavailable")?.take(&instance,&root,holder,receipt["preparedSha256"].as_str().ok_or("neoforge_receipt_identity")?)?;
                return crate::neoforge_snapshot::from_verified(&root,&instance,&self.options,verified);
            }
            return crate::neoforge_snapshot::installed(
                &root,
                receipt["preparedSha256"]
                    .as_str()
                    .ok_or("neoforge_receipt_identity")?,
                &instance,
                &self.options,
                &|| false,
            );
        }
        let metadata_path = root.join(format!("versions/{version}/{version}.json"));
        let metadata = if instance.version.loader == shtmcl_core::instances::LoaderKind::Fabric {
            let receipt: Value =
                serde_json::from_slice(&read_bounded(&root.join("shtmcl-install.json"), 4096)?)
                    .map_err(|_| "install_receipt")?;
            crate::fabric_metadata::pinned(
                &self.dirs,
                &root,
                &receipt,
                version,
                instance
                    .version
                    .loader_version
                    .as_deref()
                    .ok_or("fabric_loader_version")?,
            )?
        } else {
            VersionMetadata::parse(
                &read_bounded(&metadata_path, MAX_LOCAL_METADATA_BYTES)?,
                Some(version),
            )
            .map_err(|_| "version_metadata")?
        };
        if metadata.inherits_from.is_some() {
            return Err("unresolved_version_parent".into());
        }
        build_snapshot(instance_id, &instance, &root, metadata, &self.options)
    }
}

impl InstanceSnapshotSource for InstalledInstances {
    fn preparation(&self, instance_id: &InstanceId) -> Result<InstancePreparation, ErrorEnvelope> {
        let (instance, _) = self
            .read_instance(instance_id, None)
            .map_err(snapshot_error)?;
        let username = selected_username(&self.options).map_err(snapshot_error)?;
        let requirement = match instance.java_major {
            Some(major) => JavaRequirement::Exact(u32::from(major).max(8)),
            None => {
                shtmcl_core::launch::java::requirement_for_game_version(
                    &instance.version.game_version,
                )
                .0
            }
        };
        if requirement == JavaRequirement::Any {
            return Err(snapshot_error("unknown_java_requirement".into()));
        }
        Ok(InstancePreparation {
            revision: Revision::new(instance.revision),
            offline_username: username,
            java_requirement: Some(requirement),
            java_path: self.options.java_path.clone(),
        })
    }

    fn snapshot(&self, instance_id: &InstanceId) -> Result<InstanceLaunchSnapshot, ErrorEnvelope> {
        self.read_snapshot(instance_id, None)
            .map_err(snapshot_error)
    }

    fn snapshot_under_lease(
        &self,
        instance_id: &InstanceId,
        holder: &str,
    ) -> Result<InstanceLaunchSnapshot, ErrorEnvelope> {
        if !self
            .leases
            .as_ref()
            .is_some_and(|leases| leases.owns(instance_id, holder).unwrap_or(false))
        {
            return Err(shtmcl_core::instances::error::InstancesError::Locked(
                instance_id.as_str().into(),
            )
            .to_envelope());
        }
        self.read_snapshot(instance_id, Some(holder))
            .map_err(snapshot_error)
    }
}

fn snapshot_error(reason: String) -> ErrorEnvelope {
    if reason == "offline_identity_not_selected" {
        return ErrorEnvelope::new(
            shtmcl_domain::ErrorCode::from_static("launch.identity.required"),
            shtmcl_domain::ErrorCategory::PermissionDenied,
            false,
        );
    }
    shtmcl_core::launch::errors::plan_invalid(format!("installed-instance:{reason}"))
}

fn check_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("unsafe_launch_path".into());
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        reject_link(&current)?;
    }
    Ok(())
}

fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>, String> {
    check_path(path)?;
    let file = std::fs::File::open(path).map_err(|_| "launch_file_open")?;
    let metadata = file.metadata().map_err(|_| "launch_file_metadata")?;
    if !metadata.is_file() || metadata.len() > max as u64 {
        return Err("launch_file_size".into());
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "launch_file_read")?;
    if bytes.len() > max {
        return Err("launch_file_size".into());
    }
    Ok(bytes)
}

pub(crate) fn decision(rules: Option<&Value>) -> Result<bool, String> {
    match evaluate_rules(rules, "windows", "amd64", &BTreeMap::new()) {
        LibraryRuleDecision::Included => Ok(true),
        LibraryRuleDecision::Excluded => Ok(false),
        LibraryRuleDecision::Undetermined(reason) => {
            Err(format!("unsupported_launch_rule:{reason}"))
        }
    }
}

pub(crate) fn argument_list(value: &Value) -> Result<Vec<String>, String> {
    let values = value.as_array().ok_or("launch_arguments_array")?;
    if values.len() > 4096 {
        return Err("launch_arguments_limit".into());
    }
    let mut args = Vec::new();
    for value in values {
        if let Some(text) = value.as_str() {
            args.push(text.to_string());
        } else if value.is_object() {
            if !decision(value.get("rules"))? {
                continue;
            }
            match &value["value"] {
                Value::String(text) => args.push(text.clone()),
                Value::Array(values) => {
                    for value in values {
                        args.push(value.as_str().ok_or("launch_argument_value")?.into());
                    }
                }
                _ => return Err("launch_argument_value".into()),
            }
        } else {
            return Err("launch_argument_value".into());
        }
        if args.len() > 4096 {
            return Err("launch_arguments_limit".into());
        }
    }
    if args
        .iter()
        .any(|arg| arg.len() > 32_768 || arg.contains('\0'))
    {
        return Err("launch_argument_size".into());
    }
    Ok(args)
}

/// Legacy metadata arguments are a string, but never a shell command. Quotes
/// group a single argument; expansion happens later against the frozen plan.
fn legacy_arguments(input: &str) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut started = false;
    for character in input.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\0' => return Err("legacy_argument_null".into()),
            c if c.is_whitespace() && !quoted => {
                if started {
                    result.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return Err("legacy_argument_quote".into());
    }
    if started {
        result.push(current);
    }
    if result.len() > 4096 {
        return Err("launch_arguments_limit".into());
    }
    Ok(result)
}

fn selected_username(options: &LaunchOptions) -> Result<Option<String>, String> {
    let username = options
        .offline_username
        .read()
        .map_err(|_| "launch_identity_lock")?
        .clone();
    if options.auth == AuthRequirement::Offline && username.as_deref().is_none_or(str::is_empty) {
        return Err("offline_identity_not_selected".into());
    }
    Ok(username)
}

pub(crate) fn build_snapshot(
    id: &InstanceId,
    instance: &shtmcl_core::instances::Instance,
    root: &Path,
    metadata: VersionMetadata,
    options: &LaunchOptions,
) -> Result<InstanceLaunchSnapshot, String> {
    let username = selected_username(options)?;
    let plan = VanillaInstallPlan::parse(&metadata, "windows", "amd64")?;
    let planned_paths: BTreeSet<_> = plan.artifacts.iter().map(|a| a.path.as_str()).collect();
    let mut classpath = Vec::new();
    let mut seen = BTreeSet::new();
    for library in &metadata.libraries {
        if !decision(library.get("rules"))? {
            continue;
        }
        if let Some(path) = library
            .pointer("/downloads/artifact/path")
            .and_then(Value::as_str)
        {
            let relative = format!("libraries/{path}");
            if !planned_paths.contains(relative.as_str()) {
                return Err("unplanned_classpath".into());
            }
            if seen.insert(relative.to_ascii_lowercase()) {
                classpath.push(root.join(relative));
            }
        }
    }
    classpath.push(root.join(format!("versions/{0}/{0}.jar", metadata.id)));
    let mut required_files = vec![
        RequiredFile::new(root.join("shtmcl-install.json"), ArtifactRole::Client),
        RequiredFile::new(
            root.join(format!("versions/{0}/{0}.json", metadata.id)),
            ArtifactRole::Client,
        ),
    ];
    for artifact in &plan.artifacts {
        required_files.push(RequiredFile::new(
            root.join(&artifact.path),
            if artifact.path.starts_with("assets/") {
                ArtifactRole::Asset
            } else {
                ArtifactRole::Library
            },
        ));
    }
    let mut asset_index = None;
    let mut game_assets = None;
    if let Some(index_path) = &plan.asset_index_path {
        let bytes = read_bounded(&root.join(index_path), 32 * 1024 * 1024)?;
        let declared = metadata
            .asset_index
            .as_ref()
            .ok_or("asset_index_metadata")?;
        if declared["size"].as_u64() != Some(bytes.len() as u64)
            || declared["sha1"].as_str().is_none_or(|hash| {
                !hash.eq_ignore_ascii_case(&format!("{:x}", Sha1::digest(&bytes)))
            })
        {
            return Err("asset_index_integrity".into());
        }
        for artifact in asset_artifacts(&bytes)? {
            required_files.push(RequiredFile::new(
                root.join(artifact.path),
                ArtifactRole::Asset,
            ));
        }
        let index: Value = serde_json::from_slice(&bytes).map_err(|_| "asset_index")?;
        let index_id = metadata
            .asset_index
            .as_ref()
            .and_then(|a| a["id"].as_str())
            .ok_or("asset_index_id")?;
        asset_index = Some(index_id.to_string());
        if index["virtual"] == true {
            game_assets = Some(format!("${{game_directory}}/assets/virtual/{index_id}"));
        }
        if index["virtual"] == true || index["map_to_resources"] == true {
            for name in index["objects"].as_object().ok_or("asset_objects")?.keys() {
                for relative in [
                    (index["virtual"] == true).then(|| format!("assets/virtual/{index_id}/{name}")),
                    (index["map_to_resources"] == true).then(|| format!("resources/{name}")),
                ]
                .into_iter()
                .flatten()
                {
                    required_files
                        .push(RequiredFile::new(root.join(relative), ArtifactRole::Asset));
                }
            }
        }
    }
    let (mut jvm_args, mut game_args) = if let Some(arguments) = &metadata.arguments {
        (
            match arguments.get("jvm") {
                Some(value) => argument_list(value)?,
                None => Vec::new(),
            },
            argument_list(arguments.get("game").ok_or("game_arguments")?)?,
        )
    } else {
        (
            Vec::new(),
            legacy_arguments(
                metadata
                    .minecraft_arguments
                    .as_deref()
                    .ok_or("legacy_game_arguments_missing")?,
            )?,
        )
    };
    if let Some(value) = game_assets {
        for arg in &mut game_args {
            *arg = arg.replace("${game_assets}", &value);
        }
    }
    if let Some(logging) = metadata.raw.pointer("/logging/client") {
        let file = &logging["file"];
        let file_id = file["id"]
            .as_str()
            .filter(|id| safe_path(id) && !id.contains('/'))
            .ok_or("logging_file_id")?;
        let relative = format!("assets/log_configs/{file_id}");
        required_files.push(RequiredFile::new(
            root.join(&relative),
            ArtifactRole::Library,
        ));
        let argument = logging["argument"].as_str().ok_or("logging_argument")?;
        let logging_url = reqwest::Url::from_file_path(root.join(&relative)).map_err(|_| "logging_file_url")?;
        jvm_args.push(argument.replace("${path}", logging_url.as_str()));
    }
    // Asset objects share only a few hundred parent directories. Check every
    // leaf, but avoid repeating all ancestor metadata probes for each object.
    let mut checked_parents = BTreeSet::new();
    for path in classpath
        .iter()
        .chain(required_files.iter().map(|file| &file.path))
    {
        if !path.starts_with(root) {
            return Err("launch_path_escape".into());
        }
        if let Some(parent) = path.parent() {
            let mut current = PathBuf::new();
            for component in parent.components() {
                current.push(component);
                if checked_parents.insert(current.clone()) {
                    reject_link(&current)?;
                }
            }
        }
        reject_link(path)?;
    }
    let requirement = shtmcl_core::launch::runtime::requirement_for(&metadata);
    if requirement.java == JavaRequirement::Any {
        return Err("unknown_java_requirement".into());
    }
    let java_requirement = if metadata.java_major.is_some_and(|major| major <= 8) {
        JavaRequirement::Exact(8)
    } else {
        requirement.java
    };
    check_path(&root.join("natives"))?;
    check_path(&root.join("assets"))?;
    Ok(InstanceLaunchSnapshot {
        instance_id: id.clone(),
        revision: Revision::new(instance.revision),
        content_revision: None,
        name: instance.name.clone(),
        game_version: metadata.id,
        version_type: metadata.raw["type"].as_str().map(str::to_string),
        loader: LoaderKind::parse(
            instance.version.loader.as_str(),
            instance.version.loader_version.as_deref(),
        ),
        game_dir: root.into(),
        main_class: metadata
            .main_class
            .filter(|name| !name.is_empty())
            .ok_or("main_class")?,
        classpath,
        assets_dir: asset_index.as_ref().map(|_| root.join("assets")),
        asset_index,
        natives_dir: Some(root.join("natives")),
        jvm_args,
        game_args,
        memory_mb: options.memory_mb,
        java_requirement: Some(java_requirement),
        java_path: options.java_path.clone(),
        required_files,
        auth: options.auth,
        offline_username: username,
        multi_open: false,
        profile_ref: instance.profile_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_quoted_arguments_and_rule_features_remain_structured() {
        assert_eq!(
            legacy_arguments("--name \"A B\" --empty \"\"").unwrap(),
            vec!["--name", "A B", "--empty", ""]
        );
        assert!(legacy_arguments("\"unfinished").is_err());
        let args = argument_list(&json!(["--username", "${auth_player_name}",
            {"rules":[{"action":"allow","features":{"is_demo_user":true}}],"value":"--demo"},
            {"rules":[{"action":"allow","os":{"name":"windows"}}],"value":["-cp","${classpath}"]}]))
        .unwrap();
        assert_eq!(
            args,
            vec!["--username", "${auth_player_name}", "-cp", "${classpath}"]
        );
        assert!(argument_list(&json!([{"rules":[{"action":"allow","os":{"name":"windows","version":".*"}}],"value":"unknown"}])).is_err());
    }

    struct SnapshotModule(Value);
    impl CoreModule for SnapshotModule {
        fn id(&self) -> shtmcl_domain::ModuleId {
            shtmcl_domain::ModuleId::Instances
        }
        fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
            assert_eq!(request.payload["command"], "instances.snapshot");
            Ok(CommandResponse::Completed {
                result: self.0.clone(),
            })
        }
    }
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "shtmcl-launch-snapshot-{}",
                shtmcl_platform::entropy::random_hex(12).unwrap()
            )))
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn options() -> LaunchOptions {
        LaunchOptions {
            auth: AuthRequirement::Offline,
            offline_username: Arc::new(RwLock::new(Some("SnapshotPlayer".into()))),
            memory_mb: Some(2048),
            java_path: None,
        }
    }

    fn setup_real_metadata(temp: &Temp) -> (PlatformDirs, Value) {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/fixtures/launch-metadata");
        let dirs = PlatformDirs::under(&temp.0);
        let root = dirs.instances.join("school");
        std::fs::create_dir_all(root.join("versions/1.7.10")).unwrap();
        std::fs::create_dir_all(root.join("assets/indexes")).unwrap();
        for relative in ["versions/1.7.10/1.7.10.json", "assets/indexes/1.7.10.json"] {
            std::fs::copy(source.join(relative), root.join(relative))
                .expect("retained real 1.7.10 metadata");
        }
        std::fs::write(
            root.join("shtmcl-install.json"),
            br#"{"schemaVersion":1,"complete":true,"instanceId":"school","versionId":"1.7.10"}"#,
        )
        .unwrap();
        let instance = json!({"instance":{"id":"school","name":"校园生存",
            "version":{"gameVersion":"1.7.10","loader":"vanilla"},"location":{"root":root},
            "state":"ready","revision":7,"createdAt":1,"updatedAt":2}});
        (dirs, instance)
    }

    #[test]
    fn preparation_does_not_require_files_that_it_will_repair() {
        let temp = Temp::new();
        let (dirs, instance) = setup_real_metadata(&temp);
        let source =
            InstalledInstances::new(Arc::new(SnapshotModule(instance)), dirs.clone(), options());
        let id = InstanceId::new("school").unwrap();
        std::fs::remove_file(dirs.instances.join("school/versions/1.7.10/1.7.10.json")).unwrap();
        std::fs::remove_file(dirs.instances.join("school/assets/indexes/1.7.10.json")).unwrap();
        let preparation = source.preparation(&id).unwrap();
        assert_eq!(preparation.revision.get(), 7);
        assert_eq!(
            preparation.java_requirement,
            Some(JavaRequirement::Exact(8))
        );
        assert_eq!(
            preparation.offline_username.as_deref(),
            Some("SnapshotPlayer")
        );
        assert!(source.snapshot(&id).is_err());
        std::fs::remove_file(dirs.instances.join("school/shtmcl-install.json")).unwrap();
        assert!(
            source.preparation(&id).is_err(),
            "committed receipt remains mandatory"
        );
    }

    #[test]
    fn real_1710_metadata_builds_ordered_snapshot_without_network_or_fake_launch() {
        let temp = Temp::new();
        let (dirs, instance) = setup_real_metadata(&temp);
        let selected = options();
        let names = selected.offline_username.clone();
        let source =
            InstalledInstances::new(Arc::new(SnapshotModule(instance)), dirs.clone(), selected);
        let snapshot = source
            .read_snapshot(&InstanceId::new("school").unwrap(), None)
            .unwrap();
        assert_eq!(snapshot.name, "校园生存");
        assert_eq!(snapshot.revision.get(), 7);
        assert_eq!(snapshot.main_class, "net.minecraft.client.main.Main");
        assert_eq!(
            snapshot.classpath.last().unwrap(),
            &dirs.instances.join("school/versions/1.7.10/1.7.10.jar")
        );
        assert!(snapshot
            .classpath
            .iter()
            .any(|p| p.to_string_lossy().contains("lwjgl-2.9.1.jar")));
        assert!(!snapshot
            .classpath
            .iter()
            .any(|p| p.to_string_lossy().contains("natives-windows")));
        assert!(snapshot.required_files.len() > 600);
        assert!(snapshot
            .jvm_args
            .iter()
            .any(|arg| arg.contains("assets/log_configs/client-1.7.xml")));
        assert_eq!(snapshot.java_requirement, Some(JavaRequirement::Exact(8)));
        *names.write().unwrap() = Some("SecondPlayer".into());
        assert_eq!(
            source
                .read_snapshot(&InstanceId::new("school").unwrap(), None)
                .unwrap()
                .offline_username
                .as_deref(),
            Some("SecondPlayer")
        );
        *names.write().unwrap() = None;
        assert!(source
            .read_snapshot(&InstanceId::new("school").unwrap(), None)
            .is_err());
    }

    #[test]
    fn receipt_and_index_root_are_required_to_match_actual_instance() {
        let temp = Temp::new();
        let (dirs, mut instance) = setup_real_metadata(&temp);
        let root = dirs.instances.join("school");
        instance["instance"]["location"]["root"] = json!(dirs.root.join("outside"));
        let source =
            InstalledInstances::new(Arc::new(SnapshotModule(instance)), dirs.clone(), options());
        assert!(source
            .read_snapshot(&InstanceId::new("school").unwrap(), None)
            .is_err());
        let (_, instance) = setup_real_metadata(&temp);
        let source = InstalledInstances::new(Arc::new(SnapshotModule(instance)), dirs, options());
        std::fs::write(
            root.join("shtmcl-install.json"),
            br#"{"schemaVersion":1,"complete":false,"instanceId":"school","versionId":"1.7.10"}"#,
        )
        .unwrap();
        assert!(source
            .read_snapshot(&InstanceId::new("school").unwrap(), None)
            .is_err());
        assert!(source
            .read_snapshot(&InstanceId::new("other").unwrap(), None)
            .is_err());
        std::fs::write(root.join("shtmcl-install.json"), vec![b' '; 4097]).unwrap();
        assert!(source
            .read_snapshot(&InstanceId::new("school").unwrap(), None)
            .is_err());
    }

    #[test]
    fn retained_real_modern_metadata_filters_platform_and_feature_arguments() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../tests/fixtures/launch-metadata/version-1.20.4.json");
        let bytes = std::fs::read(source).expect("retained real 1.20.4 official metadata");
        let metadata = VersionMetadata::parse(&bytes, Some("1.20.4")).unwrap();
        let args = metadata.arguments.as_ref().unwrap();
        let game = argument_list(&args["game"]).unwrap();
        let jvm = argument_list(&args["jvm"]).unwrap();
        assert!(game.contains(&"${clientid}".into()));
        assert!(game.contains(&"${auth_xuid}".into()));
        assert!(!game.contains(&"--demo".into()));
        assert!(!game.contains(&"--width".into()));
        assert!(!game.contains(&"--quickPlaySingleplayer".into()));
        assert!(jvm.contains(&"-cp".into()));
        assert!(!jvm.contains(&"-XstartOnFirstThread".into()));
        assert!(!jvm.contains(&"-Xss1M".into()));
        assert!(jvm.iter().any(|arg| arg.starts_with("-XX:HeapDumpPath=")));
    }
}

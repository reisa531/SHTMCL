//! Repairs only files named by the official, digest-pinned vanilla plan.
//! Replacement bytes are verified in private staging before atomic publication.
use crate::{
    game_install::{
        checked_directory, extract_natives, fetch_files, metadata_failure, reject_link,
    },
    launch_leases::InstanceLeases,
    version_metadata_source::CachedVersionMetadataSource,
};
use serde_json::json;
use shtmcl_core::{
    launch::{
        catalog::{VersionCatalog, VersionMetadata},
        install::{asset_artifacts, safe_path, InstallArtifact, VanillaInstallPlan},
        ports::LaunchResourcePreparer,
    },
    CoreModule, ModuleContext,
};
use shtmcl_domain::{
    AttemptGeneration, CommandRequest, CommandResponse, ErrorEnvelope, InstanceId, RequestId,
    Revision, TaskId, TaskPhase,
};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) struct VanillaResourcePreparer {
    ctx: ModuleContext,
    instances: Arc<dyn CoreModule>,
    leases: Arc<InstanceLeases>,
    catalog: Arc<VersionCatalog>,
    metadata: Arc<CachedVersionMetadataSource>,
    downloads: Arc<dyn CoreModule>,
    neoforge_checks: Option<Arc<crate::neoforge_snapshot::LaunchChecks>>,
}

impl VanillaResourcePreparer {
    pub(crate) fn new(
        ctx: ModuleContext,
        instances: Arc<dyn CoreModule>,
        leases: Arc<InstanceLeases>,
        catalog: Arc<VersionCatalog>,
        metadata: Arc<CachedVersionMetadataSource>,
        downloads: Arc<dyn CoreModule>,
    ) -> Self {
        Self {
            ctx,
            instances,
            leases,
            catalog,
            metadata,
            downloads,
            neoforge_checks: None,
        }
    }

    pub(crate) fn with_neoforge_checks(mut self, checks: Arc<crate::neoforge_snapshot::LaunchChecks>) -> Self {
        self.neoforge_checks = Some(checks); self
    }

    fn run(
        &self,
        id: &InstanceId,
        holder: &str,
        expected_revision: Option<Revision>,
        task: &TaskId,
        generation: AttemptGeneration,
    ) -> Result<(), String> {
        let cancelled = || self.ctx.tasks().is_cancel_requested(task, generation);
        check_cancel(&cancelled)?;
        let _permit = self
            .leases
            .begin_preparation(id, holder)
            .map_err(|_| "preparation_busy")?;
        if !self.leases.owns(id, holder).map_err(|_| "lease_registry")? {
            return Err("lease_required".into());
        }
        let instance_id = id.as_str();
        if instance_id.len() > 64
            || !safe_path(instance_id)
            || !instance_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("instance_id".into());
        }
        let CommandResponse::Completed { result } = self
            .instances
            .handle(&CommandRequest {
                request_id: RequestId::new("repair-instance-snapshot").unwrap(),
                operation_id: None,
                entity_id: None,
                expected_revision: None,
                payload: json!({"command":"instances.snapshot","instanceId":instance_id}),
            })
            .map_err(|_| "instance_snapshot")?
        else {
            return Err("instance_snapshot_pending".into());
        };
        let instance: shtmcl_core::instances::Instance =
            serde_json::from_value(result["instance"].clone())
                .map_err(|_| "instance_snapshot_invalid")?;
        if instance.id != instance_id
            || !matches!(instance.version.loader,shtmcl_core::instances::LoaderKind::Vanilla|shtmcl_core::instances::LoaderKind::Fabric|shtmcl_core::instances::LoaderKind::NeoForge)
            || instance.state != shtmcl_core::instances::InstanceState::Busy
            || instance.revision == 0
        {
            return Err("instance_not_busy_vanilla".into());
        }
        if expected_revision.is_some_and(|revision| revision.get() != instance.revision) {
            return Err("instance_revision_conflict".into());
        }
        let root = self.ctx.dirs().instances.join(instance_id);
        if Path::new(&instance.location.root) != root {
            return Err("instance_root_mismatch".into());
        }
        checked_path(&root)?;
        let version = instance.version.game_version.as_str();
        if !safe_path(version) || version.contains('/') {
            return Err("version_id".into());
        }
        let receipt: serde_json::Value =
            serde_json::from_slice(&read_bounded(&root.join("shtmcl-install.json"), 4096)?)
                .map_err(|_| "install_receipt")?;
        if receipt["schemaVersion"] != 1
            || receipt["complete"] != true
            || receipt["instanceId"] != instance_id
            || receipt["versionId"] != version
        {
            return Err("install_receipt_mismatch".into());
        }
        if instance.version.loader == shtmcl_core::instances::LoaderKind::NeoForge {
            if version != "1.20.4" || instance.version.loader_version.as_deref() != Some("20.4.253")
                || receipt["loader"] != "neoforge" || receipt["loaderVersion"] != "20.4.253" {
                return Err("neoforge_receipt_identity".into());
            }
            let digest = receipt["preparedSha256"].as_str().ok_or("neoforge_receipt_identity")?;
            let _ = self.ctx.tasks().set_phase(task,generation,TaskPhase::new("verify_resources").unwrap());
            self.neoforge_checks.as_ref().ok_or("neoforge_checks_unavailable")?.prepare(&instance,&root,holder,digest,&cancelled)?;
            check_cancel(&cancelled)?;
            return crate::game_language::initialize(&root, version);
        }
        let repair_root = self.ctx.dirs().data.join("resource-repair");
        checked_directory(&repair_root)?;
        let lock_path = repair_root.join(format!("{instance_id}.lock"));
        reject_link(&lock_path)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)
            .map_err(|_| "repair_lock")?;
        lock.try_lock().map_err(|_| "repair_busy")?;
        let stage = repair_root.join(instance_id);
        checked_directory(&stage)?;
        let _ = self.ctx.tasks().set_phase(
            task,
            generation,
            TaskPhase::new("verify_resources").unwrap(),
        );
        let fabric = instance.version.loader == shtmcl_core::instances::LoaderKind::Fabric;
        if fabric && (receipt["loader"]!="fabric" || receipt["loaderVersion"].as_str()!=instance.version.loader_version.as_deref()) {return Err("fabric_receipt_identity".into());}
        if !fabric && self.catalog.list(None, None, 0, 1).is_err() {
            self.catalog.refresh().map_err(|_| "catalog_refresh")?;
        }
        let resolved = if fabric {
            shtmcl_core::launch::catalog::ResolvedVersion {
                metadata:crate::fabric_metadata::pinned(self.ctx.dirs(),&root,&receipt,version,instance.version.loader_version.as_deref().ok_or("fabric_loader_version")?)?,
                inheritance_chain:vec![version.into()],
            }
        } else {VersionMetadata::resolve(self.metadata.as_ref(), version).map_err(metadata_failure)?};
        let plan = VanillaInstallPlan::parse(&resolved.metadata, "windows", "amd64")?;
        let store = shtmcl_core::downloads::FileStore::new(self.ctx.dirs().cache.join("downloads"))
            .map_err(|_| "cache_open")?;
        let mut batch = 0;
        self.repair_artifacts(
            &root,
            &stage,
            &plan.artifacts,
            &store,
            task,
            generation,
            &mut batch,
        )?;
        if let Some(index_path) = &plan.asset_index_path {
            let bytes = read_bounded(&root.join(index_path), 32 * 1024 * 1024)?;
            let assets = asset_artifacts(&bytes)?;
            self.repair_artifacts(&root, &stage, &assets, &store, task, generation, &mut batch)?;
            let index: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| "asset_index_parse")?;
            let index_id = Path::new(index_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or("asset_index_id")?;
            for (name, value) in index["objects"].as_object().ok_or("asset_objects")? {
                check_cancel(&cancelled)?;
                if !safe_path(name) {
                    return Err("asset_mapping_path".into());
                }
                let hash = value["hash"].as_str().ok_or("asset_hash")?;
                let source = root.join(format!("assets/objects/{}/{hash}", &hash[..2]));
                for prefix in [
                    index["virtual"]
                        .as_bool()
                        .filter(|v| *v)
                        .map(|_| format!("assets/virtual/{index_id}")),
                    index["map_to_resources"]
                        .as_bool()
                        .filter(|v| *v)
                        .map(|_| "resources".into()),
                ]
                .into_iter()
                .flatten()
                {
                    let relative = format!("{prefix}/{name}");
                    let destination = root.join(&relative);
                    if !same_file_content(&source, &destination, &cancelled)? {
                        stage_copy_publish(
                            &source,
                            &stage.join("mapping").join(&relative),
                            &destination,
                            &cancelled,
                        )?;
                    }
                }
            }
        }
        // Only native archives are copied, never the whole instance. Extraction
        // checks ZIP CRC and destination conflicts before touching live DLLs.
        for archive in &plan.native_archives {
            check_cancel(&cancelled)?;
            let source = root.join(archive);
            let destination = stage.join(archive);
            if !same_file_content(&source, &destination, &cancelled)? {
                copy_verified(&source, &destination, &cancelled)?;
            }
        }
        extract_natives(&stage, &plan.native_archives, &cancelled)?;
        // Use the exact ZIP entry set, not stale files left by a prior attempt.
        let native_paths = native_entries(&stage, &plan.native_archives, &cancelled)?;
        for relative in native_paths {
            let source = stage.join("natives").join(&relative);
            let destination = root.join("natives").join(&relative);
            if !same_file_content(&source, &destination, &cancelled)? {
                publish(&source, &destination, &cancelled)?;
            }
        }
        let metadata_bytes =
            serde_json::to_vec_pretty(&resolved.metadata.raw).map_err(|_| "version_serialize")?;
        let relative = format!("versions/{version}/{version}.json");
        let destination = root.join(&relative);
        if read_bounded(&destination, 8 * 1024 * 1024).ok().as_deref()
            != Some(metadata_bytes.as_slice())
        {
            let replacement = stage.join(&relative);
            write_synced(&replacement, &metadata_bytes)?;
            publish(&replacement, &destination, &cancelled)?;
        }
        check_cancel(&cancelled)?;
        crate::game_language::initialize(&root, version)
    }

    fn repair_artifacts(
        &self,
        root: &Path,
        stage: &Path,
        artifacts: &[InstallArtifact],
        store: &shtmcl_core::downloads::FileStore,
        task: &TaskId,
        generation: AttemptGeneration,
        batch: &mut usize,
    ) -> Result<(), String> {
        let cancelled = || self.ctx.tasks().is_cancel_requested(task, generation);
        let mut broken = Vec::new();
        for artifact in artifacts {
            check_cancel(&cancelled)?;
            if !matches_artifact(&root.join(&artifact.path), artifact, &cancelled)? {
                broken.push(artifact.clone());
            }
        }
        if broken.is_empty() {
            return Ok(());
        }
        fetch_files(
            &self.ctx,
            task,
            generation,
            stage,
            store,
            &self.downloads,
            &broken,
            batch,
        )?;
        for artifact in broken {
            let source = stage.join(&artifact.path);
            if !matches_artifact(&source, &artifact, &cancelled)? {
                return Err("repair_digest".into());
            }
            publish(&source, &root.join(&artifact.path), &cancelled)?;
        }
        Ok(())
    }
}

impl LaunchResourcePreparer for VanillaResourcePreparer {
    fn prepare(
        &self,
        id: &InstanceId,
        holder: &str,
        expected_revision: Option<Revision>,
        task: &TaskId,
        generation: AttemptGeneration,
    ) -> Result<(), ErrorEnvelope> {
        self.run(id, holder, expected_revision, task, generation)
            .map_err(|reason| {
                shtmcl_core::launch::errors::plan_invalid(format!("resource-repair:{reason}"))
            })
    }
}

fn check_cancel(cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    if cancelled() {
        Err("cancelled".into())
    } else {
        Ok(())
    }
}
fn checked_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path.components().any(|p| {
            matches!(
                p,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err("repair_path".into());
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        reject_link(&current)?;
    }
    Ok(())
}
fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>, String> {
    checked_path(path)?;
    let file = std::fs::File::open(path).map_err(|_| "repair_file_read")?;
    if !file
        .metadata()
        .map_err(|_| "repair_file_metadata")?
        .is_file()
        || file.metadata().map_err(|_| "repair_file_metadata")?.len() > max as u64
    {
        return Err("repair_file_size".into());
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "repair_file_read")?;
    if bytes.len() > max {
        return Err("repair_file_size".into());
    }
    Ok(bytes)
}
fn digest_file(path: &Path, cancelled: &dyn Fn() -> bool) -> Result<Option<(u64, String)>, String> {
    use sha1::{Digest, Sha1};
    checked_path(path)?;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("repair_file_read".into()),
    };
    if !file
        .metadata()
        .map_err(|_| "repair_file_metadata")?
        .is_file()
    {
        return Err("repair_not_file".into());
    }
    let mut digest = Sha1::new();
    let mut length = 0;
    let mut buffer = [0; 65536];
    loop {
        check_cancel(cancelled)?;
        let count = file.read(&mut buffer).map_err(|_| "repair_hash")?;
        if count == 0 {
            break;
        }
        length += count as u64;
        digest.update(&buffer[..count]);
    }
    Ok(Some((length, format!("{:x}", digest.finalize()))))
}
fn matches_artifact(
    path: &Path,
    artifact: &InstallArtifact,
    cancelled: &dyn Fn() -> bool,
) -> Result<bool, String> {
    Ok(digest_file(path, cancelled)?
        .is_some_and(|(size, hash)| size == artifact.size && hash == artifact.sha1))
}
fn same_file_content(
    source: &Path,
    destination: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<bool, String> {
    let source = digest_file(source, cancelled)?.ok_or("repair_source_missing")?;
    Ok(digest_file(destination, cancelled)?.is_some_and(|actual| actual == source))
}
fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), String> {
    checked_directory(path.parent().ok_or("repair_parent")?)?;
    reject_link(path)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|_| "repair_stage_write")?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "repair_stage_write".into())
}
fn copy_verified(
    source: &Path,
    destination: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    checked_path(source)?;
    checked_directory(destination.parent().ok_or("repair_parent")?)?;
    reject_link(destination)?;
    let mut input = std::fs::File::open(source).map_err(|_| "repair_source_read")?;
    let mut output = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(destination)
        .map_err(|_| "repair_stage_write")?;
    let mut buffer = [0; 65536];
    loop {
        check_cancel(cancelled)?;
        let count = input.read(&mut buffer).map_err(|_| "repair_source_read")?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|_| "repair_stage_write")?;
    }
    output.sync_all().map_err(|_| "repair_stage_sync")?;
    drop(output);
    if !same_file_content(source, destination, cancelled)? {
        return Err("repair_copy_digest".into());
    }
    Ok(())
}
fn stage_copy_publish(
    source: &Path,
    stage: &Path,
    destination: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    copy_verified(source, stage, cancelled)?;
    publish(stage, destination, cancelled)
}
fn publish(source: &Path, destination: &Path, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    check_cancel(cancelled)?;
    checked_path(source)?;
    checked_directory(destination.parent().ok_or("repair_parent")?)?;
    reject_link(destination)?;
    // Windows rename replaces the destination atomically on the same volume.
    // The old file remains intact if replacement fails; never remove it first.
    std::fs::rename(source, destination).map_err(|_| "repair_publish".into())
}
fn native_entries(
    stage: &Path,
    archives: &[String],
    cancelled: &dyn Fn() -> bool,
) -> Result<std::collections::BTreeSet<String>, String> {
    use shtmcl_core::content::zipindex::{read_index, ZipBudget};
    let budget = ZipBudget {
        max_entries: 4096,
        max_total_uncompressed: 256 * 1024 * 1024,
        max_entry_uncompressed: 64 * 1024 * 1024,
    };
    let mut paths = std::collections::BTreeSet::new();
    for archive in archives {
        check_cancel(cancelled)?;
        let bytes = read_bounded(&stage.join(archive), 128 * 1024 * 1024)?;
        for entry in read_index(&bytes, &budget)
            .map_err(|_| "repair_native_zip")?
            .entries
        {
            if !entry.is_directory() && !entry.name.starts_with("META-INF/") {
                if !safe_path(&entry.name) {
                    return Err("repair_native_path".into());
                }
                paths.insert(entry.name);
            }
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct NoNetwork;
    impl shtmcl_core::launch::catalog::ManifestFetcher for NoNetwork {
        fn fetch(&self, _: &str, _: usize) -> Result<Vec<u8>, String> {
            panic!("repair must use cached metadata")
        }
    }
    struct NoDownloads;
    impl CoreModule for NoDownloads {
        fn id(&self) -> shtmcl_domain::ModuleId {
            shtmcl_domain::ModuleId::Downloads
        }
        fn handle(&self, _: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
            Err(ErrorEnvelope::internal_unavailable())
        }
    }
    fn root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-repair-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    #[test]
    fn same_length_damage_is_detected_and_atomic_replace_preserves_user_files() {
        use sha1::{Digest, Sha1};
        let root = root();
        let source = root.join("staged.jar");
        let target = root.join("client.jar");
        let sentinel = root.join("mods/user.jar");
        write_synced(&source, b"correct").unwrap();
        write_synced(&target, b"damaged").unwrap();
        write_synced(&sentinel, b"user mod").unwrap();
        let artifact = InstallArtifact {
            path: "client.jar".into(),
            url: String::new(),
            sha1: format!("{:x}", Sha1::digest(b"correct")),
            size: 7,
        };
        assert!(!matches_artifact(&target, &artifact, &|| false).unwrap());
        assert!(publish(&source, &target, &|| true).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"damaged");
        publish(&source, &target, &|| false).unwrap();
        assert!(matches_artifact(&target, &artifact, &|| false).unwrap());
        assert_eq!(std::fs::read(sentinel).unwrap(), b"user mod");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn failed_publish_keeps_original_and_retry_source() {
        let root = root();
        let source = root.join("staged");
        let target = root.join("directory");
        write_synced(&source, b"new").unwrap();
        std::fs::create_dir(&target).unwrap();
        assert!(publish(&source, &target, &|| false).is_err());
        assert_eq!(std::fs::read(source).unwrap(), b"new");
        assert!(target.is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn missing_and_cancelled_hash_checks_do_not_report_valid() {
        let root = root();
        assert_eq!(digest_file(&root.join("missing"), &|| false).unwrap(), None);
        write_synced(&root.join("file"), b"data").unwrap();
        assert!(digest_file(&root.join("file"), &|| true).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pinned_disk_cache_restores_missing_and_same_size_damage_without_downloads() {
        use sha1::{Digest, Sha1};
        use shtmcl_core::{
            downloads::{ArtifactHash, FileStore},
            tasks::TaskSpec,
        };
        use shtmcl_domain::{OperationId, TaskState};
        let directory = root();
        let dirs = shtmcl_platform::PlatformDirs::under(&directory);
        let app = shtmcl_core::CoreApp::new(dirs.clone());
        let ctx = app.context();
        let downloads: Arc<dyn CoreModule> = Arc::new(NoDownloads);
        let preparer = VanillaResourcePreparer::new(
            ctx.clone(),
            downloads.clone(),
            Arc::new(InstanceLeases::new(downloads.clone())),
            Arc::new(VersionCatalog::new(
                directory.join("metadata"),
                Arc::new(NoNetwork),
            )),
            Arc::new(CachedVersionMetadataSource::new(
                shtmcl_core::launch::catalog::CatalogCache::new(directory.join("metadata")),
                Arc::new(NoNetwork),
            )),
            downloads,
        );
        let store = FileStore::new(dirs.cache.join("downloads")).unwrap();
        let bytes = b"verified cached file";
        let digest = format!("{:x}", Sha1::digest(bytes));
        store
            .open_partial("fixture", 0)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        store.commit_partial("fixture").unwrap();
        store
            .publish(&ArtifactHash::new("sha1", &digest).unwrap(), "fixture")
            .unwrap();
        let target = dirs.instances.join("test");
        write_synced(&target.join("libraries/bad.jar"), &vec![b'x'; bytes.len()]).unwrap();
        write_synced(&target.join("libraries/good.jar"), bytes).unwrap();
        write_synced(&target.join("saves/world/level.dat"), b"world").unwrap();
        let before = std::fs::metadata(target.join("libraries/good.jar"))
            .unwrap()
            .modified()
            .unwrap();
        let artifacts = [
            "versions/test/test.jar",
            "libraries/bad.jar",
            "libraries/good.jar",
        ]
        .into_iter()
        .map(|path| InstallArtifact {
            path: path.into(),
            url: "https://piston-data.mojang.com/fixture".into(),
            sha1: digest.clone(),
            size: bytes.len() as u64,
        })
        .collect::<Vec<_>>();
        let task = ctx
            .tasks()
            .accept(TaskSpec::new(
                OperationId::new("repair-cache-test").unwrap(),
                TaskPhase::new("verify_resources").unwrap(),
            ))
            .unwrap();
        ctx.tasks()
            .transition(&task.id, task.attempt_generation, TaskState::Running)
            .unwrap();
        preparer
            .repair_artifacts(
                &target,
                &directory.join("stage"),
                &artifacts,
                &store,
                &task.id,
                task.attempt_generation,
                &mut 0,
            )
            .unwrap();
        for artifact in &artifacts {
            assert_eq!(std::fs::read(target.join(&artifact.path)).unwrap(), bytes);
        }
        assert_eq!(
            std::fs::metadata(target.join("libraries/good.jar"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
        assert_eq!(
            std::fs::read(target.join("saves/world/level.dat")).unwrap(),
            b"world"
        );
        // A verified cache object tampered with after publication must never
        // replace live bytes, even when its size still matches the receipt.
        write_synced(&target.join("libraries/bad.jar"), b"keep old").unwrap();
        let cache_file = dirs
            .cache
            .join("downloads/objects/sha1")
            .join(&digest[..2])
            .join(&digest);
        write_synced(&cache_file, &vec![b'z'; bytes.len()]).unwrap();
        assert!(preparer
            .repair_artifacts(
                &target,
                &directory.join("stage"),
                &artifacts,
                &store,
                &task.id,
                task.attempt_generation,
                &mut 0
            )
            .is_err());
        assert_eq!(
            std::fs::read(target.join("libraries/bad.jar")).unwrap(),
            b"keep old"
        );
        drop(preparer);
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

//! Application composition for vanilla installation; downloads stay in module 03.
#[cfg(test)]
#[path = "game_install_lifecycle_tests.rs"]
mod lifecycle_tests;
use crate::{
    install_operations::{InstallOperation, InstallOperations},
    runtime_export,
    version_metadata_source::CachedVersionMetadataSource,
};
use serde_json::json;
use shtmcl_core::launch::{
    catalog::{VersionCatalog, VersionMetadata},
    install::{asset_artifacts, safe_path, InstallArtifact, VanillaInstallPlan},
};
use shtmcl_core::{tasks::TaskSpec, CoreModule, ModuleContext};
use shtmcl_domain::{
    AttemptGeneration, CancelTaskRequest, CommandKind, CommandRequest, CommandResponse,
    ErrorEnvelope, ModuleId, OperationId, RequestId, TaskId, TaskPhase, TaskState,
};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub(crate) struct GameInstaller {
    ctx: ModuleContext,
    launch: Arc<dyn CoreModule>,
    catalog: Arc<VersionCatalog>,
    metadata: Arc<CachedVersionMetadataSource>,
    downloads: Arc<dyn CoreModule>,
    instances: Option<Arc<dyn CoreModule>>,
    busy: Arc<AtomicBool>,
}

impl GameInstaller {
    pub(crate) fn new(
        ctx: ModuleContext,
        launch: Arc<dyn CoreModule>,
        catalog: Arc<VersionCatalog>,
        metadata: Arc<CachedVersionMetadataSource>,
        downloads: Arc<dyn CoreModule>,
        instances: Option<Arc<dyn CoreModule>>,
    ) -> Self {
        Self {
            ctx,
            launch,
            catalog,
            metadata,
            downloads,
            instances,
            busy: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl CoreModule for GameInstaller {
    fn id(&self) -> ModuleId {
        ModuleId::Launch
    }
    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        if request.command()?.as_str() == "launch.neoforge.versions" {
            request.validate(CommandKind::Read)?;
            let game = request.payload["gameVersion"]
                .as_str()
                .ok_or_else(|| shtmcl_core::launch::errors::request_invalid("gameVersion"))?;
            let entries = crate::neoforge_metadata::versions(game).map_err(|error| {
                let (code, category, retryable) = match error {
                    "neoforge_game_version_invalid" | "neoforge_game_version_unsupported" => {
                        return shtmcl_core::launch::errors::request_invalid("gameVersion");
                    }
                    "neoforge_version_unavailable" => (
                        "launch.neoforge.versions_unavailable",
                        shtmcl_domain::ErrorCategory::NotFound,
                        false,
                    ),
                    "neoforge_metadata_too_large" | "neoforge_response_too_large" => (
                        "launch.neoforge.response_too_large",
                        shtmcl_domain::ErrorCategory::Network,
                        false,
                    ),
                    "neoforge_metadata_invalid"
                    | "neoforge_metadata_encoding"
                    | "neoforge_version_invalid"
                    | "neoforge_version_count_exceeded" => (
                        "launch.neoforge.catalog_invalid",
                        shtmcl_domain::ErrorCategory::Network,
                        false,
                    ),
                    _ => (
                        "launch.neoforge.catalog_unavailable",
                        shtmcl_domain::ErrorCategory::Network,
                        true,
                    ),
                };
                ErrorEnvelope::new(
                    shtmcl_domain::ErrorCode::from_static(code),
                    category,
                    retryable,
                )
                .with_detail_ref(format!("neoforge:{error}"))
            })?;
            return Ok(CommandResponse::Completed {
                result: json!({"gameVersion":game,"entries":entries}),
            });
        }
        if request.command()?.as_str() == "launch.fabric.versions" {
            request.validate(CommandKind::Read)?;
            let game = request.payload["gameVersion"].as_str().ok_or_else(||shtmcl_core::launch::errors::request_invalid("gameVersion"))?;
            let entries = crate::fabric_metadata::versions(game).map_err(|_| ErrorEnvelope::new(shtmcl_domain::ErrorCode::from_static("launch.fabric.catalog_unavailable"),shtmcl_domain::ErrorCategory::Network,true))?;
            return Ok(CommandResponse::Completed {result:json!({"gameVersion":game,"entries":entries})});
        }
        if request.command()?.as_str() == "launch.installations.list" {
            request.validate(CommandKind::Read)?;
            let entries = InstallOperations::new(self.ctx.dirs())
                .presentations(&self.ctx.tasks().snapshots())
                .map_err(|_| ErrorEnvelope::internal_unavailable())?;
            return Ok(CommandResponse::Completed {
                result: json!({"entries": entries}),
            });
        }
        if request
            .command()?
            .as_str()
            .starts_with("launch.environments.")
        {
            return crate::environment_cache::handle(
                &self.ctx,
                &self.catalog,
                &self.metadata,
                &self.downloads,
                &self.busy,
                request,
            );
        }
        if request.command()?.as_str() != "launch.install" {
            return self.launch.handle(request);
        }
        request.validate(CommandKind::Mutate)?;
        let version = request.payload["versionId"]
            .as_str()
            .filter(|id| safe_path(id) && !id.contains('/'))
            .ok_or_else(|| shtmcl_core::launch::errors::request_invalid("versionId"))?
            .to_string();
        let instance = request.payload["instanceId"]
            .as_str()
            .filter(|id| {
                !id.is_empty()
                    && safe_path(id)
                    && id.len() <= 64
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            .ok_or_else(|| shtmcl_core::launch::errors::request_invalid("instanceId"))?
            .to_string();
        let name = install_name(&request.payload, &instance)?;
        let fabric = match request.payload.get("loader") {
            None => None,
            Some(value) if value.as_str()==Some("vanilla") => None,
            Some(value) if value.as_str()==Some("fabric") => Some(request.payload["loaderVersion"].as_str().filter(|v|shtmcl_core::content::fabric::valid_version(v)).ok_or_else(||shtmcl_core::launch::errors::request_invalid("loaderVersion"))?.to_string()),
            _ => return Err(shtmcl_core::launch::errors::request_invalid("loader")),
        };
        let environment = request
            .payload
            .get("environmentId")
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| shtmcl_core::launch::errors::request_invalid("environmentId"))
            })
            .transpose()?;
        let mut cached = environment
            .map(|id| crate::environment_cache::acquire_metadata(self.ctx.dirs(), id, &version))
            .transpose()
            .map_err(|_| shtmcl_core::launch::errors::request_invalid("environmentId"))?;
        let pinned = InstallOperations::new(self.ctx.dirs()).source_metadata(&request.payload, &version)
            .map_err(|_| crate::install_recovery::recovery_error("metadata"))?;
        let source = crate::install_operations::recovery_source(&request.payload)?;
        if let (Some(raw), Some((recipe, _))) = (&pinned, &cached) {
            if raw != recipe { return Err(crate::install_recovery::recovery_error("metadata")); }
        }
        if self.instances.is_none() || self.busy.swap(true, Ordering::AcqRel) {
            return Err(shtmcl_domain::ErrorEnvelope::new(
                shtmcl_domain::ErrorCode::from_static("launch.install.busy"),
                shtmcl_domain::ErrorCategory::Conflict,
                false,
            ));
        }
        let operation = request
            .operation_id
            .clone()
            .ok_or_else(ErrorEnvelope::command_operation_required)?;
        let task = match self.ctx.tasks().accept(TaskSpec::new(
            operation.clone(),
            TaskPhase::new("install").unwrap(),
        )) {
            Ok(task) => task,
            Err(error) => {
                self.busy.store(false, Ordering::Release);
                return Err(error);
            }
        };
        let (ctx, catalog, metadata, downloads, instances, busy) = (
            self.ctx.clone(),
            self.catalog.clone(),
            self.metadata.clone(),
            self.downloads.clone(),
            self.instances.clone().unwrap(),
            self.busy.clone(),
        );
        let (task_id, generation) = (task.id.clone(), task.attempt_generation);
        let record = InstallOperation::instance(
            operation.clone(),
            task_id.clone(),
            &version,
            &instance,
            &name,
            self.ctx.dirs(),
        ).with_recovery(crate::install_operations::InstallSelection {
            loader: if fabric.is_some() { "fabric" } else { "vanilla" }.into(),
            loader_version: fabric.clone(),
            environment_id: environment.map(str::to_string),
        }).with_source(source);
        let record_lease = match InstallOperations::new(self.ctx.dirs()).insert(&record) {
            Ok(lease) => lease,
            Err(_) => {
                self.busy.store(false, Ordering::Release);
                let _ = self.ctx.tasks().fail(
                    &task_id,
                    generation,
                    ErrorEnvelope::internal_unavailable(),
                );
                return Err(ErrorEnvelope::internal_unavailable());
            }
        };
        let job_id = task_id.clone();
        let persisted = record_execution(&ctx, &task_id).and_then(|_| {
            match pinned.as_ref().or_else(|| cached.as_ref().map(|(raw, _)| raw)) {
                Some(raw) => InstallOperations::new(ctx.dirs()).pin_metadata(&task, raw),
                None => Ok(()),
            }
        });
        if persisted.is_err() {
            self.busy.store(false, Ordering::Release);
            let _ = ctx
                .tasks()
                .fail(&task_id, generation, ErrorEnvelope::internal_unavailable());
            return Err(ErrorEnvelope::internal_unavailable());
        }
        let spawn = std::thread::Builder::new().name("game-install".into()).spawn(move || {
            let _record_lease = record_lease;
            let _busy = BusyGuard(busy);
            let (cached, _environment_lease) = match cached.take() { Some((metadata,lease)) => (Some(metadata),Some(lease)), None => (pinned,None) };
            let _ = ctx.tasks().transition(&job_id, generation, TaskState::Running);
            let _ = record_execution(&ctx, &job_id);
            let result = install(&ctx, &job_id, generation, &version, &instance, &name, catalog, metadata, downloads, instances, cached, fabric.as_deref());
            match result {
                Ok(path) => { let _ = ctx.tasks().succeed(&job_id, generation, Some(json!({"instanceId":instance,"versionId":version,"path":path,"installed":true,"launchReady":false}))); }
                Err(error) if error == "cancelled" => { let _ = ctx.tasks().cancel_now(&job_id, generation); }
                Err(error) => { let _ = ctx.tasks().fail(&job_id, generation, ErrorEnvelope::new(shtmcl_domain::ErrorCode::from_static("launch.install.failed"), shtmcl_domain::ErrorCategory::Internal, false).with_detail_ref(format!("install:{error}"))); }
            }
            if let Err(error) = record_execution(&ctx, &job_id) { eprintln!("SHTMCL installation history: {error}"); }
        });
        if spawn.is_err() {
            self.busy.store(false, Ordering::Release);
            let _ =
                self.ctx
                    .tasks()
                    .fail(&task_id, generation, ErrorEnvelope::internal_unavailable());
            let _ = record_execution(&self.ctx, &task_id);
            return Err(ErrorEnvelope::internal_unavailable());
        }
        Ok(CommandResponse::Accepted {
            operation_id: operation,
            task_id,
        })
    }
}

pub(crate) fn record_execution(ctx: &ModuleContext, task: &TaskId) -> Result<(), String> {
    let snapshot = ctx.tasks().snapshot(task).ok_or("history_task_missing")?;
    for attempt in 0..20 {
        match crate::operation_history::OperationHistory::new(ctx.dirs()).record(&snapshot) {
            Err(error) if error == "history_busy" && attempt < 19 => {
                std::thread::sleep(std::time::Duration::from_millis(25))
            }
            result => return result,
        }
    }
    unreachable!()
}

pub(crate) fn install_name(payload: &serde_json::Value, instance: &str) -> Result<String, ErrorEnvelope> {
    match payload.get("name") {
        None => Ok(instance.to_string()),
        Some(value) => value
            .as_str()
            .map(str::trim)
            .filter(|name| {
                !name.is_empty()
                    && name.chars().count() <= 80
                    && !name.chars().any(char::is_control)
            })
            .map(str::to_string)
            .ok_or_else(|| shtmcl_core::launch::errors::request_invalid("name")),
    }
}

pub(crate) struct BusyGuard(pub(crate) Arc<AtomicBool>);
impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn install(
    ctx: &ModuleContext,
    task: &TaskId,
    generation: AttemptGeneration,
    version: &str,
    instance: &str,
    name: &str,
    catalog: Arc<VersionCatalog>,
    metadata: Arc<CachedVersionMetadataSource>,
    downloads: Arc<dyn CoreModule>,
    instances: Arc<dyn CoreModule>,
    cached: Option<serde_json::Value>,
    fabric: Option<&str>,
) -> Result<String, String> {
    let cancelled = || ctx.tasks().is_cancel_requested(task, generation);
    let _ = ctx
        .tasks()
        .set_phase(task, generation, TaskPhase::new("metadata").unwrap());
    if cancelled() {
        return Err("cancelled".into());
    }
    if !cfg!(all(windows, target_arch = "x86_64")) {
        return Err("unsupported_install_platform".into());
    }
    checked_directory(&ctx.dirs().instances)?;
    let lock_path = ctx
        .dirs()
        .instances
        .join(format!(".install-{instance}.lock"));
    let lock_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|_| "installation_locked")?;
    lock_file.try_lock().map_err(|_| "installation_locked")?;
    let _lock = lock_file;
    let target = ctx.dirs().instances.join(instance);
    let stage = ctx.dirs().instances.join(format!(".install-{instance}"));
    if target.exists() {
        reject_link(&target)?;
        let snapshot = CommandRequest {
            request_id: RequestId::new("install-recovery-read").unwrap(),
            operation_id: None,
            entity_id: None,
            expected_revision: None,
            payload: json!({"command":"instances.snapshot","instanceId":instance}),
        };
        match instances.handle(&snapshot) {
            Err(error) if error.code.as_str() == "instances.instance.not_found" => {
                recover_published_staging(&target, &stage, instance, version)?
            }
            _ => return Err("instance_destination_exists".into()),
        }
    }
    if stage
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err("linked_staging".into());
    }
    checked_directory(&stage)?;
    let result = (|| -> Result<String, String> {
        // Metadata source verifies the manifest-pinned version SHA-1 and cache.
        if cached.is_none() && catalog.list(None, None, 0, 1).is_err() {
            catalog.refresh().map_err(|_| "catalog_refresh")?;
        }
        let mut resolved = match cached {
            Some(raw) => shtmcl_core::launch::catalog::ResolvedVersion {
                metadata: VersionMetadata::from_value(raw, Some(version)).map_err(metadata_failure)?,
                inheritance_chain: vec![version.into()],
            },
            None => VersionMetadata::resolve(metadata.as_ref(), version).map_err(metadata_failure)?,
        };
        InstallOperations::new(ctx.dirs()).pin_metadata(
            &ctx.tasks().snapshot(task).ok_or("install_task_missing")?, &resolved.metadata.raw)?;
        if let Some(loader) = fabric {
            resolved.metadata = crate::fabric_metadata::prepare(&resolved.metadata,loader,&cancelled)?;
        }
        let plan = VanillaInstallPlan::parse(&resolved.metadata, "windows", "amd64")?;
        let store = shtmcl_core::downloads::FileStore::new(ctx.dirs().cache.join("downloads"))
            .map_err(|_| "cache_open")?;
        let mut batch = 0usize;
        let _ = ctx
            .tasks()
            .set_phase(task, generation, TaskPhase::new("download_files").unwrap());
        fetch_files(
            ctx,
            task,
            generation,
            &stage,
            &store,
            &downloads,
            &plan.artifacts,
            &mut batch,
        )?;
        if let Some(index_path) = &plan.asset_index_path {
            let bytes = std::fs::read(stage.join(index_path)).map_err(|_| "asset_index_read")?;
            let assets = asset_artifacts(&bytes)?;
            let _ = ctx
                .tasks()
                .set_phase(task, generation, TaskPhase::new("assets").unwrap());
            fetch_files(
                ctx, task, generation, &stage, &store, &downloads, &assets, &mut batch,
            )?;
            let index: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| "asset_index_parse")?;
            if index["virtual"].as_bool() == Some(true)
                || index["map_to_resources"].as_bool() == Some(true)
            {
                let index_id = Path::new(index_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .ok_or("asset_index_id")?;
                for (name, value) in index["objects"].as_object().ok_or("asset_objects")? {
                    if cancelled() {
                        return Err("cancelled".into());
                    }
                    let hash = value["hash"]
                        .as_str()
                        .ok_or("asset_hash")?
                        .to_ascii_lowercase();
                    let source = stage.join(format!("assets/objects/{}/{}", &hash[..2], hash));
                    for root in [
                        index["virtual"]
                            .as_bool()
                            .filter(|v| *v)
                            .map(|_| format!("assets/virtual/{index_id}")),
                        index["map_to_resources"]
                            .as_bool()
                            .filter(|v| *v)
                            .map(|_| "resources".to_string()),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        let dest = stage.join(root).join(name);
                        checked_directory(dest.parent().unwrap())?;
                        reject_link(&dest)?;
                        std::fs::copy(&source, dest).map_err(|_| "asset_mapping")?;
                    }
                }
            }
        }
        let _ = ctx
            .tasks()
            .set_phase(task, generation, TaskPhase::new("natives").unwrap());
        extract_natives(&stage, &plan.native_archives, &cancelled)?;
        if cancelled() {
            return Err("cancelled".into());
        }
        let metadata_path = stage.join(format!("versions/{version}/{version}.json"));
        let metadata_bytes = serde_json::to_vec_pretty(&resolved.metadata.raw).map_err(|_| "version_serialize")?;
        safe_write(&metadata_path, &metadata_bytes)?;
        let mut receipt = json!({"schemaVersion":1,"versionId":version,"instanceId":instance,"complete":true});
        if let Some(loader) = fabric {
            receipt["loader"] = json!("fabric"); receipt["loaderVersion"] = json!(loader);
            receipt["metadataSha1"] = json!(crate::fabric_metadata::persist(ctx.dirs(),&metadata_bytes)?);
        }
        safe_write(
            &stage.join("shtmcl-install.json"),
            &serde_json::to_vec(&receipt).unwrap(),
        )?;
        let _ = ctx
            .tasks()
            .set_phase(task, generation, TaskPhase::new("commit").unwrap());
        std::fs::rename(&stage, &target).map_err(|_| "publish")?;
        use sha2::{Digest, Sha256};
        let registration_key = format!(
            "install-{:x}",
            Sha256::digest(
                serde_json::to_vec(&json!([instance, version, target]))
                    .map_err(|_| "registration_identity")?
            )
        );
        let request = CommandRequest {
            request_id: RequestId::new(registration_key.clone()).map_err(|_| "request_id")?,
            operation_id: Some(OperationId::new(registration_key).map_err(|_| "operation_id")?),
            entity_id: None,
            expected_revision: None,
            payload: json!({"command":"instances.create","instance":{"id":instance,"name":name,"version":{"gameVersion":version,"loader":if fabric.is_some(){"fabric"}else{"vanilla"},"loaderVersion":fabric},"location":{"root":target},"javaMajor":resolved.metadata.java_major.unwrap_or(8)}}),
        };
        if instances.handle(&request).is_err() {
            // Roll directory publication back if registration fails. Never delete data.
            std::fs::rename(&target, &stage).map_err(|_| "registration_failed_rollback_pending")?;
            return Err("instance_registration".into());
        }
        Ok(target.to_string_lossy().into_owned())
    })();
    // fetch_group waits for its children to settle; the instance lock is still
    // held here. Generic failures keep staging available for recovery.
    finish_install_attempt(&ctx.dirs().instances, &stage, result)
}

pub(crate) fn fetch_files(
    ctx: &ModuleContext,
    task: &TaskId,
    generation: AttemptGeneration,
    stage: &Path,
    store: &shtmcl_core::downloads::FileStore,
    downloads: &Arc<dyn CoreModule>,
    files: &[InstallArtifact],
    batch: &mut usize,
) -> Result<(), String> {
    let total = files.iter().map(|file| file.size).sum();
    let mut completed = 0;
    let mut pending = Vec::new();
    // Disk cache survives CoreApp restarts; the downloader's in-memory index
    // alone cannot prove a hit. Verify/export pinned objects before networking.
    for artifact in files {
        if ctx.tasks().is_cancel_requested(task, generation) {
            return Err("cancelled".into());
        }
        let destination = stage.join(&artifact.path);
        checked_directory(destination.parent().ok_or("artifact_parent")?)?;
        reject_link(&destination)?;
        if destination.exists() && matches_artifact(&destination, artifact)? {
            completed += artifact.size;
            continue;
        }
        if destination.exists() {
            std::fs::remove_file(&destination).map_err(|_| "staging_repair")?;
        }
        let hash = shtmcl_core::downloads::ArtifactHash::new("sha1", &artifact.sha1)
            .map_err(|_| "digest")?;
        match runtime_export::export_cached_artifact(
            store,
            &hash,
            &format!("preflight/{task}/{}", artifact.sha1),
            stage,
            Path::new(&artifact.path),
            artifact.size,
            &AtomicBool::new(false),
        ) {
            Ok(_) => completed += artifact.size,
            Err(
                runtime_export::ExportError::CacheMiss
                | runtime_export::ExportError::DigestMismatch
                | runtime_export::ExportError::SizeMismatch { .. },
            ) => pending.push(artifact.clone()),
            Err(_) => return Err("cached_export_failed".into()),
        }
    }
    let _ = ctx.tasks().set_progress(
        task,
        generation,
        shtmcl_core::tasks::bytes_progress(completed, Some(total))
            .map_err(|_| "install_progress")?,
    );
    for group in pending.chunks(1536) {
        if ctx.tasks().is_cancel_requested(task, generation) {
            return Err("cancelled".into());
        }
        let mut retry_files = group.to_vec();
        for mirror in [false, true] {
            if ctx.tasks().is_cancel_requested(task, generation) {
                return Err("cancelled".into());
            }
            match fetch_group(
                ctx,
                task,
                generation,
                downloads,
                &retry_files,
                batch,
                mirror,
            ) {
                Ok(()) => break,
                Err(failure) if !mirror && failure.network => {
                    if ctx.tasks().is_cancel_requested(task, generation) {
                        return Err("cancelled".into());
                    }
                    // Completed siblings remain in the verified cache. Export them
                    // before retrying so the mirror receives cache misses only.
                    retry_files.clear();
                    for artifact in group {
                        let hash =
                            shtmcl_core::downloads::ArtifactHash::new("sha1", &artifact.sha1)
                                .map_err(|_| "digest")?;
                        match runtime_export::export_cached_artifact(
                            store,
                            &hash,
                            &format!("fallback/{task}/{}", artifact.sha1),
                            stage,
                            Path::new(&artifact.path),
                            artifact.size,
                            &AtomicBool::new(ctx.tasks().is_cancel_requested(task, generation)),
                        ) {
                            Ok(_) => {}
                            Err(runtime_export::ExportError::CacheMiss) => {
                                if mirror_source(&artifact.url).is_none() {
                                    return Err(failure.reference);
                                }
                                retry_files.push(artifact.clone());
                            }
                            Err(runtime_export::ExportError::Cancelled) => {
                                return Err("cancelled".into())
                            }
                            Err(_) => return Err("cached_export_failed".into()),
                        }
                    }
                    if retry_files.is_empty() {
                        break;
                    }
                    let _ = ctx.tasks().set_phase(
                        task,
                        generation,
                        TaskPhase::new("download_mirror").unwrap(),
                    );
                }
                Err(failure) => return Err(failure.reference),
            }
        }
        for artifact in group {
            let cancelled = ctx.tasks().is_cancel_requested(task, generation);
            let flag = AtomicBool::new(cancelled);
            let destination = stage.join(&artifact.path);
            checked_directory(destination.parent().ok_or("artifact_parent")?)?;
            reject_link(&destination)?;
            if destination.exists() && !matches_artifact(&destination, artifact)? {
                // Only installer-owned staging files are repaired here.
                std::fs::remove_file(&destination).map_err(|_| "staging_repair")?;
            }
            runtime_export::export_cached_artifact(
                store,
                &shtmcl_core::downloads::ArtifactHash::new("sha1", &artifact.sha1)
                    .map_err(|_| "digest")?,
                &format!("export/{task}/{}", artifact.sha1),
                stage,
                Path::new(&artifact.path),
                artifact.size,
                &flag,
            )
            .map_err(|_| {
                if cancelled {
                    "cancelled"
                } else {
                    "export_failed"
                }
            })?;
            completed += artifact.size;
            let _ = ctx.tasks().set_progress(
                task,
                generation,
                shtmcl_core::tasks::bytes_progress(completed, Some(total))
                    .map_err(|_| "install_progress")?,
            );
        }
    }
    Ok(())
}

pub(crate) struct GroupFailure {
    pub(crate) reference: String,
    pub(crate) network: bool,
}
impl From<&str> for GroupFailure {
    fn from(reference: &str) -> Self {
        Self {
            reference: reference.into(),
            network: false,
        }
    }
}
impl From<String> for GroupFailure {
    fn from(reference: String) -> Self {
        Self {
            reference,
            network: false,
        }
    }
}
fn is_network_failure(code: &str) -> bool {
    matches!(
        code,
        "downloads.network.failed" | "downloads.source.unavailable"
    )
}
fn mirror_source(url: &str) -> Option<&'static str> {
    // Match an exact HTTPS origin, never an embedded hostname or URL query.
    if url.starts_with("https://piston-data.mojang.com/")
        || url.starts_with("https://launcher.mojang.com/")
    {
        Some("bmclapi-launcher")
    } else if url.starts_with("https://resources.download.minecraft.net/") {
        Some("bmclapi-assets")
    } else if url.starts_with("https://libraries.minecraft.net/") || url.starts_with("https://maven.fabricmc.net/") {
        Some("bmclapi-maven")
    } else {
        None
    }
}
fn download_source(url: &str, mirror: bool) -> &'static str {
    if mirror {
        mirror_source(url).unwrap_or_else(|| source_id(url))
    } else {
        source_id(url)
    }
}
pub(crate) fn fetch_group(
    ctx: &ModuleContext,
    task: &TaskId,
    generation: AttemptGeneration,
    downloads: &Arc<dyn CoreModule>,
    files: &[InstallArtifact],
    batch: &mut usize,
    mirror: bool,
) -> Result<(), GroupFailure> {
    let mut children = ChildTasks {
        tasks: ctx.tasks().clone(),
        ids: vec![],
    };
    for chunk in files.chunks(256) {
        *batch += 1;
        let op = format!("{task}-files-{batch}");
        let request = CommandRequest {
            request_id: RequestId::new(op.clone()).map_err(|_| "request_id")?,
            operation_id: Some(OperationId::new(op).map_err(|_| "operation_id")?),
            entity_id: None,
            expected_revision: None,
            payload: json!({"command":"downloads.fetch","discardOnCancel":true,"artifacts":chunk.iter().map(|a| json!({"artifactId":format!("install/{}",a.sha1),"sourceId":download_source(&a.url, mirror),"url":a.url,"size":a.size,"hash":{"algorithm":"sha1","digest":a.sha1}})).collect::<Vec<_>>()}),
        };
        match downloads.handle(&request).map_err(|_| "download_accept")? {
            CommandResponse::Accepted { task_id, .. } => children.ids.push(task_id),
            _ => return Err("download_not_accepted".into()),
        }
    }
    let mut interrupted = false;
    let mut first_failure = None;
    let mut network_only = true;
    let mut cancelled_siblings = std::collections::BTreeSet::new();
    loop {
        let cancelled = ctx.tasks().is_cancel_requested(task, generation);
        let mut running = false;
        for child in &children.ids {
            let snapshot = ctx.tasks().snapshot(child).ok_or("download_task_missing")?;
            if snapshot.state == TaskState::Failed {
                network_only &= snapshot
                    .error
                    .as_ref()
                    .is_some_and(|error| is_network_failure(error.code.as_str()));
                if first_failure.is_none() || !network_only {
                    first_failure = Some(download_failure_reference(&snapshot));
                }
            }
            if snapshot.state == TaskState::Cancelled {
                network_only &= cancelled_siblings.contains(child);
                interrupted = true;
            }
            if !matches!(
                snapshot.state,
                TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
            ) {
                running = true;
                if cancelled || interrupted || first_failure.is_some() {
                    cancelled_siblings.insert(child.clone());
                    ctx.tasks().request_cancel(&CancelTaskRequest {
                        task_id: child.clone(),
                        attempt_generation: snapshot.attempt_generation,
                    });
                }
            }
        }
        if !running {
            if cancelled {
                return Err("cancelled".into());
            }
            if let Some(reference) = first_failure {
                return Err(GroupFailure {
                    reference,
                    network: network_only,
                });
            }
            if interrupted {
                return Err("download_interrupted".into());
            }
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Ok(())
}

/// Retain the first failed child for diagnosis, rather than replacing its
/// cause with a sibling cancellation. Only stable identifiers cross this
/// boundary; transport details stay on the original download task.
fn download_failure_reference(task: &shtmcl_domain::Task) -> String {
    let code = task
        .error
        .as_ref()
        .map(|error| error.code.as_str())
        .unwrap_or("downloads.internal");
    format!("download_failed:{code}:{}", task.id.as_str())
}

pub(crate) fn metadata_failure(error: shtmcl_core::launch::catalog::MetadataError) -> String {
    use shtmcl_core::launch::catalog::MetadataError;
    let code = match error {
        MetadataError::SourceUnavailable { code, .. } => return format!("version_metadata:{code}"),
        MetadataError::MissingParent(_) => "missing_version_or_parent",
        MetadataError::InheritanceCycle(_) => "inheritance_cycle",
        MetadataError::InheritanceTooDeep => "inheritance_depth",
        _ => "invalid_metadata",
    };
    format!("version_metadata:{code}")
}

fn source_id(url: &str) -> &'static str {
    if url.starts_with("https://maven.fabricmc.net/") {
        "fabric-maven"
    } else if url.contains("libraries.minecraft.net/") {
        "mojang-libraries"
    } else if url.contains("resources.download.minecraft.net/") {
        "mojang-assets"
    } else if url.contains("piston-meta.mojang.com/") {
        "mojang-meta"
    } else if url.contains("launchermeta.mojang.com/") {
        "mojang-legacy-meta"
    } else if url.contains("launcher.mojang.com/") {
        "mojang-launcher"
    } else {
        "mojang-data"
    }
}

pub(crate) fn extract_natives(
    stage: &Path,
    archives: &[String],
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    use shtmcl_core::content::zipindex::{read_entry, read_index, ZipBudget};
    let root = stage.join("natives");
    checked_directory(&root)?;
    let budget = ZipBudget {
        max_entries: 4096,
        max_total_uncompressed: 256 * 1024 * 1024,
        max_entry_uncompressed: 64 * 1024 * 1024,
    };
    let mut extracted_bytes = 0u64;
    let mut seen = std::collections::BTreeMap::new();
    for path in archives {
        let file = std::fs::File::open(stage.join(path)).map_err(|_| "natives_read")?;
        if file.metadata().map_err(|_| "natives_metadata")?.len() > 128 * 1024 * 1024 {
            return Err("natives_archive_limit".into());
        }
        let bytes = std::fs::read(stage.join(path)).map_err(|_| "natives_read")?;
        for entry in read_index(&bytes, &budget)
            .map_err(|_| "natives_zip")?
            .entries
        {
            if cancelled() {
                return Err("cancelled".into());
            }
            if entry.is_directory() || entry.name.starts_with("META-INF/") {
                continue;
            }
            if !safe_path(&entry.name) {
                return Err("natives_path".into());
            }
            let dest = root.join(&entry.name);
            let payload = read_entry(&bytes, &entry, &budget).map_err(|_| "natives_crc")?;
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(&payload);
            let key = entry.name.to_ascii_lowercase();
            if let Some(previous) = seen.insert(key, digest) {
                if previous != digest {
                    return Err("native_destination_conflict".into());
                }
            }
            extracted_bytes = extracted_bytes
                .checked_add(payload.len() as u64)
                .filter(|n| *n <= 1024 * 1024 * 1024)
                .ok_or("native_total_budget")?;
            safe_write(&dest, &payload)?;
        }
    }
    Ok(())
}

/// Called only by the owner of the instance installation lock, after writers
/// have stopped. Published instances and shared cache are outside this boundary.
pub(crate) fn cleanup_cancelled_stage(instances: &Path, stage: &Path) -> Result<(), String> {
    if !instances.is_absolute() || stage.parent() != Some(instances) {
        return Err("install_cleanup_boundary".into());
    }
    let name = stage.file_name().and_then(|n| n.to_str()).ok_or("install_cleanup_boundary")?;
    let id = name.strip_prefix(".install-").filter(|id| !id.is_empty()).ok_or("install_cleanup_boundary")?;
    if !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
        return Err("install_cleanup_boundary".into());
    }
    for ancestor in stage.ancestors() { reject_link(ancestor)?; }
    match std::fs::symlink_metadata(stage) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("install_cleanup_metadata".into()),
        Ok(metadata) if !metadata.is_dir() => return Err("install_cleanup_not_directory".into()),
        _ => {}
    }
    let root = instances.canonicalize().map_err(|_| "install_cleanup_boundary")?;
    let resolved = stage.canonicalize().map_err(|_| "install_cleanup_boundary")?;
    if resolved.parent() != Some(root.as_path()) {
        return Err("install_cleanup_boundary".into());
    }
    // Validate the entire tree before removing anything, including Windows
    // junctions. Refuse unfamiliar special entries without partial cleanup.
    let mut pending = vec![stage.to_path_buf()];
    while let Some(directory) = pending.pop() {
        reject_link(&directory)?;
        for entry in std::fs::read_dir(&directory).map_err(|_| "install_cleanup_read")? {
            let path = entry.map_err(|_| "install_cleanup_read")?.path();
            reject_link(&path)?;
            let metadata = path.symlink_metadata().map_err(|_| "install_cleanup_metadata")?;
            if metadata.is_dir() { pending.push(path); }
            else if !metadata.is_file() { return Err("install_cleanup_special_file".into()); }
        }
    }
    std::fs::remove_dir_all(stage).map_err(|_| "install_cleanup_failed".into())
}

fn finish_install_attempt<T>(instances: &Path, stage: &Path, result: Result<T, String>) -> Result<T, String> {
    if matches!(&result, Err(error) if error == "cancelled") {
        cleanup_cancelled_stage(instances, stage)?;
    }
    result
}

pub(crate) fn reject_link(path: &Path) -> Result<(), String> {
    match path.symlink_metadata() {
        Ok(metadata) => {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err("linked_install_path".into());
                }
            }
            if metadata.file_type().is_symlink() {
                return Err("linked_install_path".into());
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("install_path_metadata".into()),
    }
}

pub(crate) fn checked_directory(path: &Path) -> Result<(), String> {
    let mut current = std::path::PathBuf::new();
    for part in path.components() {
        current.push(part);
        reject_link(&current)?;
        if current.exists() {
            if !current.is_dir() {
                return Err("install_parent_not_directory".into());
            }
        } else {
            std::fs::create_dir(&current).map_err(|_| "install_directory_create")?;
        }
    }
    Ok(())
}

fn safe_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    checked_directory(path.parent().ok_or("install_file_parent")?)?;
    reject_link(path)?;
    if path.exists() {
        if std::fs::read(path).map_err(|_| "existing_install_file")? == bytes {
            return Ok(());
        }
        std::fs::remove_file(path).map_err(|_| "staging_repair")?;
    }
    let temporary = path.with_extension(format!(
        "{}.tmp",
        shtmcl_platform::entropy::random_hex(8).map_err(|_| "install_entropy")?
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|_| "install_temp_create")?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "install_temp_write")?;
        drop(file);
        std::fs::rename(&temporary, path).map_err(|_| "install_temp_publish")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn matches_artifact(path: &Path, artifact: &InstallArtifact) -> Result<bool, String> {
    use sha1::{Digest, Sha1};
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|_| "staging_read")?;
    if file.metadata().map_err(|_| "staging_metadata")?.len() != artifact.size {
        return Ok(false);
    }
    let mut digest = Sha1::new();
    let mut buffer = [0u8; 65536];
    loop {
        let size = file.read(&mut buffer).map_err(|_| "staging_hash")?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    Ok(format!("{:x}", digest.finalize()) == artifact.sha1)
}

fn recover_published_staging(
    target: &Path,
    stage: &Path,
    instance: &str,
    version: &str,
) -> Result<(), String> {
    reject_link(target)?;
    let receipt_path = target.join("shtmcl-install.json");
    reject_link(&receipt_path)?;
    let file = std::fs::File::open(&receipt_path).map_err(|_| "existing_destination_not_owned")?;
    if file.metadata().map_err(|_| "receipt_metadata")?.len() > 4096 {
        return Err("invalid_install_receipt".into());
    }
    let receipt: serde_json::Value =
        serde_json::from_reader(file).map_err(|_| "invalid_install_receipt")?;
    if receipt["schemaVersion"] != 1
        || receipt["complete"] != true
        || receipt["instanceId"] != instance
        || receipt["versionId"] != version
        || stage.exists()
    {
        return Err("installation_recovery_conflict".into());
    }
    std::fs::rename(target, stage).map_err(|_| "installation_recovery_rename")?;
    // Caller revalidates every artifact before publishing/registering again.
    Ok(())
}

struct ChildTasks {
    tasks: Arc<dyn shtmcl_core::tasks::TaskService>,
    ids: Vec<TaskId>,
}
impl Drop for ChildTasks {
    fn drop(&mut self) {
        for id in &self.ids {
            if let Some(task) = self.tasks.snapshot(id) {
                if !matches!(
                    task.state,
                    TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
                ) {
                    self.tasks.request_cancel(&CancelTaskRequest {
                        task_id: id.clone(),
                        attempt_generation: task.attempt_generation,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "shtmcl-install-tests-{}",
                shtmcl_platform::entropy::random_hex(10).unwrap()
            )))
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[cfg(windows)]
    #[test]
    fn stopped_install_refuses_junctions_without_touching_external_files() {
        use std::os::windows::process::CommandExt;
        let root = Temp::new();
        let instances = root.0.join("instances");
        let stage = instances.join(".install-linked");
        let outside = root.0.join("outside");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"outside").unwrap();
        std::fs::write(stage.join("keep"), b"staging").unwrap();
        let link = stage.join("linked");
        let status = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"]).arg(&link).arg(&outside)
            .creation_flags(0x0800_0000).output().unwrap();
        assert!(status.status.success());
        let result: Result<(), String> = finish_install_attempt(&instances, &stage, Err("cancelled".into()));
        assert_eq!(result, Err("linked_install_path".into()));
        assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"outside");
        assert_eq!(std::fs::read(stage.join("keep")).unwrap(), b"staging");
        std::fs::remove_dir(link).unwrap();
    }

    #[test]
    fn stopped_install_reports_cleanup_failure_and_keeps_unexpected_file() {
        let root = Temp::new();
        let instances = root.0.join("instances");
        std::fs::create_dir_all(&instances).unwrap();
        let stage = instances.join(".install-file");
        std::fs::write(&stage, b"keep").unwrap();
        let result: Result<(), String> = finish_install_attempt(&instances, &stage, Err("cancelled".into()));
        assert_eq!(result, Err("install_cleanup_not_directory".into()));
        assert_eq!(std::fs::read(stage).unwrap(), b"keep");
    }

    #[test]
    fn stopped_install_cleans_only_its_stage_and_failures_remain_recoverable() {
        let root = Temp::new();
        let instances = root.0.join("instances");
        let stage = instances.join(".install-test");
        let sibling = instances.join("kept-game");
        std::fs::create_dir_all(stage.join("assets/objects")).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(stage.join("assets/objects/data"), b"downloaded").unwrap();
        std::fs::write(sibling.join("options.txt"), b"lang:zh_cn").unwrap();
        let cache = root.0.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("verified"), b"shared").unwrap();
        let failed: Result<(), String> = Err("network_failed".into());
        assert_eq!(finish_install_attempt(&instances, &stage, failed), Err("network_failed".into()));
        assert!(stage.join("assets/objects/data").exists());
        let cancelled: Result<(), String> = Err("cancelled".into());
        assert_eq!(finish_install_attempt(&instances, &stage, cancelled), Err("cancelled".into()));
        assert!(!stage.exists());
        assert_eq!(std::fs::read(sibling.join("options.txt")).unwrap(), b"lang:zh_cn");
        assert_eq!(std::fs::read(cache.join("verified")).unwrap(), b"shared");
        cleanup_cancelled_stage(&instances, &stage).unwrap();
        assert!(cleanup_cancelled_stage(&instances, &sibling).is_err());
        assert!(cleanup_cancelled_stage(&instances, &cache).is_err());
        assert!(cleanup_cancelled_stage(&instances, &instances).is_err());
    }

    #[test]
    fn artifact_fallback_reuses_real_download_module_and_verified_disk() {
        use shtmcl_core::downloads::{ArtifactHash, ByteRange, DownloadPort, FetchOutcome};
        struct Transport {
            store: shtmcl_core::downloads::FileStore,
            sources: std::sync::Mutex<Vec<String>>,
        }
        impl DownloadPort for Transport {
            fn probe(&self, _: &str, _: &str) -> Result<Option<u64>, String> {
                Ok(Some(4))
            }
            fn fetch(
                &self,
                source: &str,
                id: &str,
                range: ByteRange,
                _: Option<&str>,
            ) -> FetchOutcome {
                self.sources.lock().unwrap().push(source.into());
                if source == "mojang-data" {
                    return FetchOutcome::TransportFailed("fixture-connect".into());
                }
                use std::io::Write;
                self.store
                    .open_partial(id, 0)
                    .unwrap()
                    .write_all(b"good")
                    .unwrap();
                FetchOutcome::Bytes { range, etag: None }
            }
            fn hash_bytes(&self, id: &str, algorithm: &str) -> Result<String, String> {
                self.store
                    .hash_partial(id, algorithm)
                    .map_err(|e| e.to_string())
            }
            fn commit_range(&self, id: &str, _: ByteRange) -> Result<(), String> {
                self.store.commit_partial(id).map_err(|e| e.to_string())
            }
            fn publish(&self, hash: &ArtifactHash, id: &str) -> Result<u64, String> {
                self.store.publish(hash, id).map_err(|e| e.to_string())
            }
            fn materialize_cached(
                &self,
                hash: &ArtifactHash,
                id: &str,
            ) -> Result<Option<u64>, String> {
                self.store.materialize(hash, id).map_err(|e| e.to_string())
            }
            fn discard(&self, id: &str) -> Result<(), String> {
                self.store.discard(id).map_err(|e| e.to_string())
            }
        }
        let root = Temp::new();
        let app = shtmcl_core::CoreApp::new(shtmcl_platform::PlatformDirs::under(&root.0));
        let ctx = app.context();
        let cache = root.0.join("cache");
        let store = shtmcl_core::downloads::FileStore::new(&cache).unwrap();
        let transport = Arc::new(Transport {
            store: shtmcl_core::downloads::FileStore::new(&cache).unwrap(),
            sources: Default::default(),
        });
        let downloads: Arc<dyn CoreModule> =
            Arc::new(shtmcl_core::downloads::DownloadsModule::new(
                ctx.clone(),
                transport.clone(),
                Arc::new(shtmcl_core::downloads::FileDisk::new(&cache)),
                Arc::new(shtmcl_core::downloads::SystemClock),
            ));
        let parent = ctx
            .tasks()
            .accept(TaskSpec::new(
                OperationId::new("real-module-fallback").unwrap(),
                TaskPhase::new("install").unwrap(),
            ))
            .unwrap();
        ctx.tasks()
            .transition(&parent.id, parent.attempt_generation, TaskState::Running)
            .unwrap();
        use sha1::{Digest, Sha1};
        let artifact = InstallArtifact {
            path: "client.jar".into(),
            url: "https://piston-data.mojang.com/v1/test/client.jar".into(),
            sha1: format!("{:x}", Sha1::digest(b"good")),
            size: 4,
        };
        let stage = root.0.join("stage");
        fetch_files(
            &ctx,
            &parent.id,
            parent.attempt_generation,
            &stage,
            &store,
            &downloads,
            &[artifact],
            &mut 0,
        )
        .unwrap();
        assert_eq!(std::fs::read(stage.join("client.jar")).unwrap(), b"good");
        assert_eq!(
            *transport.sources.lock().unwrap(),
            ["mojang-data", "bmclapi-launcher"]
        );
    }
    struct FailedDownloads {
        ctx: ModuleContext,
        code: &'static str,
        requests: std::sync::Mutex<Vec<serde_json::Value>>,
    }
    impl CoreModule for FailedDownloads {
        fn id(&self) -> ModuleId {
            ModuleId::Downloads
        }
        fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
            self.requests.lock().unwrap().push(request.payload.clone());
            let operation = request.operation_id.clone().unwrap();
            let task = self
                .ctx
                .tasks()
                .accept(TaskSpec::new(
                    operation.clone(),
                    TaskPhase::new("download").unwrap(),
                ))
                .unwrap();
            self.ctx
                .tasks()
                .transition(&task.id, task.attempt_generation, TaskState::Running)
                .unwrap();
            self.ctx
                .tasks()
                .fail(
                    &task.id,
                    task.attempt_generation,
                    ErrorEnvelope::new(
                        shtmcl_domain::ErrorCode::from_static(self.code),
                        shtmcl_domain::ErrorCategory::Network,
                        true,
                    ),
                )
                .unwrap();
            Ok(CommandResponse::Accepted {
                operation_id: operation,
                task_id: task.id,
            })
        }
    }
    #[test]
    fn artifact_fallback_is_bounded_and_keeps_pinned_identity() {
        for (code, cancelled, expected) in [
            ("downloads.network.failed", false, 2),
            ("downloads.source.unavailable", false, 2),
            ("downloads.hash.mismatch", false, 1),
            ("downloads.io.failed", false, 1),
            ("downloads.network.failed", true, 0),
        ] {
            let root = Temp::new();
            let app = shtmcl_core::CoreApp::new(shtmcl_platform::PlatformDirs::under(&root.0));
            let ctx = app.context();
            let parent = ctx
                .tasks()
                .accept(TaskSpec::new(
                    OperationId::new("fallback-test").unwrap(),
                    TaskPhase::new("install").unwrap(),
                ))
                .unwrap();
            ctx.tasks()
                .transition(&parent.id, parent.attempt_generation, TaskState::Running)
                .unwrap();
            if cancelled {
                ctx.tasks().request_cancel(&CancelTaskRequest {
                    task_id: parent.id.clone(),
                    attempt_generation: parent.attempt_generation,
                });
            }
            let fixture = Arc::new(FailedDownloads {
                ctx: ctx.clone(),
                code,
                requests: Default::default(),
            });
            let downloads: Arc<dyn CoreModule> = fixture.clone();
            let store = shtmcl_core::downloads::FileStore::new(root.0.join("cache")).unwrap();
            let artifact = InstallArtifact {
                path: "client.jar".into(),
                url: "https://piston-data.mojang.com/v1/test/client.jar".into(),
                sha1: "a".repeat(40),
                size: 7,
            };
            assert!(fetch_files(
                &ctx,
                &parent.id,
                parent.attempt_generation,
                &root.0.join("stage"),
                &store,
                &downloads,
                &[artifact],
                &mut 0
            )
            .is_err());
            let requests = fixture.requests.lock().unwrap();
            assert_eq!(requests.len(), expected, "{code} cancelled={cancelled}");
            if expected == 2 {
                let first = &requests[0]["artifacts"][0];
                let second = &requests[1]["artifacts"][0];
                for key in ["artifactId", "url", "hash", "size"] {
                    assert_eq!(first[key], second[key]);
                }
                assert_eq!(first["sourceId"], "mojang-data");
                assert_eq!(second["sourceId"], "bmclapi-launcher");
            }
        }
    }
    #[test]
    fn artifact_fallback_requires_exact_official_origin() {
        assert_eq!(
            mirror_source("https://libraries.minecraft.net/a"),
            Some("bmclapi-maven")
        );
        assert_eq!(
            mirror_source("https://resources.download.minecraft.net/a"),
            Some("bmclapi-assets")
        );
        for url in [
            "http://piston-data.mojang.com/a",
            "https://evil.test/?https://piston-data.mojang.com/a",
            "https://piston-data.mojang.com.evil/a",
        ] {
            assert_eq!(mirror_source(url), None);
        }
    }
    #[test]
    fn staging_writes_are_idempotent_and_corrupt_files_are_repaired() {
        let root = Temp::new();
        let file = root.0.join("natives/test.dll");
        safe_write(&file, b"verified").unwrap();
        safe_write(&file, b"verified").unwrap();
        std::fs::write(&file, b"corrupt").unwrap();
        safe_write(&file, b"verified").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"verified");
        assert_eq!(
            std::fs::read_dir(file.parent().unwrap()).unwrap().count(),
            1
        );
    }
    #[test]
    fn file_parent_is_rejected_without_changing_existing_bytes() {
        let root = Temp::new();
        safe_write(&root.0.join("block"), b"original").unwrap();
        assert!(safe_write(&root.0.join("block/escape"), b"wrong").is_err());
        assert_eq!(std::fs::read(root.0.join("block")).unwrap(), b"original");
    }
    #[test]
    fn installation_lock_is_released_with_its_handle() {
        let root = Temp::new();
        checked_directory(&root.0).unwrap();
        let path = root.0.join("lock");
        let first = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        first.try_lock().unwrap();
        let second = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        assert!(second.try_lock().is_err());
        drop(first);
        second.try_lock().unwrap();
    }
    #[test]
    fn hash_check_detects_same_size_corruption() {
        use sha1::{Digest, Sha1};
        let root = Temp::new();
        let path = root.0.join("file");
        safe_write(&path, b"good").unwrap();
        let artifact = InstallArtifact {
            path: "file".into(),
            url: "https://piston-data.mojang.com/a".into(),
            sha1: format!("{:x}", Sha1::digest(b"good")),
            size: 4,
        };
        assert!(matches_artifact(&path, &artifact).unwrap());
        std::fs::write(&path, b"evil").unwrap();
        assert!(!matches_artifact(&path, &artifact).unwrap());
    }

    #[test]
    fn unregistered_publication_returns_to_staging_without_deleting_content() {
        let root = Temp::new();
        let target = root.0.join("instance");
        let stage = root.0.join(".staging");
        safe_write(
            &target.join("shtmcl-install.json"),
            br#"{"schemaVersion":1,"complete":true,"instanceId":"test","versionId":"1.7.10"}"#,
        )
        .unwrap();
        safe_write(&target.join("client.jar"), b"preserved").unwrap();
        assert!(recover_published_staging(&target, &stage, "other", "1.7.10").is_err());
        assert!(target.exists());
        recover_published_staging(&target, &stage, "test", "1.7.10").unwrap();
        assert_eq!(
            std::fs::read(stage.join("client.jar")).unwrap(),
            b"preserved"
        );
    }

    #[test]
    fn dropping_a_child_group_requests_cancellation() {
        let root = Temp::new();
        let app = shtmcl_core::CoreApp::new(shtmcl_platform::PlatformDirs::under(&root.0));
        let ctx = app.context();
        let task = ctx
            .tasks()
            .accept(TaskSpec::new(
                OperationId::new("child-cancel").unwrap(),
                TaskPhase::new("download").unwrap(),
            ))
            .unwrap();
        {
            let _group = ChildTasks {
                tasks: ctx.tasks().clone(),
                ids: vec![task.id.clone()],
            };
        }
        assert!(ctx
            .tasks()
            .is_cancel_requested(&task.id, task.attempt_generation));
    }

    #[test]
    fn download_failure_keeps_child_code_without_transport_details() {
        let root = Temp::new();
        let app = shtmcl_core::CoreApp::new(shtmcl_platform::PlatformDirs::under(&root.0));
        let ctx = app.context();
        let task = ctx
            .tasks()
            .accept(TaskSpec::new(
                OperationId::new("download-diagnostic").unwrap(),
                TaskPhase::new("download").unwrap(),
            ))
            .unwrap();
        ctx.tasks()
            .transition(&task.id, task.attempt_generation, TaskState::Running)
            .unwrap();
        ctx.tasks()
            .fail(
                &task.id,
                task.attempt_generation,
                ErrorEnvelope::new(
                    shtmcl_domain::ErrorCode::from_static("downloads.source.busy"),
                    shtmcl_domain::ErrorCategory::Conflict,
                    true,
                )
                .with_detail_ref("private-transport-details"),
            )
            .unwrap();
        let failed = ctx.tasks().snapshot(&task.id).unwrap();
        assert_eq!(
            download_failure_reference(&failed),
            format!("download_failed:downloads.source.busy:{}", task.id.as_str())
        );
        assert!(!download_failure_reference(&failed).contains("private-transport-details"));
        let mut missing_error = failed;
        missing_error.error = None;
        assert_eq!(
            download_failure_reference(&missing_error),
            format!("download_failed:downloads.internal:{}", task.id.as_str())
        );
    }

    #[test]
    fn presentation_write_failure_does_not_start_installation_or_leave_busy() {
        struct NeverCalled;
        impl CoreModule for NeverCalled {
            fn id(&self) -> ModuleId {
                ModuleId::Launch
            }
            fn handle(&self, _: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
                panic!("installation must not start before its presentation is saved")
            }
        }
        impl shtmcl_core::launch::catalog::ManifestFetcher for NeverCalled {
            fn fetch(&self, _: &str, _: usize) -> Result<Vec<u8>, String> {
                panic!("network must not start before its presentation is saved")
            }
        }
        let root = Temp::new();
        let app = shtmcl_core::CoreApp::new(shtmcl_platform::PlatformDirs::under(&root.0));
        let ctx = app.context();
        checked_directory(&ctx.dirs().data).unwrap();
        std::fs::write(ctx.dirs().data.join("install-operations"), b"blocked").unwrap();
        let fetcher = Arc::new(NeverCalled);
        let cache = ctx.dirs().data.join("catalog");
        let module = GameInstaller::new(
            ctx.clone(),
            Arc::new(NeverCalled),
            Arc::new(VersionCatalog::new(&cache, fetcher.clone())),
            Arc::new(CachedVersionMetadataSource::new(
                shtmcl_core::launch::catalog::CatalogCache::new(&cache),
                fetcher,
            )),
            Arc::new(NeverCalled),
            Some(Arc::new(NeverCalled)),
        );
        for operation in ["failed-record-one", "failed-record-two"] {
            let request = CommandRequest {
                request_id: RequestId::new(operation).unwrap(),
                operation_id: Some(OperationId::new(operation).unwrap()),
                entity_id: None,
                expected_revision: None,
                payload: json!({"command":"launch.install","versionId":"1.7.10","instanceId":"school"}),
            };
            assert!(module.handle(&request).is_err());
            assert!(!module.busy.load(Ordering::Acquire));
        }
        assert_eq!(ctx.tasks().snapshots().len(), 2);
        assert!(ctx
            .tasks()
            .snapshots()
            .iter()
            .all(|task| task.state == TaskState::Failed));
    }

    #[test]
    #[ignore = "requires the retained real-game cache; forbids all network adapters"]
    fn persisted_cache_installs_game_without_network() {
        struct Offline;
        impl shtmcl_core::launch::catalog::ManifestFetcher for Offline {
            fn fetch(&self, _: &str, _: usize) -> Result<Vec<u8>, String> {
                Err("network forbidden".into())
            }
        }
        struct NoDownloads;
        impl CoreModule for NoDownloads {
            fn id(&self) -> ModuleId {
                ModuleId::Downloads
            }
            fn handle(&self, _: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
                panic!("cache reuse attempted a network download")
            }
        }
        let root = std::env::var_os("SHTMCL_INSTALL_TEST_ROOT").expect("real cache root");
        let version = std::env::var("SHTMCL_INSTALL_VERSION").unwrap_or_else(|_| "1.7.10".into());
        let instance =
            std::env::var("SHTMCL_INSTALL_INSTANCE").unwrap_or_else(|_| "offline-reused".into());
        let app = shtmcl_core::CoreApp::new(shtmcl_platform::PlatformDirs::under(root));
        let ctx = app.context();
        let cache = ctx.dirs().data.join("catalog");
        let fetcher = Arc::new(Offline);
        let catalog = Arc::new(VersionCatalog::new(&cache, fetcher.clone()));
        let metadata = Arc::new(CachedVersionMetadataSource::new(
            shtmcl_core::launch::catalog::CatalogCache::new(&cache),
            fetcher,
        ));
        let instances = Arc::new(
            shtmcl_core::instances::InstancesModule::new(
                ctx.clone(),
                Box::new(
                    shtmcl_core::instances::FileStorage::new(
                        ctx.dirs().data.join("instance-store"),
                    )
                    .unwrap(),
                ),
            )
            .unwrap(),
        );
        let task = ctx
            .tasks()
            .accept(TaskSpec::new(
                OperationId::new("offline-cache-install").unwrap(),
                TaskPhase::new("install").unwrap(),
            ))
            .unwrap();
        ctx.tasks()
            .transition(&task.id, task.attempt_generation, TaskState::Running)
            .unwrap();
        let start = std::time::Instant::now();
        let path = install(
            &ctx,
            &task.id,
            task.attempt_generation,
            &version,
            &instance,
            "离线游戏",
            catalog,
            metadata,
            Arc::new(NoDownloads),
            instances,
            std::env::var("SHTMCL_CACHED_ENVIRONMENT").ok().map(|id| {
                crate::environment_cache::load_metadata(ctx.dirs(), &id, &version)
                    .expect("cached recipe pinned metadata")
            }),
            None,
        )
        .expect("offline installation");
        println!(
            "offline_install version={version} seconds={} path={path} network_calls=0",
            start.elapsed().as_secs_f64()
        );
    }
}

#[cfg(test)]
mod display_name_tests {
    use super::*;
    #[test]
    fn names_are_readable_bounded_and_optional() {
        assert_eq!(install_name(&json!({}), "game-id").unwrap(), "game-id");
        assert_eq!(
            install_name(&json!({"name":" 校园生存 "}), "game-id").unwrap(),
            "校园生存"
        );
        for value in [
            json!(null),
            json!(42),
            json!("  "),
            json!("bad\nname"),
            json!("字".repeat(81)),
        ] {
            assert!(install_name(&json!({"name":value}), "id").is_err());
        }
    }
}

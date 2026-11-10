//! Complete game preparation for the fixed NeoForge pilot. Publication and
//! instance registration remain separate from this retained attempt directory.
use crate::{game_install, runtime_export, version_metadata_source::VerifiedVersionDetail};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use shtmcl_core::{
    content::{
        neoforge_download::processor_download_payload,
        neoforge_mappings::parse_verified_metadata,
        neoforge_pipeline::{install_client, ClientInstallInputs, PipelineError},
        neoforge_profile::read_installer_archive,
        neoforge_runtime::plan_runtime,
        neoforge_stage::{StageError, VerifiedInput},
    },
    downloads::{ArtifactHash, FileStore},
    launch::{
        catalog::VersionMetadata,
        install::{asset_artifacts, safe_path, InstallArtifact, VanillaInstallPlan},
    },
    CoreModule, ModuleContext,
};
use shtmcl_domain::{
    AttemptGeneration, CancelTaskRequest, CommandRequest, CommandResponse, OperationId, RequestId,
    TaskId, TaskPhase, TaskState,
};
use shtmcl_platform::{ProcessLauncher, ProcessSupervisor};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const INSTALLER_SHA256: &str = "8f1413379067059acc0151444b17536cfad97756d99fc70e45587bf229c986fa";

struct Acquisition<'a> {
    ctx: &'a ModuleContext,
    task: &'a TaskId,
    generation: AttemptGeneration,
    downloads: &'a Arc<dyn CoreModule>,
    store: FileStore,
    inputs: PathBuf,
    sequence: usize,
}

impl Acquisition<'_> {
    fn cancelled(&self) -> bool {
        self.ctx
            .tasks()
            .is_cancel_requested(self.task, self.generation)
    }

    fn phase(&self, phase: &str) -> Result<(), String> {
        if self.cancelled() {
            return Err("cancelled".into());
        }
        self.ctx
            .tasks()
            .set_phase(
                self.task,
                self.generation,
                TaskPhase::new(phase).map_err(|_| "phase_invalid")?,
            )
            .map(|_| ())
            .map_err(|_| "phase_update".into())
    }

    fn acquire(&mut self, payload: Value, paths: &[String]) -> Result<Vec<PathBuf>, String> {
        let artifacts = payload["artifacts"]
            .as_array()
            .ok_or("download_artifacts")?;
        if artifacts.len() != paths.len() || artifacts.len() > 256 {
            return Err("download_artifact_count".into());
        }
        let mut pending = Vec::new();
        for artifact in artifacts {
            if self.cancelled() {
                return Err("cancelled".into());
            }
            let hash = artifact_hash(artifact)?;
            let size = artifact["size"].as_u64().ok_or("download_size")?;
            match self.store.verify_cached(&hash) {
                Ok(Some(actual)) if actual == size => {}
                Ok(None) => pending.push(artifact.clone()),
                // A corrupt object must be repaired by the shared downloader,
                // never consumed as a verified input.
                Ok(Some(_)) => pending.push(artifact.clone()),
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                    pending.push(artifact.clone())
                }
                Err(_) => return Err("cache_verify".into()),
            }
        }
        if !pending.is_empty() {
            self.sequence += 1;
            let operation = format!("{}-neo-{}", self.task, self.sequence);
            let response = self
                .downloads
                .handle(&CommandRequest {
                    request_id: RequestId::new(operation.clone())
                        .map_err(|_| "download_request")?,
                    operation_id: Some(
                        OperationId::new(operation).map_err(|_| "download_operation")?,
                    ),
                    entity_id: None,
                    expected_revision: None,
                    payload: json!({"command":"downloads.fetch","artifacts":pending}),
                })
                .map_err(|error| format!("download_accept:{}", error.code))?;
            let CommandResponse::Accepted { task_id, .. } = response else {
                return Err("download_not_accepted".into());
            };
            let _guard = ChildGuard {
                ctx: self.ctx.clone(),
                task: task_id.clone(),
            };
            loop {
                let child = self
                    .ctx
                    .tasks()
                    .snapshot(&task_id)
                    .ok_or("download_task_missing")?;
                if child.state.is_terminal() {
                    if self.cancelled() {
                        return Err("cancelled".into());
                    }
                    match child.state {
                        TaskState::Succeeded => break,
                        TaskState::Cancelled => return Err("download_interrupted".into()),
                        _ => {
                            return Err(format!(
                                "download_failed:{}:{}",
                                child
                                    .error
                                    .as_ref()
                                    .map(|e| e.code.as_str())
                                    .unwrap_or("downloads.internal"),
                                task_id
                            ))
                        }
                    }
                }
                if self.cancelled() {
                    self.ctx.tasks().request_cancel(&CancelTaskRequest {
                        task_id: task_id.clone(),
                        attempt_generation: child.attempt_generation,
                    });
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        let mut result = Vec::new();
        for (artifact, path) in artifacts.iter().zip(paths) {
            if !safe_path(path) {
                return Err("input_path".into());
            }
            self.sequence += 1;
            let export_id = format!("{}-neo-export-{}", self.task, self.sequence);
            runtime_export::export_cached_artifact_with_cancel(
                &self.store,
                &artifact_hash(artifact)?,
                &export_id,
                &self.inputs,
                Path::new(path),
                artifact["size"].as_u64().ok_or("download_size")?,
                &|| self.cancelled(),
            )
            .map_err(|error| match error {
                runtime_export::ExportError::Cancelled => "cancelled".into(),
                _ => format!("input_export:{error}"),
            })?;
            result.push(self.inputs.join(path));
        }
        Ok(result)
    }
}

struct ChildGuard {
    ctx: ModuleContext,
    task: TaskId,
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(task) = self.ctx.tasks().snapshot(&self.task) {
            if !task.state.is_terminal() {
                self.ctx.tasks().request_cancel(&CancelTaskRequest {
                    task_id: self.task.clone(),
                    attempt_generation: task.attempt_generation,
                });
            }
        }
    }
}

fn artifact_hash(artifact: &Value) -> Result<ArtifactHash, String> {
    ArtifactHash::new(
        artifact["hash"]["algorithm"]
            .as_str()
            .ok_or("hash_algorithm")?,
        artifact["hash"]["digest"].as_str().ok_or("hash_digest")?,
    )
    .map_err(|_| "hash_invalid".into())
}

fn maven_path(url: &str) -> Result<String, String> {
    let relative = url
        .strip_prefix("https://maven.neoforged.net/releases/")
        .or_else(|| url.strip_prefix("https://libraries.minecraft.net/"))
        .ok_or("library_source")?;
    if !safe_path(relative)
        || relative.contains(['?', '#', '%'])
        || relative.split('/').any(|p| p == "." || p == "..")
    {
        return Err("library_path".into());
    }
    Ok(relative.into())
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    game_install::reject_link(path)?;
    let file = fs::File::open(path).map_err(|_| "input_open")?;
    let metadata = file.metadata().map_err(|_| "input_metadata")?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err("input_size".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "input_read")?;
    if bytes.len() as u64 > limit {
        return Err("input_size".into());
    }
    Ok(bytes)
}

fn create_attempt(attempt: &Path) -> Result<(), String> {
    if !attempt.is_absolute() {
        return Err("attempt_path".into());
    }
    let parent = attempt.parent().ok_or("attempt_parent")?;
    game_install::checked_directory(parent)?;
    game_install::reject_link(attempt)?;
    fs::create_dir(attempt).map_err(|_| "attempt_already_exists_or_unavailable".into())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare(
    ctx: &ModuleContext,
    task: &TaskId,
    generation: AttemptGeneration,
    detail: &VerifiedVersionDetail,
    downloads: &Arc<dyn CoreModule>,
    java: &Path,
    attempt: &Path,
    launcher: &dyn ProcessLauncher,
    supervisor: &dyn ProcessSupervisor,
) -> Result<PreparedInstall, String> {
    if detail.metadata()["id"] != "1.20.4" {
        return Err("neoforge_version_unsupported".into());
    }
    if ctx.tasks().is_cancel_requested(task, generation) {
        return Err("cancelled".into());
    }
    let metadata =
        VersionMetadata::parse(detail.bytes(), Some("1.20.4")).map_err(|_| "base_metadata")?;
    let plan = VanillaInstallPlan::parse(&metadata, "windows", "amd64")?;
    create_attempt(attempt)?;
    let inputs = attempt.join("inputs");
    game_install::checked_directory(&inputs)?;
    let mut acquisition = Acquisition {
        ctx,
        task,
        generation,
        downloads,
        store: FileStore::new(ctx.dirs().cache.join("downloads")).map_err(|_| "cache_open")?,
        inputs,
        sequence: 0,
    };
    acquisition.phase("neoforge_installer")?;
    let installer = acquisition
        .acquire(
            crate::neoforge_sources::installer_payload("1.20.4", "20.4.253")?,
            &["installer.jar".into()],
        )?
        .remove(0);
    let installer_bytes = read_bounded(&installer, 64 * 1024 * 1024)?;
    let parsed = read_installer_archive("1.20.4", "20.4.253", &installer_bytes)?;
    let runtime = plan_runtime(&parsed.profile, &parsed.version_metadata, detail.metadata())?;
    let mappings =
        parse_verified_metadata("1.20.4", detail.bytes()).map_err(|e| format!("mappings:{e}"))?;
    let processors = processor_download_payload(&parsed.profile).map_err(|_| "processor_plan")?;
    let mut libraries = BTreeMap::new();
    acquisition.phase("neoforge_libraries")?;
    for payload in [processors, runtime.download_payload()] {
        let relative: Vec<String> = payload["artifacts"]
            .as_array()
            .ok_or("library_artifacts")?
            .iter()
            .map(|a| maven_path(a["url"].as_str().ok_or("library_url")?))
            .collect::<Result<_, String>>()?;
        let destinations: Vec<_> = relative.iter().map(|p| format!("libraries/{p}")).collect();
        let exported = acquisition.acquire(payload, &destinations)?;
        for (relative, path) in relative.into_iter().zip(exported) {
            libraries.insert(relative, path);
        }
    }
    // NeoForm MCP data is a profile archive, separate from the SHA-256 tool pins.
    for library in parsed
        .profile
        .libraries
        .iter()
        .filter(|library| library.path.ends_with(".zip"))
    {
        if libraries.contains_key(&library.path) {
            continue;
        }
        let relative = maven_path(&library.url)?;
        if relative != library.path {
            return Err("profile_library_path_mismatch".into());
        }
        let payload = json!({"command":"downloads.fetch","artifacts":[{
            "artifactId":format!("neo-data-{}",library.sha1),"sourceId":"neoforge-maven",
            "url":library.url,"size":library.size,"hash":{"algorithm":"sha1","digest":library.sha1}
        }]});
        let path = acquisition
            .acquire(payload, &[format!("libraries/{relative}")])?
            .remove(0);
        libraries.insert(relative, path);
    }
    acquisition.phase("neoforge_mappings")?;
    let mappings_path = acquisition
        .acquire(mappings.download_payload(), &["mappings.txt".into()])?
        .remove(0);
    let client = plan
        .artifacts
        .iter()
        .find(|a| a.path == "versions/1.20.4/1.20.4.jar")
        .ok_or("minecraft_client_missing")?;
    let client_payload = json!({"command":"downloads.fetch","artifacts":[{
        "artifactId":format!("neo-client-{}",client.sha1),"sourceId":"mojang-data",
        "url":client.url,"size":client.size,"hash":{"algorithm":"sha1","digest":client.sha1}
    }]});
    let minecraft = VerifiedInput {
        path: acquisition
            .acquire(client_payload, &["minecraft.jar".into()])?
            .remove(0),
        size: client.size,
        sha1: client.sha1.clone(),
    };
    acquisition.phase("neoforge_processors")?;
    let completed = install_client(
        ClientInstallInputs {
            destination: &attempt.join("game"),
            installer_bytes: &installer_bytes,
            installer_sha256: INSTALLER_SHA256,
            base_metadata_bytes: detail.bytes(),
            base_metadata_sha1: detail.sha1(),
            minecraft: &minecraft,
            java,
            cached_mappings: &mappings_path,
        },
        |path| {
            libraries
                .get(path)
                .cloned()
                .ok_or_else(|| StageError::Invalid(format!("missing library {path}")))
        },
        launcher,
        supervisor,
        Duration::from_secs(300),
        || acquisition.cancelled(),
        |phase| {
            let _ = ctx.tasks().set_phase(
                task,
                generation,
                TaskPhase::new(format!("neoforge_{phase}")).unwrap(),
            );
        },
    )
    .map_err(|e| match e {
        PipelineError::Cancelled => "cancelled".into(),
        PipelineError::StopFailed => "neoforge_stop_failed_quarantined".into(),
        other => format!("neoforge_pipeline:{other}"),
    })?;
    acquisition.phase("download_files")?;
    let mut batch = 0;
    game_install::fetch_files(
        ctx,
        task,
        generation,
        &completed.root,
        &acquisition.store,
        downloads,
        &plan.artifacts,
        &mut batch,
    )?;
    let mut files = plan.artifacts;
    if let Some(index_path) = plan.asset_index_path {
        acquisition.phase("assets")?;
        let index_bytes = read_bounded(&completed.root.join(index_path), 32 * 1024 * 1024)?;
        let index: Value = serde_json::from_slice(&index_bytes).map_err(|_| "asset_index")?;
        if index["virtual"].as_bool() == Some(true)
            || index["map_to_resources"].as_bool() == Some(true)
        {
            return Err("neoforge_asset_mapping_unsupported".into());
        }
        let assets = asset_artifacts(&index_bytes)?;
        game_install::fetch_files(
            ctx,
            task,
            generation,
            &completed.root,
            &acquisition.store,
            downloads,
            &assets,
            &mut batch,
        )?;
        files.extend(assets);
    }
    acquisition.phase("natives")?;
    game_install::extract_natives(&completed.root, &plan.native_archives, &|| {
        acquisition.cancelled()
    })?;
    acquisition.phase("neoforge_prepared")?;
    let merged = read_bounded(
        &completed.root.join("versions/1.20.4/1.20.4.json"),
        8 * 1024 * 1024,
    )?;
    let pipeline_marker = read_bounded(&completed.manifest_path, 2 * 1024 * 1024)?;
    let native_files =
        native_manifest(&completed.root.join("natives"), &|| acquisition.cancelled())?;
    let mut base_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(completed.root.join("neoforge-base.json"))
        .map_err(|_| "base_metadata_create")?;
    base_file
        .write_all(detail.bytes())
        .and_then(|_| base_file.sync_all())
        .map_err(|_| "base_metadata_write")?;
    drop(base_file);
    let marker = json!({"schemaVersion":1,"gameVersion":"1.20.4","loaderVersion":"20.4.253",
        "baseMetadataPath":"neoforge-base.json","baseMetadataSha1":detail.sha1(),
        "metadataSha256":format!("{:x}",Sha256::digest(&merged)),
        "pipelineMarkerSha256":format!("{:x}",Sha256::digest(&pipeline_marker)),
        "baseArtifacts":files.iter().map(artifact_identity).collect::<Vec<_>>(),
        "nativeFiles":native_files,
        "installed":false,"launchReady":false});
    let marker_bytes = serde_json::to_vec_pretty(&marker).map_err(|_| "prepare_marker_json")?;
    let prepared_sha256 = format!("{:x}", Sha256::digest(&marker_bytes));
    acquisition.phase("neoforge_integrity")?;
    let verified = crate::neoforge_integrity::verify_document(
        &completed.root,
        &marker_bytes,
        &prepared_sha256,
        &|| acquisition.cancelled(),
    )?;
    let temporary = completed.root.join(".neoforge-prepared.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| "prepare_marker_create")?;
    file.write_all(&marker_bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "prepare_marker_write")?;
    drop(file);
    if acquisition.cancelled() {
        return Err("cancelled".into());
    }
    fs::rename(temporary, completed.root.join("neoforge-prepared.json"))
        .map_err(|_| "prepare_marker_publish")?;
    Ok(PreparedInstall {
        root: completed.root,
        prepared_sha256,
        verified_files: verified.files.len(),
        main_class: verified.metadata["mainClass"]
            .as_str()
            .ok_or("main_class")?
            .into(),
    })
}

/// Preparation identity for later publication; this is not an instance receipt.
pub(crate) struct PreparedInstall {
    pub root: PathBuf,
    pub prepared_sha256: String,
    pub verified_files: usize,
    pub main_class: String,
}

fn artifact_identity(file: &InstallArtifact) -> Value {
    json!({"path":file.path,"sha1":file.sha1,"size":file.size,"url":file.url})
}

fn native_manifest(root: &Path, cancelled: &dyn Fn() -> bool) -> Result<Vec<Value>, String> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    let mut total = 0u64;
    let mut visited = 0usize;
    while let Some(directory) = pending.pop() {
        game_install::reject_link(&directory)?;
        for entry in fs::read_dir(&directory).map_err(|_| "natives_directory")? {
            if cancelled() {
                return Err("cancelled".into());
            }
            visited += 1;
            if visited > 4096 {
                return Err("natives_count".into());
            }
            let path = entry.map_err(|_| "natives_entry")?.path();
            game_install::reject_link(&path)?;
            let metadata = fs::symlink_metadata(&path).map_err(|_| "natives_metadata")?;
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() || metadata.len() > 64 * 1024 * 1024 {
                return Err("natives_size".into());
            }
            let mut input = fs::File::open(&path).map_err(|_| "natives_open")?;
            let mut digest = Sha256::new();
            let mut size = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                if cancelled() {
                    return Err("cancelled".into());
                }
                let count = input.read(&mut buffer).map_err(|_| "natives_read")?;
                if count == 0 {
                    break;
                }
                size += count as u64;
                total += count as u64;
                if size > metadata.len() || total > 256 * 1024 * 1024 {
                    return Err("natives_size".into());
                }
                digest.update(&buffer[..count]);
            }
            if size != metadata.len() {
                return Err("natives_size".into());
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| "natives_path")?
                .to_str()
                .ok_or("natives_path")?
                .replace('\\', "/");
            if !safe_path(&relative) {
                return Err("natives_path".into());
            }
            files.push(json!({"path":format!("natives/{relative}"),"size":size,"sha256":format!("{:x}",digest.finalize())}));
        }
    }
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parent_cancellation_waits_for_shared_download_child_to_stop() {
        use shtmcl_core::{tasks::TaskSpec, CoreApp};
        use shtmcl_domain::{ErrorEnvelope, ModuleId};
        struct CancelParent {
            ctx: ModuleContext,
            parent: TaskId,
            generation: AttemptGeneration,
        }
        impl CoreModule for CancelParent {
            fn id(&self) -> ModuleId {
                ModuleId::Downloads
            }
            fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
                let operation = request.operation_id.clone().unwrap();
                let child = self.ctx.tasks().accept(TaskSpec::new(
                    operation.clone(),
                    TaskPhase::new("download").unwrap(),
                ))?;
                self.ctx.tasks().transition(
                    &child.id,
                    child.attempt_generation,
                    TaskState::Running,
                )?;
                self.ctx.tasks().request_cancel(&CancelTaskRequest {
                    task_id: self.parent.clone(),
                    attempt_generation: self.generation,
                });
                let ctx = self.ctx.clone();
                let id = child.id.clone();
                std::thread::spawn(move || {
                    while !ctx
                        .tasks()
                        .is_cancel_requested(&id, child.attempt_generation)
                    {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    std::thread::sleep(Duration::from_millis(30));
                    ctx.tasks()
                        .cancel_now(&id, child.attempt_generation)
                        .unwrap();
                });
                Ok(CommandResponse::Accepted {
                    operation_id: operation,
                    task_id: child.id,
                })
            }
        }
        let root = std::env::temp_dir().join(format!(
            "neo-cancel-{}",
            shtmcl_platform::random_hex(16).unwrap()
        ));
        let app = CoreApp::new(shtmcl_platform::PlatformDirs::under(&root));
        let ctx = app.context();
        let parent = ctx
            .tasks()
            .accept(TaskSpec::new(
                OperationId::new("parent").unwrap(),
                TaskPhase::new("install").unwrap(),
            ))
            .unwrap();
        ctx.tasks()
            .transition(&parent.id, parent.attempt_generation, TaskState::Running)
            .unwrap();
        let downloads: Arc<dyn CoreModule> = Arc::new(CancelParent {
            ctx: ctx.clone(),
            parent: parent.id.clone(),
            generation: parent.attempt_generation,
        });
        let mut acquisition = Acquisition {
            ctx: &ctx,
            task: &parent.id,
            generation: parent.attempt_generation,
            downloads: &downloads,
            store: FileStore::new(root.join("cache")).unwrap(),
            inputs: root.join("inputs"),
            sequence: 0,
        };
        let payload = json!({"artifacts":[{"artifactId":"unused","sourceId":"fixture","size":1,
            "hash":{"algorithm":"sha1","digest":"0000000000000000000000000000000000000000"}}]});
        assert_eq!(
            acquisition
                .acquire(payload, &["input.jar".into()])
                .unwrap_err(),
            "cancelled"
        );
        assert!(ctx
            .tasks()
            .snapshots()
            .iter()
            .filter(|t| t.id != parent.id)
            .all(|t| t.state == TaskState::Cancelled));
        assert!(!root.join("inputs/input.jar").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn maven_paths_only_accept_known_sources_and_plain_relative_files() {
        assert_eq!(
            maven_path("https://maven.neoforged.net/releases/net/test.jar").unwrap(),
            "net/test.jar"
        );
        for url in [
            "https://example.com/test.jar",
            "https://libraries.minecraft.net/../test.jar",
            "https://libraries.minecraft.net/%2e%2e/test.jar",
            "https://libraries.minecraft.net/test.jar?x=1",
        ] {
            assert!(maven_path(url).is_err(), "{url}");
        }
    }
    #[test]
    fn existing_attempt_is_retained_and_not_reused() {
        let root = std::env::temp_dir().join(format!(
            "neo-attempt-{}",
            shtmcl_platform::random_hex(16).unwrap()
        ));
        create_attempt(&root).unwrap();
        fs::write(root.join("preserved"), b"existing").unwrap();
        assert!(create_attempt(&root).is_err());
        assert_eq!(fs::read(root.join("preserved")).unwrap(), b"existing");
        fs::remove_dir_all(root).unwrap();
    }
}

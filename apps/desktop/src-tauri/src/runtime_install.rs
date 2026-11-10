//! Official Windows x64 Java installation, composed from the shared downloader.
use serde_json::json;
use sha1::{Digest, Sha1};
use shtmcl_core::{
    downloads::{ArtifactHash, FileStore},
    launch::catalog::ManifestFetcher,
    launch::runtime::manifest::{self, RuntimeFile, RuntimeInstallPlan, RuntimeRelease},
    tasks::TaskSpec,
    CoreModule, ModuleContext,
};
use shtmcl_domain::{
    AttemptGeneration, CancelTaskRequest, CommandKind, CommandRequest, CommandResponse,
    ErrorCategory, ErrorCode, ErrorEnvelope, ModuleId, OperationId, RequestId, TaskId, TaskPhase,
    TaskState,
};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub(crate) struct RuntimeInstaller {
    ctx: ModuleContext,
    launch: Arc<dyn CoreModule>,
    downloads: Arc<dyn CoreModule>,
    metadata: Arc<dyn ManifestFetcher>,
    busy: Arc<AtomicBool>,
}

impl RuntimeInstaller {
    pub(crate) fn new(
        ctx: ModuleContext,
        launch: Arc<dyn CoreModule>,
        downloads: Arc<dyn CoreModule>,
        metadata: Arc<dyn ManifestFetcher>,
    ) -> Self {
        Self {
            ctx,
            launch,
            downloads,
            metadata,
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Change command forwarding while sharing the runtime provider and busy gate.
    pub(crate) fn with_delegate(&self, launch: Arc<dyn CoreModule>) -> Self {
        Self {
            ctx: self.ctx.clone(),
            launch,
            downloads: self.downloads.clone(),
            metadata: self.metadata.clone(),
            busy: self.busy.clone(),
        }
    }

    /// Called inside an existing preparation task; creates no second parent.
    pub(crate) fn ensure_runtime(
        &self,
        major: u32,
        task: &TaskId,
        generation: AttemptGeneration,
    ) -> Result<PathBuf, ErrorEnvelope> {
        let result = install(
            &self.ctx,
            task,
            generation,
            major,
            &self.downloads,
            self.metadata.as_ref(),
        )
        .map_err(|reason| {
            error("launch.runtime.install_failed", ErrorCategory::Internal)
                .with_detail_ref(format!("runtime:{reason}"))
        })?;
        result["javaPath"]
            .as_str()
            .map(PathBuf::from)
            .ok_or_else(ErrorEnvelope::internal_unavailable)
    }
}

impl CoreModule for RuntimeInstaller {
    fn id(&self) -> ModuleId {
        ModuleId::Launch
    }

    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        if request.command()?.as_str() != "launch.runtimes.install" {
            return self.launch.handle(request);
        }
        request.validate(CommandKind::Mutate)?;
        if !cfg!(all(windows, target_arch = "x86_64")) {
            return Err(error(
                "launch.runtime.unsupported",
                ErrorCategory::Unsupported,
            ));
        }
        let major = request.payload["major"]
            .as_u64()
            .filter(|major| (8..=30).contains(major))
            .ok_or_else(|| shtmcl_core::launch::errors::request_invalid("major"))?
            as u32;
        let operation = request
            .operation_id
            .clone()
            .ok_or_else(ErrorEnvelope::command_operation_required)?;
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(error("launch.runtime.busy", ErrorCategory::Conflict));
        }
        let task = match self.ctx.tasks().accept(TaskSpec::new(
            operation.clone(),
            TaskPhase::new("runtime_metadata").unwrap(),
        )) {
            Ok(task) => task,
            Err(error) => {
                self.busy.store(false, Ordering::Release);
                return Err(error);
            }
        };
        let (ctx, downloads, metadata, busy) = (
            self.ctx.clone(),
            self.downloads.clone(),
            self.metadata.clone(),
            self.busy.clone(),
        );
        let (task_id, generation) = (task.id, task.attempt_generation);
        let job_id = task_id.clone();
        let spawn = std::thread::Builder::new()
            .name("java-install".into())
            .spawn(move || {
                let _busy = BusyGuard(busy);
                let _ = ctx
                    .tasks()
                    .transition(&job_id, generation, TaskState::Running);
                match install(
                    &ctx,
                    &job_id,
                    generation,
                    major,
                    &downloads,
                    metadata.as_ref(),
                ) {
                    Ok(result) => {
                        let _ = ctx.tasks().succeed(&job_id, generation, Some(result));
                    }
                    Err(reason) if reason == "cancelled" => {
                        let _ = ctx.tasks().cancel_now(&job_id, generation);
                    }
                    Err(reason) => {
                        let _ = ctx.tasks().fail(
                            &job_id,
                            generation,
                            error("launch.runtime.install_failed", ErrorCategory::Internal)
                                .with_detail_ref(format!("runtime:{reason}")),
                        );
                    }
                }
            });
        if spawn.is_err() {
            self.busy.store(false, Ordering::Release);
            let _ =
                self.ctx
                    .tasks()
                    .fail(&task_id, generation, ErrorEnvelope::internal_unavailable());
            return Err(ErrorEnvelope::internal_unavailable());
        }
        Ok(CommandResponse::Accepted {
            operation_id: operation,
            task_id,
        })
    }
}

struct BusyGuard(Arc<AtomicBool>);
impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn error(code: &'static str, category: ErrorCategory) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::from_static(code), category, false)
}

fn check_cancel(
    ctx: &ModuleContext,
    task: &TaskId,
    generation: AttemptGeneration,
) -> Result<(), String> {
    if ctx.tasks().is_cancel_requested(task, generation) {
        Err("cancelled".into())
    } else {
        Ok(())
    }
}

fn install(
    ctx: &ModuleContext,
    task: &TaskId,
    generation: AttemptGeneration,
    major: u32,
    downloads: &Arc<dyn CoreModule>,
    metadata: &dyn ManifestFetcher,
) -> Result<serde_json::Value, String> {
    if !(8..=30).contains(&major) {
        return Err("major_invalid".into());
    }
    check_cancel(ctx, task, generation)?;
    let cancellation = Cancellation::watch(ctx.clone(), task.clone(), generation)?;
    let root = ctx.dirs().data.join("java");
    directory(&root)?;
    let lock_path = root.join(".runtime-install.lock");
    reject_link(&lock_path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(|_| "lock_open")?;
    lock.try_lock().map_err(|_| "runtime_locked")?;

    // A verified installed tree is usable without any network request.
    if let Ok(catalog) = read_bounded(
        &root.join("catalog.json"),
        manifest::MAX_RUNTIME_CATALOG_BYTES,
    ) {
        if let Ok(release) = manifest::select_release(&catalog, "windows-x64", major) {
            if let Ok(plan) = cached_plan(&root, &release) {
                let target = target_path(&root, major, &release);
                if verify_tree(&target, &plan, ctx, task, generation)? {
                    return Ok(result(&release, major, &target, &plan, true));
                }
            }
        }
    }
    check_cancel(ctx, task, generation)?;
    let catalog = metadata
        .fetch(
            manifest::RUNTIME_CATALOG_URL,
            manifest::MAX_RUNTIME_CATALOG_BYTES,
        )
        .map_err(|_| "catalog_fetch")?;
    let release = manifest::select_release(&catalog, "windows-x64", major)
        .map_err(|_| "runtime_unavailable")?;
    let bytes = metadata
        .fetch(&release.manifest_url, manifest::MAX_RUNTIME_MANIFEST_BYTES)
        .map_err(|_| "manifest_fetch")?;
    if bytes.len() as u64 != release.manifest_size {
        return Err("manifest_size".into());
    }
    let plan = manifest::parse_install_plan(&bytes, &release.manifest_sha1, "windows-x64")
        .map_err(|_| "manifest_invalid")?;
    if plan
        .files
        .iter()
        .any(|file| matches!(file, RuntimeFile::Link { .. }))
    {
        return Err("windows_runtime_links".into());
    }
    check_cancel(ctx, task, generation)?;
    cache_write(
        &root.join(format!("manifest-{}.json", release.manifest_sha1)),
        &bytes,
    )?;
    cache_write(&root.join("catalog.json"), &catalog)?;
    let target = target_path(&root, major, &release);
    if verify_tree(&target, &plan, ctx, task, generation)? {
        return Ok(result(&release, major, &target, &plan, true));
    }
    reject_link(&target)?;
    if target.exists() {
        return Err("installed_runtime_damaged".into());
    }
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "clock")?
        .as_nanos();
    // Discovery recursively scans data/java. Keep incomplete executables out
    // of that tree until the final same-volume rename publishes them.
    let staging_root = ctx.dirs().data.join("runtime-staging");
    directory(&staging_root)?;
    let stage = staging_root.join(format!("java-{}-{task}-{unique}", std::process::id()));
    fs::create_dir(&stage).map_err(|_| "stage_create")?;
    let _stage = Staging(stage.clone());
    let store = FileStore::new(ctx.dirs().cache.join("downloads")).map_err(|_| "cache_open")?;
    let _ = ctx.tasks().set_phase(
        task,
        generation,
        TaskPhase::new("runtime_download").unwrap(),
    );

    let mut missing = Vec::new();
    let mut hashes = BTreeSet::new();
    for file in &plan.files {
        check_cancel(ctx, task, generation)?;
        if let RuntimeFile::Directory { path } = file {
            directory(&stage.join(path))?;
        }
        if let RuntimeFile::File {
            path, sha1, size, ..
        } = file
        {
            match export(
                &store,
                task,
                &stage,
                path,
                sha1,
                *size,
                &cancellation.cancelled,
            ) {
                Ok(_) => {}
                Err(
                    crate::runtime_export::ExportError::CacheMiss
                    | crate::runtime_export::ExportError::DigestMismatch
                    | crate::runtime_export::ExportError::SizeMismatch { .. },
                ) => {
                    if hashes.insert(sha1.clone()) {
                        missing.push(file);
                    }
                }
                Err(crate::runtime_export::ExportError::Cancelled) => {
                    return Err("cancelled".into())
                }
                Err(_) => return Err("cache_export".into()),
            }
        }
    }
    for (batch_index, batch) in missing.chunks(256).enumerate() {
        check_cancel(ctx, task, generation)?;
        let identity = format!("{task}-runtime-{batch_index}");
        let request = CommandRequest {
            request_id: RequestId::new(identity.clone()).map_err(|_| "request_id")?,
            operation_id: Some(OperationId::new(identity).map_err(|_| "operation_id")?),
            entity_id: None,
            expected_revision: None,
            payload: json!({"command":"downloads.fetch","artifacts":batch.iter().map(|file| match file {
                RuntimeFile::File { url, sha1, size, .. } => json!({"artifactId":format!("java/{sha1}"),"sourceId":if url.contains("launcher.mojang.com/") {"mojang-launcher"} else {"mojang-data"},"url":url,"size":size,"hash":{"algorithm":"sha1","digest":sha1}}),
                _ => unreachable!(),
            }).collect::<Vec<_>>()}),
        };
        let child = match downloads.handle(&request).map_err(|_| "download_accept")? {
            CommandResponse::Accepted { task_id, .. } => task_id,
            _ => return Err("download_not_accepted".into()),
        };
        wait_child(ctx, task, generation, &child)?;
    }
    let _ = ctx
        .tasks()
        .set_phase(task, generation, TaskPhase::new("runtime_export").unwrap());
    let mut completed = 0;
    for file in &plan.files {
        check_cancel(ctx, task, generation)?;
        if let RuntimeFile::File {
            path, sha1, size, ..
        } = file
        {
            export(
                &store,
                task,
                &stage,
                path,
                sha1,
                *size,
                &cancellation.cancelled,
            )
            .map_err(|error| {
                if matches!(error, crate::runtime_export::ExportError::Cancelled) {
                    "cancelled"
                } else {
                    "cache_export"
                }
            })?;
            completed += size;
            let _ = ctx.tasks().set_progress(
                task,
                generation,
                shtmcl_core::tasks::bytes_progress(completed, Some(plan.total_bytes))
                    .map_err(|_| "progress")?,
            );
        }
    }
    if !verify_tree(&stage, &plan, ctx, task, generation)? {
        return Err("staging_verification".into());
    }
    check_cancel(ctx, task, generation)?;
    let _ = ctx
        .tasks()
        .set_phase(task, generation, TaskPhase::new("runtime_commit").unwrap());
    fs::rename(&stage, &target).map_err(|_| "publish")?;
    Ok(result(&release, major, &target, &plan, false))
}

fn result(
    release: &RuntimeRelease,
    major: u32,
    target: &Path,
    plan: &RuntimeInstallPlan,
    reused: bool,
) -> serde_json::Value {
    json!({"major":major,"version":release.version,"path":target,"javaPath":target.join(&plan.java_executable),"reused":reused})
}

fn target_path(root: &Path, major: u32, release: &RuntimeRelease) -> PathBuf {
    root.join(format!("{major}-{}", release.manifest_sha1))
}

fn cached_plan(root: &Path, release: &RuntimeRelease) -> Result<RuntimeInstallPlan, String> {
    let bytes = read_bounded(
        &root.join(format!("manifest-{}.json", release.manifest_sha1)),
        manifest::MAX_RUNTIME_MANIFEST_BYTES,
    )?;
    if bytes.len() as u64 != release.manifest_size {
        return Err("manifest_size".into());
    }
    manifest::parse_install_plan(&bytes, &release.manifest_sha1, "windows-x64")
        .map_err(|_| "cached_manifest_invalid".into())
}

fn export(
    store: &FileStore,
    task: &TaskId,
    stage: &Path,
    path: &str,
    sha1: &str,
    size: u64,
    cancelled: &AtomicBool,
) -> Result<u64, crate::runtime_export::ExportError> {
    crate::runtime_export::export_cached_artifact(
        store,
        &ArtifactHash::new("sha1", sha1)
            .map_err(|_| crate::runtime_export::ExportError::InvalidPath)?,
        &format!("java-export/{task}/{sha1}"),
        stage,
        Path::new(path),
        size,
        cancelled,
    )
}

struct Cancellation {
    cancelled: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Cancellation {
    fn watch(
        ctx: ModuleContext,
        task: TaskId,
        generation: AttemptGeneration,
    ) -> Result<Self, String> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (signal, finished) = (cancelled.clone(), done.clone());
        let worker = std::thread::Builder::new()
            .name("java-install-cancel".into())
            .spawn(move || {
                while !finished.load(Ordering::Acquire) {
                    if ctx.tasks().is_cancel_requested(&task, generation) {
                        signal.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            })
            .map_err(|_| "cancel_watch")?;
        Ok(Self {
            cancelled,
            done,
            worker: Some(worker),
        })
    }
}
impl Drop for Cancellation {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn wait_child(
    ctx: &ModuleContext,
    parent: &TaskId,
    generation: AttemptGeneration,
    child: &TaskId,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(20 * 60);
    let mut cancel_deadline = None;
    loop {
        let snapshot = ctx.tasks().snapshot(child).ok_or("download_task_missing")?;
        let cancelled = ctx.tasks().is_cancel_requested(parent, generation);
        let expired = Instant::now() > deadline;
        if cancelled || expired {
            ctx.tasks().request_cancel(&CancelTaskRequest {
                task_id: child.clone(),
                attempt_generation: snapshot.attempt_generation,
            });
            cancel_deadline.get_or_insert_with(|| Instant::now() + Duration::from_secs(10));
        }
        match snapshot.state {
            TaskState::Succeeded if !cancelled && !expired => return Ok(()),
            TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled => {
                return Err(if cancelled {
                    "cancelled".into()
                } else {
                    format!(
                        "download_failed:{}",
                        snapshot
                            .error
                            .as_ref()
                            .map(|error| error.code.as_str())
                            .unwrap_or("unknown")
                    )
                })
            }
            _ => {}
        }
        // An overdue child remains owned by the shared task service. Request
        // cancellation before exiting; it never writes into runtime staging.
        if expired {
            return Err("download_timeout".into());
        }
        if cancel_deadline.is_some_and(|deadline| Instant::now() > deadline) {
            return Err("cancelled".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn verify_tree(
    root: &Path,
    plan: &RuntimeInstallPlan,
    ctx: &ModuleContext,
    task: &TaskId,
    generation: AttemptGeneration,
) -> Result<bool, String> {
    reject_link(root)?;
    if !root.exists() {
        return Ok(false);
    }
    for file in &plan.files {
        check_cancel(ctx, task, generation)?;
        if let RuntimeFile::File {
            path, sha1, size, ..
        } = file
        {
            let destination = root.join(path);
            reject_parents(&destination)?;
            if !matches_file(&destination, sha1, *size, &|| {
                ctx.tasks().is_cancel_requested(task, generation)
            })? {
                return Ok(false);
            }
        }
        if matches!(file, RuntimeFile::Link { .. }) {
            return Err("windows_runtime_links".into());
        }
    }
    Ok(true)
}

fn matches_file(
    path: &Path,
    sha1: &str,
    size: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<bool, String> {
    reject_link(path)?;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err("file_read".into()),
    };
    if file.metadata().map_err(|_| "file_metadata")?.len() != size {
        return Ok(false);
    }
    let mut hash = Sha1::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        if cancelled() {
            return Err("cancelled".into());
        }
        let count = file.read(&mut buffer).map_err(|_| "file_hash")?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()) == sha1)
}

fn reject_link(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    return Err("reparse_path".into());
                }
            }
            if meta.file_type().is_symlink() {
                return Err("symlink_path".into());
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("path_metadata".into()),
    }
}

fn reject_parents(path: &Path) -> Result<(), String> {
    for parent in path.ancestors() {
        reject_link(parent)?;
    }
    Ok(())
}
fn directory(path: &Path) -> Result<(), String> {
    reject_parents(path)?;
    fs::create_dir_all(path).map_err(|_| "directory_create".into())
}
fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    reject_parents(path)?;
    let file = File::open(path).map_err(|_| "metadata_read")?;
    if file.metadata().map_err(|_| "metadata_stat")?.len() > limit as u64 {
        return Err("metadata_too_large".into());
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "metadata_read")?;
    if bytes.len() > limit {
        return Err("metadata_too_large".into());
    }
    Ok(bytes)
}
fn cache_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    reject_parents(path)?;
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "clock")?
        .as_nanos();
    let temp = path.with_extension(format!("{}.{unique}.tmp", std::process::id()));
    reject_link(&temp)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(|_| "metadata_write")?;
    let result = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
        return Err("metadata_write".into());
    }
    if fs::rename(&temp, path).is_err() {
        let _ = fs::remove_file(&temp);
        return Err("metadata_publish".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shtmcl_platform::PlatformDirs;

    struct NoNetwork;
    impl ManifestFetcher for NoNetwork {
        fn fetch(&self, _: &str, _: usize) -> Result<Vec<u8>, String> {
            panic!("verified installed runtime must not access network")
        }
    }
    struct NoDownloads;
    impl CoreModule for NoDownloads {
        fn id(&self) -> ModuleId {
            ModuleId::Downloads
        }
        fn handle(&self, _: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
            panic!("installed runtime must not start downloads")
        }
    }
    fn root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shtmcl-runtime-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    #[test]
    fn hash_checks_content_and_observes_cancellation() {
        let root = root("hash");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("file");
        fs::write(&path, b"java").unwrap();
        let hash = format!("{:x}", Sha1::digest(b"java"));
        assert!(matches_file(&path, &hash, 4, &|| false).unwrap());
        fs::write(&path, b"bad!").unwrap();
        assert!(!matches_file(&path, &hash, 4, &|| false).unwrap());
        assert_eq!(
            matches_file(&path, &hash, 4, &|| true).unwrap_err(),
            "cancelled"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn verified_installed_runtime_is_reused_without_network() {
        let root = root("reuse");
        let app = shtmcl_core::CoreApp::new(PlatformDirs::under(&root));
        let ctx = app.context();
        let task = ctx
            .tasks()
            .accept(TaskSpec::new(
                OperationId::new("reuse").unwrap(),
                TaskPhase::new("runtime").unwrap(),
            ))
            .unwrap();
        let java_root = ctx.dirs().data.join("java");
        directory(&java_root).unwrap();
        let hash = format!("{:x}", Sha1::digest(b"fixture-java"));
        let manifest = serde_json::to_vec(&json!({"files":{"bin/java.exe":{"type":"file","executable":true,"downloads":{"raw":{"sha1":hash,"size":12,"url":format!("https://piston-data.mojang.com/v1/objects/{hash}/java.exe")}}}}})).unwrap();
        let manifest_hash = format!("{:x}", Sha1::digest(&manifest));
        let catalog = serde_json::to_vec(&json!({"windows-x64":{"jre-legacy":[{"version":{"name":"1.8.0_51","released":"2025-01-01"},"manifest":{"sha1":manifest_hash,"size":manifest.len(),"url":format!("https://piston-meta.mojang.com/v1/packages/{manifest_hash}/manifest.json")}}]}})).unwrap();
        let release = manifest::select_release(&catalog, "windows-x64", 8).unwrap();
        cache_write(&java_root.join("catalog.json"), &catalog).unwrap();
        cache_write(
            &java_root.join(format!("manifest-{manifest_hash}.json")),
            &manifest,
        )
        .unwrap();
        let target = target_path(&java_root, 8, &release);
        directory(&target.join("bin")).unwrap();
        fs::write(target.join("bin/java.exe"), b"fixture-java").unwrap();
        let result = install(
            &ctx,
            &task.id,
            task.attempt_generation,
            8,
            &(Arc::new(NoDownloads) as Arc<dyn CoreModule>),
            &NoNetwork,
        )
        .unwrap();
        assert_eq!(result["reused"], true);
        assert_eq!(
            result["javaPath"],
            target.join("bin/java.exe").to_string_lossy().as_ref()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn metadata_cache_replaces_atomically_and_reads_are_bounded() {
        let root = root("metadata");
        directory(&root).unwrap();
        let path = root.join("catalog.json");
        cache_write(&path, b"first").unwrap();
        cache_write(&path, b"second").unwrap();
        assert_eq!(read_bounded(&path, 6).unwrap(), b"second");
        assert!(read_bounded(&path, 5).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "downloads an official Java runtime into an explicitly supplied retained test root"]
    fn real_runtime_install_and_offline_reuse() {
        let root = std::env::var_os("SHTMCL_RUNTIME_TEST_ROOT")
            .map(PathBuf::from)
            .expect("set retained SHTMCL_RUNTIME_TEST_ROOT");
        let major: u32 = std::env::var("SHTMCL_RUNTIME_TEST_MAJOR")
            .unwrap_or_else(|_| "8".into())
            .parse()
            .unwrap();
        let state = crate::AppState::new(PlatformDirs::under(&root));
        let request = CommandRequest {
            request_id: RequestId::new("real-runtime-install").unwrap(),
            operation_id: Some(OperationId::new("real-runtime-install").unwrap()),
            entity_id: None,
            expected_revision: None,
            payload: json!({"command":"launch.runtimes.install","major":major}),
        };
        let accepted = state
            .core
            .dispatch_mutation(ModuleId::Launch, &request)
            .unwrap();
        let task_id = match accepted {
            CommandResponse::Accepted { task_id, .. } => task_id,
            other => panic!("{other:?}"),
        };
        let deadline = Instant::now() + Duration::from_secs(20 * 60);
        let result = loop {
            let task = state.core.context().tasks().snapshot(&task_id).unwrap();
            if matches!(
                task.state,
                TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
            ) {
                if task.state != TaskState::Succeeded {
                    for child in state.core.snapshot() {
                        println!(
                            "task={} state={:?} phase={} error={:?}",
                            child.id, child.state, child.phase, child.error
                        );
                    }
                }
                assert_eq!(task.state, TaskState::Succeeded, "{task:?}");
                break task.result.unwrap();
            }
            assert!(Instant::now() < deadline, "runtime install timeout");
            std::thread::sleep(Duration::from_millis(100));
        };
        let java = result["javaPath"].as_str().unwrap();
        let output = std::process::Command::new(java)
            .arg("-version")
            .output()
            .unwrap();
        assert!(output.status.success());
        println!(
            "installed={result} runtime_version={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let ctx = state.core.context();
        let reuse = ctx
            .tasks()
            .accept(TaskSpec::new(
                OperationId::new("offline-runtime-reuse").unwrap(),
                TaskPhase::new("runtime").unwrap(),
            ))
            .unwrap();
        let result = install(
            &ctx,
            &reuse.id,
            reuse.attempt_generation,
            major,
            &(Arc::new(NoDownloads) as Arc<dyn CoreModule>),
            &NoNetwork,
        )
        .unwrap();
        assert_eq!(result["reused"], true);
        println!("offline_reuse={result}");
    }
}

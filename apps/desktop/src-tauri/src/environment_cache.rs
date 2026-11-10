//! Vanilla environment recipes reference the shared verified object cache.
//! Downloading one never creates an instance or runs a loader/Java process.
use crate::{
    game_install::{checked_directory, fetch_group, record_execution, reject_link},
    install_operations::{InstallOperation, InstallOperations},
    version_metadata_source::CachedVersionMetadataSource,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use shtmcl_core::{
    downloads::{ArtifactHash, FileStore},
    launch::{
        catalog::{VersionCatalog, VersionMetadata},
        install::{asset_artifacts, safe_path, InstallArtifact, VanillaInstallPlan},
    },
    tasks::TaskSpec,
    CoreModule, ModuleContext,
};
use shtmcl_domain::{
    AttemptGeneration, CommandKind, CommandRequest, CommandResponse, ErrorEnvelope, TaskId,
    TaskPhase, TaskState,
};
use shtmcl_platform::PlatformDirs;
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const MAX_RECIPES: usize = 128;
const MAX_RECIPE_BYTES: u64 = 32 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Resource {
    sha1: String,
    size: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Recipe {
    schema_version: u32,
    id: String,
    version_id: String,
    metadata: Value,
    resources: Vec<Resource>,
    #[serde(default)]
    pinned: bool,
}

fn root(dirs: &PlatformDirs) -> PathBuf {
    dirs.cache.join("environments/v1")
}
fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}
fn identity(version: &str, metadata: &Value, resources: &[Resource]) -> Result<String, String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&json!(["windows-x64", version, metadata, resources]))
                .map_err(|_| "environment_json")?
        )
    ))
}
fn checked_root(dirs: &PlatformDirs) -> Result<PathBuf, String> {
    let path = root(dirs);
    checked_directory(&path)?;
    Ok(path)
}
fn lock(dirs: &PlatformDirs) -> Result<File, String> {
    let path = checked_root(dirs)?.join(".lock");
    reject_link(&path)?;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|_| "environment_lock")?;
    file.try_lock().map_err(|_| "environment_busy")?;
    Ok(file)
}
fn load(dirs: &PlatformDirs, id: &str) -> Result<Recipe, String> {
    if !valid_id(id) {
        return Err("environment_id".into());
    }
    let path = root(dirs).join(format!("{id}.json"));
    for ancestor in path.ancestors() {
        reject_link(ancestor)?;
    }
    let file = File::open(path).map_err(|_| "environment_read")?;
    if !file.metadata().map_err(|_| "environment_read")?.is_file() {
        return Err("environment_read".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECIPE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "environment_read")?;
    if bytes.len() as u64 > MAX_RECIPE_BYTES {
        return Err("environment_size".into());
    }
    let recipe: Recipe = serde_json::from_slice(&bytes).map_err(|_| "environment_json")?;
    if recipe.schema_version != 1
        || recipe.id != id
        || identity(&recipe.version_id, &recipe.metadata, &recipe.resources)? != id
        || !safe_path(&recipe.version_id)
        || recipe.version_id.contains('/')
        || recipe.resources.len() > 210_000
        || recipe.resources.iter().any(|resource| {
            resource.sha1.len() != 40 || !resource.sha1.bytes().all(|b| b.is_ascii_hexdigit())
        })
        || recipe
            .resources
            .iter()
            .try_fold(0u64, |total, resource| total.checked_add(resource.size))
            .is_none_or(|size| size > 32 * 1024 * 1024 * 1024)
    {
        return Err("environment_invalid".into());
    }
    let metadata = VersionMetadata::from_value(recipe.metadata.clone(), Some(&recipe.version_id))
        .map_err(|_| "environment_metadata")?;
    VanillaInstallPlan::parse(&metadata, "windows", "amd64")?;
    Ok(recipe)
}
#[cfg(test)]
pub(crate) fn load_metadata(dirs: &PlatformDirs, id: &str, version: &str) -> Result<Value, String> {
    let recipe = load(dirs, id)?;
    if recipe.version_id != version {
        return Err("environment_version".into());
    }
    Ok(recipe.metadata)
}
fn use_lease(dirs: &PlatformDirs, id: &str) -> Result<File, String> {
    if !valid_id(id) {
        return Err("environment_id".into());
    }
    let directory = root(dirs).join(".locks");
    checked_directory(&directory)?;
    let path = directory.join(format!("{id}.use-lock"));
    reject_link(&path)?;
    let lease = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|_| "environment_lease")?;
    lease.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => "environment_in_use",
        _ => "environment_lease",
    })?;
    Ok(lease)
}
fn acquire(dirs: &PlatformDirs, id: &str) -> Result<(Recipe, File), String> {
    let _store = lock(dirs)?;
    let recipe = load(dirs, id)?;
    let lease = use_lease(dirs, id)?;
    Ok((recipe, lease))
}
pub(crate) fn acquire_metadata(
    dirs: &PlatformDirs,
    id: &str,
    version: &str,
) -> Result<(Value, File), String> {
    let (recipe, lease) = acquire(dirs, id)?;
    if recipe.version_id != version {
        return Err("environment_version".into());
    }
    Ok((recipe.metadata, lease))
}
fn pin(dirs: &PlatformDirs, id: &str, pinned: bool) -> Result<(), String> {
    let _store = lock(dirs)?;
    let mut recipe = load(dirs, id)?;
    recipe.pinned = pinned;
    write_recipe(dirs, &recipe)
}
fn remove(dirs: &PlatformDirs, id: &str) -> Result<(), String> {
    let _store = lock(dirs)?;
    let recipe = load(dirs, id)?;
    if recipe.pinned {
        return Err("environment_pinned".into());
    }
    let _lease = use_lease(dirs, id)?;
    std::fs::remove_file(root(dirs).join(format!("{id}.json")))
        .map_err(|_| "environment_remove".into())
}
fn publish(dirs: &PlatformDirs, recipe: &Recipe) -> Result<(), String> {
    let _lock = lock(dirs)?;
    let paths = std::fs::read_dir(root(dirs)).map_err(|_| "environment_list")?;
    let mut count = 0;
    for (index, path) in paths.enumerate() {
        if index > MAX_RECIPES * 3 {
            return Err("environment_limit".into());
        }
        let path = path.map_err(|_| "environment_list")?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            count += 1;
        }
        if count > MAX_RECIPES {
            return Err("environment_limit".into());
        }
    }
    let target = root(dirs).join(format!("{}.json", recipe.id));
    reject_link(&target)?;
    if count >= MAX_RECIPES && !target.exists() {
        return Err("environment_limit".into());
    }
    let mut value = serde_json::to_value(recipe).map_err(|_| "environment_json")?;
    if let Ok(existing) = load(dirs, &recipe.id) {
        value["pinned"] = json!(existing.pinned);
    }
    write_value(dirs, &recipe.id, &value)
}
fn write_recipe(dirs: &PlatformDirs, recipe: &Recipe) -> Result<(), String> {
    write_value(
        dirs,
        &recipe.id,
        &serde_json::to_value(recipe).map_err(|_| "environment_json")?,
    )
}
fn write_value(dirs: &PlatformDirs, id: &str, value: &Value) -> Result<(), String> {
    let target = root(dirs).join(format!("{id}.json"));
    reject_link(&target)?;
    let bytes = serde_json::to_vec(value).map_err(|_| "environment_json")?;
    if bytes.len() as u64 > MAX_RECIPE_BYTES {
        return Err("environment_size".into());
    }
    let temp = root(dirs).join(format!(
        "{}.tmp",
        shtmcl_platform::entropy::random_hex(16).map_err(|_| "environment_entropy")?
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|_| "environment_write")?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "environment_write")?;
        drop(file);
        std::fs::rename(&temp, target).map_err(|_| "environment_commit".to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}
fn catalogue(dirs: &PlatformDirs) -> Result<(Vec<Value>, usize), String> {
    let _lock = lock(dirs)?;
    let mut entries = Vec::new();
    let mut skipped = 0;
    for (index, item) in std::fs::read_dir(root(dirs))
        .map_err(|_| "environment_list")?
        .enumerate()
    {
        if index > MAX_RECIPES * 3 {
            return Err("environment_limit".into());
        }
        let path = item.map_err(|_| "environment_list")?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or("environment_filename")?;
        let recipe = match load(dirs, id) {
            Ok(recipe) => recipe,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let bytes = recipe
            .resources
            .iter()
            .try_fold(0u64, |total, resource| total.checked_add(resource.size))
            .ok_or("environment_size")?;
        entries.push(json!({"id":recipe.id,"versionId":recipe.version_id,"platform":"windows-x64","resourceCount":recipe.resources.len(),"bytes":bytes,"path":root(dirs),"pinned":recipe.pinned}));
        if entries.len() > MAX_RECIPES {
            return Err("environment_limit".into());
        }
    }
    entries.sort_by(|a, b| a["versionId"].as_str().cmp(&b["versionId"].as_str()));
    if entries.is_empty() && skipped > 0 {
        return Err("environment_catalogue_unreadable".into());
    }
    Ok((entries, skipped))
}

struct IndexWork(PathBuf);
impl Drop for IndexWork {
    fn drop(&mut self) {
        if self.0.ancestors().all(|path| reject_link(path).is_ok()) {
            let _ = std::fs::remove_file(self.0.join("index.json"));
            let _ = std::fs::remove_dir(&self.0);
        }
    }
}

pub(crate) fn handle(
    ctx: &ModuleContext,
    catalog: &Arc<VersionCatalog>,
    metadata: &Arc<CachedVersionMetadataSource>,
    downloads: &Arc<dyn CoreModule>,
    busy: &Arc<AtomicBool>,
    request: &CommandRequest,
) -> Result<CommandResponse, ErrorEnvelope> {
    let command = request.command()?;
    if command.as_str() == "launch.environments.list" {
        request.validate(CommandKind::Read)?;
        let (entries, skipped_entries) =
            catalogue(ctx.dirs()).map_err(|_| ErrorEnvelope::internal_unavailable())?;
        return Ok(CommandResponse::Completed {
            result: json!({"entries":entries,"skippedEntries":skipped_entries}),
        });
    }
    if matches!(
        command.as_str(),
        "launch.environments.pin" | "launch.environments.remove"
    ) {
        request.validate(CommandKind::Mutate)?;
        let id = request.payload["environmentId"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or_else(|| shtmcl_core::launch::errors::request_invalid("environmentId"))?;
        if command.as_str().ends_with(".pin") {
            let pinned = request.payload["pinned"]
                .as_bool()
                .ok_or_else(|| shtmcl_core::launch::errors::request_invalid("pinned"))?;
            pin(ctx.dirs(), id, pinned).map_err(management_error)?;
            return Ok(CommandResponse::Completed {
                result: json!({"environmentId":id,"pinned":pinned}),
            });
        }
        remove(ctx.dirs(), id).map_err(management_error)?;
        return Ok(CommandResponse::Completed {
            result: json!({"environmentId":id,"removed":true}),
        });
    }
    if !matches!(
        command.as_str(),
        "launch.environments.download" | "launch.environments.repair"
    ) {
        return Err(shtmcl_core::launch::errors::request_invalid("command"));
    }
    request.validate(CommandKind::Mutate)?;
    let cached = if command.as_str().ends_with(".repair") {
        let id = request.payload["environmentId"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or_else(|| shtmcl_core::launch::errors::request_invalid("environmentId"))?;
        Some(acquire(ctx.dirs(), id).map_err(management_error)?)
    } else {
        None
    };
    let version = if let Some((recipe, _)) = &cached {
        recipe.version_id.clone()
    } else {
        request.payload["versionId"]
            .as_str()
            .filter(|id| safe_path(id) && !id.contains('/') && id.len() <= 128)
            .ok_or_else(|| shtmcl_core::launch::errors::request_invalid("versionId"))?
            .to_string()
    };
    let operation = request
        .operation_id
        .clone()
        .ok_or_else(ErrorEnvelope::command_operation_required)?;
    if busy.swap(true, Ordering::AcqRel) {
        return Err(shtmcl_domain::ErrorEnvelope::new(
            shtmcl_domain::ErrorCode::from_static("launch.install.busy"),
            shtmcl_domain::ErrorCategory::Conflict,
            false,
        ));
    }
    let busy_guard = crate::game_install::BusyGuard(busy.clone());
    let task = ctx.tasks().accept(TaskSpec::new(
        operation.clone(),
        TaskPhase::new("environment_cache").unwrap(),
    ))?;
    let record =
        InstallOperation::environment(operation.clone(), task.id.clone(), &version, ctx.dirs());
    let lease = match InstallOperations::new(ctx.dirs()).insert(&record) {
        Ok(lease) => lease,
        Err(_) => {
            let _ = ctx.tasks().fail(
                &task.id,
                task.attempt_generation,
                ErrorEnvelope::internal_unavailable(),
            );
            return Err(ErrorEnvelope::internal_unavailable());
        }
    };
    if record_execution(ctx, &task.id).is_err() {
        let _ = ctx.tasks().fail(
            &task.id,
            task.attempt_generation,
            ErrorEnvelope::internal_unavailable(),
        );
        return Err(ErrorEnvelope::internal_unavailable());
    }
    let (ctx, catalog, metadata, downloads) = (
        ctx.clone(),
        catalog.clone(),
        metadata.clone(),
        downloads.clone(),
    );
    let on_spawn_failure = ctx.clone();
    let (id, generation) = (task.id.clone(), task.attempt_generation);
    let job_id = id.clone();
    let spawn = std::thread::Builder::new()
        .name("environment-cache".into())
        .spawn(move || {
            let _busy = busy_guard;
            let _lease = lease;
            let (cached_recipe, _environment_lease) = match cached {
                Some((recipe, lease)) => (Some(recipe), Some(lease)),
                None => (None, None),
            };
            let _ = ctx
                .tasks()
                .transition(&job_id, generation, TaskState::Running);
            let _ = record_execution(&ctx, &job_id);
            let result = download(
                &ctx,
                &job_id,
                generation,
                &version,
                &catalog,
                &metadata,
                &downloads,
                cached_recipe,
            );
            match result {
                Ok(id) => {
                    let _ = ctx.tasks().succeed(
                        &job_id,
                        generation,
                        Some(json!({"environmentId":id,"cached":true,"instanceCreated":false})),
                    );
                }
                Err(error) if error == "cancelled" => {
                    let _ = ctx.tasks().cancel_now(&job_id, generation);
                }
                Err(error) => {
                    let _ = ctx.tasks().fail(
                        &job_id,
                        generation,
                        ErrorEnvelope::internal_unavailable()
                            .with_detail_ref(format!("environment:{error}")),
                    );
                }
            }
            if let Err(error) = record_execution(&ctx, &job_id) {
                eprintln!("SHTMCL environment history: {error}");
            }
        });
    if spawn.is_err() {
        let _ =
            on_spawn_failure
                .tasks()
                .fail(&id, generation, ErrorEnvelope::internal_unavailable());
        let _ = record_execution(&on_spawn_failure, &id);
        return Err(ErrorEnvelope::internal_unavailable());
    }
    Ok(CommandResponse::Accepted {
        operation_id: operation,
        task_id: id,
    })
}
fn management_error(error: String) -> ErrorEnvelope {
    let code = match error.as_str() {
        "environment_in_use" | "environment_busy" => "launch.install.busy",
        "environment_pinned" => "launch.environment.pinned",
        _ => "launch.environment.unavailable",
    };
    ErrorEnvelope::new(
        shtmcl_domain::ErrorCode::from_static(code),
        shtmcl_domain::ErrorCategory::Conflict,
        false,
    )
    .with_detail_ref(format!("environment:{error}"))
}

fn ensure(
    ctx: &ModuleContext,
    task: &TaskId,
    generation: AttemptGeneration,
    store: &FileStore,
    downloads: &Arc<dyn CoreModule>,
    files: &[InstallArtifact],
    batch: &mut usize,
) -> Result<(), String> {
    let mut unique = BTreeMap::new();
    for file in files {
        if let Some(previous) = unique.insert(file.sha1.clone(), file.clone()) {
            if previous.size != file.size {
                return Err("environment_digest_size".into());
            }
        }
    }
    let total = unique.values().map(|file| file.size).sum();
    let mut complete = 0;
    let mut missing = Vec::new();
    for file in unique.values() {
        if ctx.tasks().is_cancel_requested(task, generation) {
            return Err("cancelled".into());
        }
        let hash = ArtifactHash::new("sha1", &file.sha1).map_err(|_| "environment_digest")?;
        match store.verify_cached(&hash) {
            Ok(Some(size)) if size == file.size => complete += size,
            Ok(_) => missing.push(file.clone()),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                missing.push(file.clone())
            }
            Err(_) => return Err("environment_cache_read".into()),
        }
    }
    let _ = ctx.tasks().set_progress(
        task,
        generation,
        shtmcl_core::tasks::bytes_progress(complete, Some(total))
            .map_err(|_| "environment_progress")?,
    );
    for group in missing.chunks(1536) {
        match fetch_group(ctx, task, generation, downloads, group, batch, false) {
            Ok(()) => {}
            Err(error) if error.network => {
                fetch_group(ctx, task, generation, downloads, group, batch, true)
                    .map_err(|error| error.reference)?
            }
            Err(error) => return Err(error.reference),
        }
        for file in group {
            if ctx.tasks().is_cancel_requested(task, generation) {
                return Err("cancelled".into());
            }
            let hash = ArtifactHash::new("sha1", &file.sha1).map_err(|_| "environment_digest")?;
            if store
                .verify_cached(&hash)
                .map_err(|_| "environment_cache_verify")?
                != Some(file.size)
            {
                return Err("environment_cache_verify".into());
            }
            complete += file.size;
            let _ = ctx.tasks().set_progress(
                task,
                generation,
                shtmcl_core::tasks::bytes_progress(complete, Some(total))
                    .map_err(|_| "environment_progress")?,
            );
        }
    }
    Ok(())
}
fn download(
    ctx: &ModuleContext,
    task: &TaskId,
    generation: AttemptGeneration,
    version: &str,
    catalog: &VersionCatalog,
    metadata: &CachedVersionMetadataSource,
    downloads: &Arc<dyn CoreModule>,
    cached: Option<Recipe>,
) -> Result<String, String> {
    if ctx.tasks().is_cancel_requested(task, generation) {
        return Err("cancelled".into());
    }
    let _ = ctx
        .tasks()
        .set_phase(task, generation, TaskPhase::new("metadata").unwrap());
    if cached.is_none() && catalog.list(None, None, 0, 1).is_err() {
        catalog.refresh().map_err(|_| "catalog_refresh")?;
    }
    if ctx.tasks().is_cancel_requested(task, generation) {
        return Err("cancelled".into());
    }
    let pinned_metadata = match cached {
        Some(recipe) => VersionMetadata::from_value(recipe.metadata, Some(version))
            .map_err(|_| "environment_metadata")?,
        None => {
            VersionMetadata::resolve(metadata, version)
                .map_err(crate::game_install::metadata_failure)?
                .metadata
        }
    };
    let plan = VanillaInstallPlan::parse(&pinned_metadata, "windows", "amd64")?;
    let lock_id = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&json!([version, &pinned_metadata.raw]))
                .map_err(|_| "environment_json")?
        )
    );
    let recipe_root = checked_root(ctx.dirs())?;
    let lock_root = recipe_root.join(".locks");
    checked_directory(&lock_root)?;
    let path = lock_root.join(format!("{lock_id}.download-lock"));
    reject_link(&path)?;
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|_| "environment_lock")?;
    lock.try_lock().map_err(|_| "environment_busy")?;
    let cache_root = ctx.dirs().cache.join("downloads");
    checked_directory(&cache_root)?;
    checked_directory(&cache_root.join("partial"))?;
    checked_directory(&cache_root.join("objects"))?;
    let store = FileStore::new(cache_root).map_err(|_| "environment_cache_open")?;
    let mut batch = 0;
    ensure(
        ctx,
        task,
        generation,
        &store,
        downloads,
        &plan.artifacts,
        &mut batch,
    )?;
    let mut artifacts = plan.artifacts;
    if let Some(index_path) = &plan.asset_index_path {
        let index = artifacts
            .iter()
            .find(|file| &file.path == index_path)
            .ok_or("environment_asset_index")?;
        let work = recipe_root.join(format!(
            ".index-{:x}",
            Sha256::digest(task.as_str().as_bytes())
        ));
        checked_directory(&work)?;
        let _cleanup = IndexWork(work.clone());
        let hash = ArtifactHash::new("sha1", &index.sha1).map_err(|_| "environment_digest")?;
        crate::runtime_export::export_cached_artifact(
            &store,
            &hash,
            &format!("environment-index/{task}"),
            &work,
            Path::new("index.json"),
            index.size,
            &AtomicBool::new(ctx.tasks().is_cancel_requested(task, generation)),
        )
        .map_err(|_| "environment_index_export")?;
        let file = File::open(work.join("index.json")).map_err(|_| "environment_index_read")?;
        let mut bytes = Vec::new();
        file.take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "environment_index_read")?;
        let assets = asset_artifacts(&bytes)?;
        let _ = ctx
            .tasks()
            .set_phase(task, generation, TaskPhase::new("assets").unwrap());
        ensure(
            ctx, task, generation, &store, downloads, &assets, &mut batch,
        )?;
        artifacts.extend(assets);
    }
    if ctx.tasks().is_cancel_requested(task, generation) {
        return Err("cancelled".into());
    }
    let mut resources = BTreeMap::new();
    for file in artifacts {
        if resources
            .get(&file.sha1)
            .is_some_and(|resource: &Resource| resource.size != file.size)
        {
            return Err("environment_digest_size".into());
        }
        resources.insert(
            file.sha1.clone(),
            Resource {
                sha1: file.sha1,
                size: file.size,
            },
        );
    }
    let resources: Vec<_> = resources.into_values().collect();
    let id = identity(version, &pinned_metadata.raw, &resources)?;
    publish(
        ctx.dirs(),
        &Recipe {
            schema_version: 1,
            id: id.clone(),
            version_id: version.into(),
            metadata: pinned_metadata.raw,
            resources,
            pinned: false,
        },
    )?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shtmcl_core::{
        launch::catalog::{CatalogCache, ManifestFetcher},
        CoreApp,
    };
    use shtmcl_domain::ModuleId;
    struct Temp(PlatformDirs);
    impl Temp {
        fn new() -> Self {
            Self(PlatformDirs::under(std::env::temp_dir().join(format!(
                "shtmcl-environments-{}",
                shtmcl_platform::entropy::random_hex(12).unwrap()
            ))))
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.root);
        }
    }
    struct Fixture;
    fn metadata() -> Value {
        json!({"id":"cache-test","mainClass":"net.minecraft.client.main.Main","minecraftArguments":"--username ${auth_player_name}","libraries":[],"downloads":{"client":{"url":"https://piston-data.mojang.com/client.jar","sha1":format!("{:x}",sha1::Sha1::digest(b"good")),"size":4}}})
    }
    impl ManifestFetcher for Fixture {
        fn fetch(&self, url: &str, _: usize) -> Result<Vec<u8>, String> {
            let bytes = serde_json::to_vec(&metadata()).unwrap();
            if url.ends_with("manifest_v2.json") {
                Ok(serde_json::to_vec(&json!({"latest":{"release":"cache-test","snapshot":"cache-test"},"versions":[{"id":"cache-test","type":"release","url":"https://piston-meta.mojang.com/cache-test.json","sha1":format!("{:x}",sha1::Sha1::digest(&bytes)),"time":"2026-10-22T00:00:00Z","releaseTime":"2026-10-22T00:00:00Z"}]})).unwrap())
            } else {
                Ok(bytes)
            }
        }
    }
    struct NoDownloads;
    impl CoreModule for NoDownloads {
        fn id(&self) -> ModuleId {
            ModuleId::Downloads
        }
        fn handle(&self, _: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
            panic!("verified objects must not be downloaded again")
        }
    }
    fn setup(
        temp: &Temp,
    ) -> (
        ModuleContext,
        Arc<VersionCatalog>,
        Arc<CachedVersionMetadataSource>,
    ) {
        let app = CoreApp::new(temp.0.clone());
        let ctx = app.context();
        let dir = temp.0.data.join("catalog");
        let fetcher = Arc::new(Fixture);
        (
            ctx,
            Arc::new(VersionCatalog::new(&dir, fetcher.clone())),
            Arc::new(CachedVersionMetadataSource::new(
                CatalogCache::new(dir),
                fetcher,
            )),
        )
    }
    #[test]
    fn cached_download_publishes_recipe_without_creating_instance_and_binds_resource_identity() {
        let temp = Temp::new();
        let (ctx, catalog, source) = setup(&temp);
        let store = FileStore::new(temp.0.cache.join("downloads")).unwrap();
        store
            .open_partial("fixture", 0)
            .unwrap()
            .write_all(b"good")
            .unwrap();
        let hash =
            ArtifactHash::new("sha1", &format!("{:x}", sha1::Sha1::digest(b"good"))).unwrap();
        store.publish(&hash, "fixture").unwrap();
        let task = ctx
            .tasks()
            .accept(TaskSpec::new(
                shtmcl_domain::OperationId::new("cache-fixture").unwrap(),
                TaskPhase::new("cache").unwrap(),
            ))
            .unwrap();
        let downloads: Arc<dyn CoreModule> = Arc::new(NoDownloads);
        let id = download(
            &ctx,
            &task.id,
            task.attempt_generation,
            "cache-test",
            &catalog,
            &source,
            &downloads,
            None,
        )
        .unwrap();
        assert!(!temp.0.instances.exists());
        let (rows, _) = catalogue(&temp.0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["bytes"], 4);
        assert_eq!(rows[0]["resourceCount"], 1);
        pin(&temp.0, &id, true).unwrap();
        assert!(load(&temp.0, &id).unwrap().pinned);
        assert_eq!(remove(&temp.0, &id).unwrap_err(), "environment_pinned");
        let (saved, lease) = acquire(&temp.0, &id).unwrap();
        pin(&temp.0, &id, false).unwrap();
        assert_eq!(remove(&temp.0, &id).unwrap_err(), "environment_in_use");
        drop(lease);
        // A pinned-metadata repair must not ask the version catalog for new metadata.
        let repair_task = ctx
            .tasks()
            .accept(TaskSpec::new(
                shtmcl_domain::OperationId::new("repair-fixture").unwrap(),
                TaskPhase::new("cache").unwrap(),
            ))
            .unwrap();
        let offline_catalog =
            VersionCatalog::new(temp.0.data.join("unused-catalog"), Arc::new(Fixture));
        let repaired = download(
            &ctx,
            &repair_task.id,
            repair_task.attempt_generation,
            "cache-test",
            &offline_catalog,
            &source,
            &downloads,
            Some(saved),
        )
        .unwrap();
        assert_eq!(repaired, id);
        pin(&temp.0, &id, true).unwrap();
        let mut republished = load(&temp.0, &id).unwrap();
        republished.pinned = false;
        publish(&temp.0, &republished).unwrap();
        assert!(
            load(&temp.0, &id).unwrap().pinned,
            "re-download never resets user's retention choice"
        );
        pin(&temp.0, &id, false).unwrap();
        let lock_directory = root(&temp.0).join(".locks");
        for index in 0..400 {
            std::fs::write(lock_directory.join(format!("{index}.use-lock")), b"").unwrap();
        }
        assert_eq!(
            catalogue(&temp.0).unwrap().0.len(),
            1,
            "retained lock inodes do not exhaust recipe listing budget"
        );
        publish(&temp.0, &republished).unwrap();
        assert!(!temp.0.data.join("unused-catalog").exists());
        let corrupt = root(&temp.0).join(format!("{}.json", "a".repeat(64)));
        std::fs::write(&corrupt, b"truncated").unwrap();
        let (healthy, skipped) = catalogue(&temp.0).unwrap();
        assert_eq!(healthy.len(), 1);
        assert_eq!(skipped, 1);
        assert_eq!(
            load_metadata(&temp.0, &id, "cache-test").unwrap(),
            metadata()
        );
        assert!(load_metadata(&temp.0, &id, "different").is_err());
        let snapshot = std::fs::read(root(&temp.0).join(format!("{id}.json"))).unwrap();
        remove(&temp.0, &id).unwrap();
        assert!(load(&temp.0, &id).is_err());
        assert_eq!(
            store.verify_cached(&hash).unwrap(),
            Some(4),
            "removal never removes shared objects"
        );
        assert!(!temp.0.instances.exists());
        std::fs::write(root(&temp.0).join(format!("{id}.json")), snapshot).unwrap();
        let path = root(&temp.0).join(format!("{id}.json"));
        let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["resources"] = json!([]);
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            load(&temp.0, &id).is_err(),
            "resource list is part of content identity"
        );
        assert!(
            catalogue(&temp.0).is_err(),
            "all unreadable is an error, not empty library"
        );
    }
    #[test]
    fn precancelled_cache_task_does_not_even_prepare_metadata() {
        let temp = Temp::new();
        let (ctx, catalog, source) = setup(&temp);
        let task = ctx
            .tasks()
            .accept(TaskSpec::new(
                shtmcl_domain::OperationId::new("cancel-fixture").unwrap(),
                TaskPhase::new("cache").unwrap(),
            ))
            .unwrap();
        ctx.tasks()
            .request_cancel(&shtmcl_domain::CancelTaskRequest {
                task_id: task.id.clone(),
                attempt_generation: task.attempt_generation,
            });
        let downloads: Arc<dyn CoreModule> = Arc::new(NoDownloads);
        assert_eq!(
            download(
                &ctx,
                &task.id,
                task.attempt_generation,
                "cache-test",
                &catalog,
                &source,
                &downloads,
                None,
            )
            .unwrap_err(),
            "cancelled"
        );
        assert!(!temp.0.data.exists() && !temp.0.instances.exists() && !temp.0.cache.exists());
    }
}

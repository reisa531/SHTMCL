//! Copies managed instances through the existing instance index and task service.
//! The source is leased; only game files are copied, and publication precedes
//! index registration. A copied receipt belongs to the new identity.
use crate::game_install::{checked_directory, reject_link};
use serde_json::{json, Value};
use shtmcl_core::{
    instances::{error::InstancesError, Instance, InstanceState},
    tasks::TaskSpec,
    CoreModule, ModuleContext,
};
use shtmcl_domain::{
    CommandRequest, CommandResponse, ErrorEnvelope, InstanceId, ModuleId, OperationId, RequestId,
    TaskPhase, TaskState,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) struct InstanceCopies {
    inner: Arc<dyn CoreModule>,
    ctx: ModuleContext,
}

#[derive(Clone)]
struct CopyRequest {
    source: String,
    target: String,
    name: String,
    saves: bool,
}

impl InstanceCopies {
    pub(crate) fn new(inner: Arc<dyn CoreModule>, ctx: ModuleContext) -> Self {
        Self { inner, ctx }
    }

    fn start(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let input = CopyRequest::parse(&request.payload)?;
        let operation = request
            .operation_id
            .clone()
            .ok_or_else(ErrorEnvelope::command_operation_required)?;
        let source = snapshot(self.inner.as_ref(), &input.source)?;
        if source.state != InstanceState::Ready {
            return Err(InstancesError::Locked(input.source).to_envelope());
        }
        let holder = format!(
            "copy-{}",
            shtmcl_platform::entropy::random_hex(16)
                .map_err(|_| ErrorEnvelope::internal_unavailable())?
        );
        let permit = SourcePermit::acquire(
            self.inner.clone(),
            input.source.clone(),
            holder,
            operation.clone(),
        )?;
        let task = self.ctx.tasks().accept(
            TaskSpec::new(operation.clone(), TaskPhase::new("instances.copy").unwrap())
                .with_instance(InstanceId::new(input.target.clone()).ok()),
        )?;
        let ctx = self.ctx.clone();
        let inner = self.inner.clone();
        let task_id = task.id.clone();
        let generation = task.attempt_generation;
        let work = move || {
            let mut permit = permit;
            if ctx
                .tasks()
                .transition(&task_id, generation, TaskState::Running)
                .is_err()
            {
                return;
            }
            let cancelled = || ctx.tasks().is_cancel_requested(&task_id, generation);
            let result = copy_instance(
                &ctx,
                inner.as_ref(),
                &input,
                &operation,
                &mut permit,
                &cancelled,
            );
            // A terminal task also means the source is available again.
            drop(permit);
            match result {
                Ok(path) => {
                    let _ = ctx.tasks().succeed(&task_id, generation, Some(json!({"instanceId":input.target,"name":input.name,"path":path,"includeSaves":input.saves})));
                }
                Err(CopyFailure::Cancelled) => {
                    let _ = ctx.tasks().cancel_now(&task_id, generation);
                }
                Err(CopyFailure::Core(error)) => {
                    let _ = ctx.tasks().fail(&task_id, generation, error);
                }
                Err(CopyFailure::File(reason)) => {
                    let _ = ctx.tasks().fail(&task_id, generation, copy_error(reason));
                }
            }
        };
        std::thread::Builder::new()
            .name("instance-copy".into())
            .spawn(work)
            .map_err(|_| {
                let error = ErrorEnvelope::internal_unavailable();
                let _ = self.ctx.tasks().fail(&task.id, generation, error.clone());
                error
            })?;
        Ok(CommandResponse::Accepted {
            operation_id: request.operation_id.clone().unwrap(),
            task_id: task.id,
        })
    }
}

impl CoreModule for InstanceCopies {
    fn id(&self) -> ModuleId {
        self.inner.id()
    }
    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        if request.payload["command"] == "instances.copy" {
            self.start(request)
        } else {
            self.inner.handle(request)
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        && !matches!(
            id.to_ascii_uppercase().as_str(),
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
}

impl CopyRequest {
    fn parse(value: &Value) -> Result<Self, ErrorEnvelope> {
        let invalid = || InstancesError::InvalidRequest("copy").to_envelope();
        let source = value["sourceInstanceId"].as_str().ok_or_else(invalid)?;
        let target = value["newInstanceId"].as_str().ok_or_else(invalid)?;
        let name = value["name"].as_str().ok_or_else(invalid)?.trim();
        let saves = value["includeSaves"].as_bool().ok_or_else(invalid)?;
        if !valid_id(source)
            || !valid_id(target)
            || source.eq_ignore_ascii_case(target)
            || name.is_empty()
            || name.chars().count() > 128
            || name.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        Ok(Self {
            source: source.into(),
            target: target.into(),
            name: name.into(),
            saves,
        })
    }
}

fn command(
    module: &dyn CoreModule,
    operation: Option<&OperationId>,
    payload: Value,
) -> Result<Value, ErrorEnvelope> {
    match module.handle(&CommandRequest {
        request_id: RequestId::new(format!(
            "copy-{}",
            shtmcl_platform::entropy::random_hex(12)
                .map_err(|_| ErrorEnvelope::internal_unavailable())?
        ))
        .unwrap(),
        operation_id: operation.cloned(),
        entity_id: None,
        expected_revision: None,
        payload,
    })? {
        CommandResponse::Completed { result } => Ok(result),
        _ => Err(ErrorEnvelope::internal_unavailable()),
    }
}

fn snapshot(module: &dyn CoreModule, id: &str) -> Result<Instance, ErrorEnvelope> {
    serde_json::from_value(
        command(
            module,
            None,
            json!({"command":"instances.snapshot","instanceId":id}),
        )?["instance"]
            .clone(),
    )
    .map_err(|_| ErrorEnvelope::internal_unavailable())
}

struct SourcePermit {
    module: Arc<dyn CoreModule>,
    source: String,
    holder: String,
    operation: OperationId,
    refreshed: Instant,
}
impl SourcePermit {
    fn acquire(
        module: Arc<dyn CoreModule>,
        source: String,
        holder: String,
        operation: OperationId,
    ) -> Result<Self, ErrorEnvelope> {
        let mut permit = Self {
            module,
            source,
            holder,
            operation,
            refreshed: Instant::now(),
        };
        permit.renew()?;
        Ok(permit)
    }
    fn renew(&mut self) -> Result<(), ErrorEnvelope> {
        command(
            self.module.as_ref(),
            Some(&self.operation),
            json!({"command":"instances.lease.acquire","instanceId":self.source,"holder":self.holder,"ttlMs":60_000}),
        )?;
        self.refreshed = Instant::now();
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ErrorEnvelope> {
        if self.refreshed.elapsed() >= Duration::from_secs(10) {
            self.renew()?;
        }
        Ok(())
    }
}
impl Drop for SourcePermit {
    fn drop(&mut self) {
        let _ = command(
            self.module.as_ref(),
            Some(&self.operation),
            json!({"command":"instances.lease.release","instanceId":self.source,"holder":self.holder}),
        );
    }
}

enum CopyFailure {
    Cancelled,
    Core(ErrorEnvelope),
    File(&'static str),
}
impl From<ErrorEnvelope> for CopyFailure {
    fn from(e: ErrorEnvelope) -> Self {
        Self::Core(e)
    }
}
fn copy_error(reason: &'static str) -> ErrorEnvelope {
    ErrorEnvelope::new(
        shtmcl_domain::ErrorCode::from_static("instances.copy.failed"),
        shtmcl_domain::ErrorCategory::Storage,
        false,
    )
    .with_detail_ref(format!("instance-copy:{reason}"))
}
fn file<T>(result: std::io::Result<T>, reason: &'static str) -> Result<T, CopyFailure> {
    result.map_err(|_| CopyFailure::File(reason))
}
fn no_link(path: &Path) -> Result<(), CopyFailure> {
    reject_link(path).map_err(|_| CopyFailure::File("linked_path"))
}
fn existing(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// A unique staging directory is the only tree this guard may remove.
struct Stage {
    path: PathBuf,
    owned: bool,
}
impl Drop for Stage {
    fn drop(&mut self) {
        if self.owned && reject_link(&self.path).is_ok() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn copy_instance(
    ctx: &ModuleContext,
    module: &dyn CoreModule,
    input: &CopyRequest,
    operation: &OperationId,
    permit: &mut SourcePermit,
    cancelled: &dyn Fn() -> bool,
) -> Result<PathBuf, CopyFailure> {
    checked_directory(&ctx.dirs().instances)
        .map_err(|_| CopyFailure::File("instances_directory"))?;
    let root = &ctx.dirs().instances;
    let source_path = root.join(&input.source);
    no_link(&source_path)?;
    let source = snapshot(module, &input.source)?;
    if Path::new(&source.location.root) != source_path || source.state == InstanceState::Broken {
        return Err(CopyFailure::File("source_location"));
    }
    // Share the install destination lock; installation and copy cannot publish the same ID.
    let lock_path = root.join(format!(".install-{}.lock", input.target));
    no_link(&lock_path)?;
    let lock = file(
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path),
        "destination_lock",
    )?;
    lock.try_lock().map_err(|_| {
        CopyFailure::Core(InstancesError::Locked(input.target.clone()).to_envelope())
    })?;
    let target = root.join(&input.target);
    if existing(&target) {
        return Err(InstancesError::Duplicate(input.target.clone())
            .to_envelope()
            .into());
    }
    match snapshot(module, &input.target) {
        Ok(_) => {
            return Err(InstancesError::Duplicate(input.target.clone())
                .to_envelope()
                .into())
        }
        Err(error) if error.code.as_str() == "instances.instance.not_found" => {}
        Err(error) => return Err(error.into()),
    }
    let receipt_path = source_path.join("shtmcl-install.json");
    no_link(&receipt_path)?;
    let bytes = file(File::open(&receipt_path), "source_receipt")?;
    let mut receipt_bytes = Vec::new();
    file(
        bytes.take(4097).read_to_end(&mut receipt_bytes),
        "source_receipt",
    )?;
    let mut receipt: Value =
        serde_json::from_slice(&receipt_bytes).map_err(|_| CopyFailure::File("source_receipt"))?;
    if receipt_bytes.len() > 4096
        || receipt["schemaVersion"] != 1
        || receipt["complete"] != true
        || receipt["instanceId"] != input.source
        || receipt["versionId"] != source.version.game_version
    {
        return Err(CopyFailure::File("source_receipt_identity"));
    }
    let path = root.join(format!(
        ".copy-{}-{}",
        input.target,
        shtmcl_platform::entropy::random_hex(12).map_err(|_| CopyFailure::File("entropy"))?
    ));
    file(fs::create_dir(&path), "staging_create")?;
    let mut stage = Stage { path, owned: true };
    let mut checkpoint = || {
        if cancelled() {
            return Err(CopyFailure::Cancelled);
        }
        permit.checkpoint().map_err(CopyFailure::Core)
    };
    let mut budget = CopyBudget {
        entries: 0,
        bytes: 0,
    };
    for directory in [
        "versions",
        "libraries",
        "assets",
        "resources",
        "natives",
        "mods",
        "config",
        "defaultconfigs",
        "resourcepacks",
        "shaderpacks",
        "datapacks",
    ] {
        let from = source_path.join(directory);
        if existing(&from) {
            copy_tree(
                &from,
                &stage.path.join(directory),
                0,
                &mut budget,
                &mut checkpoint,
            )?;
        }
    }
    for name in [
        "options.txt",
        "optionsof.txt",
        "optionsshaders.txt",
        "servers.dat",
        "icon.png",
        "shtmcl-modpack.json",
        "modrinth.index.json",
        "manifest.json",
    ] {
        let from = source_path.join(name);
        if existing(&from) {
            copy_tree(
                &from,
                &stage.path.join(name),
                0,
                &mut budget,
                &mut checkpoint,
            )?;
        }
    }
    if input.saves && existing(&source_path.join("saves")) {
        copy_tree(
            &source_path.join("saves"),
            &stage.path.join("saves"),
            0,
            &mut budget,
            &mut checkpoint,
        )?;
    }
    checkpoint()?;
    receipt["instanceId"] = json!(input.target);
    let encoded = serde_json::to_vec(&receipt).map_err(|_| CopyFailure::File("receipt_encode"))?;
    file(
        fs::write(stage.path.join("shtmcl-install.json"), encoded),
        "receipt_write",
    )?;
    // Cancellation ends at publication: an indexed, complete instance wins over a late cancel.
    checkpoint()?;
    file(fs::rename(&stage.path, &target), "publish")?;
    let result = command(
        module,
        Some(operation),
        json!({"command":"instances.create","instance":{"id":input.target,"name":input.name,"version":source.version,"location":{"root":target},"javaMajor":source.java_major}}),
    );
    if let Err(error) = result {
        // The index may have committed before a later journal write failed.
        // Keep a matching committed instance instead of moving its files away.
        if snapshot(module, &input.target).is_ok_and(|registered| {
            registered.name == input.name
                && Path::new(&registered.location.root) == target
                && registered.version == source.version
        }) {
            stage.owned = false;
            return Ok(target);
        }
        if fs::rename(&target, &stage.path).is_err() {
            stage.owned = false;
            return Err(CopyFailure::File("registration_rollback_pending"));
        }
        return Err(error.into());
    }
    stage.owned = false;
    Ok(target)
}

struct CopyBudget {
    entries: usize,
    bytes: u64,
}
fn copy_tree(
    from: &Path,
    to: &Path,
    depth: usize,
    budget: &mut CopyBudget,
    checkpoint: &mut dyn FnMut() -> Result<(), CopyFailure>,
) -> Result<(), CopyFailure> {
    checkpoint()?;
    no_link(from)?;
    let metadata = file(fs::symlink_metadata(from), "source_metadata")?;
    budget.entries += 1;
    if depth > 64 || budget.entries > 250_000 {
        return Err(CopyFailure::File("copy_entry_budget"));
    }
    if metadata.is_dir() {
        file(fs::create_dir(to), "directory_create")?;
        for entry in file(fs::read_dir(from), "directory_read")? {
            let entry = file(entry, "directory_entry")?;
            copy_tree(
                &entry.path(),
                &to.join(entry.file_name()),
                depth + 1,
                budget,
                checkpoint,
            )?;
        }
    } else if metadata.is_file() {
        let mut input = file(File::open(from), "file_open")?;
        let mut output = file(
            OpenOptions::new().write(true).create_new(true).open(to),
            "file_create",
        )?;
        let mut buffer = [0u8; 128 * 1024];
        loop {
            checkpoint()?;
            let read = file(input.read(&mut buffer), "file_read")?;
            if read == 0 {
                break;
            }
            budget.bytes = budget
                .bytes
                .checked_add(read as u64)
                .ok_or(CopyFailure::File("copy_byte_budget"))?;
            if budget.bytes > 512 * 1024 * 1024 * 1024 {
                return Err(CopyFailure::File("copy_byte_budget"));
            }
            file(output.write_all(&buffer[..read]), "file_write")?;
        }
        file(output.sync_all(), "file_sync")?;
    } else {
        return Err(CopyFailure::File("unsupported_file"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;

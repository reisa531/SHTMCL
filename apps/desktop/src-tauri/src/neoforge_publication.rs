//! NeoForge 1.20.4 / 20.4.253 bounded publication, owned receipt, and recovery.
//!
//! Model: a caller holds the per-instance `.install-{id}.lock` for its whole
//! prepare/publish/recover sequence. Prepared files live either in an owned
//! staging directory `.install-neo-{id}` or in a fresh `.neoforge-*` attempt
//! directory. Publication never recursively deletes user data: collisions and
//! partial states are refused and left for recovery/inspection.

use serde::{Deserialize, Serialize};
use shtmcl_core::{CoreModule, ModuleContext};
use shtmcl_domain::{CommandRequest, CommandResponse};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) const VERSION_ID: &str = "1.20.4";
pub(crate) const LOADER: &str = "neoforge";
pub(crate) const LOADER_VERSION: &str = "20.4.253";
pub(crate) const GAME_VERSION: &str = "1.20.4";
pub(crate) const JAVA_MAJOR: u32 = 17;
pub(crate) const RECEIPT_NAME: &str = "shtmcl-install.json";
pub(crate) const RECEIPT_SCHEMA: u32 = 1;
const MAX_RECEIPT_BYTES: u64 = 4096;

// ---------------------------------------------------------------------------
// Receipt
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Receipt {
    pub schema_version: u32,
    pub version_id: String,
    pub instance_id: String,
    pub loader: String,
    pub loader_version: String,
    pub prepared_sha256: String,
    pub complete: bool,
}

impl Receipt {
    fn new(instance_id: &str, prepared_sha256: &str) -> Self {
        Self {
            schema_version: RECEIPT_SCHEMA,
            version_id: VERSION_ID.to_string(),
            instance_id: instance_id.to_string(),
            loader: LOADER.to_string(),
            loader_version: LOADER_VERSION.to_string(),
            prepared_sha256: prepared_sha256.to_string(),
            complete: true,
        }
    }

    fn matches(&self, instance_id: &str, prepared_sha256: &str) -> bool {
        self.schema_version == RECEIPT_SCHEMA
            && self.complete
            && self.version_id == VERSION_ID
            && self.instance_id == instance_id
            && self.loader == LOADER
            && self.loader_version == LOADER_VERSION
            && self.prepared_sha256.eq_ignore_ascii_case(prepared_sha256)
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 64 {
        return Err("instance_id_invalid_length".into());
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("instance_id_invalid_character".into());
    }
    Ok(())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn reject_link(path: &Path) -> Result<(), String> {
    crate::game_install::reject_link(path)
}

fn lock_path(ctx: &ModuleContext, id: &str) -> PathBuf {
    ctx.dirs().instances.join(format!(".install-{id}.lock"))
}

fn stage_path(ctx: &ModuleContext, id: &str) -> PathBuf {
    ctx.dirs().instances.join(format!(".install-neo-{id}"))
}

fn target_path(ctx: &ModuleContext, id: &str) -> PathBuf {
    ctx.dirs().instances.join(id)
}

// ---------------------------------------------------------------------------
// Lock
// ---------------------------------------------------------------------------

/// Acquire the per-instance install lock. The returned `File` must be held for
/// the full recover/publish sequence. Never truncates; never reports success on
/// a symlinked lock path.
pub(crate) fn lock(ctx: &ModuleContext, id: &str) -> Result<File, String> {
    validate_id(id)?;
    let instances = &ctx.dirs().instances;
    crate::game_install::checked_directory(instances)?;
    if let Ok(meta) = std::fs::metadata(instances) {
        if !meta.is_dir() {
            return Err("instances_not_directory".into());
        }
    } else {
        return Err("instances_missing".into());
    }
    let path = lock_path(ctx, id);
    reject_link(&path)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|err| format!("lock_open:{}", err.kind()))?;
    file.try_lock().map_err(|_| "lock_busy".to_string())?;
    // Re-check after locking in case a racer replaced the file.
    reject_link(&path)?;
    Ok(file)
}

// ---------------------------------------------------------------------------
// Instances lookup
// ---------------------------------------------------------------------------

fn instance_registered(instances: &Arc<dyn CoreModule>, id: &str) -> Result<bool, String> {
    let request = CommandRequest {
        request_id: shtmcl_domain::RequestId::new("neoforge-publish-probe")
            .map_err(|_| "request_id".to_string())?,
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload: serde_json::json!({
            "command": "instances.snapshot",
            "instanceId": id,
        }),
    };
    match instances.handle(&request) {
        Ok(CommandResponse::Completed { .. }) => Ok(true),
        Ok(CommandResponse::Accepted { .. }) => {
            Err("instances_snapshot_unexpected_accepted".into())
        }
        Err(err) => {
            if err.code.as_str() == "instances.instance.not_found" {
                Ok(false)
            } else {
                Err(format!("instances_snapshot:{}", err.code.as_str()))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Receipt IO
// ---------------------------------------------------------------------------

fn read_receipt(dir: &Path) -> Result<Option<Receipt>, String> {
    let path = dir.join(RECEIPT_NAME);
    reject_link(&path)?;
    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("receipt_stat:{}", err.kind())),
    };
    if !meta.is_file() {
        return Err("receipt_not_file".into());
    }
    if meta.len() > MAX_RECEIPT_BYTES {
        return Err("receipt_too_large".into());
    }
    let bytes = std::fs::read(&path).map_err(|err| format!("receipt_read:{}", err.kind()))?;
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err("receipt_too_large".into());
    }
    let receipt: Receipt =
        serde_json::from_slice(&bytes).map_err(|_| "receipt_invalid".to_string())?;
    Ok(Some(receipt))
}

/// Write a durable receipt into `dir` via create_new tmp + sync_all + rename.
/// Refuses to overwrite a non-identical existing receipt.
fn write_receipt(dir: &Path, receipt: &Receipt) -> Result<(), String> {
    reject_link(dir)?;
    if let Some(existing) = read_receipt(dir)? {
        if &existing == receipt {
            return Ok(());
        }
        return Err("receipt_conflict".into());
    }
    let bytes = serde_json::to_vec(receipt).map_err(|_| "receipt_encode".to_string())?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        shtmcl_platform::random_hex(8).map_err(|_| "receipt_tmp_id".to_string())?
    ));
    reject_link(&tmp)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .map_err(|err| format!("receipt_tmp_open:{}", err.kind()))?;
    let write_result = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    drop(file);
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("receipt_tmp_write:{}", err.kind()));
    }
    let final_path = dir.join(RECEIPT_NAME);
    reject_link(&final_path)?;
    if std::fs::symlink_metadata(&final_path).is_ok() {
        let _ = std::fs::remove_file(&tmp);
        return Err("receipt_conflict".into());
    }
    std::fs::rename(&tmp, &final_path).map_err(|err| format!("receipt_rename:{}", err.kind()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

/// Recover a partially-published instance. Caller must hold `lock`. Always
/// queries the instances module first; only `instances.instance.not_found`
/// permits proceeding. Returns the verified staging directory and its prepared
/// digest, or `None` when nothing was staged. The caller must run
/// `neoforge_integrity::verify(stage, digest, cancelled)`.
pub(crate) fn recover(
    ctx: &ModuleContext,
    instances: &Arc<dyn CoreModule>,
    id: &str,
) -> Result<Option<(PathBuf, String)>, String> {
    validate_id(id)?;
    if instance_registered(instances, id)? {
        return Err("instance_registered".into());
    }
    let stage = stage_path(ctx, id);
    let target = target_path(ctx, id);
    reject_link(&stage)?;
    reject_link(&target)?;

    let stage_exists = std::fs::symlink_metadata(&stage).is_ok();
    let target_exists = std::fs::symlink_metadata(&target).is_ok();
    if stage_exists && target_exists {
        return Err("publication_recovery_conflict".into());
    }

    if stage_exists {
        let receipt = read_receipt(&stage)?.ok_or_else(|| "stage_receipt_missing".to_string())?;
        let digest = receipt.prepared_sha256.clone();
        if !is_sha256_hex(&digest) || !receipt.matches(id, &digest) {
            return Err("stage_receipt_invalid".into());
        }
        return Ok(Some((stage, digest)));
    }

    if target_exists {
        // Adopt a registered-but-unstaged target only when it owns a valid
        // receipt for this id/version; refuse to guess.
        let receipt = read_receipt(&target)?.ok_or_else(|| "target_receipt_missing".to_string())?;
        let digest = receipt.prepared_sha256.clone();
        if !is_sha256_hex(&digest) || !receipt.matches(id, &digest) {
            return Err("target_receipt_invalid".into());
        }
        std::fs::rename(&target, &stage)
            .map_err(|err| format!("target_to_stage:{}", err.kind()))?;
        return Ok(Some((stage, digest)));
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Publication
// ---------------------------------------------------------------------------

fn roots_within_instances(ctx: &ModuleContext, prepared_root: &Path) -> Result<(), String> {
    let instances = &ctx.dirs().instances;
    let parent = prepared_root
        .parent()
        .ok_or_else(|| "prepared_root_no_parent".to_string())?;
    let parent_matches = parent == instances.as_path();
    if parent_matches {
        return Ok(());
    }
    // Immediate game child of a fresh `.neoforge-*` attempt: publish renames
    // `prepared_root` -> `.install-neo-{id}` directly, so the parent must be the
    // attempt directory itself.
    if parent.parent() == Some(instances.as_path()) {
        let name = parent
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| "prepared_root_bad_attempt".to_string())?;
        reject_link(parent)?;
        if name.starts_with(".neoforge-") && prepared_root.file_name().is_some_and(|n| n == "game")
        {
            return Ok(());
        }
    }
    Err("prepared_root_out_of_bounds".into())
}

/// Register the published target with the instances module. Operation identity
/// mirrors the installer scheme: SHA256 of `[id, version, loaderVersion, target]`.
fn register_instance(
    _ctx: &ModuleContext,
    instances: &Arc<dyn CoreModule>,
    id: &str,
    name: &str,
    target: &Path,
) -> Result<(), String> {
    let identity = operation_identity(id, VERSION_ID, LOADER_VERSION, target);
    let request = CommandRequest {
        request_id: shtmcl_domain::RequestId::new(format!("neoforge-publish-{id}"))
            .map_err(|_| "request_id".to_string())?,
        operation_id: Some(
            shtmcl_domain::OperationId::new(identity).map_err(|_| "operation_id".to_string())?,
        ),
        entity_id: None,
        expected_revision: None,
        payload: serde_json::json!({
            "command": "instances.create",
            "instance": {"id": id, "name": name,
                "location": {"root": target},
                "version": {"gameVersion": GAME_VERSION, "loader": LOADER, "loaderVersion": LOADER_VERSION},
                "javaMajor": JAVA_MAJOR},
        }),
    };
    match instances.handle(&request) {
        Ok(CommandResponse::Completed { .. }) => Ok(()),
        Ok(CommandResponse::Accepted { .. }) => {
            // `instances.create` is synchronous; an accepted response is an
            // ambiguous outcome. Retain the target so recovery owns it.
            Err("registration_ambiguous_retain_target".into())
        }
        Err(err) => Err(format!("registration:{}", err.code.as_str())),
    }
}

fn operation_identity(id: &str, version: &str, loader_version: &str, target: &Path) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "neo-install-{:x}",
        Sha256::digest(
            serde_json::to_vec(&serde_json::json!([id, version, loader_version, target])).unwrap()
        )
    )
}

/// Publish a verified prepared tree. Caller holds `lock` and has verified every
/// file via `neoforge_integrity::verify(prepared_root, sha, cancelled)`.
pub(crate) fn publish(
    ctx: &ModuleContext,
    instances: &Arc<dyn CoreModule>,
    id: &str,
    name: &str,
    prepared_root: &Path,
    sha: &str,
    cancelled: &dyn Fn() -> bool,
) -> Result<String, String> {
    validate_id(id)?;
    if !is_sha256_hex(sha) {
        return Err("prepared_sha256_invalid".into());
    }
    if instance_registered(instances, id)? {
        return Err("instance_registered".into());
    }
    reject_link(prepared_root)?;
    roots_within_instances(ctx, prepared_root)?;

    let stage = stage_path(ctx, id);
    let target = target_path(ctx, id);
    reject_link(&stage)?;
    reject_link(&target)?;
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err("target_exists".into());
    }

    let receipt = Receipt::new(id, sha);

    // Receipt must be durable before any rename so recovery can adopt it.
    write_receipt(prepared_root, &receipt)?;

    let stage_exists = std::fs::symlink_metadata(&stage).is_ok();
    if prepared_root == stage.as_path() {
        // Recovery path: stage already owns the receipt; re-validate digest.
        let existing = read_receipt(&stage)?.ok_or_else(|| "stage_receipt_missing".to_string())?;
        if !existing.matches(id, sha) {
            return Err("stage_receipt_mismatch".into());
        }
    } else if stage_exists {
        // A previous failed publication left a stage. Adopt it only when its
        // receipt exactly matches; never overwrite.
        return Err("stage_receipt_conflict".into());
    } else {
        std::fs::rename(prepared_root, &stage)
            .map_err(|err| format!("prepared_to_stage:{}", err.kind()))?;
    }

    if cancelled() {
        crate::game_install::cleanup_cancelled_stage(&ctx.dirs().instances, &stage)?;
        return Err("cancelled".into());
    }

    if std::fs::symlink_metadata(&target).is_ok() {
        return Err("target_exists".into());
    }
    std::fs::rename(&stage, &target).map_err(|err| format!("stage_to_target:{}", err.kind()))?;

    match register_instance(ctx, instances, id, name, &target) {
        Ok(()) => Ok(target.to_string_lossy().into_owned()),
        Err(error) if error.starts_with("registration_ambiguous") => {
            // Retain target for recovery; do not roll back.
            Err(error)
        }
        Err(error) => {
            // Explicit rejection: roll the target back to the owned stage.
            std::fs::rename(&target, &stage).map_err(|_| "registration_failed_rollback_pending")?;
            Err(error)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use shtmcl_domain::{ErrorCategory, ErrorCode, ErrorEnvelope};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// Immutable-response mock of the instances module.
    struct MockInstances {
        snapshot_present: bool,
        create_response: Mutex<Option<Result<CommandResponse, ErrorEnvelope>>>,
    }

    impl MockInstances {
        fn new(present: bool, create: Result<(), String>) -> Arc<Self> {
            let response = create
                .map(|_| CommandResponse::Completed {
                    result: serde_json::json!({ "instanceId": "mock" }),
                })
                .map_err(|_| ErrorEnvelope::internal_unavailable());
            Arc::new(Self {
                snapshot_present: present,
                create_response: Mutex::new(Some(response)),
            })
        }
    }

    impl CoreModule for MockInstances {
        fn id(&self) -> shtmcl_domain::ModuleId {
            shtmcl_domain::ModuleId::Launch
        }
        fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
            let command = request.command().map(|c| c.as_str().to_string())?;
            match command.as_str() {
                "instances.snapshot" => {
                    if self.snapshot_present {
                        Ok(CommandResponse::Completed {
                            result: serde_json::json!({ "instances": [{ "instanceId": "x" }] }),
                        })
                    } else {
                        Err(ErrorEnvelope::new(
                            ErrorCode::from_static("instances.instance.not_found"),
                            ErrorCategory::NotFound,
                            false,
                        ))
                    }
                }
                "instances.create" => {
                    let instance = &request.payload["instance"];
                    assert!(instance["id"].as_str().is_some());
                    assert_eq!(instance["version"]["loader"],"neoforge");
                    assert_eq!(instance["version"]["gameVersion"],"1.20.4");
                    assert_eq!(instance["version"]["loaderVersion"],"20.4.253");
                    assert_eq!(instance["javaMajor"],17);
                    assert!(Path::new(instance["location"]["root"].as_str().unwrap()).is_absolute());
                    self.create_response.lock().unwrap().take().expect("create once")
                },
                _ => Err(ErrorEnvelope::internal_unavailable()),
            }
        }
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "shtmcl-neo-pub-{}",
                shtmcl_platform::random_hex(12).unwrap()
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sha() -> String {
        "a".repeat(64)
    }

    fn ctx_at(root: &Path) -> ModuleContext {
        // Assumes a test-only constructor; Codex wires the real one.
        shtmcl_core::CoreApp::new(shtmcl_platform::PlatformDirs::under(root)).context()
    }

    fn fresh_dirs(root: &Path) -> PathBuf {
        let instances = root.join("instances");
        std::fs::create_dir_all(&instances).unwrap();
        instances
    }

    fn make_prepared(instances: &Path, id: &str) -> PathBuf {
        let root = instances.join(format!(".neoforge-test-{id}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("marker.txt"), b"ok").unwrap();
        root
    }

    #[test]
    fn lock_rejects_invalid_id() {
        let root = TempDir::new();
        let _ = fresh_dirs(root.path());
        let ctx = ctx_at(root.path());
        assert!(lock(&ctx, "bad/id").is_err());
        assert!(lock(&ctx, "").is_err());
        let _ = lock(&ctx, "ok_id").unwrap();
    }

    #[test]
    fn recovery_adopts_target_with_receipt() {
        let root = TempDir::new();
        let instances = fresh_dirs(root.path());
        let ctx = ctx_at(root.path());
        let target = instances.join("inst-1");
        std::fs::create_dir_all(&target).unwrap();
        write_receipt(&target, &Receipt::new("inst-1", &sha())).unwrap();
        let mock = MockInstances::new(false, Ok(()));
        let recovered = recover(&ctx, &(mock as Arc<dyn CoreModule>), "inst-1")
            .unwrap()
            .expect("recovered");
        assert_eq!(recovered.1, sha());
        assert!(recovered.0.ends_with(format!(".install-neo-inst-1")));
        assert!(!target.exists());
    }

    #[test]
    fn recovery_refuses_registered_instance() {
        let root = TempDir::new();
        let _ = fresh_dirs(root.path());
        let ctx = ctx_at(root.path());
        let mock = MockInstances::new(true, Ok(()));
        assert!(recover(&ctx, &(mock as Arc<dyn CoreModule>), "inst-2").is_err());
    }

    #[test]
    fn publish_refuses_registered_collision() {
        let root = TempDir::new();
        let instances = fresh_dirs(root.path());
        let ctx = ctx_at(root.path());
        let prepared = make_prepared(&instances, "inst-3");
        let mock = MockInstances::new(true, Ok(()));
        let err = publish(
            &ctx,
            &(mock as Arc<dyn CoreModule>),
            "inst-3",
            "n",
            &prepared,
            &sha(),
            &|| false,
        )
        .unwrap_err();
        assert_eq!(err, "instance_registered");
        assert!(prepared.exists());
    }

    #[test]
    fn publish_refuses_existing_target() {
        let root = TempDir::new();
        let instances = fresh_dirs(root.path());
        let ctx = ctx_at(root.path());
        let prepared = make_prepared(&instances, "inst-4");
        std::fs::create_dir_all(instances.join("inst-4")).unwrap();
        let mock = MockInstances::new(false, Ok(()));
        let err = publish(
            &ctx,
            &(mock as Arc<dyn CoreModule>),
            "inst-4",
            "n",
            &prepared,
            &sha(),
            &|| false,
        )
        .unwrap_err();
        assert_eq!(err, "target_exists");
    }

    #[test]
    fn publish_rejects_invalid_digest() {
        let root = TempDir::new();
        let instances = fresh_dirs(root.path());
        let ctx = ctx_at(root.path());
        let prepared = make_prepared(&instances, "inst-5");
        let mock = MockInstances::new(false, Ok(()));
        assert!(publish(
            &ctx,
            &(mock as Arc<dyn CoreModule>),
            "inst-5",
            "n",
            &prepared,
            "nothex",
            &|| false,
        )
        .is_err());
    }

    #[test]
    fn publish_success_registers_metadata() {
        let root = TempDir::new();
        let instances = fresh_dirs(root.path());
        let ctx = ctx_at(root.path());
        let prepared = make_prepared(&instances, "inst-6");
        let mock = MockInstances::new(false, Ok(()));
        let out = publish(
            &ctx,
            &(mock as Arc<dyn CoreModule>),
            "inst-6",
            "Nice",
            &prepared,
            &sha(),
            &|| false,
        )
        .unwrap();
        assert!(Path::new(&out).ends_with("inst-6"));
        assert!(instances.join("inst-6").exists());
        assert!(!instances.join(".install-neo-inst-6").exists());
        let receipt = read_receipt(&instances.join("inst-6")).unwrap().unwrap();
        assert!(receipt.matches("inst-6", &sha()));
    }

    #[test]
    fn registration_rejection_rolls_back_to_stage() {
        let root = TempDir::new();
        let instances = fresh_dirs(root.path());
        let ctx = ctx_at(root.path());
        let prepared = make_prepared(&instances, "inst-7");
        let mock = MockInstances::new(false, Err("rejected".into()));
        assert!(publish(
            &ctx,
            &(mock as Arc<dyn CoreModule>),
            "inst-7",
            "n",
            &prepared,
            &sha(),
            &|| false,
        )
        .is_err());
        assert!(!instances.join("inst-7").exists());
        assert!(instances.join(".install-neo-inst-7").exists());
    }

    #[test]
    fn cancellation_before_commit_cleans_owned_stage() {
        let root = TempDir::new();
        let instances = fresh_dirs(root.path());
        let ctx = ctx_at(root.path());
        let prepared = make_prepared(&instances, "inst-8");
        let mock = MockInstances::new(false, Ok(()));
        let flag = AtomicBool::new(true);
        let err = publish(
            &ctx,
            &(mock as Arc<dyn CoreModule>),
            "inst-8",
            "n",
            &prepared,
            &sha(),
            &|| flag.load(Ordering::Relaxed),
        )
        .unwrap_err();
        assert!(err.starts_with("cancelled"));
        assert!(!instances.join(".install-neo-inst-8").exists());
        assert!(!instances.join("inst-8").exists());
    }

    #[test]
    fn wrong_receipt_in_stage_is_refused() {
        let root = TempDir::new();
        let instances = fresh_dirs(root.path());
        let ctx = ctx_at(root.path());
        let stage = instances.join(".install-neo-inst-9");
        std::fs::create_dir_all(&stage).unwrap();
        write_receipt(&stage, &Receipt::new("other", &sha())).unwrap();
        let mock = MockInstances::new(false, Ok(()));
        let prepared = make_prepared(&instances, "inst-9");
        let err = publish(
            &ctx,
            &(mock as Arc<dyn CoreModule>),
            "inst-9",
            "n",
            &prepared,
            &sha(),
            &|| false,
        )
        .unwrap_err();
        assert_eq!(err, "stage_receipt_conflict");
    }

    #[test]
    fn out_of_bounds_root_is_refused() {
        let root = TempDir::new();
        let _ = fresh_dirs(root.path());
        let ctx = ctx_at(root.path());
        let outside = root.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let mock = MockInstances::new(false, Ok(()));
        let err = publish(
            &ctx,
            &(mock as Arc<dyn CoreModule>),
            "inst-10",
            "n",
            &outside,
            &sha(),
            &|| false,
        )
        .unwrap_err();
        assert_eq!(err, "prepared_root_out_of_bounds");
    }
}

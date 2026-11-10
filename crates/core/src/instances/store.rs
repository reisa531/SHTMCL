//! The instance index: one lock, one commit point, one durability order.
//!
//! Every mutation flows through [`InstanceStore::commit`], which:
//! 1. resolves the idempotency key (a repeated `requestId` returns the first
//!    outcome instead of applying twice),
//! 2. checks the caller's `expectedRevision` against the stored revision,
//! 3. refuses while a live lease holds the instance,
//! 4. writes a recovery record, applies the change, bumps `revision` by one,
//! 5. marks the recovery record committed.
//!
//! The store keeps state in memory and hands it to a [`StoragePort`] so the
//! write itself (atomic temp-file + rename) lives behind a testable boundary. A
//! failed write rolls back the in-memory change, so a rejected commit leaves no
//! trace: this is what keeps `revision` honest after an I/O error.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};
use shtmcl_domain::Revision;

use crate::instances::error::InstancesError;
use crate::instances::state::{
    Instance, InstanceLease, InstancePatch, InstanceState, RecoveryRecord,
};

mod mod_files;
pub use mod_files::ModFileTransaction;

/// Bounded idempotency memory: enough to cover a burst of retries, small enough
/// that a long session cannot grow it without limit.
const IDEMPOTENCY_CAPACITY: usize = 512;

/// Where a committed index snapshot goes. Implemented by the app layer (real
/// filesystem) and by tests (memory), so commits are testable without touching
/// the disk.
pub trait StoragePort: Send + Sync {
    /// Durable single-file Mod transaction, implemented only by real storage.
    fn commit_mod_toggle(
        &self,
        _transaction: &ModFileTransaction,
        _instances: &[Instance],
        _leases: &[InstanceLease],
    ) -> Result<(), String> {
        Err("mod transactions unavailable".into())
    }
    /// Persist the full index. Must be atomic: a partial write is never visible.
    fn save_index(&self, snapshot: &[Instance]) -> Result<(), String>;

    /// Read the last persisted index. `Err` means the file exists but is
    /// unreadable; `Ok(vec![])` means "no index yet".
    fn load_index(&self) -> Result<Vec<Instance>, String>;

    /// Mark a recovery record durable before the mutation is applied.
    fn write_recovery(&self, record: &RecoveryRecord) -> Result<(), String>;

    /// Complete a recovery entry only after the index commit is durable.
    fn complete_recovery(&self, record: &RecoveryRecord) -> Result<(), String> {
        if let Some(target) = record.import_target.as_deref() {
            if record.committed {
                self.finalize_import(target, &record.request_id)?;
            } else if record.aborted {
                self.cleanup_import(target, &record.request_id)?;
            }
        }
        self.write_recovery(record)
    }

    /// Read the durable recovery log. Old test ports may return an empty log.
    fn load_recovery(&self) -> Result<Vec<RecoveryRecord>, String> {
        Ok(Vec::new())
    }

    /// Persist the index and active leases as one logical snapshot.
    fn save_state(&self, instances: &[Instance], _leases: &[InstanceLease]) -> Result<(), String> {
        self.save_index(instances)
    }

    fn load_leases(&self) -> Result<Vec<InstanceLease>, String> {
        Ok(Vec::new())
    }

    /// Verify a directory is usable for a new instance (exists, writable).
    fn check_location(&self, root: &str) -> Result<(), String>;

    /// Copy an existing game directory into the managed location.
    fn import_directory(&self, source: &str, target: &str) -> Result<(), String>;

    fn import_directory_cancellable(
        &self,
        source: &str,
        target: &str,
        _operation_key: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        if cancelled() {
            return Err("import cancelled".into());
        }
        self.import_directory(source, target)
    }

    fn cleanup_import(&self, _target: &str, _operation_key: &str) -> Result<(), String> {
        Ok(())
    }

    fn finalize_import(&self, _target: &str, _operation_key: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Atomic JSON-backed store for the launcher data directory. All writes use a
/// sibling temporary file, flush it, then rename it into place.
pub struct FileStorage {
    root: PathBuf,
    // Recovery may clear process-local leases, so only one live store can own
    // this data directory. Closing the handle releases the OS lock on a crash.
    _owner: fs::File,
}

const IMPORT_MARKER: &str = ".shtmcl-import-marker";

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct PersistedState {
    instances: Vec<Instance>,
    leases: Vec<InstanceLease>,
}

impl FileStorage {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, String> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        let lock_path = root.join(".instance-store.lock");
        if let Ok(meta) = fs::symlink_metadata(&lock_path) {
            if meta.file_type().is_symlink() {
                return Err("instance storage lock is a symbolic link".into());
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    return Err("instance storage lock is a reparse point".into());
                }
            }
        }
        let owner = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(|error| error.to_string())?;
        owner
            .try_lock()
            .map_err(|_| "instance storage is already open in another process".to_string())?;
        Ok(Self {
            root,
            _owner: owner,
        })
    }

    fn atomic_json<T: serde::Serialize>(&self, name: &str, value: &T) -> Result<(), String> {
        let target = self.root.join(name);
        let temp = self.root.join(format!(".{name}.tmp"));
        let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        let mut file = fs::File::create(&temp).map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        replace_file(&temp, &target).map_err(|error| error.to_string())
    }

    fn read_json<T: serde::de::DeserializeOwned + Default>(&self, name: &str) -> Result<T, String> {
        let path = self.root.join(name);
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(error) => Err(error.to_string()),
        }
    }
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

impl StoragePort for FileStorage {
    fn save_index(&self, snapshot: &[Instance]) -> Result<(), String> {
        mod_files::recover(self)?;
        self.atomic_json(
            "state.json",
            &PersistedState {
                instances: snapshot.to_vec(),
                leases: Vec::new(),
            },
        )
    }

    fn load_index(&self) -> Result<Vec<Instance>, String> {
        mod_files::recover(self)?;
        Ok(self.read_json::<PersistedState>("state.json")?.instances)
    }

    fn write_recovery(&self, record: &RecoveryRecord) -> Result<(), String> {
        let mut records: Vec<RecoveryRecord> = self.read_json("recovery.json")?;
        if let Some(existing) = records
            .iter_mut()
            .find(|item| item.request_id == record.request_id)
        {
            *existing = record.clone();
        } else {
            records.push(record.clone());
        }
        self.atomic_json("recovery.json", &records)
    }

    fn complete_recovery(&self, record: &RecoveryRecord) -> Result<(), String> {
        if let Some(target) = record.import_target.as_deref() {
            if record.committed {
                self.finalize_import(target, &record.request_id)?;
            } else if record.aborted {
                self.cleanup_import(target, &record.request_id)?;
            }
        }
        self.write_recovery(record)
    }

    fn load_recovery(&self) -> Result<Vec<RecoveryRecord>, String> {
        self.read_json("recovery.json")
    }

    fn save_state(&self, instances: &[Instance], leases: &[InstanceLease]) -> Result<(), String> {
        mod_files::recover(self)?;
        self.atomic_json(
            "state.json",
            &PersistedState {
                instances: instances.to_vec(),
                leases: leases.to_vec(),
            },
        )
    }

    fn load_leases(&self) -> Result<Vec<InstanceLease>, String> {
        Ok(self.read_json::<PersistedState>("state.json")?.leases)
    }

    fn commit_mod_toggle(
        &self,
        transaction: &ModFileTransaction,
        instances: &[Instance],
        leases: &[InstanceLease],
    ) -> Result<(), String> {
        mod_files::commit(self, transaction, instances, leases)
    }

    fn check_location(&self, root: &str) -> Result<(), String> {
        let path = Path::new(root);
        fs::create_dir_all(path).map_err(|error| error.to_string())
    }

    fn import_directory(&self, source: &str, target: &str) -> Result<(), String> {
        self.import_directory_cancellable(source, target, "direct-import", &|| false)
    }

    fn import_directory_cancellable(
        &self,
        source: &str,
        target: &str,
        operation_key: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let source = fs::canonicalize(source).map_err(|error| error.to_string())?;
        if source.join(IMPORT_MARKER).exists() {
            return Err("source contains a reserved launcher marker".into());
        }
        let target = Path::new(target);
        if target.exists() {
            return Err("import target already exists".into());
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let parent = target
            .parent()
            .ok_or_else(|| "import target has no parent".to_string())?;
        let parent = fs::canonicalize(parent).map_err(|error| error.to_string())?;
        let target_name = target
            .file_name()
            .ok_or_else(|| "import target has no name".to_string())?;
        let target = parent.join(target_name);
        if source == target || source.starts_with(&target) || target.starts_with(&source) {
            return Err("source and target must be separate trees".into());
        }
        let stage = import_stage(&target);
        if stage.exists() {
            fs::remove_dir_all(&stage).map_err(|error| error.to_string())?;
        }
        if let Err(error) = copy_tree_cancellable(&source, &stage, cancelled) {
            let _ = fs::remove_dir_all(&stage);
            return Err(error);
        }
        if cancelled() {
            let _ = fs::remove_dir_all(&stage);
            return Err("import cancelled".into());
        }
        if let Err(error) = fs::write(stage.join(IMPORT_MARKER), operation_key) {
            let _ = fs::remove_dir_all(&stage);
            return Err(error.to_string());
        }
        fs::rename(&stage, target).map_err(|error| {
            let _ = fs::remove_dir_all(&stage);
            error.to_string()
        })
    }

    fn cleanup_import(&self, target: &str, operation_key: &str) -> Result<(), String> {
        let target = Path::new(target);
        let stage = import_stage(target);
        if stage.exists() {
            fs::remove_dir_all(&stage).map_err(|error| error.to_string())?;
        }
        if matches!(fs::read_to_string(target.join(IMPORT_MARKER)), Ok(content) if content == operation_key)
        {
            fs::remove_dir_all(target).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn finalize_import(&self, target: &str, operation_key: &str) -> Result<(), String> {
        let marker = Path::new(target).join(IMPORT_MARKER);
        if matches!(fs::read_to_string(&marker), Ok(content) if content == operation_key) {
            fs::remove_file(marker).map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

fn import_stage(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("instance");
    target.with_file_name(format!(".{name}.shtmcl-importing"))
}

fn copy_tree_cancellable(
    source: &Path,
    target: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    if !source.is_dir() {
        return Err("source directory unavailable".into());
    }
    if source == target || target.starts_with(source) || source.starts_with(target) {
        return Err("source and target must be separate trees".into());
    }
    fs::create_dir_all(target).map_err(|error| error.to_string())?;
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        if cancelled() {
            return Err("import cancelled".into());
        }
        let entry = entry.map_err(|error| error.to_string())?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            copy_tree_cancellable(&from, &to, cancelled)?;
        } else if kind.is_file() {
            let mut input = fs::File::open(from).map_err(|error| error.to_string())?;
            let mut output = fs::File::create(to).map_err(|error| error.to_string())?;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                if cancelled() {
                    return Err("import cancelled".into());
                }
                let read = std::io::Read::read(&mut input, &mut buffer)
                    .map_err(|error| error.to_string())?;
                if read == 0 {
                    break;
                }
                output
                    .write_all(&buffer[..read])
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(())
}

/// Outcome of one commit, returned to the caller so `revision` is never guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    pub instance: Instance,
    /// True when the request was a replay of an already-applied mutation.
    pub replayed: bool,
}

/// In-memory index with a single commit path.
pub struct InstanceStore {
    inner: Mutex<Inner>,
    storage: Box<dyn StoragePort>,
}

struct Inner {
    instances: BTreeMap<String, Instance>,
    leases: BTreeMap<String, InstanceLease>,
    recovery: BTreeMap<String, RecoveryRecord>,
    /// `requestId` -> the revision it produced. Replays return this revision.
    applied: BTreeMap<String, AppliedRequest>,
    applied_order: Vec<String>,
}

#[derive(Clone)]
struct AppliedRequest {
    instance_id: String,
    revision: u64,
    fingerprint: String,
}

impl Inner {
    fn new() -> Self {
        Self {
            instances: BTreeMap::new(),
            leases: BTreeMap::new(),
            recovery: BTreeMap::new(),
            applied: BTreeMap::new(),
            applied_order: Vec::new(),
        }
    }

    fn remember(&mut self, request_id: &str, record: AppliedRequest) {
        if self
            .applied
            .insert(request_id.to_string(), record)
            .is_none()
        {
            self.applied_order.push(request_id.to_string());
            while self.applied_order.len() > IDEMPOTENCY_CAPACITY {
                let evicted = self.applied_order.remove(0);
                self.applied.remove(&evicted);
            }
        }
    }

    fn snapshot(&self) -> Vec<Instance> {
        self.instances.values().cloned().collect()
    }

    fn applied_request(&self, request_id: &str) -> Option<AppliedRequest> {
        self.applied.get(request_id).cloned().or_else(|| {
            self.recovery
                .get(request_id)
                .filter(|record| record.committed && !record.aborted)
                .map(|record| AppliedRequest {
                    instance_id: record.instance_id.clone(),
                    revision: record.expected_revision,
                    fingerprint: record.fingerprint.clone(),
                })
        })
    }

    /// A lease is live only until its deadline; an expired lease must not keep
    /// blocking edits after a crash.
    fn live_lease(&self, instance_id: &str, now: u64) -> Option<&InstanceLease> {
        self.leases
            .get(instance_id)
            .filter(|lease| !lease.is_expired(now))
    }
}

fn fingerprint<T: serde::Serialize>(command: &str, body: &T) -> Result<String, InstancesError> {
    let bytes = serde_json::to_vec(&(command, body)).map_err(|_| InstancesError::Internal)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn create_fingerprint(draft: &Instance) -> Result<String, InstancesError> {
    fingerprint(
        "create",
        &(
            &draft.id,
            &draft.name,
            &draft.version,
            &draft.location,
            draft.java_major,
            &draft.identity_id,
            &draft.profile_id,
        ),
    )
}

impl InstanceStore {
    pub fn new(storage: Box<dyn StoragePort>) -> Result<Self, InstancesError> {
        let store = Self {
            inner: Mutex::new(Inner::new()),
            storage,
        };
        store.reload()?;
        Ok(store)
    }

    /// Rebuild the in-memory index from storage. A corrupt index is reported, not
    /// silently reset: the caller decides whether to repair.
    pub fn reload(&self) -> Result<usize, InstancesError> {
        let loaded = match self.storage.load_index() {
            Ok(instances) => instances,
            Err(reference) => {
                // "no index yet" is reported by the port as an empty list, so any
                // Err here means the file exists and could not be read.
                return Err(InstancesError::IndexCorrupt(reference));
            }
        };
        let leases = self
            .storage
            .load_leases()
            .map_err(InstancesError::IndexCorrupt)?;
        let recovery = self
            .storage
            .load_recovery()
            .map_err(InstancesError::IndexCorrupt)?;
        let mut inner = self.lock()?;
        inner.instances = loaded.into_iter().map(|i| (i.id.clone(), i)).collect();
        inner.leases = leases
            .into_iter()
            .map(|lease| (lease.instance_id.clone(), lease))
            .collect();
        inner.recovery = recovery
            .into_iter()
            .map(|record| (record.request_id.clone(), record))
            .collect();
        inner.applied.clear();
        inner.applied_order.clear();
        for mut record in inner.recovery.values().cloned().collect::<Vec<_>>() {
            // An aborted intent has already been rolled back. Replaying its
            // before-image would overwrite a later successful transaction.
            if record.aborted {
                continue;
            }
            if !record.committed {
                let is_after = match &record.after {
                    Some(after) => inner.instances.get(&record.instance_id) == Some(after),
                    None => !inner.instances.contains_key(&record.instance_id),
                };
                if !is_after {
                    if let Some(target) = record.import_target.as_deref() {
                        self.storage
                            .cleanup_import(target, &record.request_id)
                            .map_err(InstancesError::IndexIo)?;
                    }
                    match &record.before {
                        Some(before) => {
                            inner
                                .instances
                                .insert(record.instance_id.clone(), before.clone());
                        }
                        None => {
                            inner.instances.remove(&record.instance_id);
                        }
                    }
                    self.storage
                        .save_state(
                            &inner.snapshot(),
                            &inner.leases.values().cloned().collect::<Vec<_>>(),
                        )
                        .map_err(InstancesError::IndexIo)?;
                    record.aborted = true;
                    self.storage
                        .complete_recovery(&record)
                        .map_err(InstancesError::IndexIo)?;
                    inner.recovery.insert(record.request_id.clone(), record);
                    continue;
                }
                record.committed = true;
                self.storage
                    .complete_recovery(&record)
                    .map_err(InstancesError::IndexIo)?;
                inner
                    .recovery
                    .insert(record.request_id.clone(), record.clone());
            }
            if record.committed && !record.aborted && !record.fingerprint.is_empty() {
                inner.remember(
                    &record.request_id,
                    AppliedRequest {
                        instance_id: record.instance_id.clone(),
                        revision: record.expected_revision,
                        fingerprint: record.fingerprint.clone(),
                    },
                );
            }
        }
        // An independently running game can outlive its launcher. Only the
        // application process-identity recovery may release its protection.
        // MAX was used for running leases before the explicit field existed.
        inner
            .leases
            .retain(|_, lease| lease.running || lease.expires_at == u64::MAX);
        let protected: std::collections::BTreeSet<String> = inner.leases.keys().cloned().collect();
        let mut repaired = false;
        for instance in inner.instances.values_mut() {
            if instance.state == InstanceState::Busy && !protected.contains(&instance.id) {
                instance.state = InstanceState::Ready;
                repaired = true;
            }
        }
        if repaired {
            self.storage
                .save_state(
                    &inner.snapshot(),
                    &inner.leases.values().cloned().collect::<Vec<_>>(),
                )
                .map_err(InstancesError::IndexIo)?;
        }
        Ok(inner.instances.len())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, InstancesError> {
        self.inner.lock().map_err(|_| InstancesError::Internal)
    }

    /// List every live instance, sorted by id for a stable UI order.
    pub fn list(&self) -> Result<Vec<Instance>, InstancesError> {
        Ok(self.lock()?.snapshot())
    }

    pub fn leases(&self) -> Result<Vec<InstanceLease>, InstancesError> {
        Ok(self.lock()?.leases.values().cloned().collect())
    }

    pub fn get(&self, id: &str) -> Result<Instance, InstancesError> {
        self.lock()?
            .instances
            .get(id)
            .cloned()
            .ok_or_else(|| InstancesError::NotFound(id.to_string()))
    }

    pub fn revision_of(&self, id: &str) -> Result<u64, InstancesError> {
        Ok(self.get(id)?.revision)
    }

    /// Create one instance. `request_id` is the idempotency key.
    pub fn create(
        &self,
        request_id: &str,
        draft: Instance,
        now: u64,
    ) -> Result<CommitOutcome, InstancesError> {
        let mut inner = self.lock()?;
        let fingerprint = create_fingerprint(&draft)?;
        if let Some(applied) = inner.applied_request(request_id) {
            if applied.instance_id != draft.id || applied.fingerprint != fingerprint {
                return Err(InstancesError::IdempotencyConflict);
            }
            let instance = inner
                .recovery
                .get(request_id)
                .and_then(|record| record.after.clone())
                .ok_or(InstancesError::IdempotencyConflict)?;
            return Ok(CommitOutcome {
                instance,
                replayed: true,
            });
        }
        if inner.instances.contains_key(&draft.id) {
            return Err(InstancesError::Duplicate(draft.id));
        }
        draft.location.validate().map_err(|reason| match reason {
            "absolute" => InstancesError::LocationNotAbsolute(draft.location.root.clone()),
            _ => InstancesError::LocationUnavailable(draft.location.root.clone()),
        })?;
        draft
            .version
            .validate()
            .map_err(InstancesError::InvalidRequest)?;
        if draft.name.trim().is_empty() {
            return Err(InstancesError::InvalidRequest("name"));
        }
        self.storage
            .check_location(&draft.location.root)
            .map_err(|reference| InstancesError::LocationNotWritable(reference))?;

        let (operation_id, import_target) = inner
            .recovery
            .get(request_id)
            .map(|record| (record.operation_id.clone(), record.import_target.clone()))
            .unwrap_or_else(|| (request_id.to_string(), None));

        let record = RecoveryRecord {
            request_id: request_id.to_string(),
            operation_id,
            instance_id: draft.id.clone(),
            expected_revision: 1,
            committed: false,
            before: None,
            after: Some(Instance {
                state: InstanceState::Ready,
                revision: 1,
                created_at: now,
                updated_at: now,
                ..draft.clone()
            }),
            fingerprint: fingerprint.clone(),
            aborted: false,
            import_target,
        };
        self.storage
            .write_recovery(&record)
            .map_err(InstancesError::IndexIo)?;

        let created = Instance {
            state: InstanceState::Ready,
            revision: 1,
            created_at: now,
            updated_at: now,
            ..draft
        };

        inner.instances.insert(created.id.clone(), created.clone());
        inner
            .recovery
            .insert(request_id.to_string(), record.clone());

        // Durability point: if the write fails, drop the in-memory change so a
        // rejected commit cannot leave a phantom revision behind.
        if let Err(reference) = self.storage.save_state(
            &inner.snapshot(),
            &inner.leases.values().cloned().collect::<Vec<_>>(),
        ) {
            inner.instances.remove(&created.id);
            let mut aborted = record;
            aborted.aborted = true;
            let _ = self.storage.complete_recovery(&aborted);
            inner.recovery.insert(request_id.to_string(), aborted);
            return Err(InstancesError::IndexIo(reference));
        }

        let mut committed = record;
        committed.committed = true;
        let _ = self.storage.complete_recovery(&committed);
        inner.recovery.insert(request_id.to_string(), committed);
        inner.remember(
            request_id,
            AppliedRequest {
                instance_id: created.id.clone(),
                revision: created.revision,
                fingerprint,
            },
        );
        Ok(CommitOutcome {
            instance: created,
            replayed: false,
        })
    }

    /// Apply a patch under optimistic concurrency.
    ///
    /// `expected` is what the caller last read. A mismatch is `RevisionStale`
    /// (the caller is behind) rather than a silent merge: merging two writers is
    /// how a launch configuration gets corrupted.
    pub fn update(
        &self,
        request_id: &str,
        id: &str,
        expected: Revision,
        patch: &InstancePatch,
        now: u64,
    ) -> Result<CommitOutcome, InstancesError> {
        let mut inner = self.lock()?;
        let fingerprint = fingerprint("update", &(id, expected.get(), patch))?;
        if let Some(applied) = inner.applied_request(request_id) {
            if applied.instance_id != id || applied.fingerprint != fingerprint {
                return Err(InstancesError::IdempotencyConflict);
            }
            let current = inner
                .recovery
                .get(request_id)
                .and_then(|record| record.after.clone())
                .ok_or(InstancesError::IdempotencyConflict)?;
            return Ok(CommitOutcome {
                instance: current,
                replayed: true,
            });
        }
        let current = inner
            .instances
            .get(id)
            .cloned()
            .ok_or_else(|| InstancesError::NotFound(id.to_string()))?;

        if inner.live_lease(id, now).is_some() {
            return Err(if current.state == InstanceState::Busy {
                InstancesError::Running(id.to_string())
            } else {
                InstancesError::Locked(id.to_string())
            });
        }
        if current.revision != expected.get() {
            return Err(InstancesError::RevisionStale {
                expected: expected.get(),
                actual: current.revision,
            });
        }
        if current.state == InstanceState::Busy && !inner.leases.contains_key(id) {
            return Err(InstancesError::Running(id.to_string()));
        }
        if current.state == InstanceState::Broken {
            return Err(match current.state {
                _ => InstancesError::Terminal(id.to_string()),
            });
        }
        // An unchanged patch must not bump the revision, otherwise a no-op click
        // invalidates every other open window for nothing.
        if patch.is_empty() {
            return Ok(CommitOutcome {
                instance: current,
                replayed: true,
            });
        }
        patch.validate().map_err(InstancesError::InvalidRequest)?;
        if let Some(location) = &patch.location {
            location.validate().map_err(|reason| match reason {
                "absolute" => InstancesError::LocationNotAbsolute(location.root.clone()),
                _ => InstancesError::LocationUnavailable(location.root.clone()),
            })?;
            self.storage
                .check_location(&location.root)
                .map_err(InstancesError::LocationNotWritable)?;
        }

        let next_revision = current.revision + 1;
        let updated = Instance {
            name: patch.name.clone().unwrap_or_else(|| current.name.clone()),
            version: patch
                .version
                .clone()
                .unwrap_or_else(|| current.version.clone()),
            location: patch
                .location
                .clone()
                .unwrap_or_else(|| current.location.clone()),
            java_major: patch.java_major.or(current.java_major),
            identity_id: patch.identity_id.clone().or(current.identity_id.clone()),
            profile_id: patch.profile_id.clone().or(current.profile_id.clone()),
            revision: next_revision,
            updated_at: now,
            state: InstanceState::Ready,
            ..current.clone()
        };

        let record = RecoveryRecord {
            request_id: request_id.to_string(),
            operation_id: request_id.to_string(),
            instance_id: id.to_string(),
            expected_revision: next_revision,
            committed: false,
            before: Some(current.clone()),
            after: Some(updated.clone()),
            fingerprint: fingerprint.clone(),
            aborted: false,
            import_target: None,
        };
        self.storage
            .write_recovery(&record)
            .map_err(InstancesError::IndexIo)?;

        let expired_lease = inner.leases.remove(id);
        inner.instances.insert(id.to_string(), updated.clone());
        inner
            .recovery
            .insert(request_id.to_string(), record.clone());

        if let Err(reference) = self.storage.save_state(
            &inner.snapshot(),
            &inner.leases.values().cloned().collect::<Vec<_>>(),
        ) {
            inner.instances.insert(id.to_string(), current);
            if let Some(lease) = expired_lease {
                inner.leases.insert(id.to_string(), lease);
            }
            let mut aborted = record;
            aborted.aborted = true;
            let _ = self.storage.complete_recovery(&aborted);
            inner.recovery.insert(request_id.to_string(), aborted);
            return Err(InstancesError::IndexIo(reference));
        }

        let mut committed = record;
        committed.committed = true;
        let _ = self.storage.complete_recovery(&committed);
        inner.recovery.insert(request_id.to_string(), committed);
        inner.remember(
            request_id,
            AppliedRequest {
                instance_id: id.to_string(),
                revision: next_revision,
                fingerprint,
            },
        );
        Ok(CommitOutcome {
            instance: updated,
            replayed: false,
        })
    }

    /// Mark an instance busy while a long task owns it.
    pub fn acquire_lease(
        &self,
        instance_id: &str,
        holder: &str,
        operation_id: &str,
        now: u64,
        ttl_ms: u64,
    ) -> Result<InstanceLease, InstancesError> {
        self.acquire_lease_checked(instance_id, holder, operation_id, now, ttl_ms, false)
    }

    /// Running leases cannot turn an incomplete or damaged instance into Ready.
    pub fn acquire_running_lease(
        &self,
        instance_id: &str,
        holder: &str,
        operation_id: &str,
        now: u64,
        ttl_ms: u64,
    ) -> Result<InstanceLease, InstancesError> {
        self.acquire_lease_checked(instance_id, holder, operation_id, now, ttl_ms, true)
    }

    fn acquire_lease_checked(
        &self,
        instance_id: &str,
        holder: &str,
        operation_id: &str,
        now: u64,
        ttl_ms: u64,
        running: bool,
    ) -> Result<InstanceLease, InstancesError> {
        let mut inner = self.lock()?;
        let current = inner
            .instances
            .get(instance_id)
            .cloned()
            .ok_or_else(|| InstancesError::NotFound(instance_id.to_string()))?;
        if let Some(existing) = inner.live_lease(instance_id, now) {
            if existing.holder != holder {
                return Err(InstancesError::Locked(instance_id.to_string()));
            }
        }
        if running
            && current.state != InstanceState::Ready
            && !(current.state == InstanceState::Busy
                && inner
                    .live_lease(instance_id, now)
                    .is_some_and(|lease| lease.holder == holder))
        {
            return Err(InstancesError::Terminal(instance_id.to_owned()));
        }
        let previous_lease = inner.leases.get(instance_id).cloned();
        let previous_state = current.clone();
        let lease = InstanceLease {
            running,
            instance_id: instance_id.to_string(),
            holder: holder.to_string(),
            operation_id: operation_id.to_string(),
            expires_at: now.saturating_add(ttl_ms),
        };
        inner.leases.insert(instance_id.to_string(), lease.clone());
        let busy = Instance {
            state: InstanceState::Busy,
            updated_at: now,
            ..current
        };
        inner.instances.insert(instance_id.to_string(), busy);
        if let Err(reference) = self.storage.save_state(
            &inner.snapshot(),
            &inner.leases.values().cloned().collect::<Vec<_>>(),
        ) {
            inner
                .instances
                .insert(instance_id.to_string(), previous_state);
            match previous_lease {
                Some(old) => {
                    inner.leases.insert(instance_id.to_string(), old);
                }
                None => {
                    inner.leases.remove(instance_id);
                }
            }
            return Err(InstancesError::IndexIo(reference));
        }
        Ok(lease)
    }

    /// Release a lease. Releasing someone else's lease is refused, so a stale
    /// task cannot unlock an instance that a newer task now owns.
    pub fn release_lease(&self, instance_id: &str, holder: &str) -> Result<(), InstancesError> {
        let mut inner = self.lock()?;
        match inner.leases.get(instance_id).cloned() {
            Some(lease) if lease.holder == holder => {
                inner.leases.remove(instance_id);
                let previous = inner.instances.get(instance_id).cloned();
                if let Some(current) = previous
                    .clone()
                    .filter(|item| item.state == InstanceState::Busy)
                {
                    inner.instances.insert(
                        instance_id.to_string(),
                        Instance {
                            state: InstanceState::Ready,
                            ..current
                        },
                    );
                }
                if let Err(reference) = self.storage.save_state(
                    &inner.snapshot(),
                    &inner.leases.values().cloned().collect::<Vec<_>>(),
                ) {
                    if let Some(previous) = previous {
                        inner.instances.insert(instance_id.to_string(), previous);
                    }
                    inner.leases.insert(instance_id.to_string(), lease.clone());
                    return Err(InstancesError::IndexIo(reference));
                }
                Ok(())
            }
            Some(_) => Err(InstancesError::LeaseUnauthorized(instance_id.to_string())),
            None => Ok(()),
        }
    }

    /// Change lifecycle state. Kept separate from `update` because a task owns it,
    /// not the user, and it must not require the user's `expectedRevision`.
    pub fn set_state(
        &self,
        instance_id: &str,
        state: InstanceState,
        now: u64,
    ) -> Result<Instance, InstancesError> {
        let mut inner = self.lock()?;
        let current = inner
            .instances
            .get(instance_id)
            .cloned()
            .ok_or_else(|| InstancesError::NotFound(instance_id.to_string()))?;
        // Keep the previous value for rollback: the struct update below consumes
        // `current`, and a failed write must still be able to restore it.
        let previous = current.clone();
        let updated = Instance {
            state,
            revision: current.revision + 1,
            updated_at: now,
            ..current
        };
        inner
            .instances
            .insert(instance_id.to_string(), updated.clone());
        if let Err(reference) = self.storage.save_state(
            &inner.snapshot(),
            &inner.leases.values().cloned().collect::<Vec<_>>(),
        ) {
            inner.instances.insert(instance_id.to_string(), previous);
            return Err(InstancesError::IndexIo(reference));
        }
        Ok(updated)
    }

    /// Remove an instance. The record is dropped from the index; the directory is
    /// left to the caller's storage policy (01 does not delete user files on a
    /// metadata operation).
    pub fn remove(
        &self,
        request_id: &str,
        id: &str,
        expected: Revision,
        now: u64,
    ) -> Result<u64, InstancesError> {
        let mut inner = self.lock()?;
        let operation_fingerprint = fingerprint("remove", &(id, expected.get()))?;
        if let Some(applied) = inner.applied_request(request_id) {
            if applied.instance_id != id || applied.fingerprint != operation_fingerprint {
                return Err(InstancesError::IdempotencyConflict);
            }
            return Ok(applied.revision);
        }
        let current = inner
            .instances
            .get(id)
            .cloned()
            .ok_or_else(|| InstancesError::NotFound(id.to_string()))?;
        if inner.live_lease(id, now).is_some() {
            return Err(InstancesError::Locked(id.to_string()));
        }
        if current.revision != expected.get() {
            return Err(InstancesError::RevisionStale {
                expected: expected.get(),
                actual: current.revision,
            });
        }
        let revision = current.revision + 1;
        let record = RecoveryRecord {
            request_id: request_id.to_string(),
            operation_id: request_id.to_string(),
            instance_id: id.to_string(),
            expected_revision: revision,
            committed: false,
            before: Some(current.clone()),
            after: None,
            fingerprint: operation_fingerprint.clone(),
            aborted: false,
            import_target: None,
        };
        self.storage
            .write_recovery(&record)
            .map_err(InstancesError::IndexIo)?;
        inner
            .recovery
            .insert(request_id.to_string(), record.clone());
        inner.instances.remove(id);
        if let Err(reference) = self.storage.save_state(
            &inner.snapshot(),
            &inner.leases.values().cloned().collect::<Vec<_>>(),
        ) {
            inner.instances.insert(id.to_string(), current);
            let mut aborted = record;
            aborted.aborted = true;
            let _ = self.storage.complete_recovery(&aborted);
            inner.recovery.insert(request_id.to_string(), aborted);
            return Err(InstancesError::IndexIo(reference));
        }
        let mut committed = record;
        committed.committed = true;
        let _ = self.storage.complete_recovery(&committed);
        inner.recovery.insert(request_id.to_string(), committed);
        inner.remember(
            request_id,
            AppliedRequest {
                instance_id: id.to_string(),
                revision,
                fingerprint: operation_fingerprint,
            },
        );
        Ok(revision)
    }

    /// Copy an existing game directory into the managed location, then index it.
    pub fn import_directory(
        &self,
        request_id: &str,
        operation_id: &str,
        source_root: &str,
        draft: Instance,
        now: u64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<CommitOutcome, InstancesError> {
        let import_fingerprint =
            fingerprint("import", &(source_root, create_fingerprint(&draft)?))?;
        let import_key = format!("{request_id}#{import_fingerprint}");
        {
            let inner = self.lock()?;
            if let Some(applied) = inner.applied_request(&import_key) {
                if applied.instance_id != draft.id
                    || applied.fingerprint != create_fingerprint(&draft)?
                {
                    return Err(InstancesError::IdempotencyConflict);
                }
                return Ok(CommitOutcome {
                    instance: inner
                        .recovery
                        .get(&import_key)
                        .and_then(|record| record.after.clone())
                        .ok_or(InstancesError::IdempotencyConflict)?,
                    replayed: true,
                });
            }
            if inner
                .instances
                .values()
                .any(|i| i.location == draft.location)
            {
                return Err(InstancesError::Duplicate(draft.location.root.clone()));
            }
            if inner.instances.contains_key(&draft.id) {
                return Err(InstancesError::Duplicate(draft.id));
            }
        }
        draft
            .location
            .validate()
            .map_err(InstancesError::InvalidRequest)?;
        draft
            .version
            .validate()
            .map_err(InstancesError::InvalidRequest)?;
        if draft.name.trim().is_empty() {
            return Err(InstancesError::InvalidRequest("name"));
        }
        if Path::new(source_root) == Path::new(&draft.location.root) {
            return Err(InstancesError::InvalidRequest("sourceRoot"));
        }
        let created = Instance {
            state: InstanceState::Ready,
            revision: 1,
            created_at: now,
            updated_at: now,
            ..draft.clone()
        };
        let intent = RecoveryRecord {
            request_id: import_key.clone(),
            operation_id: operation_id.to_string(),
            instance_id: draft.id.clone(),
            expected_revision: 1,
            committed: false,
            before: None,
            after: Some(created),
            fingerprint: create_fingerprint(&draft)?,
            aborted: false,
            import_target: Some(draft.location.root.clone()),
        };
        self.storage
            .write_recovery(&intent)
            .map_err(InstancesError::IndexIo)?;
        self.lock()?
            .recovery
            .insert(import_key.clone(), intent.clone());
        if let Err(reference) = self.storage.import_directory_cancellable(
            source_root,
            &draft.location.root,
            &import_key,
            cancelled,
        ) {
            self.storage
                .cleanup_import(&draft.location.root, &import_key)
                .map_err(InstancesError::IndexIo)?;
            let mut aborted = intent;
            aborted.aborted = true;
            self.storage
                .complete_recovery(&aborted)
                .map_err(InstancesError::IndexIo)?;
            self.lock()?.recovery.insert(import_key, aborted);
            return Err(InstancesError::ImportSourceMissing(reference));
        }
        if cancelled() {
            let mut aborted = intent;
            aborted.aborted = true;
            self.storage
                .complete_recovery(&aborted)
                .map_err(InstancesError::IndexIo)?;
            self.lock()?.recovery.insert(import_key, aborted);
            return Err(InstancesError::ImportSourceMissing(
                "import cancelled".into(),
            ));
        }
        let outcome = match self.create(&import_key, draft, now) {
            Ok(outcome) => outcome,
            Err(error) => {
                let mut aborted = intent;
                aborted.aborted = true;
                self.storage
                    .complete_recovery(&aborted)
                    .map_err(InstancesError::IndexIo)?;
                self.lock()?.recovery.insert(import_key, aborted);
                return Err(error);
            }
        };
        {
            let mut inner = self.lock()?;
            let mut committed = inner
                .recovery
                .get(&import_key)
                .cloned()
                .ok_or(InstancesError::IdempotencyConflict)?;
            committed.operation_id = operation_id.to_string();
            committed.import_target = Some(
                inner
                    .instances
                    .get(&committed.instance_id)
                    .ok_or(InstancesError::IdempotencyConflict)?
                    .location
                    .root
                    .clone(),
            );
            self.storage
                .complete_recovery(&committed)
                .map_err(InstancesError::IndexIo)?;
            inner.recovery.insert(import_key, committed);
        }
        Ok(outcome)
    }

    /// Recovery journal for diagnostics: which mutations were interrupted.
    pub fn recovery_records(&self) -> Result<Vec<RecoveryRecord>, InstancesError> {
        Ok(self.lock()?.recovery.values().cloned().collect())
    }

    /// Report an operation the journal does not know about, instead of pretending
    /// it completed.
    pub fn recovery_status(&self, operation_id: &str) -> Result<RecoveryRecord, InstancesError> {
        let inner = self.lock()?;
        inner
            .recovery
            .get(operation_id)
            .cloned()
            .or_else(|| {
                inner
                    .recovery
                    .values()
                    .find(|record| record.operation_id == operation_id)
                    .cloned()
            })
            .ok_or_else(|| InstancesError::RecoveryUnknownOperation(operation_id.to_string()))
    }

    /// Attach the public operation id to the durable idempotency record so the
    /// recovery query can address the same operation after a restart.
    pub fn bind_operation(
        &self,
        request_id: &str,
        operation_id: &str,
    ) -> Result<(), InstancesError> {
        let mut inner = self.lock()?;
        let Some(record) = inner.recovery.get_mut(request_id) else {
            return Ok(());
        };
        record.operation_id = operation_id.to_string();
        let saved = record.clone();
        self.storage
            .complete_recovery(&saved)
            .map_err(InstancesError::IndexIo)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Mutex;

    use super::*;

    /// In-memory storage for tests: records every save so a test can assert the
    /// durability order, and can be told to fail the next write.
    #[derive(Default)]
    pub struct MemoryStorage {
        pub saved: Mutex<Vec<Vec<Instance>>>,
        pub recovery: Mutex<Vec<RecoveryRecord>>,
        pub imported: Mutex<Vec<(String, String)>>,
        /// When set, the next `save_index` fails with this reference.
        pub fail_next_save: std::sync::Arc<Mutex<Option<String>>>,
        pub reject_location: Mutex<Option<String>>,
    }

    impl StoragePort for MemoryStorage {
        fn save_index(&self, snapshot: &[Instance]) -> Result<(), String> {
            let mut failing = self.fail_next_save.lock().expect("lock");
            if let Some(reference) = failing.take() {
                return Err(reference);
            }
            self.saved.lock().expect("lock").push(snapshot.to_vec());
            Ok(())
        }

        fn load_index(&self) -> Result<Vec<Instance>, String> {
            Ok(self
                .saved
                .lock()
                .expect("lock")
                .last()
                .cloned()
                .unwrap_or_default())
        }

        fn write_recovery(&self, record: &RecoveryRecord) -> Result<(), String> {
            let mut records = self.recovery.lock().expect("lock");
            if let Some(existing) = records
                .iter_mut()
                .find(|item| item.request_id == record.request_id)
            {
                *existing = record.clone();
            } else {
                records.push(record.clone());
            }
            Ok(())
        }

        fn complete_recovery(&self, record: &RecoveryRecord) -> Result<(), String> {
            self.write_recovery(record)
        }

        fn load_recovery(&self) -> Result<Vec<RecoveryRecord>, String> {
            Ok(self.recovery.lock().expect("lock").clone())
        }

        fn save_state(
            &self,
            snapshot: &[Instance],
            _leases: &[InstanceLease],
        ) -> Result<(), String> {
            self.save_index(snapshot)
        }

        fn check_location(&self, root: &str) -> Result<(), String> {
            match self.reject_location.lock().expect("lock").as_deref() {
                Some(rejected) if rejected == root => Err(format!("dir:{root}")),
                _ => Ok(()),
            }
        }

        fn import_directory(&self, source: &str, target: &str) -> Result<(), String> {
            self.imported
                .lock()
                .expect("lock")
                .push((source.to_string(), target.to_string()));
            Ok(())
        }
    }

    pub fn store() -> InstanceStore {
        InstanceStore::new(Box::new(MemoryStorage::default())).expect("store")
    }

    pub fn store_with(storage: MemoryStorage) -> InstanceStore {
        InstanceStore::new(Box::new(storage)).expect("store")
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{store, store_with, MemoryStorage};

    #[test]
    fn only_one_file_store_can_recover_and_write_a_data_directory() {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-instance-owner-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        let first = super::FileStorage::new(&root).unwrap();
        assert!(super::FileStorage::new(&root).is_err());
        drop(first);
        let restarted = super::FileStorage::new(&root).unwrap();
        drop(restarted);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn running_lease_never_relabels_a_broken_instance() {
        let store = store();
        store.create("create", draft("broken-game"), 1).unwrap();
        store
            .set_state("broken-game", super::InstanceState::Broken, 2)
            .unwrap();
        assert!(store
            .acquire_running_lease("broken-game", "launch-1", "start", 3, u64::MAX)
            .is_err());
        assert_eq!(
            store.get("broken-game").unwrap().state,
            super::InstanceState::Broken
        );
    }

    #[test]
    fn restart_preserves_running_lease_until_process_recovery_releases_it() {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-running-restart-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        let store = InstanceStore::new(Box::new(FileStorage::new(&root).unwrap())).unwrap();
        store
            .create("create", draft_under("survivor", &root), 1)
            .unwrap();
        store
            .acquire_running_lease("survivor", "launch-1", "start", 2, u64::MAX)
            .unwrap();
        drop(store);
        let restored = InstanceStore::new(Box::new(FileStorage::new(&root).unwrap())).unwrap();
        assert_eq!(restored.get("survivor").unwrap().state, InstanceState::Busy);
        assert!(restored.leases().unwrap()[0].running);
        assert!(restored
            .update(
                "edit",
                "survivor",
                Revision::new(1),
                &InstancePatch {
                    name: Some("Unsafe".into()),
                    ..Default::default()
                },
                99
            )
            .is_err());
        restored.release_lease("survivor", "launch-1").unwrap();
        assert_eq!(
            restored.get("survivor").unwrap().state,
            InstanceState::Ready
        );
        drop(restored);
        fs::remove_dir_all(root).unwrap();
    }
    use super::*;
    use crate::instances::state::{GameVersion, InstanceLocation, LoaderKind};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FaultStorage {
        inner: FileStorage,
        fail_state: std::sync::Arc<AtomicBool>,
        fail_complete: std::sync::Arc<AtomicBool>,
    }

    impl FaultStorage {
        fn new(root: &Path) -> Self {
            Self {
                inner: FileStorage::new(root).expect("file storage"),
                fail_state: std::sync::Arc::new(AtomicBool::new(false)),
                fail_complete: std::sync::Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl StoragePort for FaultStorage {
        fn save_index(&self, snapshot: &[Instance]) -> Result<(), String> {
            self.inner.save_index(snapshot)
        }
        fn load_index(&self) -> Result<Vec<Instance>, String> {
            self.inner.load_index()
        }
        fn write_recovery(&self, record: &RecoveryRecord) -> Result<(), String> {
            self.inner.write_recovery(record)
        }
        fn complete_recovery(&self, record: &RecoveryRecord) -> Result<(), String> {
            if self.fail_complete.swap(false, Ordering::SeqCst) {
                return Err("injected journal completion failure".into());
            }
            self.inner.complete_recovery(record)
        }
        fn load_recovery(&self) -> Result<Vec<RecoveryRecord>, String> {
            self.inner.load_recovery()
        }
        fn save_state(
            &self,
            instances: &[Instance],
            leases: &[InstanceLease],
        ) -> Result<(), String> {
            if self.fail_state.swap(false, Ordering::SeqCst) {
                return Err("injected state write failure".into());
            }
            self.inner.save_state(instances, leases)
        }
        fn load_leases(&self) -> Result<Vec<InstanceLease>, String> {
            self.inner.load_leases()
        }
        fn check_location(&self, root: &str) -> Result<(), String> {
            self.inner.check_location(root)
        }
        fn import_directory(&self, source: &str, target: &str) -> Result<(), String> {
            self.inner.import_directory(source, target)
        }
        fn import_directory_cancellable(
            &self,
            source: &str,
            target: &str,
            operation_key: &str,
            cancelled: &dyn Fn() -> bool,
        ) -> Result<(), String> {
            self.inner
                .import_directory_cancellable(source, target, operation_key, cancelled)
        }
        fn cleanup_import(&self, target: &str, operation_key: &str) -> Result<(), String> {
            self.inner.cleanup_import(target, operation_key)
        }
    }

    fn draft(id: &str) -> Instance {
        Instance {
            id: id.into(),
            name: format!("instance {id}"),
            version: GameVersion {
                game_version: "1.21.4".into(),
                loader: LoaderKind::Vanilla,
                loader_version: None,
            },
            location: InstanceLocation {
                root: format!("/data/instances/{id}"),
            },
            java_major: Some(21),
            identity_id: None,
            profile_id: None,
            state: InstanceState::Ready,
            revision: 1,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn draft_under(id: &str, root: &Path) -> Instance {
        let mut value = draft(id);
        value.location.root = root
            .join("instances")
            .join(id)
            .to_string_lossy()
            .into_owned();
        value
    }

    #[test]
    fn create_persists_once_and_starts_at_revision_one() {
        let storage = MemoryStorage::default();
        let store = store_with(storage);
        let outcome = store.create("req-1", draft("i1"), 10).expect("create");
        assert_eq!(outcome.instance.revision, 1);
        assert!(!outcome.replayed);
        assert_eq!(store.list().expect("list").len(), 1);
    }

    #[test]
    fn a_repeated_request_id_does_not_apply_twice() {
        let store = store();
        let first = store.create("req-1", draft("i1"), 10).expect("create");
        let again = store.create("req-1", draft("i1"), 11).expect("replay");
        assert!(again.replayed);
        assert_eq!(again.instance.revision, first.instance.revision);
        assert_eq!(store.list().expect("list").len(), 1);
    }

    #[test]
    fn reusing_an_idempotency_key_for_another_target_or_body_is_rejected() {
        let store = store();
        store.create("req-1", draft("i1"), 10).expect("create");
        assert!(matches!(
            store.create("req-1", draft("i2"), 11),
            Err(InstancesError::IdempotencyConflict)
        ));
        let first = store
            .update(
                "update-1",
                "i1",
                Revision::new(1),
                &InstancePatch {
                    name: Some("first".into()),
                    ..Default::default()
                },
                12,
            )
            .expect("update");
        store
            .update(
                "update-2",
                "i1",
                Revision::new(2),
                &InstancePatch {
                    name: Some("second".into()),
                    ..Default::default()
                },
                13,
            )
            .expect("later update");
        let replay = store
            .update(
                "update-1",
                "i1",
                Revision::new(1),
                &InstancePatch {
                    name: Some("first".into()),
                    ..Default::default()
                },
                14,
            )
            .expect("replay original result");
        assert!(replay.replayed);
        assert_eq!(replay.instance, first.instance);
        assert!(matches!(
            store.update(
                "update-1",
                "i2",
                Revision::new(1),
                &InstancePatch {
                    name: Some("first".into()),
                    ..Default::default()
                },
                14
            ),
            Err(InstancesError::IdempotencyConflict)
        ));
    }

    #[test]
    fn update_requires_the_revision_the_caller_read() {
        let store = store();
        store.create("req-1", draft("i1"), 10).expect("create");
        let patch = InstancePatch {
            name: Some("renamed".into()),
            ..Default::default()
        };
        let stale = store.update("req-2", "i1", Revision::new(99), &patch, 12);
        assert!(matches!(
            stale,
            Err(InstancesError::RevisionStale {
                expected: 99,
                actual: 1
            })
        ));
        let ok = store
            .update("req-3", "i1", Revision::new(1), &patch, 13)
            .expect("update");
        assert_eq!(ok.instance.revision, 2);
        assert_eq!(ok.instance.name, "renamed");
    }

    #[test]
    fn an_empty_patch_does_not_bump_the_revision() {
        let store = store();
        store.create("req-1", draft("i1"), 10).expect("create");
        let outcome = store
            .update(
                "req-2",
                "i1",
                Revision::new(1),
                &InstancePatch::default(),
                11,
            )
            .expect("noop");
        assert_eq!(outcome.instance.revision, 1);
        assert!(outcome.replayed);
    }

    #[test]
    fn a_failed_write_rolls_back_so_the_revision_stays_honest() {
        let storage = MemoryStorage::default();
        *storage.fail_next_save.lock().expect("lock") = Some("io:index".into());
        let store = store_with(storage);
        let failed = store.create("req-1", draft("i1"), 10);
        assert!(matches!(failed, Err(InstancesError::IndexIo(_))));
        // Nothing was kept: a retry starts from a clean index.
        assert_eq!(store.list().expect("list").len(), 0);
    }

    #[test]
    fn a_live_lease_blocks_mutation_and_an_expired_one_does_not() {
        let store = store();
        store.create("req-1", draft("i1"), 0).expect("create");
        store
            .acquire_lease("i1", "task-1", "op-1", 0, 100)
            .expect("lease");
        let patch = InstancePatch {
            name: Some("x".into()),
            ..Default::default()
        };
        assert!(matches!(
            store.update("req-2", "i1", Revision::new(1), &patch, 50),
            Err(InstancesError::Running(_))
        ));
        // Past the deadline the lease no longer blocks, so a crash cannot lock a
        // user out of their own instance forever.
        let after = store
            .update("req-3", "i1", Revision::new(1), &patch, 200)
            .expect("after expiry");
        assert_eq!(after.instance.revision, 2);
    }

    #[test]
    fn releasing_someone_elses_lease_is_refused() {
        let store = store();
        store.create("req-1", draft("i1"), 0).expect("create");
        store
            .acquire_lease("i1", "task-1", "op-1", 0, 1_000)
            .expect("lease");
        assert!(matches!(
            store.release_lease("i1", "task-2"),
            Err(InstancesError::LeaseUnauthorized(_))
        ));
        assert!(store.release_lease("i1", "task-1").is_ok());
        assert!(store.release_lease("i1", "task-1").is_ok());
    }

    #[test]
    fn busy_instances_refuse_edits() {
        let store = store();
        store.create("req-1", draft("i1"), 0).expect("create");
        store.set_state("i1", InstanceState::Busy, 1).expect("busy");
        let patch = InstancePatch {
            name: Some("x".into()),
            ..Default::default()
        };
        assert!(matches!(
            store.update("req-2", "i1", Revision::new(2), &patch, 2),
            Err(InstancesError::Running(_))
        ));
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let store = store();
        store.create("req-1", draft("i1"), 0).expect("create");
        assert!(matches!(
            store.create("req-2", draft("i1"), 0),
            Err(InstancesError::Duplicate(_))
        ));
    }

    #[test]
    fn the_recovery_journal_records_then_confirms_a_commit() {
        let store = store();
        store.create("req-1", draft("i1"), 0).expect("create");
        let record = store.recovery_status("req-1").expect("record");
        assert!(record.committed);
        assert!(matches!(
            store.recovery_status("req-unknown"),
            Err(InstancesError::RecoveryUnknownOperation(_))
        ));
    }

    #[test]
    fn a_rejected_writable_location_does_not_create_the_instance() {
        let storage = MemoryStorage::default();
        *storage.reject_location.lock().expect("lock") = Some("/data/instances/i1".into());
        let store = store_with(storage);
        assert!(matches!(
            store.create("req-1", draft("i1"), 0),
            Err(InstancesError::LocationNotWritable(_))
        ));
        assert_eq!(store.list().expect("list").len(), 0);
    }

    #[test]
    fn removal_is_idempotent_and_bumps_the_revision_once() {
        let store = store();
        store.create("req-1", draft("i1"), 0).expect("create");
        let first = store
            .remove("req-del", "i1", Revision::new(1), 1)
            .expect("remove");
        let again = store
            .remove("req-del", "i1", Revision::new(1), 2)
            .expect("replay");
        assert_eq!(first, again);
        assert_eq!(store.list().expect("list").len(), 0);
    }

    #[test]
    fn file_storage_reloads_commits_and_replays_idempotent_requests() {
        let root = std::env::temp_dir().join(format!("shtmcl-01-reload-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store =
            InstanceStore::new(Box::new(FileStorage::new(&root).expect("storage"))).expect("store");
        let draft = draft_under("persisted", &root);
        store.create("create-1", draft.clone(), 10).expect("create");
        let patch = InstancePatch {
            name: Some("saved".into()),
            ..Default::default()
        };
        store
            .update("update-1", "persisted", Revision::new(1), &patch, 11)
            .expect("update");
        drop(store);

        let reopened = InstanceStore::new(Box::new(FileStorage::new(&root).expect("storage")))
            .expect("reload");
        assert_eq!(reopened.get("persisted").expect("instance").name, "saved");
        assert!(matches!(
            reopened.remove("stale-remove", "persisted", Revision::new(1), 12),
            Err(InstancesError::RevisionStale {
                expected: 1,
                actual: 2
            })
        ));
        assert_eq!(reopened.get("persisted").expect("preserved").name, "saved");
        let replay = reopened
            .update("update-1", "persisted", Revision::new(1), &patch, 12)
            .expect("replay");
        assert!(replay.replayed);
        assert_eq!(replay.instance.revision, 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_storage_replays_index_commit_after_crash_before_journal_confirmation() {
        let root = std::env::temp_dir().join(format!("shtmcl-01-wal-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let storage = FileStorage::new(&root).expect("storage");
        let committed = Instance {
            state: InstanceState::Ready,
            revision: 1,
            created_at: 7,
            updated_at: 7,
            ..draft("replayed")
        };
        storage
            .save_index(std::slice::from_ref(&committed))
            .expect("index");
        let record = RecoveryRecord {
            request_id: "create-crash".into(),
            operation_id: "op-crash".into(),
            instance_id: "replayed".into(),
            expected_revision: 1,
            committed: false,
            before: None,
            after: Some(committed),
            fingerprint: "digest".into(),
            aborted: false,
            import_target: None,
        };
        storage.write_recovery(&record).expect("intent");

        drop(storage);
        let store = InstanceStore::new(Box::new(FileStorage::new(&root).expect("reopen")))
            .expect("recovery");
        assert_eq!(store.get("replayed").expect("instance").revision, 1);
        assert!(
            store
                .recovery_status("create-crash")
                .expect("journal")
                .committed
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_storage_rolls_back_an_unpublished_intent_during_restart() {
        let root = std::env::temp_dir().join(format!("shtmcl-01-rollback-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let storage = FileStorage::new(&root).expect("storage");
        let after = Instance {
            state: InstanceState::Ready,
            revision: 1,
            created_at: 7,
            updated_at: 7,
            ..draft("not-published")
        };
        storage
            .write_recovery(&RecoveryRecord {
                request_id: "create-unpublished".into(),
                operation_id: "op-unpublished".into(),
                instance_id: "not-published".into(),
                expected_revision: 1,
                committed: false,
                before: None,
                after: Some(after),
                fingerprint: "digest".into(),
                aborted: false,
                import_target: None,
            })
            .expect("intent");
        drop(storage);
        let store = InstanceStore::new(Box::new(FileStorage::new(&root).expect("reopen")))
            .expect("rollback");
        assert!(matches!(
            store.get("not-published"),
            Err(InstancesError::NotFound(_))
        ));
        assert!(
            store
                .recovery_status("op-unpublished")
                .expect("recovery")
                .aborted
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_storage_recovers_an_interrupted_published_import_by_its_marker() {
        let root =
            std::env::temp_dir().join(format!("shtmcl-01-import-crash-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("source");
        let target = root.join("managed/instance-1");
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("options.txt"), b"contents").expect("source file");
        let storage = FileStorage::new(root.join("index")).expect("storage");
        storage
            .import_directory_cancellable(
                &source.to_string_lossy(),
                &target.to_string_lossy(),
                "import-key",
                &|| false,
            )
            .expect("staged publish");
        let after = Instance {
            state: InstanceState::Ready,
            revision: 1,
            created_at: 3,
            updated_at: 3,
            ..draft_under("interrupted", &root)
        };
        storage
            .write_recovery(&RecoveryRecord {
                request_id: "import-key".into(),
                operation_id: "op-import-crash".into(),
                instance_id: after.id.clone(),
                expected_revision: 1,
                committed: false,
                before: None,
                after: Some(after),
                fingerprint: "digest".into(),
                aborted: false,
                import_target: Some(target.to_string_lossy().into_owned()),
            })
            .expect("intent");

        drop(storage);
        let recovered = InstanceStore::new(Box::new(
            FileStorage::new(root.join("index")).expect("reopen"),
        ))
        .expect("recovery");
        assert!(
            !target.exists(),
            "only the directory bearing this import marker is removed"
        );
        assert!(
            recovered
                .recovery_status("op-import-crash")
                .expect("record")
                .aborted
        );
        assert!(source.join("options.txt").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_storage_copies_imports_to_a_separate_managed_directory() {
        let root = std::env::temp_dir().join(format!("shtmcl-01-import-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("external");
        let target = root.join("managed/instance-1");
        fs::create_dir_all(source.join("saves/world")).expect("source tree");
        fs::write(source.join("saves/world/level.dat"), b"world-data").expect("source file");
        let mut imported = draft("import-1");
        imported.location.root = target.to_string_lossy().into_owned();
        let store = InstanceStore::new(Box::new(
            FileStorage::new(root.join("index")).expect("storage"),
        ))
        .expect("store");

        let result = store
            .import_directory(
                "import-request",
                "op-import",
                &source.to_string_lossy(),
                imported,
                50,
                &|| false,
            )
            .expect("import");
        assert_eq!(
            fs::read(target.join("saves/world/level.dat")).expect("target bytes"),
            b"world-data"
        );
        assert_eq!(
            fs::read(source.join("saves/world/level.dat")).expect("source bytes"),
            b"world-data"
        );
        assert!(!target.join(IMPORT_MARKER).exists());
        assert!(
            store
                .recovery_status("op-import")
                .expect("recovery")
                .committed
        );
        assert_eq!(result.instance.id, "import-1");
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_storage_cancels_copy_at_a_chunk_boundary_and_removes_staging() {
        let root = std::env::temp_dir().join(format!("shtmcl-01-cancel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("source");
        let target = root.join("managed/instance-1");
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("large.bin"), vec![7u8; 2 * 1024 * 1024]).expect("large file");
        let storage = FileStorage::new(root.join("index")).expect("storage");
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let cancelled = || checks.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 4;
        assert!(storage
            .import_directory_cancellable(
                &source.to_string_lossy(),
                &target.to_string_lossy(),
                "cancel-token",
                &cancelled
            )
            .is_err());
        assert!(source.join("large.bin").exists());
        assert!(!target.exists());
        assert!(!import_stage(&target).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn release_lease_write_failure_keeps_busy_state_and_lease() {
        let storage = MemoryStorage::default();
        let fail_next_save = storage.fail_next_save.clone();
        let store = store_with(storage);
        store.create("req-1", draft("lease-1"), 0).expect("create");
        store
            .acquire_lease("lease-1", "task", "op", 1, 100)
            .expect("lease");
        *fail_next_save.lock().expect("fail lock") = Some("state unavailable".into());
        assert!(matches!(
            store.release_lease("lease-1", "task"),
            Err(InstancesError::IndexIo(_))
        ));
        assert_eq!(
            store.get("lease-1").expect("instance").state,
            InstanceState::Busy
        );
        assert!(matches!(
            store.update(
                "edit",
                "lease-1",
                Revision::new(1),
                &InstancePatch {
                    name: Some("x".into()),
                    ..Default::default()
                },
                2
            ),
            Err(InstancesError::Running(_))
        ));
    }

    #[test]
    fn real_index_write_failure_is_rolled_back_on_restart() {
        let root =
            std::env::temp_dir().join(format!("shtmcl-01-index-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let storage = FaultStorage::new(&root);
        let fail_state = storage.fail_state.clone();
        let store = InstanceStore::new(Box::new(storage)).expect("store");
        fail_state.store(true, Ordering::SeqCst);
        assert!(matches!(
            store.create("failed-create", draft_under("failed", &root), 1),
            Err(InstancesError::IndexIo(_))
        ));
        drop(store);
        let reopened =
            InstanceStore::new(Box::new(FileStorage::new(&root).expect("reopen storage")))
                .expect("reopen");
        assert!(reopened.list().expect("list").is_empty());
        assert!(
            reopened
                .recovery_status("failed-create")
                .expect("recovery")
                .aborted
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn real_journal_confirmation_failure_replays_from_durable_index() {
        let root =
            std::env::temp_dir().join(format!("shtmcl-01-confirm-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let storage = FaultStorage::new(&root);
        let fail_complete = storage.fail_complete.clone();
        let store = InstanceStore::new(Box::new(storage)).expect("store");
        fail_complete.store(true, Ordering::SeqCst);
        store
            .create("confirmed-by-replay", draft_under("replayed", &root), 2)
            .expect("durable index is accepted");
        drop(store);
        let reopened =
            InstanceStore::new(Box::new(FileStorage::new(&root).expect("reopen storage")))
                .expect("replay");
        assert!(
            reopened
                .recovery_status("confirmed-by-replay")
                .expect("record")
                .committed
        );
        assert_eq!(reopened.list().expect("list").len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn real_lease_release_failure_preserves_lock_until_restart_recovery() {
        let root =
            std::env::temp_dir().join(format!("shtmcl-01-lease-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let storage = FaultStorage::new(&root);
        let fail_state = storage.fail_state.clone();
        let store = InstanceStore::new(Box::new(storage)).expect("store");
        store
            .create("create", draft_under("leased", &root), 1)
            .expect("create");
        store
            .acquire_lease("leased", "task", "operation", 2, 60_000)
            .expect("lease");
        fail_state.store(true, Ordering::SeqCst);
        assert!(matches!(
            store.release_lease("leased", "task"),
            Err(InstancesError::IndexIo(_))
        ));
        assert_eq!(
            store.get("leased").expect("locked").state,
            InstanceState::Busy
        );
        drop(store);
        let reopened =
            InstanceStore::new(Box::new(FileStorage::new(&root).expect("reopen storage")))
                .expect("recovery");
        assert_eq!(
            reopened.get("leased").expect("recovered").state,
            InstanceState::Ready
        );
        let _ = fs::remove_dir_all(root);
    }
}

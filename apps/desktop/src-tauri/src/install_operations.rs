//! Immutable user-facing installation identity. TaskService remains the sole
//! owner of execution state; a persisted record does not prove installation success.
use crate::game_install::{checked_directory, reject_link};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shtmcl_domain::{OperationId, TaskId};
use shtmcl_platform::PlatformDirs;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

const MAX_RECORD_BYTES: u64 = 16 * 1024;
const MAX_RECORDS: usize = 2048;
#[path = "install_operation_recovery.rs"]
mod recovery;
#[cfg(test)]
#[path = "install_recovery_tests.rs"]
mod recovery_tests;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InstallOperation {
    operation_id: OperationId,
    task_id: TaskId,
    name: String,
    version: String,
    resource_type: String,
    destination: InstanceDestination,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovery: Option<InstallSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry_of: Option<RecoverySource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RecoverySource {
    operation_id: OperationId,
    task_id: TaskId,
}

pub(crate) fn recovery_source(payload: &serde_json::Value) -> Result<Option<RecoverySource>, shtmcl_domain::ErrorEnvelope> {
    payload.get("recoverySource").map(|raw| serde_json::from_value(raw.clone()))
        .transpose().map_err(|_| shtmcl_core::launch::errors::request_invalid("recoverySource"))
}

/// Validated installation inputs. Old records deliberately have no inferred loader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InstallSelection {
    pub(crate) loader: String,
    pub(crate) loader_version: Option<String>,
    pub(crate) environment_id: Option<String>,
}

impl InstallSelection {
    fn validate(&self) -> bool {
        let valid_version = |value: &Option<String>| value.as_ref().is_some_and(|v|
            v.len() <= 128 && shtmcl_core::content::fabric::valid_version(v));
        let loader_valid = match self.loader.as_str() {
            "vanilla" => self.loader_version.is_none(),
            "fabric" | "neoforge" => valid_version(&self.loader_version),
            _ => false,
        };
        loader_valid && self.environment_id.as_ref().is_none_or(|id|
            self.loader != "neoforge" && id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InstanceDestination {
    kind: String,
    path: PathBuf,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    instance_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    instance_name: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRecord {
    schema_version: u32,
    entry: InstallOperation,
}

impl InstallOperation {
    pub(crate) fn with_source(mut self, source: Option<RecoverySource>) -> Self {
        self.retry_of = source;
        self
    }
    pub(crate) fn with_recovery(mut self, input: InstallSelection) -> Self {
        self.recovery = Some(input);
        self
    }
    pub(crate) fn environment(
        operation_id: OperationId,
        task_id: TaskId,
        version: &str,
        dirs: &PlatformDirs,
    ) -> Self {
        Self {
            operation_id,
            task_id,
            name: "Minecraft".into(),
            version: version.into(),
            resource_type: "game_environment".into(),
            recovery: None,
            retry_of: None,
            metadata_sha256: None,
            destination: InstanceDestination {
                kind: "cache".into(),
                path: dirs.cache.join("environments/v1"),
                instance_id: String::new(),
                instance_name: String::new(),
            },
        }
    }
    pub(crate) fn instance(
        operation_id: OperationId,
        task_id: TaskId,
        version: &str,
        instance: &str,
        name: &str,
        dirs: &PlatformDirs,
    ) -> Self {
        Self {
            operation_id,
            task_id,
            name: "Minecraft".into(),
            version: version.into(),
            resource_type: "game_environment".into(),
            recovery: None,
            retry_of: None,
            metadata_sha256: None,
            destination: InstanceDestination {
                kind: "instance".into(),
                path: dirs.instances.join(instance),
                instance_id: instance.into(),
                instance_name: name.into(),
            },
        }
    }

    fn key(&self) -> String {
        // Length-delimited pair: neither restart-reused task IDs nor repeated
        // operations can alias a different task's presentation record.
        let mut hash = Sha256::new();
        for value in [self.operation_id.as_str(), self.task_id.as_str()] {
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        format!("{:x}.json", hash.finalize())
    }

    fn validate(&self, dirs: &PlatformDirs) -> Result<(), String> {
        if self.metadata_sha256.as_ref().is_some_and(|hash|
            hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Err("invalid_metadata_identity".into());
        }
        if self.recovery.as_ref().is_some_and(|input|
            self.destination.kind != "instance" || !input.validate()) {
            return Err("invalid_installation_recovery".into());
        }
        let dest = &self.destination;
        let safe_text = |text: &str, limit: usize| {
            !text.is_empty() && text.chars().count() <= limit && !text.chars().any(char::is_control)
        };
        if !safe_text(&self.name, 200)
            || !safe_text(&self.version, 128)
            || !shtmcl_core::launch::install::safe_path(&self.version)
            || self.version.contains('/')
            || self.resource_type != "game_environment"
            || !matches!(dest.kind.as_str(), "instance" | "cache")
        {
            return Err("invalid_installation_record".into());
        }
        if dest.kind == "cache" {
            return if dest.path == dirs.cache.join("environments/v1")
                && dest.instance_id.is_empty()
                && dest.instance_name.is_empty()
            {
                Ok(())
            } else {
                Err("invalid_installation_record".into())
            };
        }
        if dest.kind != "instance"
            || !safe_text(&dest.instance_name, 80)
            || dest.instance_id.is_empty()
            || dest.instance_id.len() > 64
            || !dest
                .instance_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || !shtmcl_core::launch::install::safe_path(&dest.instance_id)
            || dest.path != dirs.instances.join(&dest.instance_id)
        {
            return Err("invalid_installation_record".into());
        }
        Ok(())
    }
}

pub(crate) struct InstallOperations {
    dirs: PlatformDirs,
    root: PathBuf,
}

/// Separate lease file: locking the JSON itself would block Windows readers.
pub(crate) struct OperationLease {
    file: Option<File>,
    path: PathBuf,
}
impl Drop for OperationLease {
    fn drop(&mut self) {
        drop(self.file.take());
        // Retain the sidecar inode: unlinking after unlock races another
        // process acquiring it. Retention removes it under the store lock.
    }
}

impl InstallOperations {
    pub(crate) fn new(dirs: &PlatformDirs) -> Self {
        Self {
            dirs: dirs.clone(),
            root: dirs.data.join("install-operations/v1"),
        }
    }

    fn lock(&self) -> Result<File, String> {
        if !self.root.is_absolute()
            || self
                .root
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            return Err("unsafe_installation_record_root".into());
        }
        checked_directory(&self.root)?;
        let path = self.root.join(".lock");
        reject_link(&path)?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|_| "installation_records_lock")?;
        lock.try_lock().map_err(|_| "installation_records_busy")?;
        Ok(lock)
    }

    fn files(&self) -> Result<Vec<PathBuf>, String> {
        let mut files = Vec::new();
        // Bound every directory entry, including abandoned temporary files.
        for (index, entry) in std::fs::read_dir(&self.root)
            .map_err(|_| "installation_records_read")?
            .enumerate()
        {
            if index > MAX_RECORDS * 4 {
                return Err("installation_records_limit".into());
            }
            let entry = entry.map_err(|_| "installation_records_read")?;
            let name = entry.file_name();
            let name = name.to_str().ok_or("installation_record_filename")?;
            if name == ".lock" || name.ends_with(".tmp") || name.ends_with(".lease") || name.ends_with(".metadata") {
                continue;
            }
            if name.len() != 69
                || !name.ends_with(".json")
                || !name.as_bytes()[..64].iter().all(|b| b.is_ascii_hexdigit())
            {
                return Err("installation_record_filename".into());
            }
            files.push(entry.path());
            if files.len() > MAX_RECORDS {
                return Err("installation_records_limit".into());
            }
        }
        files.sort();
        Ok(files)
    }

    fn read(&self, path: &Path) -> Result<InstallOperation, String> {
        reject_link(path)?;
        let file = File::open(path).map_err(|_| "installation_record_read")?;
        let metadata = file
            .metadata()
            .map_err(|_| "installation_record_metadata")?;
        if !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES {
            return Err("installation_record_size".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "installation_record_read")?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err("installation_record_size".into());
        }
        let mut record: StoredRecord =
            serde_json::from_slice(&bytes).map_err(|_| "installation_record_json")?;
        if record.schema_version != 1 {
            return Err("installation_record_schema".into());
        }
        // Persist a root-relative location so a portable installation remains
        // usable after moving the EXE and its adjacent data directory together.
        let relative = if record.entry.destination.kind == "cache" {
            PathBuf::from("cache/environments/v1")
        } else {
            Path::new("instances").join(&record.entry.destination.instance_id)
        };
        if record.entry.destination.path != relative {
            return Err("installation_record_destination".into());
        }
        record.entry.destination.path = if record.entry.destination.kind == "cache" {
            self.dirs.cache.join("environments/v1")
        } else {
            self.dirs
                .instances
                .join(&record.entry.destination.instance_id)
        };
        record.entry.validate(&self.dirs)?;
        if path.file_name() != Some(std::ffi::OsStr::new(&record.entry.key())) {
            return Err("installation_record_identity".into());
        }
        Ok(record.entry)
    }

    pub(crate) fn list(&self) -> Result<Vec<InstallOperation>, String> {
        let _lock = self.lock()?;
        self.files()?
            .into_iter()
            .map(|path| self.read(&path))
            .collect()
    }

    pub(crate) fn presentations(
        &self,
        tasks: &[shtmcl_domain::Task],
    ) -> Result<Vec<serde_json::Value>, String> {
        let history = crate::operation_history::OperationHistory::new(&self.dirs).list()?;
        let history: std::collections::HashMap<_, _> = history
            .into_iter()
            .map(|item| ((item.operation_id.clone(), item.task_id.clone()), item))
            .collect();
        let live: std::collections::HashSet<_> = tasks
            .iter()
            .map(|task| (task.operation_id.clone(), task.id.clone()))
            .collect();
        self.list()?.into_iter().map(|entry| {
            let mut row = serde_json::to_value(&entry).map_err(|_| "installation_record_json")?;
            row.as_object_mut().unwrap().remove("recovery");
            row.as_object_mut().unwrap().remove("metadataSha256");
            row["canRetryInstall"] = serde_json::json!(entry.recovery.is_some());
            if !live.contains(&(entry.operation_id.clone(), entry.task_id.clone())) {
                if let Some(snapshot) = history.get(&(entry.operation_id.clone(), entry.task_id.clone())) {
                    let mut snapshot = snapshot.clone();
                    let mut inactive_lease = None;
                    let active = if snapshot.state.is_terminal() { false } else {
                        match self.lease(&self.root.join(entry.key())) {
                            Ok(lease) => { inactive_lease = Some(lease);
                                if let Some(current) = crate::operation_history::OperationHistory::new(&self.dirs).lookup(entry.operation_id.as_str(), entry.task_id.as_str())? { snapshot = current; }
                                false
                            },
                            Err(error) if error == "installation_record_active" => true,
                            Err(error) => return Err(error),
                        }
                    };
                    row["history"] = serde_json::json!({"state":snapshot.state,"phase":snapshot.phase,"attemptGeneration":snapshot.attempt_generation,
                        "interrupted": !snapshot.state.is_terminal() && !active,"activeElsewhere":active});
                    if let Some(code) = &snapshot.error_code { row["history"]["errorCode"] = serde_json::json!(code); }
                    drop(inactive_lease);
                }
            }
            Ok(row)
        }).collect()
    }

    pub(crate) fn destination(&self, operation: &str, task: &str) -> Result<PathBuf, String> {
        let entry = self
            .list()?
            .into_iter()
            .find(|entry| {
                entry.operation_id.as_str() == operation && entry.task_id.as_str() == task
            })
            .ok_or("installation_record_not_found")?;
        let path = entry.destination.path;
        for ancestor in path.ancestors() {
            reject_link(ancestor)?;
        }
        if !path.is_dir() {
            return Err("installation_destination_missing".into());
        }
        let actual = path
            .canonicalize()
            .map_err(|_| "installation_destination_read")?;
        let root = if entry.destination.kind == "cache" {
            &self.dirs.cache
        } else {
            &self.dirs.instances
        }
        .canonicalize()
        .map_err(|_| "installation_destination_read")?;
        if !actual.starts_with(root) {
            return Err("installation_destination_outside".into());
        }
        // Explorer accepts the normal absolute path; canonical Windows paths
        // can carry a verbatim prefix intended for filesystem APIs instead.
        Ok(path)
    }

    fn lease(&self, target: &Path) -> Result<OperationLease, String> {
        let path = target.with_extension("lease");
        reject_link(&path)?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|_| "installation_record_lease")?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => "installation_record_active",
            _ => "installation_record_lease",
        })?;
        Ok(OperationLease {
            file: Some(file),
            path,
        })
    }

    fn retain_room(&self, mut files: Vec<PathBuf>, limit: usize) -> Result<(), String> {
        files.sort_by_key(|path| {
            std::fs::symlink_metadata(path)
                .and_then(|m| m.modified())
                .ok()
        });
        let mut count = files.len();
        for path in files {
            if count < limit {
                return Ok(());
            }
            self.read(&path)?;
            let Ok(lease) = self.lease(&path) else {
                continue;
            };
            let metadata = path.with_extension("metadata");
            reject_link(&metadata)?;
            if metadata.exists() { std::fs::remove_file(metadata).map_err(|_| "installation_record_retention")?; }
            // An active worker in any process holds this lease. JSON readers
            // remain independent, and crashed workers release their OS lock.
            std::fs::remove_file(&path).map_err(|_| "installation_record_retention")?;
            let lease_path = lease.path.clone();
            drop(lease);
            std::fs::remove_file(lease_path).map_err(|_| "installation_record_retention")?;
            count -= 1;
        }
        if count < limit {
            Ok(())
        } else {
            Err("installation_records_all_active".into())
        }
    }

    pub(crate) fn insert(&self, entry: &InstallOperation) -> Result<OperationLease, String> {
        self.insert_with_limit(entry, MAX_RECORDS)
    }

    fn insert_with_limit(
        &self,
        entry: &InstallOperation,
        limit: usize,
    ) -> Result<OperationLease, String> {
        entry.validate(&self.dirs)?;
        let _lock = self.lock()?;
        let files = self.files()?;
        let target = self.root.join(entry.key());
        reject_link(&target)?;
        if target.exists() {
            return if self.read(&target)? == *entry {
                self.lease(&target)
            } else {
                Err("installation_record_conflict".into())
            };
        }
        self.retain_room(files, limit)?;
        let mut stored_entry = entry.clone();
        stored_entry.destination.path = if entry.destination.kind == "cache" {
            PathBuf::from("cache/environments/v1")
        } else {
            Path::new("instances").join(&entry.destination.instance_id)
        };
        let bytes = serde_json::to_vec(&StoredRecord {
            schema_version: 1,
            entry: stored_entry,
        })
        .map_err(|_| "installation_record_json")?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err("installation_record_size".into());
        }
        let temp = self.root.join(format!(
            "{}.tmp",
            shtmcl_platform::entropy::random_hex(16).map_err(|_| "installation_record_entropy")?
        ));
        let result: Result<(), String> = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|_| "installation_record_write")?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| "installation_record_write")?;
            drop(file);
            std::fs::rename(&temp, &target).map_err(|_| "installation_record_commit")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result?;
        // Keep history retention aligned while the identity lock excludes new
        // insertions. Active identities were protected by retain_room's leases.
        let retained = self
            .files()?
            .into_iter()
            .map(|path| {
                self.read(&path)
                    .map(|entry| (entry.operation_id, entry.task_id))
            })
            .collect::<Result<std::collections::HashSet<_>, _>>()?;
        crate::operation_history::OperationHistory::new(&self.dirs).retain_identities(&retained)?;
        self.lease(&target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PlatformDirs);
    impl Temp {
        fn new() -> Self {
            Self(PlatformDirs::under(std::env::temp_dir().join(format!(
                "shtmcl-operations-{}",
                shtmcl_platform::entropy::random_hex(12).unwrap()
            ))))
        }
        fn entry(&self, op: &str, task: &str) -> InstallOperation {
            InstallOperation::instance(
                OperationId::new(op).unwrap(),
                TaskId::new(task).unwrap(),
                "1.7.10",
                "school",
                "校园生存",
                &self.0,
            )
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.root);
        }
    }

    #[test]
    fn restart_restores_identity_and_destination_without_execution_state() {
        let temp = Temp::new();
        let entry = temp.entry("operation-one", "task-one");
        InstallOperations::new(&temp.0).insert(&entry).unwrap();
        let restored = InstallOperations::new(&temp.0).list().unwrap();
        assert_eq!(restored, vec![entry]);
        let json = serde_json::to_value(&restored[0]).unwrap();
        assert_eq!(json["destination"]["instanceName"], "校园生存");
        assert!(json.get("state").is_none());
    }

    #[test]
    fn history_projection_distinguishes_live_owner_interruption_and_actual_success() {
        use shtmcl_core::{tasks::TaskSpec, CoreApp};
        use shtmcl_domain::{TaskPhase, TaskState};
        let temp = Temp::new();
        let app = CoreApp::new(temp.0.clone());
        let ctx = app.context();
        let task = ctx
            .tasks()
            .accept(TaskSpec::new(
                OperationId::new("projected-history").unwrap(),
                TaskPhase::new("install").unwrap(),
            ))
            .unwrap();
        let record = InstallOperation::environment(
            task.operation_id.clone(),
            task.id.clone(),
            "1.7.10",
            &temp.0,
        );
        let store = InstallOperations::new(&temp.0);
        let lease = store.insert(&record).unwrap();
        crate::operation_history::OperationHistory::new(&temp.0)
            .record(&task)
            .unwrap();
        let active = store.presentations(&[]).unwrap();
        assert_eq!(active[0]["history"]["activeElsewhere"], true);
        assert!(active[0]["destination"].get("instanceId").is_none());
        assert!(store.presentations(&[task.clone()]).unwrap()[0]
            .get("history")
            .is_none());
        drop(lease);
        let interrupted = store.presentations(&[]).unwrap();
        assert_eq!(interrupted[0]["history"]["interrupted"], true);
        ctx.tasks()
            .transition(&task.id, task.attempt_generation, TaskState::Running)
            .unwrap();
        ctx.tasks()
            .succeed(&task.id, task.attempt_generation, None)
            .unwrap();
        crate::game_install::record_execution(&ctx, &task.id).unwrap();
        let done = store.presentations(&[]).unwrap();
        assert_eq!(done[0]["history"]["state"], "succeeded");
        assert_eq!(done[0]["history"]["interrupted"], false);
    }

    #[test]
    fn identity_retention_also_releases_orphan_interrupted_history() {
        use shtmcl_core::{tasks::TaskSpec, CoreApp};
        use shtmcl_domain::TaskPhase;
        let temp = Temp::new();
        let app = CoreApp::new(temp.0.clone());
        let ctx = app.context();
        let task = ctx
            .tasks()
            .accept(TaskSpec::new(
                OperationId::new("orphan-history").unwrap(),
                TaskPhase::new("install").unwrap(),
            ))
            .unwrap();
        let entry = InstallOperation::environment(
            task.operation_id.clone(),
            task.id.clone(),
            "1.7.10",
            &temp.0,
        );
        let store = InstallOperations::new(&temp.0);
        drop(store.insert_with_limit(&entry, 1).unwrap());
        crate::operation_history::OperationHistory::new(&temp.0)
            .record(&task)
            .unwrap();
        let next = temp.entry("next-history", "next-task");
        drop(store.insert_with_limit(&next, 1).unwrap());
        assert!(crate::operation_history::OperationHistory::new(&temp.0)
            .lookup(task.operation_id.as_str(), task.id.as_str())
            .unwrap()
            .is_none());
    }

    #[test]
    fn destination_requires_matching_identity_and_existing_instance_directory() {
        let temp = Temp::new();
        let entry = temp.entry("operation-open", "task-open");
        let store = InstallOperations::new(&temp.0);
        store.insert(&entry).unwrap();
        assert!(store.destination("operation-open", "task-other").is_err());
        assert!(store.destination("operation-other", "task-open").is_err());
        assert!(store.destination("operation-open", "task-open").is_err());
        std::fs::create_dir_all(temp.0.instances.join("school")).unwrap();
        assert_eq!(
            store.destination("operation-open", "task-open").unwrap(),
            temp.0.instances.join("school")
        );
    }

    #[test]
    fn double_identity_is_immutable_and_restart_task_reuse_is_distinct() {
        let temp = Temp::new();
        let store = InstallOperations::new(&temp.0);
        let first = temp.entry("operation-one", "task-one");
        store.insert(&first).unwrap();
        store.insert(&first).unwrap();
        let mut conflict = first.clone();
        conflict.destination.instance_name = "changed".into();
        assert!(store.insert(&conflict).is_err());
        store
            .insert(&temp.entry("operation-two", "task-one"))
            .unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn corrupted_and_oversized_records_fail_without_mutating_them() {
        let temp = Temp::new();
        let store = InstallOperations::new(&temp.0);
        let entry = temp.entry("operation-one", "task-one");
        store.insert(&entry).unwrap();
        let path = store.root.join(entry.key());
        std::fs::write(&path, b"{broken").unwrap();
        assert!(store.list().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
        std::fs::write(&path, vec![b' '; MAX_RECORD_BYTES as usize + 1]).unwrap();
        assert!(store.list().is_err());
    }

    #[test]
    fn destination_escape_and_parent_file_are_rejected() {
        let temp = Temp::new();
        let store = InstallOperations::new(&temp.0);
        let mut entry = temp.entry("operation-one", "task-one");
        entry.destination.path = temp.0.root.join("outside");
        assert!(store.insert(&entry).is_err());
        checked_directory(&temp.0.data).unwrap();
        std::fs::write(temp.0.data.join("install-operations"), b"preserve").unwrap();
        assert!(store.list().is_err());
        assert_eq!(
            std::fs::read(temp.0.data.join("install-operations")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn unexpected_filenames_are_not_used_as_paths() {
        let temp = Temp::new();
        let store = InstallOperations::new(&temp.0);
        store.list().unwrap();
        std::fs::write(store.root.join("foreign.json"), b"{}").unwrap();
        assert!(store.list().is_err());
    }

    #[test]
    fn portable_directory_move_reprojects_destination() {
        let original = Temp::new();
        let moved = Temp::new();
        let entry = original.entry("operation-one", "task-one");
        InstallOperations::new(&original.0).insert(&entry).unwrap();
        std::fs::rename(&original.0.root, &moved.0.root).unwrap();
        let entries = InstallOperations::new(&moved.0).list().unwrap();
        assert_eq!(
            entries[0].destination.path,
            moved.0.instances.join("school")
        );
        assert_eq!(entries[0].destination.instance_name, "校园生存");
    }

    #[test]
    fn unicode_filenames_are_rejected_without_panicking() {
        let temp = Temp::new();
        let store = InstallOperations::new(&temp.0);
        store.list().unwrap();
        let name = format!("{}字a.json", "a".repeat(60));
        assert_eq!(name.len(), 69);
        std::fs::write(store.root.join(name), b"{}").unwrap();
        assert!(store.list().is_err());
    }

    #[test]
    fn bounded_retention_keeps_active_records_and_recovers_after_unlock() {
        let temp = Temp::new();
        let store = InstallOperations::new(&temp.0);
        let first = temp.entry("operation-one", "task-one");
        let second = temp.entry("operation-two", "task-two");
        let third = temp.entry("operation-three", "task-three");
        let first_lease = store.insert_with_limit(&first, 2).unwrap();
        let second_lease = store.insert_with_limit(&second, 2).unwrap();
        // Windows must be able to read while separate lease files are locked.
        assert_eq!(store.list().unwrap().len(), 2);
        assert!(store.insert_with_limit(&third, 2).is_err());
        drop(second_lease);
        let third_lease = store.insert_with_limit(&third, 2).unwrap();
        let entries = store.list().unwrap();
        assert!(entries.contains(&first));
        assert!(entries.contains(&third));
        assert!(!entries.contains(&second));
        assert!(!store
            .root
            .join(second.key())
            .with_extension("lease")
            .exists());
        drop(first_lease);
        drop(third_lease);
    }

    #[test]
    fn retention_removes_oldest_inactive_record_by_file_time() {
        let temp = Temp::new();
        let store = InstallOperations::new(&temp.0);
        let oldest = temp.entry("operation-oldest", "task-oldest");
        let newest = temp.entry("operation-newest", "task-newest");
        let incoming = temp.entry("operation-incoming", "task-incoming");
        drop(store.insert_with_limit(&oldest, 2).unwrap());
        drop(store.insert_with_limit(&newest, 2).unwrap());
        for (entry, seconds) in [(&oldest, 1_000_000), (&newest, 2_000_000)] {
            let file = OpenOptions::new()
                .write(true)
                .open(store.root.join(entry.key()))
                .unwrap();
            let modified =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds);
            file.set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
        }
        drop(store.insert_with_limit(&incoming, 2).unwrap());
        let entries = store.list().unwrap();
        assert!(!entries.contains(&oldest));
        assert!(entries.contains(&newest));
        assert!(entries.contains(&incoming));
    }
}

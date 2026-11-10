//! Bounded execution snapshots, separate from installation identity and task results.
//! Nonterminal snapshots remain nonterminal after restart: callers combine these
//! observations with live tasks and operation leases to describe interruption.
use crate::game_install::{checked_directory, reject_link};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shtmcl_domain::{
    AttemptGeneration, ErrorCode, OperationId, Task, TaskId, TaskPhase, TaskState,
};
use shtmcl_platform::PlatformDirs;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

const MAX_RECORDS: usize = 2048;
const MAX_BYTES: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OperationSnapshot {
    pub(crate) operation_id: OperationId,
    pub(crate) task_id: TaskId,
    pub(crate) attempt_generation: AttemptGeneration,
    pub(crate) state: TaskState,
    pub(crate) phase: TaskPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error_code: Option<ErrorCode>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record {
    schema_version: u32,
    snapshot: OperationSnapshot,
}

fn key(operation: &str, task: &str) -> String {
    let mut hash = Sha256::new();
    for value in [operation, task] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    format!("{:x}.json", hash.finalize())
}

pub(crate) struct OperationHistory {
    root: PathBuf,
}

impl OperationHistory {
    pub(crate) fn new(dirs: &PlatformDirs) -> Self {
        Self {
            root: dirs.data.join("operation-history/v1"),
        }
    }

    fn lock(&self) -> Result<File, String> {
        if !self.root.is_absolute()
            || self
                .root
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            return Err("history_unsafe_root".into());
        }
        checked_directory(&self.root)?;
        let path = self.root.join(".lock");
        reject_link(&path)?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|_| "history_lock")?;
        file.try_lock().map_err(|_| "history_busy")?;
        Ok(file)
    }

    fn files(&self) -> Result<Vec<PathBuf>, String> {
        let mut paths = Vec::new();
        for (index, entry) in std::fs::read_dir(&self.root)
            .map_err(|_| "history_read")?
            .enumerate()
        {
            if index > MAX_RECORDS * 3 {
                return Err("history_limit".into());
            }
            let entry = entry.map_err(|_| "history_read")?;
            let name = entry.file_name();
            let name = name.to_str().ok_or("history_filename")?;
            reject_link(&entry.path())?;
            if name == ".lock" || name.ends_with(".tmp") {
                continue;
            }
            if name.len() != 69
                || !name.ends_with(".json")
                || !name.as_bytes()[..64].iter().all(|b| b.is_ascii_hexdigit())
            {
                return Err("history_filename".into());
            }
            paths.push(entry.path());
            if paths.len() > MAX_RECORDS {
                return Err("history_limit".into());
            }
        }
        paths.sort();
        Ok(paths)
    }

    fn read(&self, path: &Path) -> Result<OperationSnapshot, String> {
        reject_link(path)?;
        let file = File::open(path).map_err(|_| "history_read")?;
        let metadata = file.metadata().map_err(|_| "history_metadata")?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err("history_size".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "history_read")?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("history_size".into());
        }
        let record: Record = serde_json::from_slice(&bytes).map_err(|_| "history_json")?;
        let snapshot = record.snapshot;
        if record.schema_version != 1
            || snapshot.attempt_generation.get() == 0
            || TaskPhase::new(snapshot.phase.as_str()).is_err()
        {
            return Err("history_schema".into());
        }
        if path.file_name()
            != Some(std::ffi::OsStr::new(&key(
                snapshot.operation_id.as_str(),
                snapshot.task_id.as_str(),
            )))
        {
            return Err("history_identity".into());
        }
        Ok(snapshot)
    }

    pub(crate) fn list(&self) -> Result<Vec<OperationSnapshot>, String> {
        let _lock = self.lock()?;
        self.files()?.into_iter().map(|p| self.read(&p)).collect()
    }

    pub(crate) fn lookup(
        &self,
        operation: &str,
        task: &str,
    ) -> Result<Option<OperationSnapshot>, String> {
        OperationId::new(operation).map_err(|_| "history_identity")?;
        TaskId::new(task).map_err(|_| "history_identity")?;
        let _lock = self.lock()?;
        let path = self.root.join(key(operation, task));
        reject_link(&path)?;
        if !path.exists() {
            return Ok(None);
        }
        self.read(&path).map(Some)
    }

    /// Remove execution snapshots whose immutable installation identity has
    /// been pruned. The caller must hold the identity-store lock throughout
    /// this call and supply its complete retained set. That store protects
    /// active leases from pruning; this method does not infer task outcomes.
    pub(crate) fn retain_identities(
        &self,
        identities: &std::collections::HashSet<(OperationId, TaskId)>,
    ) -> Result<(), String> {
        let _lock = self.lock()?;
        // Validate the complete bounded set before making any deletions.
        let records = self
            .files()?
            .into_iter()
            .map(|path| self.read(&path).map(|snapshot| (path, snapshot)))
            .collect::<Result<Vec<_>, _>>()?;
        for (path, snapshot) in records {
            if !identities.contains(&(snapshot.operation_id, snapshot.task_id)) {
                std::fs::remove_file(path).map_err(|_| "history_retention")?;
            }
        }
        Ok(())
    }

    pub(crate) fn record(&self, task: &Task) -> Result<(), String> {
        self.record_with_limit(task, MAX_RECORDS)
    }

    fn record_with_limit(&self, task: &Task, limit: usize) -> Result<(), String> {
        task.validate().map_err(|_| "history_task")?;
        let next = OperationSnapshot {
            operation_id: task.operation_id.clone(),
            task_id: task.id.clone(),
            attempt_generation: task.attempt_generation,
            state: task.state,
            phase: task.phase.clone(),
            error_code: task.error.as_ref().map(|e| e.code.clone()),
        };
        let _lock = self.lock()?;
        let target = self
            .root
            .join(key(task.operation_id.as_str(), task.id.as_str()));
        reject_link(&target)?;
        if target.exists() {
            let old = self.read(&target)?;
            if next == old {
                return Ok(());
            }
            if next.attempt_generation < old.attempt_generation {
                return Err("history_stale_generation".into());
            }
            if next.attempt_generation == old.attempt_generation {
                if old.state.is_terminal() {
                    return Err("history_terminal_immutable".into());
                }
                // Observers may miss intermediate running/paused states, so
                // do not re-run the scheduler transition machine here.
                if old.state != TaskState::Queued && next.state == TaskState::Queued {
                    return Err("history_state_regression".into());
                }
            }
        } else {
            let mut files = self.files()?;
            files.sort_by_key(|p| std::fs::symlink_metadata(p).and_then(|m| m.modified()).ok());
            let mut count = files.len();
            for path in files {
                if count < limit {
                    break;
                }
                if self.read(&path)?.state.is_terminal() {
                    std::fs::remove_file(path).map_err(|_| "history_retention")?;
                    count -= 1;
                }
            }
            if count >= limit {
                return Err("history_all_active".into());
            }
        }
        let bytes = serde_json::to_vec(&Record {
            schema_version: 1,
            snapshot: next,
        })
        .map_err(|_| "history_json")?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("history_size".into());
        }
        let temp = self.root.join(format!(
            "{}.tmp",
            shtmcl_platform::entropy::random_hex(16).map_err(|_| "history_entropy")?
        ));
        let result: Result<(), String> = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|_| "history_write")?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| "history_write")?;
            drop(file);
            // std::fs::rename replaces an existing regular target atomically
            // on Windows as well. Never unlink the previous committed snapshot.
            std::fs::rename(&temp, &target).map_err(|_| "history_commit")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PlatformDirs);
    impl Temp {
        fn new() -> Self {
            Self(PlatformDirs::under(std::env::temp_dir().join(format!(
                "shtmcl-history-{}",
                shtmcl_platform::entropy::random_hex(12).unwrap()
            ))))
        }
        fn store(&self) -> OperationHistory {
            OperationHistory::new(&self.0)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.root);
        }
    }
    fn task(op: &str, id: &str) -> Task {
        Task::new(
            TaskId::new(id).unwrap(),
            OperationId::new(op).unwrap(),
            AttemptGeneration::INITIAL,
        )
    }

    #[test]
    fn restart_preserves_only_observed_status_and_separates_identity_pairs() {
        let temp = Temp::new();
        let mut a = task("one", "same");
        a.state = TaskState::Running;
        a.result = Some(serde_json::json!({"secret": "do-not-store"}));
        temp.store().record(&a).unwrap();
        temp.store().record(&task("two", "same")).unwrap();
        let store = temp.store();
        assert_eq!(store.list().unwrap().len(), 2);
        assert_eq!(
            store.lookup("one", "same").unwrap().unwrap().state,
            TaskState::Running
        );
        assert!(store.lookup("one", "other").unwrap().is_none());
        let text = std::fs::read_to_string(store.root.join(key("one", "same"))).unwrap();
        assert!(!text.contains("secret"));
        assert!(!text.contains("result"));
        let moved = temp.0.root.with_extension("moved");
        std::fs::rename(&temp.0.root, &moved).unwrap();
        assert_eq!(
            OperationHistory::new(&PlatformDirs::under(&moved))
                .list()
                .unwrap()
                .len(),
            2
        );
        std::fs::rename(moved, &temp.0.root).unwrap();
    }

    #[test]
    fn updates_are_atomic_and_generation_fenced_and_terminal_immutable() {
        let temp = Temp::new();
        let store = temp.store();
        let mut a = task("op", "task");
        store.record(&a).unwrap();
        a.state = TaskState::Running;
        store.record(&a).unwrap();
        a.state = TaskState::Succeeded;
        store.record(&a).unwrap();
        store.record(&a).unwrap();
        a.state = TaskState::Failed;
        assert_eq!(store.record(&a).unwrap_err(), "history_terminal_immutable");
        a.attempt_generation = AttemptGeneration::new(2);
        a.state = TaskState::Queued;
        store.record(&a).unwrap();
        a.attempt_generation = AttemptGeneration::INITIAL;
        assert_eq!(store.record(&a).unwrap_err(), "history_stale_generation");
        assert_eq!(
            store
                .lookup("op", "task")
                .unwrap()
                .unwrap()
                .attempt_generation
                .get(),
            2
        );
    }

    #[test]
    fn rejects_corrupt_oversized_and_misnamed_records() {
        let temp = Temp::new();
        let store = temp.store();
        store.record(&task("op", "task")).unwrap();
        let path = store.root.join(key("op", "task"));
        let valid = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"{").unwrap();
        assert_eq!(store.list().unwrap_err(), "history_json");
        std::fs::write(&path, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(store.list().unwrap_err(), "history_size");
        std::fs::write(&path, valid).unwrap();
        std::fs::rename(&path, store.root.join(key("other", "task"))).unwrap();
        assert_eq!(store.list().unwrap_err(), "history_identity");
    }

    #[test]
    fn retention_preserves_nonterminal_records_and_store_lock_is_exclusive() {
        let temp = Temp::new();
        let store = temp.store();
        let mut a = task("active", "task");
        store.record_with_limit(&a, 1).unwrap();
        assert_eq!(
            store
                .record_with_limit(&task("other", "task"), 1)
                .unwrap_err(),
            "history_all_active"
        );
        a.state = TaskState::Failed;
        store.record_with_limit(&a, 1).unwrap();
        store.record_with_limit(&task("other", "task"), 1).unwrap();
        assert!(store.lookup("active", "task").unwrap().is_none());
        let _held = store.lock().unwrap();
        assert_eq!(store.list().unwrap_err(), "history_busy");
    }

    #[test]
    fn observer_can_skip_intermediate_states_but_cannot_regress_to_queued() {
        let temp = Temp::new();
        let store = temp.store();
        let mut task = task("op", "task");
        store.record(&task).unwrap();
        task.state = TaskState::Succeeded;
        store.record(&task).unwrap();
        task.attempt_generation = AttemptGeneration::new(2);
        task.state = TaskState::Running;
        store.record(&task).unwrap();
        task.state = TaskState::Queued;
        assert_eq!(store.record(&task).unwrap_err(), "history_state_regression");
    }

    #[test]
    fn coordinated_identity_retention_removes_only_orphans_without_state_rewriting() {
        let temp = Temp::new();
        let store = temp.store();
        let orphan = task("removed", "task");
        let retained = task("retained", "task");
        let mut complete = task("complete", "task");
        complete.state = TaskState::Succeeded;
        for item in [&orphan, &retained, &complete] {
            store.record(item).unwrap();
        }
        let identities = [&retained, &complete]
            .into_iter()
            .map(|item| (item.operation_id.clone(), item.id.clone()))
            .collect();
        store.retain_identities(&identities).unwrap();
        assert!(store.lookup("removed", "task").unwrap().is_none());
        assert_eq!(
            store.lookup("retained", "task").unwrap().unwrap().state,
            TaskState::Queued
        );
        assert_eq!(
            store.lookup("complete", "task").unwrap().unwrap().state,
            TaskState::Succeeded
        );
        assert_eq!(store.list().unwrap().len(), 2);
    }
}

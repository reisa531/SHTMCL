//! Persisted installation inputs and metadata used by a new recovery operation.
use super::*;

impl InstallOperations {
    /// The lease is acquired while retention is excluded, then held by the
    /// caller until the replacement request has been accepted or rejected.
    pub(crate) fn retry_request(
        &self,
        request: &shtmcl_domain::CommandRequest,
        tasks: &[shtmcl_domain::Task],
    ) -> Result<(shtmcl_domain::CommandRequest, OperationLease), String> {
        use shtmcl_domain::TaskState;
        let operation = request.payload["sourceOperationId"]
            .as_str()
            .ok_or("recovery_identity")?;
        let task = request.payload["sourceTaskId"]
            .as_str()
            .ok_or("recovery_identity")?;
        OperationId::new(operation).map_err(|_| "recovery_identity")?;
        TaskId::new(task).map_err(|_| "recovery_identity")?;
        if request
            .operation_id
            .as_ref()
            .is_none_or(|id| id.as_str() == operation)
        {
            return Err("recovery_new_operation_required".into());
        }
        let _lock = self.lock()?;
        let entry = self
            .files()?
            .into_iter()
            .map(|p| self.read(&p))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|e| e.operation_id.as_str() == operation && e.task_id.as_str() == task)
            .ok_or("installation_record_not_found")?;
        let input = entry.recovery.as_ref().ok_or("recovery_inputs_missing")?;
        let lease = self.lease(&self.root.join(entry.key()))?;
        let live = tasks
            .iter()
            .find(|t| t.id == entry.task_id && t.operation_id == entry.operation_id);
        let allowed = if let Some(live) = live {
            matches!(live.state, TaskState::Failed | TaskState::Cancelled)
        } else {
            crate::operation_history::OperationHistory::new(&self.dirs)
                .lookup(operation, task)?
                .is_some_and(|s| {
                    matches!(
                        s.state,
                        TaskState::Failed
                            | TaskState::Cancelled
                            | TaskState::Queued
                            | TaskState::Running
                    )
                })
        };
        if !allowed {
            return Err("recovery_not_stopped".into());
        }
        let mut next = request.clone();
        next.entity_id = None;
        next.expected_revision = None;
        // Rebuild a closed payload from disk, never copy browser-supplied paths
        // or loader selections into a recovery operation.
        next.payload = serde_json::json!({"command":"launch.install", "versionId":entry.version,
            "instanceId":entry.destination.instance_id, "name":entry.destination.instance_name,
            "loader":input.loader,"recoverySource":{"operationId":operation,"taskId":task}});
        if let Some(version) = &input.loader_version {
            next.payload["loaderVersion"] = version.clone().into();
        }
        if let Some(id) = &input.environment_id {
            next.payload["environmentId"] = id.clone().into();
        }
        Ok((next, lease))
    }
    pub(crate) fn source_metadata(
        &self,
        payload: &serde_json::Value,
        version: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        let Some(source) = payload.get("recoverySource") else {
            return Ok(None);
        };
        let op = source["operationId"].as_str().ok_or("recovery_identity")?;
        let task = source["taskId"].as_str().ok_or("recovery_identity")?;
        let _lock = self.lock()?;
        let entry = self
            .files()?
            .into_iter()
            .map(|p| self.read(&p))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|e| {
                e.operation_id.as_str() == op && e.task_id.as_str() == task && e.version == version
            })
            .ok_or("installation_record_not_found")?;
        let path = self.root.join(entry.key()).with_extension("metadata");
        reject_link(&path)?;
        if !path.exists() {
            return if entry.metadata_sha256.is_some() {
                Err("recovery_metadata_missing".into())
            } else {
                Ok(None)
            };
        }
        let file = File::open(path).map_err(|_| "recovery_metadata_read")?;
        let mut bytes = Vec::new();
        file.take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "recovery_metadata_read")?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("recovery_metadata_size".into());
        }
        let saved: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| "recovery_metadata_json")?;
        let raw = saved["metadata"].clone();
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&raw).map_err(|_| "recovery_metadata_json")?)
        );
        if saved["sha256"] != digest
            || raw["id"] != version
            || entry
                .metadata_sha256
                .as_ref()
                .is_some_and(|expected| expected != &digest)
        {
            return Err("recovery_metadata_identity".into());
        }
        Ok(Some(raw))
    }

    pub(crate) fn pin_metadata(
        &self,
        task: &shtmcl_domain::Task,
        raw: &serde_json::Value,
    ) -> Result<(), String> {
        let _lock = self.lock()?;
        let entry = self
            .files()?
            .into_iter()
            .map(|p| self.read(&p))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|e| e.operation_id == task.operation_id && e.task_id == task.id);
        let Some(mut entry) = entry.filter(|e| e.recovery.is_some()) else {
            return Ok(());
        };
        if raw["id"] != entry.version {
            return Err("recovery_metadata_identity".into());
        }
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(raw).map_err(|_| "recovery_metadata_json")?)
        );
        if entry
            .metadata_sha256
            .as_ref()
            .is_some_and(|expected| expected != &digest)
        {
            return Err("recovery_metadata_changed".into());
        }
        let bytes = serde_json::to_vec(&serde_json::json!({"sha256":digest,"metadata":raw}))
            .map_err(|_| "recovery_metadata_json")?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("recovery_metadata_size".into());
        }
        let path = self.root.join(entry.key()).with_extension("metadata");
        reject_link(&path)?;
        if path.exists() {
            let mut saved = Vec::new();
            File::open(&path)
                .map_err(|_| "recovery_metadata_read")?
                .take(8 * 1024 * 1024 + 1)
                .read_to_end(&mut saved)
                .map_err(|_| "recovery_metadata_read")?;
            if saved != bytes {
                return Err("recovery_metadata_changed".into());
            }
        } else {
            self.write_recovery_file(&path, &bytes)?;
        }
        entry.metadata_sha256 = Some(digest);
        let target = self.root.join(entry.key());
        entry.destination.path = Path::new("instances").join(&entry.destination.instance_id);
        let bytes = serde_json::to_vec(&StoredRecord {
            schema_version: 1,
            entry,
        })
        .map_err(|_| "installation_record_json")?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err("installation_record_size".into());
        }
        self.write_recovery_file(&target, &bytes)
    }

    fn write_recovery_file(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
        reject_link(path)?;
        let temp = self.root.join(format!(
            "{}.tmp",
            shtmcl_platform::entropy::random_hex(16).map_err(|_| "recovery_entropy")?
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|_| "recovery_metadata_write")?;
            file.write_all(bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| "recovery_metadata_write")?;
            drop(file);
            std::fs::rename(&temp, &path).map_err(|_| "recovery_metadata_commit")
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.map_err(str::to_string)
    }
}

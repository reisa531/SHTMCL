//! Adapts module 01's persisted lease to module 02's process lifetime guard.
use crate::launch_lease_journal::{Journal, Record};
use serde_json::json;
use shtmcl_core::{
    launch::ports::{InstanceLaunchLease, InstanceLaunchLeaseSource},
    CoreModule,
};
use shtmcl_domain::{
    CommandRequest, CommandResponse, ErrorEnvelope, InstanceId, OperationId, RequestId,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

pub(crate) struct InstanceLeases {
    instances: Arc<dyn CoreModule>,
    holders: Arc<Mutex<BTreeMap<String, LeaseGroup>>>,
    journal: Arc<Journal>,
}

struct LeaseGroup {
    owner: String,
    operation: OperationId,
    sessions: BTreeSet<String>,
    poisoned: bool,
    record: Record,
    preparation: Option<String>,
}

/// Keeps resource writes exclusive without holding the registry lock during I/O.
pub(crate) struct PreparationPermit {
    holders: Arc<Mutex<BTreeMap<String, LeaseGroup>>>,
    instance: String,
    token: String,
}

impl Drop for PreparationPermit {
    fn drop(&mut self) {
        if let Ok(mut holders) = self.holders.lock() {
            if let Some(group) = holders.get_mut(&self.instance) {
                if group.preparation.as_ref() == Some(&self.token) {
                    group.preparation = None;
                }
            }
        }
    }
}

impl InstanceLeases {
    pub(crate) fn begin_preparation(
        &self,
        id: &InstanceId,
        holder: &str,
    ) -> Result<PreparationPermit, ErrorEnvelope> {
        let mut holders = self
            .holders
            .lock()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        let group = holders
            .get_mut(id.as_str())
            .ok_or_else(ErrorEnvelope::internal_unavailable)?;
        if group.poisoned
            || group.preparation.is_some()
            || group.sessions.len() != 1
            || !group.sessions.contains(holder)
            || !matches!(group.record.sessions.get(holder), Some(None))
        {
            return Err(
                shtmcl_core::instances::error::InstancesError::Locked(id.as_str().into())
                    .to_envelope(),
            );
        }
        let token = shtmcl_platform::entropy::random_hex(12)
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        group.preparation = Some(token.clone());
        Ok(PreparationPermit {
            holders: self.holders.clone(),
            instance: id.as_str().into(),
            token,
        })
    }

    #[cfg(test)]
    pub(crate) fn new(instances: Arc<dyn CoreModule>) -> Self {
        Self {
            instances,
            holders: Arc::new(Mutex::new(BTreeMap::new())),
            journal: Arc::new(Journal::memory()),
        }
    }

    pub(crate) fn persistent(
        instances: Arc<dyn CoreModule>,
        root: std::path::PathBuf,
    ) -> Result<Self, ErrorEnvelope> {
        Ok(Self {
            instances,
            holders: Arc::new(Mutex::new(BTreeMap::new())),
            journal: Arc::new(
                Journal::open(root).map_err(|_| ErrorEnvelope::internal_unavailable())?,
            ),
        })
    }

    /// Only identities confirmed exited by the OS permit persisted lease release.
    /// Missing records remain protected in 01; corrupt files abort this pass.
    pub(crate) fn recover(&self) -> Result<usize, ErrorEnvelope> {
        let holders = self
            .holders
            .lock()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        let records = self
            .journal
            .records()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        let mut released = 0;
        for record in records {
            if holders.contains_key(&record.instance) || !record.exited() {
                continue;
            }
            let id = InstanceId::new(&record.instance)
                .map_err(|_| ErrorEnvelope::internal_unavailable())?;
            let operation = OperationId::new(&record.operation)
                .map_err(|_| ErrorEnvelope::internal_unavailable())?;
            command(
                self.instances.as_ref(),
                &id,
                &record.owner,
                &operation,
                false,
            )?;
            self.journal
                .remove(&record.instance)
                .map_err(|_| ErrorEnvelope::internal_unavailable())?;
            released += 1;
        }
        Ok(released)
    }

    pub(crate) fn owns(&self, id: &InstanceId, holder: &str) -> Result<bool, ErrorEnvelope> {
        Ok(self
            .holders
            .lock()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?
            .get(id.as_str())
            .is_some_and(|current| current.sessions.contains(holder)))
    }
}

fn command(
    instances: &dyn CoreModule,
    id: &InstanceId,
    holder: &str,
    operation: &OperationId,
    acquire: bool,
) -> Result<(), ErrorEnvelope> {
    let response = instances.handle(&CommandRequest {
        request_id: RequestId::new(format!("lease-{holder}")).map_err(|_| ErrorEnvelope::internal_unavailable())?,
        operation_id: Some(operation.clone()), entity_id: None, expected_revision: None,
        // A running process must not become writable merely because a heartbeat
        // was delayed. The data-directory owner lock makes restart recovery the
        // authority that clears an orphaned process lifetime lease.
        payload: json!({"command":if acquire {"instances.lease.acquire"} else {"instances.lease.release"},"instanceId":id.as_str(),"holder":holder,"ttlMs":u64::MAX,"purpose":"launch"}),
    })?;
    match response {
        CommandResponse::Completed { .. } => Ok(()),
        _ => Err(ErrorEnvelope::internal_unavailable()),
    }
}

impl InstanceLaunchLeaseSource for InstanceLeases {
    fn acquire(
        &self,
        id: &InstanceId,
        holder: &str,
        operation: &OperationId,
    ) -> Result<Box<dyn InstanceLaunchLease>, ErrorEnvelope> {
        let mut holders = self
            .holders
            .lock()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        let (owner, operation) = if let Some(group) = holders.get_mut(id.as_str()) {
            if group.preparation.is_some() {
                return Err(shtmcl_core::instances::error::InstancesError::Locked(
                    id.as_str().into(),
                )
                .to_envelope());
            }
            if group.poisoned {
                return Err(ErrorEnvelope::internal_unavailable());
            }
            if group.sessions.contains(holder) {
                return Err(ErrorEnvelope::internal_unavailable());
            }
            let mut record = group.record.clone();
            record.sessions.insert(holder.into(), None);
            self.journal
                .write(&record)
                .map_err(|_| ErrorEnvelope::internal_unavailable())?;
            group.record = record;
            group.sessions.insert(holder.into());
            (group.owner.clone(), group.operation.clone())
        } else {
            if self
                .journal
                .contains(id.as_str())
                .map_err(|_| ErrorEnvelope::internal_unavailable())?
            {
                return Err(ErrorEnvelope::internal_unavailable());
            }
            let record = Record {
                instance: id.as_str().into(),
                owner: holder.into(),
                operation: operation.as_str().into(),
                sessions: BTreeMap::from([(holder.into(), None)]),
            };
            self.journal
                .write(&record)
                .map_err(|_| ErrorEnvelope::internal_unavailable())?;
            if let Err(error) = command(self.instances.as_ref(), id, holder, operation, true) {
                // These 01 errors precede all lease/state writes. No guard was
                // returned and no process could have spawned through this call.
                // Never infer that from a broad category or retryable flag:
                // persistence/internal failures still retain pending evidence.
                use shtmcl_core::instances::error::codes;
                if matches!(
                    error.code.as_str(),
                    codes::INSTANCE_NOT_FOUND | codes::INSTANCE_LOCKED | codes::INSTANCE_TERMINAL
                ) {
                    // A failed cleanup remains pending and prevents a retry.
                    let _ = self.journal.remove(id.as_str());
                }
                return Err(error);
            }
            holders.insert(
                id.as_str().into(),
                LeaseGroup {
                    owner: holder.into(),
                    operation: operation.clone(),
                    sessions: BTreeSet::from([holder.into()]),
                    poisoned: false,
                    record,
                    preparation: None,
                },
            );
            (holder.to_owned(), operation.clone())
        };
        Ok(Box::new(Lease {
            instances: self.instances.clone(),
            holders: self.holders.clone(),
            journal: self.journal.clone(),
            id: id.clone(),
            holder: holder.into(),
            owner,
            operation,
            process_bound: false,
        }))
    }
}

struct Lease {
    instances: Arc<dyn CoreModule>,
    holders: Arc<Mutex<BTreeMap<String, LeaseGroup>>>,
    journal: Arc<Journal>,
    id: InstanceId,
    holder: String,
    owner: String,
    operation: OperationId,
    process_bound: bool,
}

impl InstanceLaunchLease for Lease {
    fn bind_process(&mut self, pid: u32) -> Result<(), ErrorEnvelope> {
        self.process_bound = true;
        let identity = shtmcl_platform::process_identity::capture(pid)
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        let mut holders = self
            .holders
            .lock()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        let group = holders
            .get_mut(self.id.as_str())
            .ok_or_else(ErrorEnvelope::internal_unavailable)?;
        let mut record = group.record.clone();
        *record
            .sessions
            .get_mut(&self.holder)
            .ok_or_else(ErrorEnvelope::internal_unavailable)? = Some(identity.into());
        self.journal
            .write(&record)
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        group.record = record;
        Ok(())
    }

    fn renew(&mut self) -> Result<(), ErrorEnvelope> {
        command(
            self.instances.as_ref(),
            &self.id,
            &self.owner,
            &self.operation,
            true,
        )
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let Ok(mut holders) = self.holders.lock() else {
            return;
        };
        let Some(group) = holders.get_mut(self.id.as_str()) else {
            return;
        };
        if self.process_bound {
            use shtmcl_platform::process_identity::{probe, ProcessIdentity, ProcessLiveness};
            let exited = group
                .record
                .sessions
                .get(&self.holder)
                .copied()
                .flatten()
                .is_some_and(|identity| {
                    probe(&ProcessIdentity {
                        pid: identity.pid,
                        creation_time: identity.creation_time,
                    }) == ProcessLiveness::Exited
                });
            if !exited {
                group.poisoned = true;
                return;
            }
        }
        if group.sessions.len() > 1 {
            let mut record = group.record.clone();
            record.sessions.remove(&self.holder);
            if self.journal.write(&record).is_err() {
                group.poisoned = true;
                return;
            }
            group.record = record;
            group.sessions.remove(&self.holder);
            return;
        }
        match command(
            self.instances.as_ref(),
            &self.id,
            &self.owner,
            &self.operation,
            false,
        ) {
            Ok(()) => {
                if self.journal.contains(self.id.as_str()).unwrap_or(true)
                    && self.journal.remove(self.id.as_str()).is_err()
                {
                    if let Some(group) = holders.get_mut(self.id.as_str()) {
                        group.poisoned = true;
                    }
                    return;
                }
                holders.remove(self.id.as_str());
            }
            Err(error) => {
                if let Some(group) = holders.get_mut(self.id.as_str()) {
                    group.poisoned = true;
                }
                eprintln!("SHTMCL: instance lease release failed: {}", error.code);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shtmcl_core::{
        instances::{FileStorage, InstancesModule},
        CoreApp,
    };
    use shtmcl_platform::PlatformDirs;

    #[test]
    fn resource_preparation_excludes_other_sessions_and_live_games() {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-preparation-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        let app = CoreApp::new(PlatformDirs::under(&root));
        let instances = Arc::new(
            InstancesModule::new(
                app.context(),
                Box::new(FileStorage::new(root.join("index")).unwrap()),
            )
            .unwrap(),
        );
        instances.handle(&CommandRequest {
            request_id: RequestId::new("create-preparation").unwrap(),
            operation_id: Some(OperationId::new("create-preparation").unwrap()),
            entity_id: None, expected_revision: None,
            payload: json!({"command":"instances.create","instance":{"id":"school","name":"School","version":{"gameVersion":"1.7.10","loader":"vanilla"},"location":{"root":root.join("game")},"javaMajor":8}}),
        }).unwrap();
        let leases = InstanceLeases::new(instances.clone());
        let id = InstanceId::new("school").unwrap();
        let op = OperationId::new("prepare-fixture").unwrap();
        let first = leases.acquire(&id, "first", &op).unwrap();
        let permit = leases.begin_preparation(&id, "first").unwrap();
        assert!(leases.acquire(&id, "second", &op).is_err());
        assert!(leases.begin_preparation(&id, "first").is_err());
        assert!(leases.begin_preparation(&id, "stranger").is_err());
        drop(permit);
        let second = leases.acquire(&id, "second", &op).unwrap();
        assert!(
            leases.begin_preparation(&id, "first").is_err(),
            "shared sessions cannot write resources"
        );
        drop(second);
        // Model the persisted binding without starting a real process: the
        // existing process-identity tests cover OS liveness separately.
        leases
            .holders
            .lock()
            .unwrap()
            .get_mut("school")
            .unwrap()
            .record
            .sessions
            .insert(
                "first".into(),
                Some(crate::launch_lease_journal::Identity {
                    pid: 1,
                    creation_time: 1,
                }),
            );
        assert!(
            leases.begin_preparation(&id, "first").is_err(),
            "a bound game excludes resource writes"
        );
        drop(first);
        drop(leases);
        drop(instances);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejected_acquire_can_retry_after_definite_instance_rejection_is_resolved() {
        use shtmcl_core::instances::{error::codes, InstanceState};

        for rejected_code in [
            codes::INSTANCE_LOCKED,
            codes::INSTANCE_NOT_FOUND,
            codes::INSTANCE_TERMINAL,
        ] {
            let root = std::env::temp_dir().join(format!(
                "shtmcl-rejected-acquire-{}",
                shtmcl_platform::entropy::random_hex(8).unwrap()
            ));
            let app = CoreApp::new(PlatformDirs::under(&root));
            let instances = Arc::new(
                InstancesModule::new(
                    app.context(),
                    Box::new(FileStorage::new(root.join("index")).unwrap()),
                )
                .unwrap(),
            );
            let create = || {
                instances.handle(&CommandRequest {
                    request_id: RequestId::new("create-fixture").unwrap(),
                    operation_id: Some(OperationId::new("create-fixture").unwrap()),
                    entity_id: None,
                    expected_revision: None,
                    payload: json!({"command":"instances.create","instance":{"id":"school","name":"School","version":{"gameVersion":"1.7.10","loader":"vanilla"},"location":{"root":root.join("game")},"javaMajor":8}}),
                }).unwrap();
            };
            if rejected_code != codes::INSTANCE_NOT_FOUND {
                create();
            }
            match rejected_code {
                codes::INSTANCE_LOCKED => {
                    instances
                        .store()
                        .acquire_lease("school", "installer", "install", 0, u64::MAX)
                        .unwrap();
                }
                codes::INSTANCE_TERMINAL => {
                    instances
                        .store()
                        .set_state("school", InstanceState::Broken, 1)
                        .unwrap();
                }
                _ => {}
            }
            let leases =
                InstanceLeases::persistent(instances.clone(), root.join("journal")).unwrap();
            let id = InstanceId::new("school").unwrap();
            let operation = OperationId::new("start-fixture").unwrap();
            let error = leases
                .acquire(&id, "session-first", &operation)
                .err()
                .unwrap();
            assert_eq!(error.code.as_str(), rejected_code);
            assert!(!leases.journal.contains(id.as_str()).unwrap());
            assert!(leases.journal.records().unwrap().is_empty());

            match rejected_code {
                codes::INSTANCE_LOCKED => instances
                    .store()
                    .release_lease("school", "installer")
                    .unwrap(),
                codes::INSTANCE_NOT_FOUND => create(),
                codes::INSTANCE_TERMINAL => {
                    instances
                        .store()
                        .set_state("school", InstanceState::Ready, 2)
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let guard = leases.acquire(&id, "session-retry", &operation).unwrap();
            assert_eq!(
                instances.store().get("school").unwrap().state,
                InstanceState::Busy
            );
            drop(guard);
            assert_eq!(
                instances.store().get("school").unwrap().state,
                InstanceState::Ready
            );
            drop(leases);
            drop(instances);
            drop(app);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn rejected_acquire_retains_pending_for_uncertain_persistence_errors() {
        use shtmcl_core::instances::error::InstancesError;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Rejected {
            calls: AtomicUsize,
            error: InstancesError,
        }
        impl CoreModule for Rejected {
            fn id(&self) -> shtmcl_domain::ModuleId {
                shtmcl_domain::ModuleId::Instances
            }
            fn handle(&self, _: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(self.error.to_envelope())
            }
        }
        for error in [
            InstancesError::IndexIo("injected".into()),
            InstancesError::Internal,
        ] {
            let root = std::env::temp_dir().join(format!(
                "shtmcl-uncertain-acquire-{}",
                shtmcl_platform::entropy::random_hex(8).unwrap()
            ));
            let rejected = Arc::new(Rejected {
                calls: AtomicUsize::new(0),
                error,
            });
            let leases = InstanceLeases::persistent(rejected.clone(), root.clone()).unwrap();
            let id = InstanceId::new("school").unwrap();
            let operation = OperationId::new("start-fixture").unwrap();
            let error = leases
                .acquire(&id, "session-first", &operation)
                .err()
                .unwrap();
            assert_eq!(error.code.as_str(), rejected.error.code());
            assert!(leases.journal.contains(id.as_str()).unwrap());
            assert!(leases.journal.records().unwrap()[0].sessions["session-first"].is_none());
            assert!(leases.acquire(&id, "session-retry", &operation).is_err());
            assert_eq!(leases.recover().unwrap(), 0);
            assert_eq!(rejected.calls.load(Ordering::SeqCst), 1);
            drop(leases);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn rejected_acquire_cleanup_failure_keeps_retry_blocked() {
        struct CleanupFailure(std::path::PathBuf);
        impl CoreModule for CleanupFailure {
            fn id(&self) -> shtmcl_domain::ModuleId {
                shtmcl_domain::ModuleId::Instances
            }
            fn handle(&self, _: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
                // Replace this fixture's pending file with a directory so the
                // adapter's remove_file must fail, without OS permission tricks.
                let record = std::fs::read_dir(&self.0)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| path.extension().is_some_and(|ext| ext == "json"))
                    .unwrap();
                std::fs::remove_file(&record).unwrap();
                std::fs::create_dir(record).unwrap();
                Err(
                    shtmcl_core::instances::error::InstancesError::Locked("school".into())
                        .to_envelope(),
                )
            }
        }
        let root = std::env::temp_dir().join(format!(
            "shtmcl-acquire-cleanup-failure-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        let leases =
            InstanceLeases::persistent(Arc::new(CleanupFailure(root.clone())), root.clone())
                .unwrap();
        let id = InstanceId::new("school").unwrap();
        let operation = OperationId::new("start-fixture").unwrap();
        assert_eq!(
            leases
                .acquire(&id, "session-first", &operation)
                .err()
                .unwrap()
                .code
                .as_str(),
            shtmcl_core::instances::error::codes::INSTANCE_LOCKED
        );
        assert!(leases.journal.contains(id.as_str()).unwrap());
        assert!(leases.acquire(&id, "session-retry", &operation).is_err());
        assert!(leases.recover().is_err());
        drop(leases);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn restarted_adapter_keeps_live_and_pending_processes_protected() {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-persisted-lease-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        let app = CoreApp::new(PlatformDirs::under(&root));
        let instances = Arc::new(
            InstancesModule::new(
                app.context(),
                Box::new(FileStorage::new(root.join("index")).unwrap()),
            )
            .unwrap(),
        );
        for id in ["live", "pending", "exited"] {
            instances.handle(&CommandRequest {request_id:RequestId::new(format!("create-{id}")).unwrap(),operation_id:Some(OperationId::new(format!("create-{id}")).unwrap()),entity_id:None,expected_revision:None,payload:json!({"command":"instances.create","instance":{"id":id,"name":id,"version":{"gameVersion":"1.7.10","loader":"vanilla"},"location":{"root":root.join(id)},"javaMajor":8}})}).unwrap();
        }
        let journal = root.join("journal");
        let leases = InstanceLeases::persistent(instances.clone(), journal.clone()).unwrap();
        let operation = OperationId::new("start-test").unwrap();
        let live = InstanceId::new("live").unwrap();
        let mut guard = leases.acquire(&live, "session-live", &operation).unwrap();
        guard.bind_process(std::process::id()).unwrap();
        drop(guard); // An accidental guard drop cannot unlock a live game.
        drop(leases);
        drop(instances); // Release the FileStorage OS owner lock, not merely the adapter.
        let instances = Arc::new(
            InstancesModule::new(
                app.context(),
                Box::new(FileStorage::new(root.join("index")).unwrap()),
            )
            .unwrap(),
        );
        let recovered = InstanceLeases::persistent(instances.clone(), journal).unwrap();
        assert_eq!(recovered.recover().unwrap(), 0);
        assert_eq!(
            instances.store().get("live").unwrap().state,
            shtmcl_core::instances::InstanceState::Busy
        );
        assert!(recovered.acquire(&live, "new-session", &operation).is_err());
        let pending = InstanceId::new("pending").unwrap();
        recovered
            .journal
            .write(&Record {
                instance: "pending".into(),
                owner: "session-pending".into(),
                operation: operation.as_str().into(),
                sessions: BTreeMap::from([("session-pending".into(), None)]),
            })
            .unwrap();
        command(
            instances.as_ref(),
            &pending,
            "session-pending",
            &operation,
            true,
        )
        .unwrap();
        assert_eq!(recovered.recover().unwrap(), 0);
        assert_eq!(
            instances.store().get("pending").unwrap().state,
            shtmcl_core::instances::InstanceState::Busy
        );
        let exited = InstanceId::new("exited").unwrap();
        use std::os::windows::process::CommandExt;
        let mut child = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
            .creation_flags(0x08000000)
            .spawn()
            .unwrap();
        let mut guard = recovered
            .acquire(&exited, "session-exited", &operation)
            .unwrap();
        guard.bind_process(child.id()).unwrap();
        drop(guard); // Live guard retains the durable record.
        child.kill().unwrap();
        child.wait().unwrap();
        drop(recovered);
        drop(instances);
        let instances = Arc::new(
            InstancesModule::new(
                app.context(),
                Box::new(FileStorage::new(root.join("index")).unwrap()),
            )
            .unwrap(),
        );
        let recovered =
            InstanceLeases::persistent(instances.clone(), root.join("journal")).unwrap();
        assert_eq!(
            instances.store().get("exited").unwrap().state,
            shtmcl_core::instances::InstanceState::Busy
        );
        assert_eq!(recovered.recover().unwrap(), 1);
        assert_eq!(
            instances.store().get("live").unwrap().state,
            shtmcl_core::instances::InstanceState::Busy
        );
        assert_eq!(
            instances.store().get("pending").unwrap().state,
            shtmcl_core::instances::InstanceState::Busy
        );
        assert_eq!(
            instances.store().get("exited").unwrap().state,
            shtmcl_core::instances::InstanceState::Ready
        );
        drop(recovered);
        drop(instances);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn all_running_sessions_must_release_before_instance_mutation() {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-launch-lease-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        let app = CoreApp::new(PlatformDirs::under(&root));
        let instances = Arc::new(
            InstancesModule::new(
                app.context(),
                Box::new(FileStorage::new(root.join("index")).unwrap()),
            )
            .unwrap(),
        );
        let mut request = CommandRequest {
            request_id: RequestId::new("create-fixture").unwrap(),
            operation_id: Some(OperationId::new("create-fixture").unwrap()),
            entity_id: None,
            expected_revision: None,
            payload: json!({"command":"instances.create","instance":{"id":"school","name":"School","version":{"gameVersion":"1.7.10","loader":"vanilla"},"location":{"root":root.join("game")},"javaMajor":8}}),
        };
        instances.handle(&request).unwrap();
        let leases = InstanceLeases::new(instances.clone());
        let id = InstanceId::new("school").unwrap();
        let operation = OperationId::new("start-fixture").unwrap();
        let first = leases.acquire(&id, "launch-1", &operation).unwrap();
        let mut second = leases.acquire(&id, "launch-2", &operation).unwrap();
        assert!(leases.owns(&id, "launch-1").unwrap());
        assert!(!leases.owns(&id, "stranger").unwrap());
        assert_eq!(
            instances.store().get("school").unwrap().state,
            shtmcl_core::instances::InstanceState::Busy
        );
        request.expected_revision = Some(shtmcl_domain::Revision::new(1));
        request.payload =
            json!({"command":"instances.update","instanceId":"school","patch":{"name":"Changed"}});
        assert_eq!(
            instances.handle(&request).unwrap_err().code.as_str(),
            "instances.instance.running"
        );
        drop(first);
        assert!(!leases.owns(&id, "launch-1").unwrap());
        second.renew().unwrap();
        assert_eq!(
            instances.handle(&request).unwrap_err().code.as_str(),
            "instances.instance.running",
            "one live session still protects content"
        );
        drop(second);
        assert!(!leases.owns(&id, "launch-2").unwrap());
        assert_eq!(
            instances.store().get("school").unwrap().state,
            shtmcl_core::instances::InstanceState::Ready
        );
        request.request_id = RequestId::new("update-after-exit").unwrap();
        instances.handle(&request).unwrap();
        drop(leases);
        drop(instances);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}

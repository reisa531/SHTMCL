use super::*;
use serde_json::json;
use shtmcl_core::{tasks::TaskSpec, CoreApp, CoreModule};
use shtmcl_domain::{CommandRequest, CommandResponse, RequestId, TaskPhase, TaskState};
use std::sync::{Arc, Mutex};

struct Fixture {
    app: CoreApp,
    store: InstallOperations,
    task: shtmcl_domain::Task,
    record: InstallOperation,
}
impl Fixture {
    fn new(loader: &str, loader_version: Option<&str>, environment: Option<String>) -> Self {
        let dirs = PlatformDirs::under(std::env::temp_dir().join(format!(
            "shtmcl-recovery-{}",
            shtmcl_platform::entropy::random_hex(12).unwrap()
        )));
        let app = CoreApp::new(dirs.clone());
        let task = app
            .context()
            .tasks()
            .accept(TaskSpec::new(
                OperationId::new("original").unwrap(),
                TaskPhase::new("install").unwrap(),
            ))
            .unwrap();
        let record = InstallOperation::instance(
            task.operation_id.clone(),
            task.id.clone(),
            "1.20.4",
            "same-instance",
            "校园生存",
            &dirs,
        )
        .with_recovery(InstallSelection {
            loader: loader.into(),
            loader_version: loader_version.map(str::to_string),
            environment_id: environment,
        });
        Self {
            app,
            store: InstallOperations::new(&dirs),
            task,
            record,
        }
    }
    fn request(&self) -> CommandRequest {
        CommandRequest {
            request_id: RequestId::new("retry-request").unwrap(),
            operation_id: Some(OperationId::new("retry-operation").unwrap()),
            entity_id: None,
            expected_revision: None,
            payload: json!({"command":"launch.installations.retry",
                "sourceOperationId":"original","sourceTaskId":self.task.id,"loader":"vanilla","instanceId":"wrong-target"}),
        }
    }
    fn stop(&self) {
        let tasks = self.app.context().tasks().clone();
        tasks
            .transition(
                &self.task.id,
                self.task.attempt_generation,
                TaskState::Running,
            )
            .unwrap();
        tasks.request_cancel(&shtmcl_domain::CancelTaskRequest {
            task_id: self.task.id.clone(),
            attempt_generation: self.task.attempt_generation,
        });
        tasks
            .cancel_now(&self.task.id, self.task.attempt_generation)
            .unwrap();
        crate::operation_history::OperationHistory::new(self.app.context().dirs())
            .record(&tasks.snapshot(&self.task.id).unwrap())
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.app.context().dirs().root.clone());
    }
}

#[test]
fn retry_roundtrips_loaders_and_recipe_without_trusting_ui_selections() {
    for (loader, version, recipe) in [
        ("vanilla", None, None),
        ("fabric", Some("0.16.10"), Some("a".repeat(64))),
        ("neoforge", Some("20.4.253"), None),
    ] {
        let f = Fixture::new(loader, version, recipe.clone());
        drop(f.store.insert(&f.record).unwrap());
        f.stop();
        let (retry, _lease) = InstallOperations::new(f.app.context().dirs())
            .retry_request(&f.request(), &[])
            .unwrap();
        assert_eq!(retry.payload["loader"], loader);
        assert_eq!(retry.payload["instanceId"], "same-instance");
        assert_eq!(retry.payload["name"], "校园生存");
        assert_eq!(retry.payload["loaderVersion"], json!(version));
        assert_eq!(retry.payload["environmentId"], json!(recipe));
        assert_eq!(retry.operation_id, f.request().operation_id);
        assert!(retry.payload.get("sourceOperationId").is_none());
        let source = recovery_source(&retry.payload).unwrap().unwrap();
        assert_eq!(source.operation_id, f.task.operation_id);
        let row = &f.store.presentations(&[]).unwrap()[0];
        assert_eq!(row["canRetryInstall"], true);
        assert!(row.get("recovery").is_none());
    }
}

#[test]
fn retry_rejects_active_unknown_completed_and_legacy_records() {
    let f = Fixture::new("vanilla", None, None);
    let lease = f.store.insert(&f.record).unwrap();
    assert!(f.store.retry_request(&f.request(), &[]).is_err());
    drop(lease);
    assert!(
        f.store.retry_request(&f.request(), &[]).is_err(),
        "no persisted execution state"
    );
    crate::operation_history::OperationHistory::new(f.app.context().dirs())
        .record(&f.task)
        .unwrap();
    assert!(
        f.store
            .retry_request(&f.request(), &[f.task.clone()])
            .is_err(),
        "live queued task"
    );
    assert!(
        f.store.retry_request(&f.request(), &[]).is_ok(),
        "orphaned queued task with released lease"
    );
    f.stop();
    for state in [
        TaskState::Succeeded,
        TaskState::Paused,
        TaskState::WaitingUser,
    ] {
        let mut current = f.task.clone();
        current.state = state;
        assert!(f.store.retry_request(&f.request(), &[current]).is_err());
    }
    let mut same = f.request();
    same.operation_id = Some(f.task.operation_id.clone());
    assert!(f.store.retry_request(&same, &[]).is_err());
    let mut old = f.record.clone();
    old.recovery = None;
    let path = f.store.root.join(old.key());
    std::fs::remove_file(path).unwrap();
    drop(f.store.insert(&old).unwrap());
    assert!(
        f.store.retry_request(&f.request(), &[]).is_err(),
        "legacy loader must not be inferred"
    );
}

#[test]
fn pinned_metadata_survives_reopen_and_rejects_modified_bytes() {
    let f = Fixture::new("fabric", Some("0.16.10"), None);
    drop(f.store.insert(&f.record).unwrap());
    f.stop();
    let raw = json!({"id":"1.20.4","libraries":[],"mainClass":"fixture.Main"});
    f.store.pin_metadata(&f.task, &raw).unwrap();
    f.store.pin_metadata(&f.task, &raw).unwrap();
    assert!(f
        .store
        .pin_metadata(&f.task, &json!({"id":"1.20.4","changed":true}))
        .is_err());
    let (retry, _lease) = f.store.retry_request(&f.request(), &[]).unwrap();
    assert_eq!(
        InstallOperations::new(f.app.context().dirs())
            .source_metadata(&retry.payload, "1.20.4")
            .unwrap(),
        Some(raw)
    );
    let path = f.store.root.join(f.record.key()).with_extension("metadata");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    saved["metadata"]["mainClass"] = json!("changed.Main");
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    assert!(f.store.source_metadata(&retry.payload, "1.20.4").is_err());
    saved["sha256"] = json!(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&saved["metadata"]).unwrap())
    ));
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    assert!(
        f.store.source_metadata(&retry.payload, "1.20.4").is_err(),
        "record pins the original digest independently"
    );
    std::fs::remove_file(&path).unwrap();
    assert!(
        f.store.source_metadata(&retry.payload, "1.20.4").is_err(),
        "missing pinned metadata must not resolve a different version document"
    );
}

#[test]
fn retention_preserves_leased_metadata_then_removes_unreferenced_sidecar() {
    let f = Fixture::new("vanilla", None, None);
    drop(f.store.insert(&f.record).unwrap());
    f.stop();
    f.store
        .pin_metadata(&f.task, &json!({"id":"1.20.4"}))
        .unwrap();
    let (_, lease) = f.store.retry_request(&f.request(), &[]).unwrap();
    let mut next = f.record.clone();
    next.operation_id = OperationId::new("next").unwrap();
    next.task_id = TaskId::new("next-task").unwrap();
    let sidecar = f.store.root.join(f.record.key()).with_extension("metadata");
    assert!(f.store.insert_with_limit(&next, 1).is_err());
    assert!(sidecar.exists());
    drop(lease);
    drop(f.store.insert_with_limit(&next, 1).unwrap());
    assert!(!sidecar.exists());
}

struct Delegate(Mutex<Vec<CommandRequest>>);
impl CoreModule for Delegate {
    fn id(&self) -> shtmcl_domain::ModuleId {
        shtmcl_domain::ModuleId::Launch
    }
    fn handle(
        &self,
        request: &CommandRequest,
    ) -> Result<CommandResponse, shtmcl_domain::ErrorEnvelope> {
        self.0.lock().unwrap().push(request.clone());
        Ok(CommandResponse::Accepted {
            operation_id: request.operation_id.clone().unwrap(),
            task_id: TaskId::new("replacement").unwrap(),
        })
    }
}

#[test]
fn recovery_adapter_uses_real_instance_registry_and_refuses_existing_target() {
    let f = Fixture::new("fabric", Some("0.16.10"), None);
    drop(f.store.insert(&f.record).unwrap());
    f.stop();
    let instances: Arc<dyn CoreModule> = Arc::new(
        shtmcl_core::instances::InstancesModule::new(
            f.app.context(),
            Box::new(
                shtmcl_core::instances::FileStorage::new(
                    f.app.context().dirs().data.join("registry"),
                )
                .unwrap(),
            ),
        )
        .unwrap(),
    );
    let delegate = Arc::new(Delegate(Mutex::new(vec![])));
    let adapter = crate::install_recovery::InstallRecovery::new(
        f.app.context(),
        delegate.clone(),
        Some(instances.clone()),
    );
    assert!(adapter.handle(&f.request()).is_ok());
    assert_eq!(delegate.0.lock().unwrap()[0].payload["loader"], "fabric");
    let mut create = f.request();
    create.operation_id = Some(OperationId::new("create-existing").unwrap());
    create.payload = json!({"command":"instances.create","instance":{"id":"same-instance","name":"keep",
        "version":{"gameVersion":"1.20.4","loader":"vanilla"},"location":{"root":f.app.context().dirs().instances.join("same-instance")},"javaMajor":17}});
    instances.handle(&create).unwrap();
    let error = adapter.handle(&f.request()).unwrap_err();
    assert_eq!(error.code.as_str(), "launch.install.target_exists");
    assert_eq!(
        delegate.0.lock().unwrap().len(),
        1,
        "existing game must not reach installer"
    );
}

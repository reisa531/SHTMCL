//! Controlled orchestration regressions; no Java or Minecraft process is created.
use crate::{game_launch::GameLauncher, runtime_install::RuntimeInstaller};
use serde_json::json;
use sha1::{Digest, Sha1};
use shtmcl_core::{
    launch::{
        catalog::ManifestFetcher,
        java::JavaRequirement,
        plan::{InstanceLaunchSnapshot, LoaderKind},
        ports::{AuthRequirement, InstanceSnapshotSource},
    },
    tasks::TaskSpec,
    CoreApp, CoreModule, ModuleContext,
};
use shtmcl_domain::{
    CommandRequest, CommandResponse, ErrorEnvelope, InstanceId, ModuleId, OperationId, RequestId,
    Revision, Task, TaskId, TaskPhase, TaskState,
};
use shtmcl_platform::PlatformDirs;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "shtmcl-game-launch-regression-{}",
            shtmcl_platform::entropy::random_hex(12).unwrap()
        )))
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct NoNetwork;
impl ManifestFetcher for NoNetwork {
    fn fetch(&self, _: &str, _: usize) -> Result<Vec<u8>, String> {
        panic!("fixture must remain offline")
    }
}

struct Snapshot;
impl InstanceSnapshotSource for Snapshot {
    fn snapshot(&self, id: &InstanceId) -> Result<InstanceLaunchSnapshot, ErrorEnvelope> {
        Ok(InstanceLaunchSnapshot {
            instance_id: id.clone(),
            revision: Revision::new(7),
            content_revision: None,
            name: "Controlled fixture".into(),
            game_version: "1.7.10".into(),
            version_type: Some("release".into()),
            loader: LoaderKind::Vanilla,
            game_dir: PathBuf::new(),
            main_class: "unused".into(),
            classpath: vec![],
            assets_dir: None,
            asset_index: None,
            natives_dir: None,
            jvm_args: vec![],
            game_args: vec![],
            memory_mb: None,
            java_requirement: Some(JavaRequirement::Exact(8)),
            java_path: None,
            required_files: vec![],
            auth: AuthRequirement::Offline,
            offline_username: Some("TestPlayer".into()),
            multi_open: false,
            profile_ref: None,
        })
    }
}

#[derive(Clone, Copy)]
enum ChildResult {
    Cancelled,
    EmptySuccess,
    Success,
}
struct Child {
    ctx: ModuleContext,
    result: ChildResult,
    payload: Arc<Mutex<Option<CommandRequest>>>,
}
impl CoreModule for Child {
    fn id(&self) -> ModuleId {
        ModuleId::Launch
    }
    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        assert_eq!(request.payload["command"], "launch.prepare");
        *self.payload.lock().unwrap() = Some(request.clone());
        let task = self.ctx.tasks().accept(TaskSpec::new(
            request.operation_id.clone().unwrap(),
            TaskPhase::new("prepare").unwrap(),
        ))?;
        self.ctx
            .tasks()
            .transition(&task.id, task.attempt_generation, TaskState::Running)?;
        match self.result {
            ChildResult::Cancelled => {
                self.ctx
                    .tasks()
                    .request_cancel(&shtmcl_domain::CancelTaskRequest {
                        task_id: task.id.clone(),
                        attempt_generation: task.attempt_generation,
                    });
                self.ctx
                    .tasks()
                    .cancel_now(&task.id, task.attempt_generation)?;
            }
            ChildResult::EmptySuccess => {
                self.ctx
                    .tasks()
                    .succeed(&task.id, task.attempt_generation, None)?;
            }
            ChildResult::Success => {
                self.ctx.tasks().succeed(
                    &task.id,
                    task.attempt_generation,
                    Some(json!({"pid":123,"fixture":true})),
                )?;
            }
        }
        Ok(CommandResponse::Accepted {
            operation_id: request.operation_id.clone().unwrap(),
            task_id: task.id,
        })
    }
}

fn cached_fixture_runtime(dirs: &PlatformDirs) {
    let java_root = dirs.data.join("java");
    std::fs::create_dir_all(&java_root).unwrap();
    let bytes = b"fixture-java";
    let hash = format!("{:x}", Sha1::digest(bytes));
    let manifest = serde_json::to_vec(&json!({"files":{"bin/java.exe":{"type":"file","executable":true,
        "downloads":{"raw":{"sha1":hash,"size":bytes.len(),"url":format!("https://piston-data.mojang.com/v1/objects/{hash}/java.exe")}}}}})).unwrap();
    let manifest_hash = format!("{:x}", Sha1::digest(&manifest));
    let catalog = json!({"windows-x64":{"jre-legacy":[{"version":{"name":"1.8.0_51","released":"2025-01-01"},
        "manifest":{"sha1":manifest_hash,"size":manifest.len(),"url":format!("https://piston-meta.mojang.com/v1/packages/{manifest_hash}/manifest.json")}}]}});
    std::fs::write(
        java_root.join("catalog.json"),
        serde_json::to_vec(&catalog).unwrap(),
    )
    .unwrap();
    std::fs::write(
        java_root.join(format!("manifest-{manifest_hash}.json")),
        &manifest,
    )
    .unwrap();
    let target = java_root.join(format!("8-{manifest_hash}"));
    std::fs::create_dir_all(target.join("bin")).unwrap();
    std::fs::write(target.join("bin/java.exe"), bytes).unwrap();
}

fn launcher(
    ctx: &ModuleContext,
    mode: ChildResult,
) -> (GameLauncher, Arc<Mutex<Option<CommandRequest>>>) {
    let payload = Arc::new(Mutex::new(None));
    let child: Arc<dyn CoreModule> = Arc::new(Child {
        ctx: ctx.clone(),
        result: mode,
        payload: payload.clone(),
    });
    let runtime = Arc::new(RuntimeInstaller::new(
        ctx.clone(),
        child.clone(),
        child,
        Arc::new(NoNetwork),
    ));
    (
        GameLauncher::new(ctx.clone(), runtime, Some(Arc::new(Snapshot))),
        payload,
    )
}

fn request(operation: &str) -> CommandRequest {
    CommandRequest {
        request_id: RequestId::new(operation).unwrap(),
        operation_id: Some(OperationId::new(operation).unwrap()),
        entity_id: None,
        expected_revision: Some(Revision::new(7)),
        payload: json!({"command":"launch.game.start","instanceId":"school","profileRef":"chosen-profile",
            "quickPlayAddress":"example.test:25565","idempotencyKey":"chosen-key"}),
    }
}

fn wait(ctx: &ModuleContext, id: &TaskId) -> Task {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let task = ctx.tasks().snapshot(id).expect("parent task");
        if matches!(
            task.state,
            TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
        ) {
            return task;
        }
        assert!(
            Instant::now() < deadline,
            "parent remained {:?}",
            task.state
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn accepted(response: CommandResponse) -> TaskId {
    match response {
        CommandResponse::Accepted { task_id, .. } => task_id,
        other => panic!("expected accepted, got {other:?}"),
    }
}

#[test]
fn child_cancellation_without_parent_cancellation_settles_as_failed() {
    let temp = Temp::new();
    let app = CoreApp::new(PlatformDirs::under(&temp.0));
    let ctx = app.context();
    cached_fixture_runtime(ctx.dirs());
    let (module, _) = launcher(&ctx, ChildResult::Cancelled);
    let id = accepted(module.handle(&request("child-cancel-regression")).unwrap());
    assert_eq!(wait(&ctx, &id).state, TaskState::Failed);
}

#[test]
fn empty_success_result_settles_as_failed_instead_of_staying_running() {
    let temp = Temp::new();
    let app = CoreApp::new(PlatformDirs::under(&temp.0));
    let ctx = app.context();
    cached_fixture_runtime(ctx.dirs());
    let (module, _) = launcher(&ctx, ChildResult::EmptySuccess);
    let id = accepted(module.handle(&request("empty-result-regression")).unwrap());
    assert_eq!(wait(&ctx, &id).state, TaskState::Failed);
}

#[test]
fn launch_options_and_revision_are_preserved_to_real_launch_boundary() {
    let temp = Temp::new();
    let app = CoreApp::new(PlatformDirs::under(&temp.0));
    let ctx = app.context();
    cached_fixture_runtime(ctx.dirs());
    let (module, payload) = launcher(&ctx, ChildResult::Success);
    let id = accepted(
        module
            .handle(&request("forward-options-regression"))
            .unwrap(),
    );
    assert_eq!(wait(&ctx, &id).state, TaskState::Succeeded);
    let request = payload.lock().unwrap().clone().unwrap();
    assert_eq!(request.payload["profileRef"], "chosen-profile");
    assert_eq!(request.payload["quickPlayAddress"], "example.test:25565");
    assert_eq!(request.payload["idempotencyKey"], "chosen-key");
    assert_eq!(request.payload["expectedOfflineUsername"], "TestPlayer");
    assert_eq!(request.expected_revision, Some(Revision::new(7)));
}

#[test]
fn stale_revision_fails_before_runtime_network_or_child_launch() {
    let temp = Temp::new();
    let app = CoreApp::new(PlatformDirs::under(&temp.0));
    let ctx = app.context();
    let (module, payload) = launcher(&ctx, ChildResult::Success);
    let mut input = request("stale-revision-regression");
    input.expected_revision = Some(Revision::new(6));
    let id = accepted(module.handle(&input).unwrap());
    assert_eq!(wait(&ctx, &id).state, TaskState::Failed);
    assert!(payload.lock().unwrap().is_none());
    assert!(!ctx.dirs().data.join("java").exists());
}

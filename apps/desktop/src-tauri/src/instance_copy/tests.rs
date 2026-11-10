use super::*;
use shtmcl_core::{
    instances::{FileStorage, InstancesModule},
    CoreApp,
};
use shtmcl_platform::PlatformDirs;

struct Fixture {
    ctx: ModuleContext,
    module: Arc<dyn CoreModule>,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-copy-test-{}",
            shtmcl_platform::entropy::random_hex(12).unwrap()
        ));
        let dirs = PlatformDirs::under(&root);
        dirs.create_all().unwrap();
        let app = CoreApp::new(dirs);
        let ctx = app.context();
        let module: Arc<dyn CoreModule> = Arc::new(
            InstancesModule::new(
                ctx.clone(),
                Box::new(FileStorage::new(root.join("index")).unwrap()),
            )
            .unwrap(),
        );
        let source = ctx.dirs().instances.join("source");
        for name in ["mods", "saves/world", "versions/test", "logs"] {
            fs::create_dir_all(source.join(name)).unwrap();
        }
        fs::write(source.join("mods/example.jar"), "original mod").unwrap();
        fs::write(source.join("saves/world/level.dat"), "world").unwrap();
        fs::write(source.join("logs/latest.log"), "private log").unwrap();
        fs::write(source.join("accounts.json"), "secret").unwrap();
        fs::write(source.join("options.txt"), "music:0.5").unwrap();
        fs::write(source.join("shtmcl-install.json"), serde_json::to_vec(&json!({"schemaVersion":1,"complete":true,"instanceId":"source","versionId":"test"})).unwrap()).unwrap();
        command(module.as_ref(), Some(&OperationId::new("fixture-create").unwrap()), json!({"command":"instances.create","instance":{"id":"source","name":"源实例","version":{"gameVersion":"test","loader":"vanilla"},"location":{"root":source},"javaMajor":8,"identityId":"private-player"}})).unwrap();
        Self { ctx, module, root }
    }
    fn copy(&self, saves: bool, cancelled: &dyn Fn() -> bool) -> Result<PathBuf, CopyFailure> {
        let input = CopyRequest {
            source: "source".into(),
            target: "copied".into(),
            name: "副本".into(),
            saves,
        };
        let op = OperationId::new("copy-test").unwrap();
        let mut permit = SourcePermit::acquire(
            self.module.clone(),
            input.source.clone(),
            "copy-holder".into(),
            op.clone(),
        )
        .unwrap();
        copy_instance(
            &self.ctx,
            self.module.as_ref(),
            &input,
            &op,
            &mut permit,
            cancelled,
        )
    }
    fn assert_no_copy(&self) {
        assert!(!self.ctx.dirs().instances.join("copied").exists());
        assert!(snapshot(self.module.as_ref(), "copied").is_err());
        assert!(!fs::read_dir(&self.ctx.dirs().instances).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".copy-")));
        assert_eq!(
            snapshot(self.module.as_ref(), "source").unwrap().state,
            InstanceState::Ready
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn copy_is_independent_rewrites_receipt_and_excludes_private_files() {
    let f = Fixture::new();
    let target = f
        .copy(false, &|| false)
        .unwrap_or_else(|_| panic!("copy failed"));
    let receipt: Value =
        serde_json::from_slice(&fs::read(target.join("shtmcl-install.json")).unwrap()).unwrap();
    assert_eq!(receipt["instanceId"], "copied");
    assert!(!target.join("saves").exists());
    assert!(!target.join("logs").exists());
    assert!(!target.join("accounts.json").exists());
    assert_eq!(fs::read(target.join("options.txt")).unwrap(), b"music:0.5");
    fs::write(target.join("mods/example.jar"), "changed").unwrap();
    assert_eq!(
        fs::read(f.ctx.dirs().instances.join("source/mods/example.jar")).unwrap(),
        b"original mod"
    );
    let copy = snapshot(f.module.as_ref(), "copied").unwrap();
    assert_eq!(copy.name, "副本");
    assert!(copy.identity_id.is_none());
    assert_eq!(
        snapshot(f.module.as_ref(), "source").unwrap().state,
        InstanceState::Ready
    );
}

#[test]
fn saves_are_explicit_and_target_conflicts_preserve_existing_files() {
    let f = Fixture::new();
    let target = f
        .copy(true, &|| false)
        .unwrap_or_else(|_| panic!("copy failed"));
    assert!(target.join("saves/world/level.dat").exists());
    fs::write(target.join("marker"), "keep").unwrap();
    assert!(f.copy(true, &|| false).is_err());
    assert_eq!(fs::read(target.join("marker")).unwrap(), b"keep");
}

#[test]
fn cancellation_cleans_owned_staging_and_releases_source() {
    let f = Fixture::new();
    let calls = std::cell::Cell::new(0);
    let cancel = || {
        calls.set(calls.get() + 1);
        calls.get() > 5
    };
    assert!(matches!(
        f.copy(false, &cancel),
        Err(CopyFailure::Cancelled)
    ));
    f.assert_no_copy();
}

#[test]
fn invalid_receipt_cannot_be_published() {
    let f = Fixture::new();
    fs::write(
        f.ctx.dirs().instances.join("source/shtmcl-install.json"),
        r#"{"schemaVersion":1,"complete":true,"instanceId":"wrong","versionId":"test"}"#,
    )
    .unwrap();
    assert!(f.copy(false, &|| false).is_err());
    f.assert_no_copy();
}

#[test]
fn running_source_is_refused_before_acceptance() {
    let f = Fixture::new();
    command(f.module.as_ref(),Some(&OperationId::new("game-running").unwrap()),json!({"command":"instances.lease.acquire","instanceId":"source","holder":"running","purpose":"launch","ttlMs":60_000})).unwrap();
    let wrapper = InstanceCopies::new(f.module.clone(), f.ctx.clone());
    let error=command(&wrapper,Some(&OperationId::new("refused").unwrap()),json!({"command":"instances.copy","sourceInstanceId":"source","newInstanceId":"copied","name":"副本","includeSaves":false})).unwrap_err();
    assert_eq!(error.code.as_str(), "instances.instance.locked");
}

struct RejectCreate(Arc<dyn CoreModule>);
impl CoreModule for RejectCreate {
    fn id(&self) -> ModuleId {
        self.0.id()
    }
    fn handle(&self, r: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        if r.payload["command"] == "instances.create" {
            Err(ErrorEnvelope::internal_unavailable())
        } else {
            self.0.handle(r)
        }
    }
}
#[test]
fn registration_failure_rolls_back_published_directory() {
    let f = Fixture::new();
    let reject = RejectCreate(f.module.clone());
    let op = OperationId::new("registration-failure").unwrap();
    let mut permit = SourcePermit::acquire(
        f.module.clone(),
        "source".into(),
        "holder".into(),
        op.clone(),
    )
    .unwrap();
    assert!(copy_instance(
        &f.ctx,
        &reject,
        &CopyRequest {
            source: "source".into(),
            target: "copied".into(),
            name: "副本".into(),
            saves: false
        },
        &op,
        &mut permit,
        &|| false
    )
    .is_err());
    drop(permit);
    f.assert_no_copy();
}

struct CommitThenError(Arc<dyn CoreModule>);
impl CoreModule for CommitThenError {
    fn id(&self) -> ModuleId {
        self.0.id()
    }
    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let response = self.0.handle(request)?;
        if request.payload["command"] == "instances.create" {
            Err(ErrorEnvelope::internal_unavailable())
        } else {
            Ok(response)
        }
    }
}

#[test]
fn registration_error_after_commit_does_not_remove_indexed_files() {
    let f = Fixture::new();
    let op = OperationId::new("post-commit-error").unwrap();
    let mut permit = SourcePermit::acquire(
        f.module.clone(),
        "source".into(),
        "holder".into(),
        op.clone(),
    )
    .unwrap();
    let path = copy_instance(
        &f.ctx,
        &CommitThenError(f.module.clone()),
        &CopyRequest {
            source: "source".into(),
            target: "copied".into(),
            name: "副本".into(),
            saves: false,
        },
        &op,
        &mut permit,
        &|| false,
    )
    .unwrap_or_else(|_| panic!("committed instance was rolled back"));
    assert!(path.join("shtmcl-install.json").exists());
    assert_eq!(
        snapshot(f.module.as_ref(), "copied").unwrap().location.root,
        path.to_string_lossy()
    );
}

#[test]
fn install_destination_lock_prevents_concurrent_publication() {
    let f = Fixture::new();
    let lock = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(f.ctx.dirs().instances.join(".install-copied.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert!(matches!(
        f.copy(false, &|| false),
        Err(CopyFailure::Core(_))
    ));
    f.assert_no_copy();
}

struct PauseSourceRead {
    inner: Arc<dyn CoreModule>,
    reads: std::sync::atomic::AtomicUsize,
    gate: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}
impl CoreModule for PauseSourceRead {
    fn id(&self) -> ModuleId {
        self.inner.id()
    }
    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        if request.payload["command"] == "instances.snapshot"
            && request.payload["instanceId"] == "source"
            && self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1
        {
            let (mutex, changed) = self.gate.as_ref();
            let permitted = mutex.lock().unwrap();
            let (permitted, timeout) = changed
                .wait_timeout_while(permitted, Duration::from_secs(5), |ready| !*ready)
                .unwrap();
            assert!(
                *permitted && !timeout.timed_out(),
                "test cancellation gate timed out"
            );
        }
        self.inner.handle(request)
    }
}

#[test]
fn shared_task_cancellation_finishes_after_cleanup_and_source_release() {
    let f = Fixture::new();
    let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let paused: Arc<dyn CoreModule> = Arc::new(PauseSourceRead {
        inner: f.module.clone(),
        reads: std::sync::atomic::AtomicUsize::new(0),
        gate: gate.clone(),
    });
    let wrapper = InstanceCopies::new(paused, f.ctx.clone());
    let response = wrapper.handle(&CommandRequest {
        request_id: RequestId::new("cancel-request").unwrap(), operation_id: Some(OperationId::new("cancel-copy-task").unwrap()),
        entity_id: None, expected_revision: None,
        payload: json!({"command":"instances.copy","sourceInstanceId":"source","newInstanceId":"copied","name":"副本","includeSaves":false}),
    }).unwrap();
    let CommandResponse::Accepted { task_id, .. } = response else {
        panic!("expected accepted task")
    };
    let current = f.ctx.tasks().snapshot(&task_id).unwrap();
    f.ctx
        .tasks()
        .request_cancel(&shtmcl_domain::CancelTaskRequest {
            task_id: task_id.clone(),
            attempt_generation: current.attempt_generation,
        });
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let task = f.ctx.tasks().snapshot(&task_id).unwrap();
        if task.state == TaskState::Cancelled {
            break;
        }
        assert!(!matches!(
            task.state,
            TaskState::Failed | TaskState::Succeeded
        ));
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    f.assert_no_copy();
}

#[cfg(windows)]
#[test]
fn junctions_are_refused_without_copying_external_data() {
    let f = Fixture::new();
    let outside = f.root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("marker"), "outside").unwrap();
    let link = f.ctx.dirs().instances.join("source/mods/linked");
    let status = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link.to_string_lossy().replace('/', "\\"))
        .arg(outside.to_string_lossy().replace('/', "\\"))
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "junction creation failed: {} {}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(f.copy(false, &|| false).is_err());
    f.assert_no_copy();
    assert_eq!(fs::read(outside.join("marker")).unwrap(), b"outside");
    fs::remove_dir(link).unwrap();
}

#[test]
fn request_validation_rejects_paths_devices_and_implicit_save_choices() {
    for id in ["../target", "C:\\target", "CON", "lPt1", ""] {
        assert!(CopyRequest::parse(&json!({"sourceInstanceId":"source","newInstanceId":id,"name":"test","includeSaves":false})).is_err());
    }
    assert!(CopyRequest::parse(
        &json!({"sourceInstanceId":"source","newInstanceId":"target","name":"test"})
    )
    .is_err());
}

#[test]
#[ignore = "explicit real local game copy acceptance; no network or game process"]
fn real_game_copy_via_production_dispatch_and_launch_snapshot() {
    use shtmcl_core::launch::ports::{AuthRequirement, InstanceSnapshotSource};
    use shtmcl_domain::TaskId;
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.ancestors().nth(3).unwrap().to_path_buf();
    let original = root.join("work/validation/environment-library/20261022-01/real-cache-root/instances/offline-from-environment");
    assert!(
        original.join("shtmcl-install.json").exists(),
        "real game fixture missing"
    );
    let evidence = root.join("work/validation/instance-copy/20261022-01");
    let isolated = evidence.join(format!(
        "real-copy-{}",
        shtmcl_platform::entropy::random_hex(6).unwrap()
    ));
    let dirs = PlatformDirs::under(&isolated);
    dirs.create_all().unwrap();
    let source = dirs.instances.join("offline-from-environment");
    let mut budget = CopyBudget {
        entries: 0,
        bytes: 0,
    };
    copy_tree(&original, &source, 0, &mut budget, &mut || Ok(()))
        .unwrap_or_else(|_| panic!("fixture setup failed"));
    let state = crate::AppState::new(dirs.clone());
    let op = OperationId::new("real-game-copy").unwrap();
    state.core.dispatch_mutation(ModuleId::Instances,&CommandRequest {request_id:RequestId::new("real-copy-source").unwrap(),operation_id:Some(op.clone()),entity_id:None,expected_revision:None,payload:json!({"command":"instances.create","instance":{"id":"offline-from-environment","name":"远古原版","version":{"gameVersion":"c0.0.11a","loader":"vanilla"},"location":{"root":source},"javaMajor":8}})}).unwrap();
    let response=state.core.dispatch_mutation(ModuleId::Instances,&CommandRequest {request_id:RequestId::new("real-copy-request").unwrap(),operation_id:Some(op),entity_id:None,expected_revision:None,payload:json!({"command":"instances.copy","sourceInstanceId":"offline-from-environment","newInstanceId":"real-copy","name":"远古原版副本","includeSaves":false})}).unwrap();
    let CommandResponse::Accepted { task_id, .. } = response else {
        panic!("expected accepted")
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    let ctx = state.core.context();
    let task = loop {
        let task = ctx.tasks().snapshot(&task_id).unwrap();
        if matches!(
            task.state,
            TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
        ) {
            break task;
        }
        assert!(Instant::now() < deadline, "copy timeout");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(task.state, TaskState::Succeeded, "{:?}", task.error);
    struct Indexed(crate::AppState);
    impl CoreModule for Indexed {
        fn id(&self) -> ModuleId {
            ModuleId::Instances
        }
        fn handle(&self, r: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
            self.0.core.dispatch_read(ModuleId::Instances, r)
        }
    }
    let adapter = crate::launch_instances::InstalledInstances::new(
        Arc::new(Indexed(state)),
        dirs.clone(),
        crate::launch_instances::LaunchOptions {
            auth: AuthRequirement::Offline,
            offline_username: Arc::new(std::sync::RwLock::new(Some("CopyTest".into()))),
            memory_mb: Some(1024),
            java_path: None,
        },
    );
    let launch = adapter
        .snapshot(&InstanceId::new("real-copy").unwrap())
        .unwrap();
    assert_eq!(launch.instance_id.as_str(), "real-copy");
    assert_eq!(launch.game_version, "c0.0.11a");
    let copied = dirs.instances.join("real-copy");
    let mut files = 0;
    fn compare(a: &Path, b: &Path, count: &mut usize) {
        for entry in fs::read_dir(a).unwrap() {
            let entry = entry.unwrap();
            let target = b.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                compare(&entry.path(), &target, count)
            } else if entry.file_name() != "shtmcl-install.json" {
                assert_eq!(fs::read(entry.path()).unwrap(), fs::read(target).unwrap());
                *count += 1;
            }
        }
    }
    compare(&source, &copied, &mut files);
    let result = json!({"root":isolated,"taskId":TaskId::as_str(&task_id),"state":"succeeded","comparedFiles":files,"copiedBytes":budget.bytes,"launchSnapshot":"passed","gameVersion":"c0.0.11a","gameProcessStarted":false});
    fs::write(
        evidence.join("real-copy-result.json"),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
    println!("{}", result);
}

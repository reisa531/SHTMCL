//! Opt-in Windows acceptance: a real Java game outlives its crashed host.
//! This uses the production composition root, without a WebView or synthetic game.
#![cfg(windows)]

use crate::AppState;
use serde_json::{json, Value};
use shtmcl_domain::{
    CommandRequest, CommandResponse, ModuleId, OperationId, RequestId, Revision, TaskState,
};
use shtmcl_platform::{
    process_identity::{self, ProcessIdentity, ProcessLiveness},
    PlatformDirs,
};
use std::os::windows::process::CommandExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn request(command: &str, payload: Value) -> CommandRequest {
    CommandRequest {
        request_id: RequestId::new("restart-acceptance").unwrap(),
        operation_id: Some(
            OperationId::new(format!(
                "restart-{}",
                shtmcl_platform::entropy::random_hex(8).unwrap()
            ))
            .unwrap(),
        ),
        entity_id: None,
        expected_revision: None,
        payload: {
            let mut p = payload;
            p["command"] = json!(command);
            p
        },
    }
}
fn read(state: &AppState, command: &str, instance: &str) -> Value {
    state.refresh_running_games();
    match state
        .core
        .dispatch_read(
            ModuleId::Instances,
            &request(command, json!({"instanceId":instance})),
        )
        .unwrap()
    {
        CommandResponse::Completed { result } => result,
        other => panic!("expected read response: {other:?}"),
    }
}
fn evidence(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}
fn journal_evidence(root: &Path) -> Value {
    let entries = fs::read_dir(root.join("data/running-games")).expect("running journal directory");
    let mut records = Vec::new();
    for entry in entries {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            assert!(fs::metadata(&path).unwrap().len() <= 64 * 1024);
            let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            records.push(value);
        }
        assert!(records.len() <= 4096);
    }
    json!(records)
}
fn identity_from_file(path: &Path) -> Option<ProcessIdentity> {
    let v: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    Some(ProcessIdentity {
        pid: u32::try_from(v["pid"].as_u64()?).ok()?,
        creation_time: v["creationTime"].as_u64()?,
    })
}

/// Retains one OS handle for identity verification and termination: PID reuse
/// cannot make the termination select an unrelated process.
fn stop_owned_game(identity: ProcessIdentity) -> std::io::Result<()> {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetProcessTimes(
            handle: *mut c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn TerminateProcess(handle: *mut c_void, code: u32) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    }
    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    if process_identity::probe(&identity) == ProcessLiveness::Exited {
        return Ok(());
    }
    // SAFETY: scalar arguments, inheritance disabled; uniquely owned handle.
    let handle = Handle(unsafe { OpenProcess(0x1000 | 0x0010_0000 | 0x0001, 0, identity.pid) });
    if handle.0.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let (mut creation, mut exit, mut kernel, mut user) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    // SAFETY: live handle and writable FileTime outputs.
    if unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if (u64::from(creation.high) << 32) | u64::from(creation.low) != identity.creation_time {
        return Err(std::io::Error::other("owned game identity changed"));
    }
    // SAFETY: the same handle was checked above; it belongs to our test game.
    if unsafe { WaitForSingleObject(handle.0, 0) } != 0
        && unsafe { TerminateProcess(handle.0, 93) } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { WaitForSingleObject(handle.0, 10_000) } != 0 {
        return Err(std::io::Error::other("owned game exit timeout"));
    }
    Ok(())
}

struct Cleanup {
    host: Child,
    identity_file: PathBuf,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        // Stop the unique host first so it cannot create another game while we
        // inspect its identity artifact. Never enumerate or kill other games.
        if self.host.try_wait().ok().flatten().is_none() {
            let _ = self.host.kill();
        }
        let _ = self.host.wait();
        if let Some(identity) = identity_from_file(&self.identity_file) {
            let _ = stop_owned_game(identity);
        }
    }
}

#[test]
#[ignore = "helper for the explicit real-game crash controller; never invoke standalone"]
fn crash_host_helper() {
    assert_eq!(std::env::var("SHTMCL_CRASH_HELPER").as_deref(), Ok("1"));
    let root = PathBuf::from(std::env::var_os("SHTMCL_RESTART_TEST_ROOT").expect("explicit root"));
    let instance =
        std::env::var("SHTMCL_INSTALL_INSTANCE").unwrap_or_else(|_| "actual-launch-1710".into());
    let ready =
        PathBuf::from(std::env::var_os("SHTMCL_CRASH_READY_FILE").expect("controller ready path"));
    let state = AppState::new(PlatformDirs::under(&root));
    state
        .core
        .dispatch_mutation(
            ModuleId::Launch,
            &request(
                "launch.identity.select",
                json!({"kind":"offline","username":"SHTMCL_Test"}),
            ),
        )
        .unwrap();
    let task = match state
        .core
        .dispatch_mutation(
            ModuleId::Launch,
            &request("launch.game.start", json!({"instanceId":instance})),
        )
        .unwrap()
    {
        CommandResponse::Accepted { task_id, .. } => task_id,
        other => panic!("expected start task: {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(1200);
    loop {
        let snapshot = state.core.context().tasks().snapshot(&task).unwrap();
        // Capture as soon as a process appears, before waiting for task success.
        let sessions = state
            .core
            .dispatch_read(
                ModuleId::Launch,
                &request("launch.sessions", json!({"instanceId":instance})),
            )
            .unwrap();
        if let CommandResponse::Completed { result } = sessions {
            if let Some(pid) = result["sessions"]
                .as_array()
                .and_then(|v| v.first())
                .and_then(|v| v["pid"].as_u64())
            {
                let identity = process_identity::capture(u32::try_from(pid).unwrap()).unwrap();
                let path = ready.with_extension("identity.json");
                if !path.exists() {
                    evidence(
                        &path,
                        &json!({"pid":identity.pid,"creationTime":identity.creation_time,"instance":instance}),
                    );
                }
            }
        }
        if snapshot.state == TaskState::Succeeded {
            break;
        }
        assert!(
            !matches!(snapshot.state, TaskState::Failed | TaskState::Cancelled),
            "launch failed: {snapshot:?}"
        );
        assert!(Instant::now() < deadline, "helper launch timeout");
        std::thread::sleep(Duration::from_millis(100));
    }
    let snapshot = read(&state, "instances.snapshot", &instance);
    assert_eq!(snapshot["instance"]["state"], "busy");
    evidence(&ready, &snapshot);
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
#[ignore = "starts real Minecraft, crashes only its own host, and verifies restart protection"]
fn real_game_survives_host_crash_and_recovers() {
    let root = PathBuf::from(
        std::env::var_os("SHTMCL_RESTART_TEST_ROOT").expect("explicit retained root"),
    );
    let instance =
        std::env::var("SHTMCL_INSTALL_INSTANCE").unwrap_or_else(|_| "actual-launch-1710".into());
    let dir = PathBuf::from(
        std::env::var_os("SHTMCL_CRASH_EVIDENCE_DIR").expect("explicit evidence directory"),
    );
    assert!(root.is_absolute() && dir.is_absolute());
    fs::create_dir_all(&dir).unwrap();
    let initial = AppState::new(PlatformDirs::under(&root));
    assert_eq!(
        read(&initial, "instances.snapshot", &instance)["instance"]["state"],
        "ready",
        "test requires an idle installed instance"
    );
    drop(initial);
    let run = shtmcl_platform::entropy::random_hex(12).unwrap();
    let ready = dir.join(format!("{run}-host-ready.json"));
    let identity_file = ready.with_extension("identity.json");
    let host = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "restart_acceptance_tests::crash_host_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("SHTMCL_CRASH_HELPER", "1")
        .env("SHTMCL_CRASH_READY_FILE", &ready)
        .stdin(Stdio::null())
        .stdout(fs::File::create(dir.join(format!("{run}-host.log"))).unwrap())
        .stderr(fs::File::create(dir.join(format!("{run}-host-stderr.log"))).unwrap())
        .creation_flags(0x0800_0000)
        .spawn()
        .unwrap();
    let mut cleanup = Cleanup {
        host,
        identity_file,
    };
    let deadline = Instant::now() + Duration::from_secs(1250);
    while !ready.exists() {
        assert!(
            cleanup.host.try_wait().unwrap().is_none(),
            "helper exited before ready"
        );
        assert!(Instant::now() < deadline, "controller ready timeout");
        std::thread::sleep(Duration::from_millis(100));
    }
    let identity = identity_from_file(&cleanup.identity_file).expect("owned game identity");
    assert_eq!(process_identity::probe(&identity), ProcessLiveness::Alive);
    evidence(
        &dir.join(format!("{run}-before-crash-journal.json")),
        &journal_evidence(&root),
    );
    cleanup.host.kill().unwrap();
    let host_exit = cleanup.host.wait().unwrap();
    assert!(!host_exit.success(), "host must be terminated abruptly");
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        process_identity::probe(&identity),
        ProcessLiveness::Alive,
        "game survived host termination"
    );
    let restored = AppState::new(PlatformDirs::under(&root));
    let busy = read(&restored, "instances.snapshot", &instance);
    assert_eq!(busy["instance"]["state"], "busy");
    evidence(
        &dir.join(format!("{run}-protected-journal.json")),
        &journal_evidence(&root),
    );
    let mut update = request(
        "instances.update",
        json!({"instanceId":instance,"patch":{"name":"Forbidden crash edit"}}),
    );
    update.expected_revision = Some(Revision::new(
        busy["instance"]["revision"].as_u64().unwrap(),
    ));
    let refusal = restored
        .core
        .dispatch_mutation(ModuleId::Instances, &update)
        .unwrap_err();
    assert_eq!(refusal.code.as_str(), "instances.instance.running");
    evidence(
        &dir.join(format!("{run}-after-host-crash.json")),
        &json!({"instance":busy,"pid":identity.pid,"creationTime":identity.creation_time,"gameAlive":true,"hostExit":host_exit.code(),"mutationRefusal":refusal.code.as_str()}),
    );
    stop_owned_game(identity).unwrap();
    assert_eq!(process_identity::probe(&identity), ProcessLiveness::Exited);
    restored
        .lease_recovery
        .as_ref()
        .expect("persistent recovery")
        .recover()
        .unwrap();
    let ready_snapshot = read(&restored, "instances.snapshot", &instance);
    assert_eq!(ready_snapshot["instance"]["state"], "ready");
    evidence(
        &dir.join(format!("{run}-after-game-exit.json")),
        &ready_snapshot,
    );
    evidence(
        &dir.join(format!("{run}-recovered-journal.json")),
        &journal_evidence(&root),
    );
    println!(
        "crash_acceptance_passed run={run} pid={} creation_time={} instance={instance}",
        identity.pid, identity.creation_time
    );
}

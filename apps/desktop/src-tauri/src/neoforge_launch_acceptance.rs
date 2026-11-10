// Ignored real-Java launch acceptance harness (module neoforge_launch_acceptance).
// Run manually with required SHTMCL_NEOFORGE_LAUNCH_* env vars; not executed here.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use shtmcl_core::instances::Instance;
use shtmcl_core::launch::java::{Arch, HostPlatform, JavaProber, JavaRuntime};
use shtmcl_core::launch::plan::{build_plan, PlanInput};
use shtmcl_core::launch::ports::{AuthRequirement, GameSession};
use shtmcl_platform::{ProcessLauncher, ProcessSupervisor, StdProcesses};

use crate::launch_instances::LaunchOptions;

const MAX_CAPTURE: usize = 8 * 1024 * 1024;
const MAX_READ: u64 = 8 * 1024 * 1024;
const READY_DEADLINE: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(200);
const ALIVE_AFTER_READY: Duration = Duration::from_secs(5);
const REAP_LIMIT: Duration = Duration::from_secs(10);

fn renderer_ready(contents: &str) -> bool {
    contents.contains("NeoForge mod loading, version 20.4.253, for MC 1.20.4")
        && contents.contains("Reloading ResourceManager: mod_resources, mod:neoforge, vanilla")
        && contents.contains("Sound engine started")
        && contents.contains("minecraft:textures/atlas/gui.png-atlas")
}

#[test]
fn readiness_requires_loader_sound_and_gui_resources() {
    let observations = [
        "NeoForge mod loading, version 20.4.253, for MC 1.20.4",
        "Reloading ResourceManager: mod_resources, mod:neoforge, vanilla",
        "Sound engine started",
        "Created: 1024x512x0 minecraft:textures/atlas/gui.png-atlas",
    ];
    assert!(renderer_ready(&observations.join("\n")));
    for missing in 0..observations.len() {
        let incomplete = observations
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != missing)
            .map(|(_, entry)| *entry)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!renderer_ready(&incomplete));
    }
}

fn env_required(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("missing required env {key}"))
}

fn read_bounded(path: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut buf = Vec::new();
    let mut limited = (&mut f).take(MAX_READ);
    limited.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn copy_tree_no_links(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    crate::game_install::reject_link(src).map_err(std::io::Error::other)?;
    if meta.is_dir() {
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree_no_links(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else if meta.is_file() {
        fs::copy(src, dst)?;
    } else {
        return Err(std::io::Error::other("non regular input"));
    }
    Ok(())
}

struct LogDrain {
    handle: Option<thread::JoinHandle<std::io::Result<()>>>,
}

fn drain_stream<R: Read + Send + 'static>(mut stream: R, path: PathBuf) -> LogDrain {
    let handle = thread::spawn(move || -> std::io::Result<()> {
        let mut file = fs::File::create(&path)?;
        let mut written: usize = 0;
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if written < MAX_CAPTURE {
                let room = MAX_CAPTURE - written;
                let take = room.min(n);
                file.write_all(&buf[..take])?;
                written += take;
            }
        }
        file.flush()?;
        Ok(())
    });
    LogDrain {
        handle: Some(handle),
    }
}

impl LogDrain {
    fn finish(&mut self) -> std::io::Result<()> {
        if let Some(h) = self.handle.take() {
            match h.join() {
                Ok(r) => r,
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "log drain panicked",
                )),
            }
        } else {
            Ok(())
        }
    }
}

struct StopGuard {
    procs: Arc<StdProcesses>,
    pid: u32,
    stopped: AtomicBool,
}

impl StopGuard {
    fn new(procs: Arc<StdProcesses>, pid: u32) -> Self {
        Self {
            procs,
            pid,
            stopped: AtomicBool::new(false),
        }
    }

    // Forced kill; never described as graceful. Returns wait outcome.
    fn force_stop_and_reap(&self) -> std::io::Result<shtmcl_platform::WaitOutcome> {
        if self.procs.is_alive(self.pid) {
            self.procs.request_stop(self.pid)?;
        }
        let outcome = self.procs.wait_for_exit(self.pid, Some(REAP_LIMIT));
        if matches!(outcome, shtmcl_platform::WaitOutcome::Exited(_)) {
            self.stopped.store(true, Ordering::SeqCst);
        }
        Ok(outcome)
    }
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            let _ = self.procs.request_stop(self.pid);
            let _ = self.procs.wait_for_exit(self.pid, Some(REAP_LIMIT));
        }
    }
}

#[test]
#[ignore]
fn prepared_game_reaches_renderer() {
    let source = PathBuf::from(env_required("SHTMCL_NEOFORGE_LAUNCH_SOURCE"));
    let root = PathBuf::from(env_required("SHTMCL_NEOFORGE_LAUNCH_ROOT"));
    let expected_sha = env_required("SHTMCL_NEOFORGE_LAUNCH_SHA256");
    let java_path = PathBuf::from(env_required("SHTMCL_NEOFORGE_LAUNCH_JAVA"));
    assert!(root.is_absolute(), "launch root must be absolute");
    assert!(!root.exists(), "launch root must not already exist");
    assert!(source.is_dir(), "launch source must be a directory");

    let game = root.join("game");
    let report = |name: &str, value: &serde_json::Value| {
        fs::write(root.join(name), value.to_string()).expect("write acceptance report");
    };

    fs::create_dir(&root).expect("create fresh launch root");
    let copy = copy_tree_no_links(&source, &game);
    if let Err(e) = copy {
        report(
            "result.json",
            &json!({"stage":"copy","ok":false,"error":format!("{e}")}),
        );
        panic!("source copy failed: {e}");
    }

    let instance: Instance = serde_json::from_value(json!({
        "id": "neo-acceptance",
        "name": "NeoForge acceptance",
        "version": {"gameVersion": "1.20.4", "loader": "neo_forge", "loaderVersion": "20.4.253"},
        "location": {"root": game},
        "javaMajor": 17,
        "state": "ready",
        "revision": 1,
        "createdAt": 1,
        "updatedAt": 1
    }))
    .expect("fixture Instance deserializes");

    let options = LaunchOptions {
        auth: AuthRequirement::Offline,
        offline_username: Arc::new(RwLock::new(Some("NeoAcceptance".to_string()))),
        memory_mb: Some(2048),
        java_path: Some(java_path.clone()),
    };

    let cancelled = || false;
    let snapshot = match crate::neoforge_snapshot::staged(
        &game,
        &expected_sha,
        &instance,
        &options,
        &cancelled,
    ) {
        Ok(s) => s,
        Err(e) => {
            report(
                "result.json",
                &json!({"stage":"staged","ok":false,"error":e}),
            );
            panic!("staged snapshot failed: {e}");
        }
    };
    let snapshot_json = snapshot.to_json();
    fs::write(root.join("snapshot.json"), snapshot_json.to_string()).unwrap();
    assert!(
        snapshot
            .collect_required()
            .iter()
            .all(|file| file.path.exists()),
        "required file absent before spawn"
    );

    let procs = Arc::new(StdProcesses::capturing());
    let prober = JavaProber::new(procs.clone(), procs.clone());
    let probe = prober.probe(&java_path);
    if !probe.outcome.is_usable() {
        report("result.json", &json!({"stage":"probe","ok":false}));
        panic!("java probe not usable");
    }
    let version = probe.version.expect("probe version present");
    assert!(
        version.major == 17,
        "java major must be 17, got {}",
        version.major
    );
    let arch = probe.arch;
    assert_eq!(arch, Arch::X64);
    let runtime = JavaRuntime {
        id: "acceptance-java".into(),
        path: java_path.clone(),
        platform: HostPlatform::current(),
        arch,
        vendor: probe.vendor,
        version,
        verified_at_unix_ms: 0,
        file_identity: shtmcl_platform::fs::probe_file(&java_path).expect("java file identity"),
    };

    let session = GameSession::offline("NeoAcceptance");
    let plan = match build_plan(
        PlanInput {
            snapshot: &snapshot,
            runtime: &runtime,
            session: &session,
            quick_play_address: None,
        },
        "neo-real".into(),
    ) {
        Ok(p) => p,
        Err(r) => {
            report(
                "result.json",
                &json!({"stage":"plan","ok":false,"refusal":format!("{r:?}")}),
            );
            panic!("build_plan refused: {r:?}");
        }
    };
    fs::write(root.join("plan.json"), plan.summary_json().to_string()).unwrap();
    let request = plan.to_process_request();
    for arg in request.args.iter() {
        assert!(!arg.contains("${"), "unexpanded placeholder in arg: {arg}");
    }

    let started = Instant::now();
    let pid = procs.spawn(&request).expect("spawn java").pid;
    let guard = StopGuard::new(procs.clone(), pid);
    let streams = procs.take_streams(pid).expect("take child streams");
    let mut out_drain = drain_stream(streams.stdout, root.join("stdout.log"));
    let mut err_drain = drain_stream(streams.stderr, root.join("stderr.log"));

    let latest = game.join("logs").join("latest.log");
    let mut readiness = false;
    let mut elapsed;
    let mut exit_reason = "forced_stop";
    loop {
        elapsed = started.elapsed();
        if let Ok(contents) = read_bounded(&latest) {
            if renderer_ready(&contents) {
                readiness = true;
                break;
            }
        }
        if !procs.is_alive(pid) {
            exit_reason = "exited_before_ready";
            break;
        }
        if elapsed >= READY_DEADLINE {
            exit_reason = "readiness_timeout";
            break;
        }
        thread::sleep(POLL);
    }

    if readiness {
        let until = Instant::now() + ALIVE_AFTER_READY;
        while Instant::now() < until {
            if !procs.is_alive(pid) {
                exit_reason = "exited_after_ready";
                break;
            }
            thread::sleep(POLL);
        }
    }

    let stop_result = guard.force_stop_and_reap();
    let (wait_outcome, stop_err) = match stop_result {
        Ok(w) => (format!("{w:?}"), None),
        Err(e) => ("error".to_string(), Some(format!("{e}"))),
    };
    let still_alive = procs.is_alive(pid);
    let drain_errors = if still_alive {
        Some("child could not be stopped; log readers left detached".to_string())
    } else {
        out_drain
            .finish()
            .and_then(|_| err_drain.finish())
            .err()
            .map(|e| e.to_string())
    };
    drop(guard);

    report(
        "result.json",
        &json!({
            "readiness": readiness,
            "elapsedMs": elapsed.as_millis() as u64,
            "pid": pid,
            "exitReason": exit_reason,
            "waitOutcome": wait_outcome,
            "stopError": stop_err,
            "stillAlive": still_alive,
            "drainError": drain_errors,
            "instanceRegistered": false,
            "gameLaunched": true
        }),
    );

    assert!(
        readiness,
        "readiness not reached; see logs (reason={exit_reason})"
    );
    assert!(!still_alive, "java process still alive after forced stop");
    assert_eq!(
        exit_reason, "forced_stop",
        "game did not remain alive after readiness"
    );
    assert!(
        stop_err.is_none() && drain_errors.is_none(),
        "stop/log failure"
    );
}

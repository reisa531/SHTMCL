//! Opt-in real Java cancellation and fresh-attempt validation.
use super::super::neoforge_mappings::MAPPINGS_PATH;
use super::*;
use serde_json::{json, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use shtmcl_platform::{
    ProcessHandle, ProcessLauncher, ProcessRequest, ProcessSupervisor, StdProcesses,
};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_BASE: u64 = 8 * 1024 * 1024;
const MAX_INSTALLER: usize = 64 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(300);

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing required env {name}"))
}
fn path_env(name: &str) -> PathBuf {
    PathBuf::from(env(name))
}
fn sha256(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn sha1(b: &[u8]) -> String {
    format!("{:x}", Sha1::digest(b))
}
fn read_bounded(p: &Path, max: usize) -> Vec<u8> {
    let m = fs::metadata(p).unwrap_or_else(|e| panic!("stat {}: {e}", p.display()));
    assert!(m.len() <= max as u64, "{} exceeds {max} bytes", p.display());
    let b = fs::read(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    assert!(b.len() <= max, "{} exceeds {max} bytes", p.display());
    b
}
fn save_json(dir: &Path, name: &str, v: &Value) {
    let mut f = fs::File::create(dir.join(name)).expect("create result json");
    serde_json::to_writer_pretty(&mut f, v).expect("write result json");
    f.write_all(b"\n").ok();
}

struct RecordingLauncher {
    inner: StdProcesses,
    cancel_after_spawn: bool,
    flag: AtomicBool,
    cancellation_pid: AtomicU32,
    alive_at_cancellation: AtomicBool,
    pids: Mutex<Vec<u32>>,
}
impl RecordingLauncher {
    fn new(cancel_after_spawn: bool) -> Self {
        Self {
            inner: StdProcesses::capturing(),
            cancel_after_spawn,
            flag: AtomicBool::new(false),
            cancellation_pid: AtomicU32::new(0),
            alive_at_cancellation: AtomicBool::new(false),
            pids: Mutex::new(Vec::new()),
        }
    }
    fn pids(&self) -> Vec<u32> {
        self.pids.lock().unwrap().clone()
    }
}
impl ProcessLauncher for RecordingLauncher {
    fn spawn(&self, req: &ProcessRequest) -> io::Result<ProcessHandle> {
        let h = self.inner.spawn(req)?;
        self.pids.lock().unwrap().push(h.pid);
        if self.cancel_after_spawn
            && req
                .args
                .iter()
                .any(|arg| arg == "net.neoforged.jarsplitter.ConsoleTool")
        {
            // Earlier processors must complete; interrupt a later live Java child.
            std::thread::sleep(Duration::from_millis(250));
            self.cancellation_pid.store(h.pid, Ordering::SeqCst);
            self.alive_at_cancellation
                .store(self.inner.is_alive(h.pid), Ordering::SeqCst);
            self.flag.store(true, Ordering::SeqCst);
        }
        Ok(h)
    }
}

fn assert_stopped(inner: &StdProcesses, pids: &[u32]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    for &pid in pids {
        loop {
            if !inner.is_alive(pid) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "java pid {pid} still alive after cancel/stop"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn journal_entries(root: &Path) -> Vec<Value> {
    let jd = root.join("pipeline-journal");
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&jd) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Ok(b) = fs::read(&p) {
                if let Ok(v) = serde_json::from_slice::<Value>(&b) {
                    out.push(v);
                }
            }
        }
    }
    out
}

fn resolver_from(
    completed: &Path,
    runtime: &Path,
) -> impl FnMut(&str) -> Result<PathBuf, StageError> {
    let roots = [completed.to_path_buf(), runtime.to_path_buf()];
    move |rel: &str| {
        for r in roots.iter() {
            let c = r.join("libraries").join(rel);
            if c.is_file() {
                return Ok(c);
            }
        }
        Err(StageError::Invalid(format!("no cached library for {rel}")))
    }
}

#[test]
#[ignore = "requires retained official artifacts and a real Java 17 process"]
fn real_neoforge_transaction_cancel_then_restart() {
    let root = path_env("SHTMCL_NEOFORGE_TRANSACTION_ROOT");
    assert!(
        root.is_dir(),
        "transaction root must pre-exist: {}",
        root.display()
    );
    let completed = path_env("SHTMCL_NEOFORGE_COMPLETED_STAGE");
    let runtime = path_env("SHTMCL_NEOFORGE_RUNTIME_STAGE");
    let java = path_env("SHTMCL_NEOFORGE_RUNNER_JAVA");
    assert!(java.is_absolute() && java.is_file(), "javaw.exe required");
    let mappings = completed.join(MAPPINGS_PATH);
    assert!(
        mappings.is_file(),
        "cached mappings missing at {}",
        mappings.display()
    );

    let installer = read_bounded(
        &completed.join("inputs").join("installer.jar"),
        MAX_INSTALLER,
    );
    assert_eq!(
        sha256(&installer),
        env("SHTMCL_NEOFORGE_INSTALLER_SHA256"),
        "installer sha256 mismatch"
    );
    let mc_path = completed.join("inputs").join("minecraft.jar");
    let mc_meta = fs::metadata(&mc_path).expect("minecraft.jar");
    let mc_bytes = read_bounded(&mc_path, 32 * 1024 * 1024);
    let minecraft = VerifiedInput {
        path: mc_path.clone(),
        size: mc_meta.len(),
        sha1: sha1(&mc_bytes),
    };

    let base = read_bounded(
        &PathBuf::from(env("SHTMCL_NEOFORGE_GAME_METADATA")),
        MAX_BASE as usize,
    );
    assert_eq!(
        sha1(&base),
        env("SHTMCL_NEOFORGE_GAME_METADATA_SHA1"),
        "game metadata sha1 mismatch"
    );
    let base_value: Value = serde_json::from_slice(&base).unwrap();
    assert_eq!(base_value["downloads"]["client"]["sha1"], minecraft.sha1);
    assert_eq!(base_value["downloads"]["client"]["size"], minecraft.size);
    let installer_sha = sha256(&installer);
    let metadata_sha = env("SHTMCL_NEOFORGE_GAME_METADATA_SHA1");

    let cancelled = root.join("cancelled-stage");
    let restarted = root.join("restarted-stage");
    assert!(!cancelled.exists(), "cancelled-stage must not exist");
    assert!(!restarted.exists(), "restarted-stage must not exist");

    // First attempt: interrupt JarSplitter after earlier processors complete.
    let launcher = RecordingLauncher::new(true);
    let phases: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let ph = phases.clone();
    let cancel_flag = &launcher.flag;
    let input1 = ClientInstallInputs {
        destination: &cancelled,
        installer_bytes: &installer,
        installer_sha256: &installer_sha,
        base_metadata_bytes: &base,
        base_metadata_sha1: &metadata_sha,
        minecraft: &minecraft,
        java: &java,
        cached_mappings: &mappings,
    };
    let first = install_client(
        input1,
        resolver_from(&completed, &runtime),
        &launcher,
        &launcher.inner,
        TIMEOUT,
        || cancel_flag.load(Ordering::SeqCst),
        |p: &str| ph.lock().unwrap().push(p.to_string()),
    );
    let pids = launcher.pids();
    assert_eq!(pids.len(), 3);
    assert!(launcher.alive_at_cancellation.load(Ordering::SeqCst));
    assert!(pids.iter().any(|&p| p != 0), "expected recorded java pid");
    assert!(
        matches!(first, Err(PipelineError::Cancelled)),
        "first run must be Cancelled, got {:?}",
        first.as_ref().err()
    );
    assert_stopped(&launcher.inner, &pids);
    assert!(!cancelled.join("neoforge-complete.json").exists());
    let jr = journal_entries(&cancelled);
    assert!(!jr.is_empty(), "journal entries expected");
    let terminal_ok = jr.iter().any(|e| {
        let s = e.get("status").and_then(Value::as_str).unwrap_or("");
        s.to_ascii_lowercase().contains("cancel")
    });
    assert!(
        terminal_ok,
        "expected a failure journal entry with a cancelled status field"
    );
    save_json(
        &root,
        "cancel-result.json",
        &json!({
        "pids": pids, "allStopped": true, "result": "Cancelled",
        "cancellationPid": launcher.cancellation_pid.load(Ordering::SeqCst),
        "aliveAtCancellation": launcher.alive_at_cancellation.load(Ordering::SeqCst),
            "journalEntries": jr, "phaseTrace": *phases.lock().unwrap(),
            "instanceCommitted": false, "gameLaunched": false,
        }),
    );

    // Second attempt against the same cancelled root must fail without launching.
    let before = launcher.pids().len();
    let phases2: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let ph2 = phases2.clone();
    let input2 = ClientInstallInputs {
        destination: &cancelled,
        installer_bytes: &installer,
        installer_sha256: &installer_sha,
        base_metadata_bytes: &base,
        base_metadata_sha1: &metadata_sha,
        minecraft: &minecraft,
        java: &java,
        cached_mappings: &mappings,
    };
    let second = install_client(
        input2,
        resolver_from(&completed, &runtime),
        &launcher,
        &launcher.inner,
        TIMEOUT,
        || false,
        |p: &str| ph2.lock().unwrap().push(p.to_string()),
    );
    assert!(
        second.is_err(),
        "reusing cancelled root must fail, got {:?}",
        second.as_ref().ok().map(|c| &c.root)
    );
    assert_eq!(
        launcher.pids().len(),
        before,
        "no Java launch on reused cancelled root"
    );
    save_json(
        &root,
        "reuse-result.json",
        &json!({
            "failed": true, "error": format!("{:?}", second.err()), "pidsBefore": before,
            "pidsAfter": launcher.pids().len(), "phaseTrace": *phases2.lock().unwrap(),
            "instanceCommitted": false, "gameLaunched": false,
        }),
    );

    // Third: fresh restart from previously verified cached inputs, no cancellation.
    let phases3: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let ph3 = phases3.clone();
    let input3 = ClientInstallInputs {
        destination: &restarted,
        installer_bytes: &installer,
        installer_sha256: &installer_sha,
        base_metadata_bytes: &base,
        base_metadata_sha1: &metadata_sha,
        minecraft: &minecraft,
        java: &java,
        cached_mappings: &mappings,
    };
    let fresh_launcher = RecordingLauncher::new(false);
    let ok = install_client(
        input3,
        resolver_from(&completed, &runtime),
        &fresh_launcher,
        &fresh_launcher.inner,
        TIMEOUT,
        || false,
        |p: &str| ph3.lock().unwrap().push(p.to_string()),
    );
    let done = match ok {
        Ok(v) => v,
        Err(e) => {
            save_json(
                &root,
                "success-result.json",
                &json!({"failed": true, "error": format!("{:?}", e), "instanceCommitted": false, "gameLaunched": false}),
            );
            panic!("restart failed: {e:?}");
        }
    };
    let meta: Value =
        serde_json::from_slice(&fs::read(&done.manifest_path).expect("manifest bytes"))
            .expect("manifest json");
    assert_eq!(done.metadata["id"], "1.20.4");
    assert_eq!(
        done.metadata["mainClass"],
        "cpw.mods.bootstraplauncher.BootstrapLauncher"
    );
    let libs = meta
        .get("files")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        libs, 53,
        "expected 53 library obligations (50 NeoForge + 3 generated)"
    );
    let all_pids = fresh_launcher.pids();
    assert_eq!(all_pids.len(), 5);
    assert_stopped(&fresh_launcher.inner, &all_pids);
    save_json(
        &root,
        "success-result.json",
        &json!({
            "manifestPath": done.manifest_path, "id": "1.20.4", "libraryCount": libs,
            "pids": all_pids, "allChildrenStopped": true, "phaseTrace": *phases3.lock().unwrap(),
            "instanceCommitted": false, "gameLaunched": false,
            "note": "expected hashes verified by external integration script",
        }),
    );
}

#[test]
fn preflight_rejects_bad_installer_without_touching_existing_root() {
    let root = std::env::temp_dir().join(shtmcl_platform::entropy::random_hex(16).unwrap());

    fs::create_dir_all(&root).unwrap();
    let marker = root.join("keep.txt");
    fs::write(&marker, b"keep").unwrap();
    let mc_path = root.join("mc.jar");
    fs::write(&mc_path, b"abc").unwrap();
    let mc = VerifiedInput {
        path: mc_path,
        size: 3,
        sha1: sha1(b"abc"),
    };
    let launcher = RecordingLauncher::new(false);
    let mappings = root.join("mappings.txt");
    let base = serde_json::to_vec(
        &json!({"id":"1.20.4","downloads":{"client":{"sha1":mc.sha1,"size":3}}}),
    )
    .unwrap();
    let input = ClientInstallInputs {
        destination: &root,
        installer_bytes: b"bad",
        installer_sha256: "00ff",
        base_metadata_bytes: &base,
        base_metadata_sha1: &sha1(&base),
        minecraft: &mc,
        java: Path::new("/nonexistent/javaw"),
        cached_mappings: &mappings,
    };
    let r = install_client(
        input,
        |r: &str| Err(StageError::Invalid(r.to_string())),
        &launcher,
        &launcher.inner,
        TIMEOUT,
        || false,
        |_p: &str| {},
    );
    assert!(r.is_err(), "bad installer token must fail preflight");
    assert_eq!(
        fs::read(&marker).unwrap(),
        b"keep",
        "existing root must be preserved"
    );
    assert!(
        launcher.pids().is_empty(),
        "no Java launch on preflight failure"
    );
}

//! Controlled doubles for module 02 behaviour tests.
//!
//! These are **test doubles, not implementations**: they stand in for module
//! 01 (instance snapshots), 05 (game session), 08 (LogStore) and for the
//! application layer's OS process adapter. Real instance/account/process
//! behaviour is not exercised here and is listed as unverified in the handoff.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use shtmcl_core::launch::logs::{LogRecord, LogSink};
use shtmcl_core::launch::plan::{ArtifactRole, InstanceLaunchSnapshot, LoaderKind, RequiredFile};
use shtmcl_core::launch::ports::{
    AccountSessionSource, AuthRequirement, ChildStreams, FileProbe, GameProbe, GameSession,
    InstanceLaunchLease, InstanceLaunchLeaseSource, InstanceSnapshotSource, ProcessSupervisor,
    StepObserver, WaitOutcome,
};
use shtmcl_core::launch::{module_with, LaunchModule, LaunchPorts};
use shtmcl_core::tasks::{InMemoryEvents, InMemoryTasks, TaskService};
use shtmcl_core::ModuleContext;
use shtmcl_domain::{
    CommandRequest, CommandResponse, ErrorEnvelope, InstanceId, LaunchSessionId, OperationId,
    RequestId, Revision, TaskId,
};
use shtmcl_platform::process::{ProcessHandle, ProcessLauncher, ProcessRequest};
use shtmcl_platform::PlatformDirs;

// ---------------------------------------------------------------------------
// 01 · instance snapshots
// ---------------------------------------------------------------------------

pub struct FakeInstances {
    pub snapshot: Mutex<Option<Result<InstanceLaunchSnapshot, ErrorEnvelope>>>,
    pub calls: AtomicUsize,
}

impl FakeInstances {
    pub fn new(snapshot: InstanceLaunchSnapshot) -> Arc<Self> {
        Arc::new(Self {
            snapshot: Mutex::new(Some(Ok(snapshot))),
            calls: AtomicUsize::new(0),
        })
    }
}

impl InstanceSnapshotSource for FakeInstances {
    fn snapshot(&self, _instance_id: &InstanceId) -> Result<InstanceLaunchSnapshot, ErrorEnvelope> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.snapshot
            .lock()
            .expect("instances lock")
            .clone()
            .unwrap_or_else(|| Err(ErrorEnvelope::module_unavailable()))
    }
}

#[derive(Default)]
pub struct FakeLeases {
    pub active: Arc<AtomicUsize>,
    pub renewals: Arc<AtomicUsize>,
    pub fail_renew: Arc<AtomicBool>,
    pub fail_bind: Arc<AtomicBool>,
}

struct FakeLease {
    active: Arc<AtomicUsize>,
    renewals: Arc<AtomicUsize>,
    fail_renew: Arc<AtomicBool>,
    fail_bind: Arc<AtomicBool>,
}

impl InstanceLaunchLeaseSource for FakeLeases {
    fn acquire(
        &self,
        _: &InstanceId,
        _: &str,
        _: &OperationId,
    ) -> Result<Box<dyn InstanceLaunchLease>, ErrorEnvelope> {
        self.active.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeLease {
            active: self.active.clone(),
            renewals: self.renewals.clone(),
            fail_renew: self.fail_renew.clone(),
            fail_bind: self.fail_bind.clone(),
        }))
    }
}

impl InstanceLaunchLease for FakeLease {
    fn bind_process(&mut self, _: u32) -> Result<(), ErrorEnvelope> {
        if self.fail_bind.load(Ordering::SeqCst) {
            Err(ErrorEnvelope::module_unavailable())
        } else {
            Ok(())
        }
    }
    fn renew(&mut self) -> Result<(), ErrorEnvelope> {
        self.renewals.fetch_add(1, Ordering::SeqCst);
        if self.fail_renew.load(Ordering::SeqCst) {
            Err(ErrorEnvelope::module_unavailable())
        } else {
            Ok(())
        }
    }
}

impl Drop for FakeLease {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// 05 · game session
// ---------------------------------------------------------------------------

pub struct FakeAccounts {
    pub session: Mutex<Result<GameSession, ErrorEnvelope>>,
    pub calls: AtomicUsize,
}

impl FakeAccounts {
    pub fn online(username: &str) -> Arc<Self> {
        Arc::new(Self {
            session: Mutex::new(Ok(GameSession::new(
                Some("account-1".to_string()),
                username,
                "11111111222222223333333344444444",
                "msa",
                "token-value-that-must-not-leak",
            ))),
            calls: AtomicUsize::new(0),
        })
    }

    pub fn failing() -> Arc<Self> {
        Arc::new(Self {
            session: Mutex::new(Err(ErrorEnvelope::module_unavailable())),
            calls: AtomicUsize::new(0),
        })
    }
}

impl AccountSessionSource for FakeAccounts {
    fn ensure_game_session(
        &self,
        _requirement: AuthRequirement,
        _fallback_username: &str,
    ) -> Result<GameSession, ErrorEnvelope> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.session.lock().expect("accounts lock").clone()
    }
}

pub fn offline_session(username: &str) -> GameSession {
    GameSession::offline(username)
}

// ---------------------------------------------------------------------------
// filesystem probe
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct MapFiles {
    /// Paths the probe reports as missing; everything else exists.
    pub missing: Mutex<Vec<PathBuf>>,
    pub checks: AtomicUsize,
}

impl MapFiles {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn hide(&self, path: impl Into<PathBuf>) {
        self.missing.lock().expect("files lock").push(path.into());
    }
}

impl FileProbe for MapFiles {
    fn exists(&self, path: &std::path::Path) -> bool {
        self.checks.fetch_add(1, Ordering::Relaxed);
        !self
            .missing
            .lock()
            .expect("files lock")
            .iter()
            .any(|missing| missing == path)
    }
}

// ---------------------------------------------------------------------------
// process: launcher + supervisor in one double
// ---------------------------------------------------------------------------

pub struct FakeProcess {
    pub spawns: Mutex<Vec<ProcessRequest>>,
    /// Text a `-version` probe writes to stderr.
    pub probe_text: Mutex<String>,
    pub probe_exit: Mutex<i32>,
    /// Simulate a runtime that never answers `-version`.
    pub probe_hangs: AtomicBool,
    /// `None` = the game keeps running; `Some(code)` = it exits immediately.
    pub game_exit: Mutex<Option<i32>>,
    pub fail_spawn: AtomicBool,
    /// Fail only game spawns (runtime probes still work).
    pub fail_game_spawn: AtomicBool,
    pub next_pid: AtomicUsize,
    pub streams: Mutex<BTreeMap<u32, Option<ChildStreams>>>,
    pub exited: Mutex<BTreeMap<u32, Option<i32>>>,
    pub stops: Mutex<Vec<u32>>,
    pub game_stdout: Mutex<String>,
    pub game_stderr: Mutex<String>,
    pub stop_fails: AtomicBool,
}

impl FakeProcess {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            spawns: Mutex::new(Vec::new()),
            probe_text: Mutex::new(default_java_banner("21.0.2")),
            probe_exit: Mutex::new(0),
            probe_hangs: AtomicBool::new(false),
            game_exit: Mutex::new(None),
            fail_spawn: AtomicBool::new(false),
            fail_game_spawn: AtomicBool::new(false),
            next_pid: AtomicUsize::new(1000),
            streams: Mutex::new(BTreeMap::new()),
            exited: Mutex::new(BTreeMap::new()),
            stops: Mutex::new(Vec::new()),
            game_stdout: Mutex::new(String::new()),
            game_stderr: Mutex::new(String::new()),
            stop_fails: AtomicBool::new(false),
        })
    }

    pub fn set_java(&self, banner: &str) {
        *self.probe_text.lock().expect("probe") = banner.to_string();
    }

    pub fn set_game_output(&self, stdout: &str, stderr: &str) {
        *self.game_stdout.lock().expect("stdout") = stdout.to_string();
        *self.game_stderr.lock().expect("stderr") = stderr.to_string();
    }

    /// Make every game process exit now (already-running pids included).
    pub fn exit_game_now(&self, code: i32) {
        *self.game_exit.lock().expect("exit") = Some(code);
        if let Ok(mut exited) = self.exited.lock() {
            for value in exited.values_mut() {
                if value.is_none() {
                    *value = Some(code);
                }
            }
        }
    }

    /// Process spawns that are not runtime probes.
    pub fn game_spawns(&self) -> Vec<ProcessRequest> {
        self.spawns
            .lock()
            .map(|spawns| {
                spawns
                    .iter()
                    .filter(|spawn| spawn.args != ["-version".to_string()])
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn probe_spawns(&self) -> Vec<ProcessRequest> {
        self.spawns
            .lock()
            .map(|spawns| {
                spawns
                    .iter()
                    .filter(|spawn| spawn.args == ["-version".to_string()])
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn is_probe(request: &ProcessRequest) -> bool {
        request.args == ["-version".to_string()]
    }

    fn streams_for(&self, probe: bool, pid: u32) -> ChildStreams {
        let (out, err) = if probe {
            (
                String::new(),
                self.probe_text.lock().expect("probe").clone(),
            )
        } else {
            (
                self.game_stdout.lock().expect("stdout").clone(),
                self.game_stderr.lock().expect("stderr").clone(),
            )
        };
        let _ = pid;
        ChildStreams {
            stdout: Box::new(Cursor::new(out.into_bytes())),
            stderr: Box::new(Cursor::new(err.into_bytes())),
        }
    }
}

impl ProcessLauncher for FakeProcess {
    fn spawn(&self, request: &ProcessRequest) -> std::io::Result<ProcessHandle> {
        self.spawns
            .lock()
            .expect("spawns lock")
            .push(request.clone());
        if self.fail_spawn.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "simulated spawn failure",
            ));
        }
        let probe_request = Self::is_probe(request);
        if !probe_request && self.fail_game_spawn.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "simulated game spawn failure",
            ));
        }
        let pid = self.next_pid.fetch_add(1, Ordering::SeqCst) as u32;
        let probe = probe_request;
        let streams = self.streams_for(probe, pid);
        self.streams
            .lock()
            .expect("streams lock")
            .insert(pid, Some(streams));
        let exit = if probe {
            if self.probe_hangs.load(Ordering::SeqCst) {
                None
            } else {
                Some(*self.probe_exit.lock().expect("probe exit"))
            }
        } else {
            *self.game_exit.lock().expect("exit")
        };
        self.exited.lock().expect("exited lock").insert(pid, exit);
        Ok(ProcessHandle { pid })
    }
}

impl ProcessSupervisor for FakeProcess {
    fn take_streams(&self, pid: u32) -> Option<ChildStreams> {
        self.streams
            .lock()
            .expect("streams lock")
            .get_mut(&pid)
            .and_then(Option::take)
    }

    fn wait_for_exit(&self, pid: u32, _timeout: Option<Duration>) -> WaitOutcome {
        match self.exited.lock().expect("exited lock").get(&pid) {
            Some(Some(code)) => WaitOutcome::Exited(*code),
            Some(None) => WaitOutcome::TimedOut,
            None => WaitOutcome::Unknown,
        }
    }

    fn is_alive(&self, pid: u32) -> bool {
        matches!(
            self.exited.lock().expect("exited lock").get(&pid),
            Some(None)
        )
    }

    fn request_stop(&self, pid: u32) -> std::io::Result<()> {
        self.stops.lock().expect("stops lock").push(pid);
        if self.stop_fails.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "simulated stop failure",
            ));
        }
        self.exited
            .lock()
            .expect("exited lock")
            .insert(pid, Some(0));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 08 · log sink
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct RecordingSink {
    pub records: Mutex<Vec<LogRecord>>,
    pub dropped: Mutex<(u64, u64)>,
    pub append_calls: AtomicUsize,
    /// Simulate a full disk: the sink accepts nothing and never blocks.
    pub reject_all: AtomicBool,
}

impl RecordingSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn line_count(&self) -> usize {
        self.records
            .lock()
            .map(|records| records.len())
            .unwrap_or(0)
    }
}

impl LogSink for RecordingSink {
    fn append(&self, _session: &LaunchSessionId, records: &[LogRecord]) -> usize {
        self.append_calls.fetch_add(1, Ordering::Relaxed);
        if self.reject_all.load(Ordering::Relaxed) {
            return 0;
        }
        if let Ok(mut slot) = self.records.lock() {
            slot.extend_from_slice(records);
        }
        records.len()
    }

    fn note_dropped(&self, _session: &LaunchSessionId, bytes: u64, lines: u64) {
        if let Ok(mut dropped) = self.dropped.lock() {
            dropped.0 += bytes;
            dropped.1 += lines;
        }
    }
}

/// Stand-in for 08's LogStore: it is the primary store and serves pages itself
/// (D02-6). 02 must then keep only a tail instead of a second full ring.
#[derive(Default)]
pub struct PagingSink {
    pub records: Mutex<Vec<LogRecord>>,
    pub page_calls: AtomicUsize,
}

impl PagingSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn line_count(&self) -> usize {
        self.records
            .lock()
            .map(|records| records.len())
            .unwrap_or(0)
    }
}

impl LogSink for PagingSink {
    fn append(&self, _session: &LaunchSessionId, records: &[LogRecord]) -> usize {
        if let Ok(mut slot) = self.records.lock() {
            slot.extend_from_slice(records);
        }
        records.len()
    }

    fn note_dropped(&self, _session: &LaunchSessionId, _bytes: u64, _lines: u64) {}

    fn supports_paging(&self) -> bool {
        true
    }

    fn page(
        &self,
        session: &LaunchSessionId,
        cursor: Option<u64>,
        limit: usize,
    ) -> Option<shtmcl_core::launch::logs::LogPage> {
        self.page_calls.fetch_add(1, Ordering::Relaxed);
        let records = self.records.lock().ok()?;
        let after = cursor.unwrap_or(0);
        let limit = limit.clamp(1, 1000);
        let mut page = Vec::new();
        let mut has_more = false;
        for record in records.iter() {
            if record.sequence <= after {
                continue;
            }
            if page.len() == limit {
                has_more = true;
                break;
            }
            page.push(record.clone());
        }
        let next_cursor = page.last().map_or(after, |record| record.sequence);
        let _ = session;
        Some(shtmcl_core::launch::logs::LogPage {
            records: page,
            next_cursor,
            dropped_bytes: 0,
            dropped_lines: 0,
            has_more,
        })
    }
}

// ---------------------------------------------------------------------------
// step gate: deterministic cancellation points
// ---------------------------------------------------------------------------

struct GateState {
    phases: Vec<&'static str>,
    block_at: Option<&'static str>,
    blocked: bool,
    released: bool,
}

pub struct GateSteps {
    state: Mutex<GateState>,
    signal: Condvar,
}

impl GateSteps {
    pub fn new(block_at: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState {
                phases: Vec::new(),
                block_at,
                blocked: false,
                released: false,
            }),
            signal: Condvar::new(),
        })
    }

    /// Wait until the job is parked at the blocked phase.
    pub fn wait_blocked(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().expect("gate lock");
        while !state.blocked {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (next, _) = self
                .signal
                .wait_timeout(state, deadline - now)
                .expect("gate wait");
            state = next;
        }
        true
    }

    pub fn release(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.released = true;
            self.signal.notify_all();
        }
    }

    pub fn phases(&self) -> Vec<&'static str> {
        self.state
            .lock()
            .map(|state| state.phases.clone())
            .unwrap_or_default()
    }
}

impl StepObserver for GateSteps {
    fn on_phase(&self, phase: &'static str) {
        let mut state = self.state.lock().expect("gate lock");
        state.phases.push(phase);
        if state.block_at == Some(phase) && !state.released {
            state.blocked = true;
            self.signal.notify_all();
            while !state.released {
                state = self.signal.wait(state).expect("gate wait");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// game probe
// ---------------------------------------------------------------------------

pub struct FakeGameProbe {
    pub reached: Mutex<Option<bool>>,
}

impl FakeGameProbe {
    pub fn unknown() -> Arc<Self> {
        Arc::new(Self {
            reached: Mutex::new(None),
        })
    }
}

impl GameProbe for FakeGameProbe {
    fn reached_game(&self, _pid: u32) -> Option<bool> {
        *self.reached.lock().expect("game probe")
    }
}

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

pub struct Fixture {
    pub tasks: Arc<InMemoryTasks>,
    pub events: Arc<InMemoryEvents>,
    pub instances: Arc<FakeInstances>,
    pub leases: Arc<FakeLeases>,
    pub accounts: Arc<FakeAccounts>,
    pub process: Arc<FakeProcess>,
    pub files: Arc<MapFiles>,
    pub sink: Arc<RecordingSink>,
    /// Set instead of `sink` when the fixture wires a paging store (D02-6).
    pub paging_sink: Option<Arc<PagingSink>>,
    pub steps: Arc<GateSteps>,
    pub game: Arc<FakeGameProbe>,
    pub module: Arc<LaunchModule>,
    pub dirs: PlatformDirs,
    pub root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub fn temp_root(tag: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let index = COUNTER.fetch_add(1, Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!("shtmcl-02-{tag}-{}-{index}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

pub fn java_banner(major: u32) -> String {
    default_java_banner(&format!("{major}.0.2"))
}

pub fn default_java_banner(version: &str) -> String {
    format!(
        "openjdk version \"{version}\" 2024-01-16 LTS\nOpenJDK Runtime Environment Temurin-{version}+13 (build {version}+13-LTS)\nOpenJDK 64-Bit Server VM Temurin-{version}+13 (build {version}+13-LTS, mixed mode)\n"
    )
}

impl Fixture {
    pub fn new(tag: &str, snapshot: InstanceLaunchSnapshot) -> Self {
        Self::with_gate(tag, snapshot, None)
    }

    pub fn with_gate(
        tag: &str,
        snapshot: InstanceLaunchSnapshot,
        block_at: Option<&'static str>,
    ) -> Self {
        Self::build(tag, snapshot, block_at, false)
    }

    /// Fixture whose log sink is a store that serves pages itself (08 shape).
    pub fn with_paging_sink(tag: &str, snapshot: InstanceLaunchSnapshot) -> Self {
        Self::build(tag, snapshot, None, true)
    }

    fn build(
        tag: &str,
        snapshot: InstanceLaunchSnapshot,
        block_at: Option<&'static str>,
        paging: bool,
    ) -> Self {
        let root = temp_root(tag);
        let dirs = PlatformDirs::under(&root);
        dirs.create_all().expect("create dirs");

        // A fake but real-on-disk JDK layout: discovery only needs a file to
        // report as `bin/java`, and the process double answers `-version`.
        let jdk = dirs.data.join("java").join("jdk21");
        std::fs::create_dir_all(jdk.join("bin")).expect("create jdk dir");
        std::fs::write(jdk.join("bin").join(java_executable()), b"fake").expect("write java");

        let events = Arc::new(InMemoryEvents::new());
        let tasks = Arc::new(InMemoryTasks::new(events.clone()));
        let ctx = ModuleContext::new(tasks.clone(), events.clone(), dirs.clone());

        let instances = FakeInstances::new(snapshot);
        let leases = Arc::new(FakeLeases::default());
        let accounts = FakeAccounts::online("Steve");
        let process = FakeProcess::new();
        let files = MapFiles::new();
        let sink = RecordingSink::new();
        let paging_sink = if paging {
            Some(PagingSink::new())
        } else {
            None
        };
        let steps = GateSteps::new(block_at);
        let game = FakeGameProbe::unknown();

        let launcher: Arc<dyn ProcessLauncher> = process.clone();
        let supervisor: Arc<dyn ProcessSupervisor> = process.clone();
        let log_sink: Arc<dyn LogSink> = match paging_sink.clone() {
            Some(paging_sink) => paging_sink,
            None => sink.clone(),
        };
        let ports = LaunchPorts::unavailable()
            .with_instances(instances.clone())
            .with_leases(leases.clone())
            .with_accounts(accounts.clone())
            .with_process(launcher, supervisor)
            .with_files(files.clone())
            .with_logs(log_sink)
            .with_game_probe(game.clone())
            .with_steps(steps.clone());

        let module = module_with(ctx, ports);
        Self {
            tasks,
            events,
            instances,
            leases,
            accounts,
            process,
            files,
            sink,
            paging_sink,
            steps,
            game,
            module,
            dirs,
            root,
        }
    }

    pub fn task(&self, task_id: &TaskId) -> shtmcl_domain::Task {
        self.tasks.snapshot(task_id).expect("task exists")
    }

    pub fn task_service(&self) -> &Arc<InMemoryTasks> {
        &self.tasks
    }

    pub fn logs(&self) -> Arc<shtmcl_core::launch::logs::LogPipeline> {
        self.module.logs().clone()
    }

    pub fn runtimes(&self) -> Vec<shtmcl_core::launch::java::JavaRuntime> {
        self.module.runtimes()
    }

    pub fn session_by_task(
        &self,
        task_id: &TaskId,
    ) -> Option<shtmcl_core::launch::session::LaunchSession> {
        self.module
            .sessions()
            .into_iter()
            .find(|session| &session.task_id == task_id)
    }

    /// Poll for a session state without sleeping on a fixed schedule.
    pub fn wait_session_state(
        &self,
        session_id: &LaunchSessionId,
        state: shtmcl_core::launch::session::LaunchSessionState,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .module
                .session(session_id)
                .map(|session| session.state == state)
                .unwrap_or(false)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Poll until a task reaches a terminal state (used where joining all jobs
    /// would wait for a still-running game process).
    pub fn wait_task_terminal(&self, task_id: &TaskId, timeout: Duration) -> shtmcl_domain::Task {
        let deadline = Instant::now() + timeout;
        loop {
            let task = self.task(task_id);
            if task.state.is_terminal() || Instant::now() >= deadline {
                return task;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

pub fn java_executable() -> &'static str {
    if cfg!(windows) {
        "java.exe"
    } else {
        "java"
    }
}

/// Build a `JavaRuntime` for selection tests without probing anything.
pub fn runtime_at(
    path: &str,
    major: u32,
    arch: shtmcl_core::launch::java::Arch,
) -> shtmcl_core::launch::java::JavaRuntime {
    shtmcl_core::launch::java::JavaRuntime {
        id: format!("java-{major}-{}", arch.as_str()),
        path: PathBuf::from(path),
        platform: shtmcl_core::launch::java::HostPlatform::current(),
        arch,
        vendor: shtmcl_core::launch::java::JavaVendor::Temurin,
        version: shtmcl_core::launch::java::JavaVersion {
            major,
            minor: 0,
            security: 2,
            update: None,
            raw: format!("{major}.0.2"),
        },
        verified_at_unix_ms: 0,
        file_identity: shtmcl_platform::FileIdentity {
            len: 4,
            modified_unix_ms: None,
        },
    }
}

// ---------------------------------------------------------------------------
// snapshot + request builders
// ---------------------------------------------------------------------------

pub fn sample_snapshot(game_version: &str) -> InstanceLaunchSnapshot {
    let base = PathBuf::from("/shtmcl-fixture/instances/one");
    InstanceLaunchSnapshot {
        instance_id: InstanceId::new("instance-1").expect("id"),
        revision: Revision::new(7),
        content_revision: Some(Revision::new(3)),
        name: "One".to_string(),
        game_version: game_version.to_string(),
        version_type: Some("release".to_string()),
        loader: LoaderKind::Vanilla,
        game_dir: base.clone(),
        main_class: "net.minecraft.client.main.Main".to_string(),
        classpath: vec![base.join("client.jar"), base.join("libraries/gson.jar")],
        assets_dir: Some(base.join("assets")),
        asset_index: Some("17".to_string()),
        natives_dir: Some(base.join("natives")),
        jvm_args: vec!["-XX:+UseG1GC".to_string()],
        game_args: vec![
            "--username".to_string(),
            "${auth_player_name}".to_string(),
            "--uuid".to_string(),
            "${auth_uuid}".to_string(),
            "--accessToken".to_string(),
            "${auth_access_token}".to_string(),
            "--userType".to_string(),
            "${user_type}".to_string(),
            "--version".to_string(),
            "${version_name}".to_string(),
            "--gameDir".to_string(),
            "${game_directory}".to_string(),
            "--assetsDir".to_string(),
            "${assets_root}".to_string(),
            "--assetIndex".to_string(),
            "${assets_index_name}".to_string(),
        ],
        memory_mb: Some(2048),
        java_requirement: None,
        java_path: None,
        required_files: vec![
            RequiredFile::new(base.join("client.jar"), ArtifactRole::Client),
            RequiredFile::new(base.join("libraries/gson.jar"), ArtifactRole::Library),
        ],
        auth: AuthRequirement::Online,
        offline_username: Some("Player".to_string()),
        multi_open: false,
        profile_ref: Some("default".to_string()),
    }
}

pub fn request(command: &str, payload: serde_json::Value) -> CommandRequest {
    let mut payload = payload;
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "command".to_string(),
            serde_json::Value::String(command.to_string()),
        );
    }
    CommandRequest {
        request_id: RequestId::new(format!("req-{command}")).expect("request id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload,
    }
}

pub fn mutation(command: &str, payload: serde_json::Value) -> CommandRequest {
    let mut request = request(command, payload);
    request.operation_id = Some(OperationId::new(format!("op-{command}")).expect("operation id"));
    request
}

pub fn accepted(response: CommandResponse) -> (OperationId, TaskId) {
    match response {
        CommandResponse::Accepted {
            operation_id,
            task_id,
        } => (operation_id, task_id),
        other => panic!("expected accepted, got {other:?}"),
    }
}

pub fn completed(response: CommandResponse) -> serde_json::Value {
    match response {
        CommandResponse::Completed { result } => result,
        other => panic!("expected completed, got {other:?}"),
    }
}

//! Local Java processor execution for the fixed 1.20.4 / 20.4.253 pilot.
//! The caller owns the exclusive staging directory and supplies a trusted
//! installer digest. No network, instance writes or task scheduler live here.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use shtmcl_platform::{ProcessLauncher, ProcessRequest, ProcessSupervisor, WaitOutcome};

use super::neoforge_plan::{plan_client_staging, StagingArgument, StagingStep, INSTALLER_INPUT};
use super::neoforge_processor_allowlist::ARTIFACTS;
use super::neoforge_profile::{read_installer_archive, ProcessorOperation};
use super::paths::normalise_content_path;
use super::zipindex::{read_index, ZipBudget};

const FILE_LIMIT: u64 = 512 * 1024 * 1024;
const LOG_LIMIT: usize = 64 * 1024;
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum RunnerError {
    Io(io::Error),
    Invalid(String),
    MappingsRequired,
    Cancelled,
    Timeout,
    StopFailed,
    Process { code: i32, logs: StepLogs },
    LogsUnavailable,
}

impl From<io::Error> for RunnerError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for RunnerError {}

#[derive(Debug, Default)]
pub struct CapturedLog {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}
#[derive(Debug)]
pub struct StepLogs {
    pub stdout: CapturedLog,
    pub stderr: CapturedLog,
}
#[derive(Debug)]
pub struct StepOutput {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
}
#[derive(Debug)]
pub struct StepResult {
    pub index: usize,
    pub outputs: Vec<StepOutput>,
    pub logs: StepLogs,
}

/// Execute exactly one regenerated client step. Staging must be exclusively
/// owned throughout this call; checks do not provide an OS filesystem sandbox.
#[allow(clippy::too_many_arguments)]
pub fn execute_client_step(
    root: &Path,
    installer_sha256: &str,
    original_index: usize,
    java: &Path,
    launcher: &dyn ProcessLauncher,
    supervisor: &dyn ProcessSupervisor,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<StepResult, RunnerError> {
    if cancelled() {
        return Err(RunnerError::Cancelled);
    }
    validate_absolute(root)?;
    if !fs::metadata(root)?.is_dir() {
        return invalid("staging root is not a directory");
    }
    validate_absolute(java)?;
    regular_file(java)?;
    let installer = bind(root, INSTALLER_INPUT)?;
    let (_, actual) = hash_file(&installer)?;
    if installer_sha256.len() != 64 || !actual.eq_ignore_ascii_case(installer_sha256) {
        return invalid("installer SHA256 mismatch");
    }
    let bytes = read_bounded(&installer)?;
    let parsed = read_installer_archive("1.20.4", "20.4.253", &bytes)
        .map_err(|e| RunnerError::Invalid(e.into()))?;
    let steps =
        plan_client_staging(&parsed.profile).map_err(|e| RunnerError::Invalid(e.to_string()))?;
    let step = steps
        .iter()
        .find(|s| s.index == original_index)
        .ok_or_else(|| RunnerError::Invalid("unknown client processor index".into()))?;
    if step.operation == ProcessorOperation::DownloadMojmaps {
        return Err(RunnerError::MappingsRequired);
    }
    let (input_flags, output_flags) = io_flags(step.operation)?;
    for flag in input_flags {
        regular_file(&bind(root, argument_path(step, flag)?)?)?;
    }
    let outputs: Vec<PathBuf> = output_flags
        .iter()
        .map(|flag| bind(root, argument_path(step, flag)?))
        .collect::<Result<_, _>>()?;
    for path in &outputs {
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
            Ok(_) => return invalid("processor output already exists"),
        }
    }
    let mut classpath = Vec::new();
    for relative in &step.classpath {
        let pin = ARTIFACTS
            .iter()
            .find(|p| relative == &format!("libraries/{}", p.path))
            .ok_or_else(|| RunnerError::Invalid("unpinned classpath artifact".into()))?;
        let path = bind(root, relative)?;
        let (size, digest) = hash_file(&path)?;
        if size != pin.size || digest != pin.sha256 {
            return invalid("classpath integrity mismatch");
        }
        classpath.push(path);
    }
    let main = main_class(&step.tool_path)?;
    let cp = std::env::join_paths(classpath).map_err(|e| RunnerError::Invalid(e.to_string()))?;
    let mut args = vec![
        "-Xmx1G".into(),
        "-cp".into(),
        unicode(cp.as_ref())?,
        main.into(),
    ];
    for arg in &step.args {
        args.push(match arg {
            StagingArgument::Literal(s) => s.clone(),
            StagingArgument::StagedPath(s) => unicode(bind(root, s)?.as_os_str())?,
            StagingArgument::StagingRoot => unicode(root.as_os_str())?,
        });
    }
    if cancelled() {
        return Err(RunnerError::Cancelled);
    }
    if timeout.is_zero() {
        return Err(RunnerError::Timeout);
    }
    for path in &outputs {
        fs::create_dir_all(path.parent().unwrap())?;
    }
    let mut request = ProcessRequest::new(java.to_owned(), root.to_owned()).with_args(args);
    for key in [
        "JAVA_TOOL_OPTIONS",
        "_JAVA_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "CLASSPATH",
    ] {
        request = request.with_env(key, "");
    }
    run_prepared_step(
        &request,
        &outputs,
        original_index,
        launcher,
        supervisor,
        timeout,
        &cancelled,
    )
}

/// All paths and the command have already passed preflight. Kept separate so
/// failure cleanup and output validation can be tested without running Java.
#[allow(clippy::too_many_arguments)]
fn run_prepared_step(
    request: &ProcessRequest,
    outputs: &[PathBuf],
    index: usize,
    launcher: &dyn ProcessLauncher,
    supervisor: &dyn ProcessSupervisor,
    timeout: Duration,
    cancelled: &impl Fn() -> bool,
) -> Result<StepResult, RunnerError> {
    let pid = launcher.spawn(request)?.pid;
    let Some(streams) = supervisor.take_streams(pid) else {
        stop(pid, supervisor)?;
        cleanup(outputs);
        return Err(RunnerError::LogsUnavailable);
    };
    let stdout = drain_async(streams.stdout);
    let stderr = drain_async(streams.stderr);
    let outcome = wait(pid, supervisor, timeout, cancelled);
    // Never remove files while the child may still be writing them.
    if matches!(outcome, Err(RunnerError::StopFailed)) {
        return Err(RunnerError::StopFailed);
    }
    let result = (|| {
        let code = outcome?;
        let logs = StepLogs {
            stdout: receive_log(stdout)?,
            stderr: receive_log(stderr)?,
        };
        if code != 0 {
            return Err(RunnerError::Process { code, logs });
        }
        let mut verified = Vec::new();
        for path in outputs {
            validate_absolute(path)?;
            let (size, sha256) = hash_file(path)?;
            if size == 0 {
                return invalid("processor output is empty");
            }
            if path.extension().is_some_and(|e| e == "jar") {
                read_index(&read_bounded(path)?, &ZipBudget::default())
                    .map_err(|e| RunnerError::Invalid(format!("invalid output JAR: {e}")))?;
            }
            verified.push(StepOutput {
                path: path.clone(),
                size,
                sha256,
            });
        }
        Ok(StepResult {
            index,
            outputs: verified,
            logs,
        })
    })();
    if result.is_err() {
        cleanup(outputs);
    }
    result
}

fn invalid<T>(message: &str) -> Result<T, RunnerError> {
    Err(RunnerError::Invalid(message.into()))
}
fn unicode(value: &std::ffi::OsStr) -> Result<String, RunnerError> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| RunnerError::Invalid("non-Unicode path".into()))
}
fn validate_absolute(path: &Path) -> Result<(), RunnerError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return invalid("absolute normalized path required");
    }
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(m) => {
                if m.file_type().is_symlink() {
                    return invalid("symlink in processor path");
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if m.file_attributes() & 0x400 != 0 {
                        return invalid("reparse point in processor path");
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub(super) fn bind(root: &Path, relative: &str) -> Result<PathBuf, RunnerError> {
    if normalise_content_path(relative).ok().as_deref() != Some(relative) {
        return invalid("unsafe processor path");
    }
    let path = root.join(relative);
    validate_absolute(&path)?;
    Ok(path)
}
pub(super) fn regular_file(path: &Path) -> Result<u64, RunnerError> {
    validate_absolute(path)?;
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() || m.len() > FILE_LIMIT {
        return invalid("processor input is not a bounded regular file");
    }
    Ok(m.len())
}
fn read_bounded(path: &Path) -> Result<Vec<u8>, RunnerError> {
    regular_file(path)?;
    let mut bytes = Vec::new();
    File::open(path)?
        .take(FILE_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > FILE_LIMIT {
        return invalid("file exceeds size budget");
    }
    Ok(bytes)
}
fn hash_file(path: &Path) -> Result<(u64, String), RunnerError> {
    regular_file(path)?;
    let mut file = File::open(path)?;
    let mut buf = [0; 64 * 1024];
    let mut size = 0;
    let mut hash = Sha256::new();
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        if size > FILE_LIMIT {
            return invalid("file exceeds size budget");
        }
        hash.update(&buf[..n]);
    }
    Ok((size, format!("{:x}", hash.finalize())))
}
fn argument_path<'a>(step: &'a StagingStep, flag: &str) -> Result<&'a str, RunnerError> {
    step.args
        .windows(2)
        .find_map(|pair| match (&pair[0], &pair[1]) {
            (StagingArgument::Literal(f), StagingArgument::StagedPath(p)) if f == flag => {
                Some(p.as_str())
            }
            _ => None,
        })
        .ok_or_else(|| RunnerError::Invalid(format!("missing path for {flag}")))
}
type Flags = &'static [&'static str];
fn io_flags(operation: ProcessorOperation) -> Result<(Flags, Flags), RunnerError> {
    use ProcessorOperation::*;
    match operation {
        McpData => Ok((&["--input"], &["--output"])),
        MergeMapping => Ok((&["--left", "--right"], &["--output"])),
        JarSplitter => Ok((&["--input", "--srg"], &["--slim", "--extra"])),
        AutoRenamingTool => Ok((&["--input", "--names"], &["--output"])),
        BinaryPatcher => Ok((&["--clean", "--apply"], &["--output"])),
        DownloadMojmaps => Err(RunnerError::MappingsRequired),
        _ => invalid("unsupported local processor operation"),
    }
}
fn main_class(path: &str) -> Result<&'static str, RunnerError> {
    match path {
        "libraries/net/neoforged/installertools/installertools/2.1.2/installertools-2.1.2.jar" => {
            Ok("net.neoforged.installertools.ConsoleTool")
        }
        "libraries/net/neoforged/installertools/jarsplitter/2.1.2/jarsplitter-2.1.2.jar" => {
            Ok("net.neoforged.jarsplitter.ConsoleTool")
        }
        "libraries/net/neoforged/AutoRenamingTool/1.0.13/AutoRenamingTool-1.0.13-all.jar" => {
            Ok("net.minecraftforge.fart.Main")
        }
        "libraries/net/neoforged/installertools/binarypatcher/2.1.2/binarypatcher-2.1.2.jar" => {
            Ok("net.neoforged.binarypatcher.ConsoleTool")
        }
        _ => invalid("unknown processor entry point"),
    }
}
fn drain_async(mut stream: Box<dyn Read + Send>) -> Receiver<io::Result<CapturedLog>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| {
            let mut log = CapturedLog::default();
            let mut buffer = [0; 8192];
            loop {
                let n = stream.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                let keep = n.min(LOG_LIMIT.saturating_sub(log.bytes.len()));
                log.bytes.extend_from_slice(&buffer[..keep]);
                log.truncated |= keep < n;
            }
            Ok(log)
        })();
        let _ = tx.send(result);
    });
    rx
}
fn receive_log(rx: Receiver<io::Result<CapturedLog>>) -> Result<CapturedLog, RunnerError> {
    rx.recv_timeout(Duration::from_secs(2))
        .map_err(|_| RunnerError::LogsUnavailable)?
        .map_err(Into::into)
}
fn stop(pid: u32, supervisor: &dyn ProcessSupervisor) -> Result<(), RunnerError> {
    let _ = supervisor.request_stop(pid);
    match supervisor.wait_for_exit(pid, Some(Duration::from_secs(2))) {
        WaitOutcome::Exited(_) => Ok(()),
        _ => Err(RunnerError::StopFailed),
    }
}
fn wait(
    pid: u32,
    supervisor: &dyn ProcessSupervisor,
    timeout: Duration,
    cancelled: &impl Fn() -> bool,
) -> Result<i32, RunnerError> {
    let start = Instant::now();
    loop {
        let reason = if cancelled() {
            Some(RunnerError::Cancelled)
        } else if start.elapsed() >= timeout {
            Some(RunnerError::Timeout)
        } else {
            None
        };
        if let Some(reason) = reason {
            stop(pid, supervisor)?;
            return Err(reason);
        }
        match supervisor.wait_for_exit(pid, Some(POLL.min(timeout.saturating_sub(start.elapsed()))))
        {
            WaitOutcome::Exited(code) => return Ok(code),
            WaitOutcome::TimedOut => (),
            WaitOutcome::Unknown => {
                stop(pid, supervisor)?;
                return invalid("processor exit status unavailable");
            }
        }
    }
}
fn cleanup(outputs: &[PathBuf]) {
    for path in outputs {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shtmcl_platform::ChildStreams;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct TestRoot(PathBuf);
    impl TestRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "shtmcl-neoforge-runner-{}",
                shtmcl_platform::random_hex(12).unwrap()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    struct FakeProcess {
        supervisor: Supervisor,
        output: PathBuf,
        bytes: Option<Vec<u8>>,
        streams: bool,
    }
    impl ProcessLauncher for FakeProcess {
        fn spawn(&self, _: &ProcessRequest) -> io::Result<shtmcl_platform::ProcessHandle> {
            if let Some(bytes) = &self.bytes {
                fs::write(&self.output, bytes)?;
            }
            Ok(shtmcl_platform::ProcessHandle { pid: 1 })
        }
    }
    impl ProcessSupervisor for FakeProcess {
        fn take_streams(&self, _: u32) -> Option<ChildStreams> {
            self.streams.then(|| ChildStreams {
                stdout: Box::new(io::Cursor::new(vec![b'x'; LOG_LIMIT * 2])),
                stderr: Box::new(io::Cursor::new(b"diagnostic".to_vec())),
            })
        }
        fn wait_for_exit(&self, p: u32, d: Option<Duration>) -> WaitOutcome {
            self.supervisor.wait_for_exit(p, d)
        }
        fn is_alive(&self, p: u32) -> bool {
            self.supervisor.is_alive(p)
        }
        fn request_stop(&self, p: u32) -> io::Result<()> {
            self.supervisor.request_stop(p)
        }
    }
    fn fake_run(
        root: &TestRoot,
        fake: &FakeProcess,
        cancelled: bool,
    ) -> Result<StepResult, RunnerError> {
        run_prepared_step(
            &ProcessRequest::new(root.0.join("unused-java"), root.0.clone()),
            &[fake.output.clone()],
            3,
            fake,
            fake,
            Duration::from_secs(1),
            &|| cancelled,
        )
    }

    #[test]
    fn process_error_keeps_logs_and_removes_partial_output() {
        let root = TestRoot::new();
        let fake = FakeProcess {
            supervisor: Supervisor::new(WaitOutcome::Exited(7)),
            output: root.0.join("mapping.txt"),
            bytes: Some(b"partial".to_vec()),
            streams: true,
        };
        match fake_run(&root, &fake, false).unwrap_err() {
            RunnerError::Process { code, logs } => {
                assert_eq!(code, 7);
                assert_eq!(logs.stdout.bytes.len(), LOG_LIMIT);
                assert!(logs.stdout.truncated);
                assert_eq!(logs.stderr.bytes, b"diagnostic");
            }
            error => panic!("unexpected error: {error}"),
        }
        assert!(!fake.output.exists());
    }
    #[test]
    fn zero_exit_requires_nonempty_output_and_valid_jar() {
        let root = TestRoot::new();
        for (name, bytes) in [
            ("missing.txt", None),
            ("empty.txt", Some(Vec::new())),
            ("broken.jar", Some(b"not a zip".to_vec())),
        ] {
            let fake = FakeProcess {
                supervisor: Supervisor::new(WaitOutcome::Exited(0)),
                output: root.0.join(name),
                bytes,
                streams: true,
            };
            assert!(fake_run(&root, &fake, false).is_err());
            assert!(!fake.output.exists());
        }
    }
    #[test]
    fn success_hashes_real_output_bytes() {
        let root = TestRoot::new();
        let fake = FakeProcess {
            supervisor: Supervisor::new(WaitOutcome::Exited(0)),
            output: root.0.join("mapping.txt"),
            bytes: Some(b"abc".to_vec()),
            streams: true,
        };
        let result = fake_run(&root, &fake, false).unwrap();
        assert_eq!(result.outputs[0].size, 3);
        assert_eq!(
            result.outputs[0].sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
    #[test]
    fn cancelled_process_cleans_only_after_confirmed_stop() {
        let root = TestRoot::new();
        let mut fake = FakeProcess {
            supervisor: Supervisor::new(WaitOutcome::TimedOut),
            output: root.0.join("mapping.txt"),
            bytes: Some(b"partial".to_vec()),
            streams: true,
        };
        assert!(matches!(
            fake_run(&root, &fake, true),
            Err(RunnerError::Cancelled)
        ));
        assert!(!fake.output.exists());
        fake.supervisor.stop_fails = true;
        assert!(matches!(
            fake_run(&root, &fake, true),
            Err(RunnerError::StopFailed)
        ));
        assert_eq!(fs::read(&fake.output).unwrap(), b"partial");
    }
    #[test]
    fn missing_streams_stops_process_and_cleans_partial_output() {
        let root = TestRoot::new();
        let fake = FakeProcess {
            supervisor: Supervisor::new(WaitOutcome::TimedOut),
            output: root.0.join("mapping.txt"),
            bytes: Some(b"partial".to_vec()),
            streams: false,
        };
        assert!(matches!(
            fake_run(&root, &fake, false),
            Err(RunnerError::LogsUnavailable)
        ));
        assert!(fake.supervisor.stopped.load(Ordering::Relaxed));
        assert!(!fake.output.exists());
    }

    struct Supervisor {
        initial: WaitOutcome,
        stopped: AtomicBool,
        stop_fails: bool,
    }
    impl Supervisor {
        fn new(initial: WaitOutcome) -> Self {
            Self {
                initial,
                stopped: AtomicBool::new(false),
                stop_fails: false,
            }
        }
    }
    impl ProcessSupervisor for Supervisor {
        fn take_streams(&self, _: u32) -> Option<ChildStreams> {
            None
        }
        fn wait_for_exit(&self, _: u32, _: Option<Duration>) -> WaitOutcome {
            if self.stopped.load(Ordering::Relaxed) && !self.stop_fails {
                WaitOutcome::Exited(-1)
            } else {
                self.initial
            }
        }
        fn is_alive(&self, _: u32) -> bool {
            !self.stopped.load(Ordering::Relaxed) || self.stop_fails
        }
        fn request_stop(&self, _: u32) -> io::Result<()> {
            self.stopped.store(true, Ordering::Relaxed);
            if self.stop_fails {
                Err(io::Error::other("stop failed"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn cancellation_stops_and_reaps() {
        let s = Supervisor::new(WaitOutcome::TimedOut);
        assert!(matches!(
            wait(1, &s, Duration::from_secs(1), &|| true),
            Err(RunnerError::Cancelled)
        ));
        assert!(s.stopped.load(Ordering::Relaxed));
    }
    #[test]
    fn timeout_stops_and_reaps() {
        let s = Supervisor::new(WaitOutcome::TimedOut);
        assert!(matches!(
            wait(1, &s, Duration::ZERO, &|| false),
            Err(RunnerError::Timeout)
        ));
        assert!(s.stopped.load(Ordering::Relaxed));
    }
    #[test]
    fn unknown_exit_attempts_stop() {
        let s = Supervisor::new(WaitOutcome::Unknown);
        assert!(matches!(
            wait(1, &s, Duration::from_secs(1), &|| false),
            Err(RunnerError::Invalid(_))
        ));
        assert!(s.stopped.load(Ordering::Relaxed));
    }
    #[test]
    fn failed_stop_is_distinct() {
        let mut s = Supervisor::new(WaitOutcome::Unknown);
        s.stop_fails = true;
        assert!(matches!(
            wait(1, &s, Duration::ZERO, &|| false),
            Err(RunnerError::StopFailed)
        ));
    }
    #[test]
    fn nonzero_exit_is_preserved() {
        let s = Supervisor::new(WaitOutcome::Exited(7));
        assert_eq!(wait(1, &s, Duration::from_secs(1), &|| false).unwrap(), 7);
        assert!(!s.stopped.load(Ordering::Relaxed));
    }
    #[test]
    fn logs_keep_draining_after_capture_limit() {
        struct Tracked {
            remaining: usize,
            drained: std::sync::Arc<AtomicBool>,
        }
        impl Read for Tracked {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = self.remaining.min(buf.len());
                buf[..n].fill(b'x');
                self.remaining -= n;
                if n == 0 {
                    self.drained.store(true, Ordering::Relaxed);
                }
                Ok(n)
            }
        }
        let drained = std::sync::Arc::new(AtomicBool::new(false));
        let log = receive_log(drain_async(Box::new(Tracked {
            remaining: LOG_LIMIT * 8,
            drained: drained.clone(),
        })))
        .unwrap();
        assert_eq!(log.bytes.len(), LOG_LIMIT);
        assert!(log.truncated);
        assert!(drained.load(Ordering::Relaxed));
    }
    #[test]
    fn rejects_path_escape_and_nonlocal_operations() {
        let root = std::env::current_dir().unwrap();
        for p in ["../outside", "a/../outside", "a\\b", "/absolute"] {
            assert!(bind(&root, p).is_err());
        }
        assert!(matches!(
            io_flags(ProcessorOperation::DownloadMojmaps),
            Err(RunnerError::MappingsRequired)
        ));
        assert!(main_class("libraries/untrusted.jar").is_err());
    }
    #[test]
    fn fixture_has_entry_points_and_separate_output_paths() {
        let profile = super::super::neoforge_profile::parse_install_profile(
            "1.20.4",
            "20.4.253",
            include_bytes!("fixtures/neoforge-plan-20.4.253.json"),
        )
        .unwrap();
        for step in plan_client_staging(&profile).unwrap() {
            if step.operation == ProcessorOperation::DownloadMojmaps {
                continue;
            }
            main_class(&step.tool_path).unwrap();
            let (inputs, outputs) = io_flags(step.operation).unwrap();
            for flag in outputs {
                let output = argument_path(&step, flag).unwrap();
                for input in inputs {
                    assert_ne!(output, argument_path(&step, input).unwrap());
                }
            }
        }
    }

    #[test]
    #[ignore = "requires a copied verified staging tree and Java 17"]
    fn captured_mcp_data_execution() {
        let root = PathBuf::from(std::env::var_os("SHTMCL_NEOFORGE_RUNNER_STAGE").unwrap());
        let java = PathBuf::from(std::env::var_os("SHTMCL_NEOFORGE_RUNNER_JAVA").unwrap());
        let digest = std::env::var("SHTMCL_NEOFORGE_INSTALLER_SHA256").unwrap();
        let processes = shtmcl_platform::StdProcesses::capturing();
        // Both errors must precede process spawning and leave the stage untouched.
        assert!(matches!(
            execute_client_step(
                &root,
                &digest,
                4,
                &java,
                &processes,
                &processes,
                Duration::from_secs(30),
                || false
            ),
            Err(RunnerError::MappingsRequired)
        ));
        assert!(matches!(
            execute_client_step(
                &root,
                &"00".repeat(32),
                3,
                &java,
                &processes,
                &processes,
                Duration::from_secs(30),
                || false
            ),
            Err(RunnerError::Invalid(_))
        ));
        assert_eq!(processes.tracked(), 0);
        let installer = fs::read(root.join(INSTALLER_INPUT)).unwrap();
        let parsed = read_installer_archive("1.20.4", "20.4.253", &installer).unwrap();
        let plan = plan_client_staging(&parsed.profile).unwrap();
        let step = plan.iter().find(|s| s.index == 3).unwrap();
        let tool = root.join(&step.tool_path);
        let original = fs::read(&tool).unwrap();
        let mut corrupted = original.clone();
        corrupted[0] ^= 1;
        fs::write(&tool, corrupted).unwrap();
        let tamper_result = execute_client_step(
            &root,
            &digest,
            3,
            &java,
            &processes,
            &processes,
            Duration::from_secs(30),
            || false,
        );
        fs::write(&tool, original).unwrap();
        assert!(matches!(tamper_result, Err(RunnerError::Invalid(_))));
        assert_eq!(processes.tracked(), 0);
        struct RecordingLauncher<'a> {
            inner: &'a shtmcl_platform::StdProcesses,
            request: std::sync::Mutex<Option<ProcessRequest>>,
        }
        impl ProcessLauncher for RecordingLauncher<'_> {
            fn spawn(
                &self,
                request: &ProcessRequest,
            ) -> io::Result<shtmcl_platform::ProcessHandle> {
                *self.request.lock().unwrap() = Some(request.clone());
                self.inner.spawn(request)
            }
        }
        let launcher = RecordingLauncher {
            inner: &processes,
            request: std::sync::Mutex::new(None),
        };
        let result = execute_client_step(
            &root,
            &digest,
            3,
            &java,
            &launcher,
            &processes,
            Duration::from_secs(60),
            || false,
        )
        .unwrap();
        let request = launcher.request.lock().unwrap().clone().unwrap();
        assert_eq!(request.program, java);
        assert_eq!(request.working_dir, root);
        assert_eq!(&request.args[..2], &["-Xmx1G", "-cp"]);
        assert_eq!(request.args[3], "net.neoforged.installertools.ConsoleTool");
        for key in [
            "JAVA_TOOL_OPTIONS",
            "_JAVA_OPTIONS",
            "JDK_JAVA_OPTIONS",
            "CLASSPATH",
        ] {
            assert_eq!(request.environment.get(key).map(String::as_str), Some(""));
        }
        fs::write(root.parent().unwrap().join("mcp-data-command.json"), serde_json::to_vec_pretty(&serde_json::json!({
            "program": request.program, "args": request.args, "workingDir": request.working_dir, "environmentOverrides": request.environment,
            "preflight": { "javaMappingsDenied": true, "installerTamperDenied": true, "classpathTamperDenied": true }
        })).unwrap()).unwrap();
        assert_eq!(result.outputs.len(), 1);
        assert!(result.outputs[0].size > 1000);
        let output = &result.outputs[0];
        println!(
            "output={} size={} sha256={}",
            output.path.display(),
            output.size,
            output.sha256
        );
        println!(
            "stdout={}\nstderr={}",
            String::from_utf8_lossy(&result.logs.stdout.bytes),
            String::from_utf8_lossy(&result.logs.stderr.bytes)
        );
        let result_json = serde_json::json!({"index": result.index, "output": output.path, "size": output.size, "sha256": output.sha256});
        fs::write(
            root.parent().unwrap().join("mcp-data-result.json"),
            serde_json::to_vec_pretty(&result_json).unwrap(),
        )
        .unwrap();
        let before = fs::read(&output.path).unwrap();
        assert!(matches!(
            execute_client_step(
                &root,
                &digest,
                3,
                &java,
                &processes,
                &processes,
                Duration::from_secs(30),
                || false
            ),
            Err(RunnerError::Invalid(_))
        ));
        assert_eq!(fs::read(&output.path).unwrap(), before);
    }
}

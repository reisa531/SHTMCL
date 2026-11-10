//! Process ports. Implementations stay outside this crate (application layer or
//! a module-specific adapter) so the public interface has no OS-specific
//! behavior.
//!
//! Two ports, one responsibility each:
//! - [`ProcessLauncher::spawn`] creates a process and returns its handle;
//! - [`ProcessSupervisor`] owns the captured pipes, waiting, liveness and
//!   stopping for a handle that was already created.
//!
//! They are separate because only the module that owns a session may read its
//! output (02 is the single reader of game stdout/stderr), while creation is a
//! plain platform call. A module must combine the two instead of inventing a
//! third process abstraction.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRequest {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub working_dir: PathBuf,
    pub environment: BTreeMap<String, String>,
}

impl ProcessRequest {
    pub fn new(program: PathBuf, working_dir: PathBuf) -> Self {
        Self {
            program,
            args: Vec::new(),
            working_dir,
            environment: BTreeMap::new(),
        }
    }

    /// Arguments are always a separate array; never build a single command line.
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessHandle {
    pub pid: u32,
}

pub trait ProcessLauncher: Send + Sync {
    fn spawn(&self, request: &ProcessRequest) -> std::io::Result<ProcessHandle>;
}

/// Owned pipes of one child process. The owner drains both; a dropped stream
/// closes the read end and lets the child finish writing.
pub struct ChildStreams {
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Box<dyn Read + Send>,
}

impl std::fmt::Debug for ChildStreams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChildStreams { stdout, stderr }")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The process ended with this exit code.
    Exited(i32),
    /// The timeout elapsed while the process was still running.
    TimedOut,
    /// The platform could not report on this pid (already reaped, unknown pid).
    Unknown,
}

/// Supervision of an already-created process: pipes, exit, liveness, stop.
///
/// Implementations must tolerate `pid` values they no longer own (return
/// `Unknown` / `false`) instead of panicking, because a session can outlive the
/// OS handle after a restart.
pub trait ProcessSupervisor: Send + Sync {
    /// Take the captured pipes for `pid`. `None` when the launcher did not
    /// capture them; the caller then simply has no log stream.
    fn take_streams(&self, pid: u32) -> Option<ChildStreams>;

    /// Wait for exit. `None` waits until the process ends.
    fn wait_for_exit(&self, pid: u32, timeout: Option<Duration>) -> WaitOutcome;

    fn is_alive(&self, pid: u32) -> bool;

    /// Ask the process to stop, normal termination first.
    fn request_stop(&self, pid: u32) -> std::io::Result<()>;
}

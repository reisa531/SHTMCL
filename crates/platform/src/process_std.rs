//! Concrete std-only process implementation behind [`ProcessLauncher`] and
//! [`ProcessSupervisor`].
//!
//! Why this exists: the ports are traits so tests can inject doubles, but every
//! real launch eventually needs one object that creates a child with piped
//! stdio, hands the pipes to the single reader, reports exit and can stop it.
//! Without it each module would reimplement process handling.
//!
//! Honest limits (do not treat these as bugs to paper over):
//! - `request_stop` is a forceful kill. A graceful stop depends on the game or
//!   tunnel protocol (for example writing a command to stdin) and belongs to the
//!   owning module, not here.
//! - Waiting is a short poll loop instead of OS wait notifications, so a wait for
//!   an arbitrary process costs one wakeup per poll interval.
//! - Only children created through this object can be supervised; a pid adopted
//!   from a previous run has no handle and reports `Unknown` / `false`.

use std::collections::BTreeMap;
use std::io;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::process::{
    ChildStreams, ProcessHandle, ProcessLauncher, ProcessRequest, ProcessSupervisor, WaitOutcome,
};

/// Poll interval for waits. Small enough to feel instant, large enough to cost
/// nothing: a wait loop is one mutex acquisition per interval.
const POLL_INTERVAL: Duration = Duration::from_millis(15);
/// Number of reaped exit codes remembered so a second `wait_for_exit` call still
/// answers `Exited` instead of `Unknown`.
const EXIT_MEMORY: usize = 64;

#[derive(Default)]
struct State {
    running: BTreeMap<u32, Child>,
    exited: BTreeMap<u32, i32>,
    exit_order: Vec<u32>,
}

/// Creates children with captured stdio and supervises them by pid.
pub struct StdProcesses {
    state: Mutex<State>,
    capture_stdio: bool,
}

impl StdProcesses {
    /// Capture stdout/stderr so the owning module can drain one log stream.
    pub fn capturing() -> Self {
        Self {
            state: Mutex::new(State::default()),
            capture_stdio: true,
        }
    }

    /// Do not capture: the child inherits the host streams. Use only for
    /// processes whose output nobody needs.
    pub fn inheriting() -> Self {
        Self {
            state: Mutex::new(State::default()),
            capture_stdio: false,
        }
    }

    /// Number of children this object still owns (running or recently exited).
    pub fn tracked(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.running.len() + state.exited.len())
            .unwrap_or(0)
    }

    fn remember_exit(&self, pid: u32, code: i32) {
        if let Ok(mut state) = self.state.lock() {
            state.running.remove(&pid);
            state.exited.insert(pid, code);
            state.exit_order.push(pid);
            while state.exit_order.len() > EXIT_MEMORY {
                let oldest = state.exit_order.remove(0);
                state.exited.remove(&oldest);
            }
        }
    }
}

impl ProcessLauncher for StdProcesses {
    fn spawn(&self, request: &ProcessRequest) -> io::Result<ProcessHandle> {
        let mut command = Command::new(&request.program);
        command
            .args(&request.args)
            .current_dir(&request.working_dir)
            // Overlay the requested variables on the inherited environment; a
            // game needs the host environment plus a few overrides.
            .envs(request.environment.iter())
            .stdin(Stdio::null());
        if self.capture_stdio {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        } else {
            command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        }

        let child = command.spawn()?;
        let pid = child.id();
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("process registry is poisoned"))?;
        state.running.insert(pid, child);
        Ok(ProcessHandle { pid })
    }
}

impl ProcessSupervisor for StdProcesses {
    fn take_streams(&self, pid: u32) -> Option<ChildStreams> {
        let mut state = self.state.lock().ok()?;
        let child = state.running.get_mut(&pid)?;
        // Tolerate a caller that took stdout only: each side is optional in the
        // child, so a missing side becomes an empty stream instead of losing the
        // other one.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        if stdout.is_none() && stderr.is_none() {
            return None;
        }
        Some(ChildStreams {
            stdout: match stdout {
                Some(stream) => Box::new(stream),
                None => Box::new(io::empty()),
            },
            stderr: match stderr {
                Some(stream) => Box::new(stream),
                None => Box::new(io::empty()),
            },
        })
    }

    fn wait_for_exit(&self, pid: u32, timeout: Option<Duration>) -> WaitOutcome {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            {
                let mut state = match self.state.lock() {
                    Ok(state) => state,
                    Err(_) => return WaitOutcome::Unknown,
                };
                if let Some(code) = state.exited.get(&pid) {
                    return WaitOutcome::Exited(*code);
                }
                let child = match state.running.get_mut(&pid) {
                    Some(child) => child,
                    None => return WaitOutcome::Unknown,
                };
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let code = status.code().unwrap_or(-1);
                        drop(state);
                        self.remember_exit(pid, code);
                        return WaitOutcome::Exited(code);
                    }
                    Ok(None) => {}
                    Err(_) => return WaitOutcome::Unknown,
                }
            }

            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    return WaitOutcome::TimedOut;
                }
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn is_alive(&self, pid: u32) -> bool {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return false,
        };
        if state.exited.contains_key(&pid) {
            return false;
        }
        match state.running.get_mut(&pid) {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code().unwrap_or(-1);
                    drop(state);
                    self.remember_exit(pid, code);
                    false
                }
                Ok(None) => true,
                Err(_) => false,
            },
            None => false,
        }
    }

    fn request_stop(&self, pid: u32) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("process registry is poisoned"))?;
        match state.running.get_mut(&pid) {
            Some(child) => match child.kill() {
                Ok(()) => Ok(()),
                // Already exited between the caller's check and this call.
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(()),
                Err(error) => Err(error),
            },
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the supervisor does not own this process",
            )),
        }
    }
}

impl std::fmt::Debug for StdProcesses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdProcesses")
            .field("capture_stdio", &self.capture_stdio)
            .field("tracked", &self.tracked())
            .finish()
    }
}

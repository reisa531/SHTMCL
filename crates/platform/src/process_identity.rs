//! Read-only process identity checks for games surviving a launcher restart.
//! A PID alone is insufficient: Windows can reuse it after the original exits.
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Windows creation FILETIME, in 100 ns ticks since 1601-01-01 UTC.
    pub creation_time: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessLiveness {
    Alive,
    /// The recorded process exited, including when its PID has been reused.
    Exited,
    /// Insufficient access, unsupported platform, or an inconclusive OS result.
    /// Callers must keep instance protection rather than interpret this as exit.
    Unknown,
}

pub fn capture(pid: u32) -> io::Result<ProcessIdentity> {
    platform::capture(pid)
}

pub fn probe(identity: &ProcessIdentity) -> ProcessLiveness {
    platform::probe(identity)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;

    type Handle = *mut c_void;
    const QUERY_AND_SYNCHRONIZE: u32 = 0x1000 | 0x0010_0000;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetProcessTimes(
            handle: Handle,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    }

    struct Process(Handle);
    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: this handle is uniquely owned and came from OpenProcess.
            unsafe { CloseHandle(self.0) };
        }
    }
    impl Process {
        fn open(pid: u32) -> io::Result<Self> {
            if pid == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "zero PID"));
            }
            // SAFETY: no pointers are passed; handle inheritance is disabled.
            let handle = unsafe { OpenProcess(QUERY_AND_SYNCHRONIZE, 0, pid) };
            if handle.is_null() {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }

        fn creation_time(&self) -> io::Result<u64> {
            let mut creation = FileTime::default();
            let mut exit = FileTime::default();
            let mut kernel = FileTime::default();
            let mut user = FileTime::default();
            // SAFETY: the handle is live, and all output pointers are writable.
            if unsafe { GetProcessTimes(self.0, &mut creation, &mut exit, &mut kernel, &mut user) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok((u64::from(creation.high) << 32) | u64::from(creation.low))
        }
    }

    pub(super) fn capture(pid: u32) -> io::Result<ProcessIdentity> {
        let process = Process::open(pid)?;
        Ok(ProcessIdentity {
            pid,
            creation_time: process.creation_time()?,
        })
    }

    pub(super) fn probe(identity: &ProcessIdentity) -> ProcessLiveness {
        if identity.pid == 0 || identity.creation_time == 0 {
            return ProcessLiveness::Unknown;
        }
        let process = match Process::open(identity.pid) {
            Ok(process) => process,
            Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER) => {
                return ProcessLiveness::Exited;
            }
            Err(_) => return ProcessLiveness::Unknown,
        };
        match process.creation_time() {
            Ok(time) if time != identity.creation_time => return ProcessLiveness::Exited,
            Err(_) => return ProcessLiveness::Unknown,
            Ok(_) => {}
        }
        // SAFETY: the same owned handle is used for identity and liveness, so
        // PID reuse between these checks cannot select a different process.
        match unsafe { WaitForSingleObject(process.0, 0) } {
            WAIT_OBJECT_0 => ProcessLiveness::Exited,
            WAIT_TIMEOUT => ProcessLiveness::Alive,
            _ => ProcessLiveness::Unknown,
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    pub(super) fn capture(_pid: u32) -> io::Result<ProcessIdentity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process identity requires Windows",
        ))
    }
    pub(super) fn probe(_identity: &ProcessIdentity) -> ProcessLiveness {
        ProcessLiveness::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn current_process_identity_is_stable_and_alive() {
        let identity = capture(std::process::id()).unwrap();
        assert_ne!(identity.creation_time, 0);
        assert_eq!(capture(identity.pid).unwrap(), identity);
        assert_eq!(probe(&identity), ProcessLiveness::Alive);
    }

    #[cfg(windows)]
    #[test]
    fn reused_pid_identity_does_not_match() {
        let mut identity = capture(std::process::id()).unwrap();
        identity.creation_time += 1;
        assert_eq!(probe(&identity), ProcessLiveness::Exited);
    }

    #[cfg(windows)]
    #[test]
    fn own_child_exit_is_detected_without_adopting_or_stopping_it() {
        use std::os::windows::process::CommandExt;
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "exit", "0"])
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        // The Child owns a Windows handle even if it exits before capture.
        let identity = capture(child.id()).unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(probe(&identity), ProcessLiveness::Exited);
    }

    #[test]
    fn invalid_identity_is_unknown() {
        assert_eq!(
            probe(&ProcessIdentity {
                pid: 0,
                creation_time: 1
            }),
            ProcessLiveness::Unknown
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unsupported_platform_is_explicit() {
        assert_eq!(
            capture(std::process::id()).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }
}

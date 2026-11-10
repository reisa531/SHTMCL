//! Operating-system boundary shared by core modules.
//!
//! This crate owns path layout, lexical path containment, file identity and the
//! process launch port. It intentionally contains no game, download or account
//! business logic. Concrete process launching is supplied by the application or
//! by 02's runtime implementation.

pub mod dirs;
pub mod entropy;
pub mod fs;
pub mod process;
pub mod process_identity;
pub mod process_std;

pub use dirs::{PathProvider, PlatformDirs};
pub use entropy::{fill_random, random_bytes, random_hex};
pub use fs::{probe_file, safe_join, FileIdentity, PathError};
pub use process::{
    ChildStreams, ProcessHandle, ProcessLauncher, ProcessRequest, ProcessSupervisor, WaitOutcome,
};
pub use process_std::StdProcesses;

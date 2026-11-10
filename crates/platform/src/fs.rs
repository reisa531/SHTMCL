//! Lexical path containment and file identity probes.
//!
//! `safe_join` is a lexical guard only: it rejects absolute inputs and `..`
//! components before the path reaches the filesystem. Callers that follow
//! symlinks or junctions must still re-check the resolved location.

use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    Absolute,
    EscapesBase,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absolute => f.write_str("path must be relative to the base directory"),
            Self::EscapesBase => f.write_str("path must not leave the base directory"),
        }
    }
}

impl std::error::Error for PathError {}

pub fn safe_join(base: &Path, relative: &Path) -> Result<PathBuf, PathError> {
    if relative.is_absolute() {
        return Err(PathError::Absolute);
    }

    let mut joined = base.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => joined.push(part),
            Component::CurDir => {}
            Component::ParentDir => return Err(PathError::EscapesBase),
            Component::RootDir | Component::Prefix(_) => return Err(PathError::Absolute),
        }
    }
    Ok(joined)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub len: u64,
    pub modified_unix_ms: Option<u128>,
}

impl FileIdentity {
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        let modified_unix_ms = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis());
        Self {
            len: metadata.len(),
            modified_unix_ms,
        }
    }
}

pub fn probe_file(path: &Path) -> io::Result<FileIdentity> {
    let metadata = std::fs::metadata(path)?;
    Ok(FileIdentity::from_metadata(&metadata))
}

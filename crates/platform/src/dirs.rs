//! Application directory layout. The host application chooses the root; this
//! crate only derives consistent children and creates them when asked.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformDirs {
    pub root: PathBuf,
    pub data: PathBuf,
    pub config: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
    pub instances: PathBuf,
}

impl PlatformDirs {
    pub fn under(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        Self {
            data: root.join("data"),
            config: root.join("config"),
            cache: root.join("cache"),
            logs: root.join("logs"),
            instances: root.join("instances"),
            root,
        }
    }

    pub fn create_all(&self) -> std::io::Result<()> {
        for path in [
            &self.root,
            &self.data,
            &self.config,
            &self.cache,
            &self.logs,
            &self.instances,
        ] {
            std::fs::create_dir_all(path)?;
        }
        Ok(())
    }
}

pub trait PathProvider {
    fn dirs(&self) -> &PlatformDirs;
}

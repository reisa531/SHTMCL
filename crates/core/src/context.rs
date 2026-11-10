//! What the application hands to every module implementation.
//!
//! Modules receive one [`ModuleContext`] at construction and keep it. They use it
//! to create tasks, publish events and resolve directories; they never reach for
//! global state, and they never build a second scheduler.

use std::sync::Arc;

use shtmcl_platform::PlatformDirs;

use crate::tasks::{EventSink, TaskService};

#[derive(Clone)]
pub struct ModuleContext {
    tasks: Arc<dyn TaskService>,
    events: Arc<dyn EventSink>,
    dirs: PlatformDirs,
}

impl ModuleContext {
    pub fn new(
        tasks: Arc<dyn TaskService>,
        events: Arc<dyn EventSink>,
        dirs: PlatformDirs,
    ) -> Self {
        Self {
            tasks,
            events,
            dirs,
        }
    }

    pub fn tasks(&self) -> &Arc<dyn TaskService> {
        &self.tasks
    }

    pub fn events(&self) -> &Arc<dyn EventSink> {
        &self.events
    }

    pub fn dirs(&self) -> &PlatformDirs {
        &self.dirs
    }
}

impl std::fmt::Debug for ModuleContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleContext")
            .field("root", &self.dirs.root)
            .finish_non_exhaustive()
    }
}

//! Application layer: the single composition root for modules, tasks and events.
//!
//! The desktop bridge owns one [`CoreApp`]. Modules are registered here after
//! they receive [`crate::ModuleContext`]; the UI only reads snapshots and
//! requests actions through this object, so there is exactly one task
//! scheduler, one event stream and one cancellation entry point.

use std::sync::Arc;

use shtmcl_domain::{
    CancelTaskOutcome, CancelTaskRequest, CommandKind, CommandRequest, CommandResponse,
    ErrorEnvelope, ModuleId, Task,
};
use shtmcl_platform::PlatformDirs;

use crate::context::ModuleContext;
use crate::registry::RegistryError;
use crate::tasks::{EventSink, InMemoryEvents, InMemoryTasks, TaskService};
use crate::{CoreModule, ModuleRegistry};

pub struct CoreApp {
    tasks: Arc<dyn TaskService>,
    events: Arc<dyn EventSink>,
    registry: ModuleRegistry,
    dirs: PlatformDirs,
}

impl CoreApp {
    pub fn new(dirs: PlatformDirs) -> Self {
        let events: Arc<dyn EventSink> = Arc::new(InMemoryEvents::new());
        let tasks: Arc<dyn TaskService> = Arc::new(InMemoryTasks::new(events.clone()));
        Self {
            tasks,
            events,
            registry: ModuleRegistry::new(),
            dirs,
        }
    }

    /// The context a module is constructed with. Copies share the same task table
    /// and event stream.
    pub fn context(&self) -> ModuleContext {
        ModuleContext::new(self.tasks.clone(), self.events.clone(), self.dirs.clone())
    }

    pub fn register(&mut self, module: Arc<dyn CoreModule>) -> Result<(), RegistryError> {
        self.registry.register(module)
    }

    pub fn dispatch(
        &self,
        module: ModuleId,
        request: &CommandRequest,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        self.registry.dispatch(module, request)
    }

    /// Dispatch a mutation. Enforces the shared transport rule once, at the
    /// single dispatch point, instead of relying on every module to remember it.
    pub fn dispatch_mutation(
        &self,
        module: ModuleId,
        request: &CommandRequest,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        Self::check_command_owner(module, request)?;
        self.registry.dispatch(module, request)
    }

    /// Dispatch a read. A read may omit `operation_id`.
    pub fn dispatch_read(
        &self,
        module: ModuleId,
        request: &CommandRequest,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Read)?;
        Self::check_command_owner(module, request)?;
        self.registry.dispatch(module, request)
    }

    /// A command name belongs to exactly one module namespace
    /// (`launch.prepare`, `content.plan`, …), so logs and permissions never have
    /// to guess the owner.
    fn check_command_owner(
        module: ModuleId,
        request: &CommandRequest,
    ) -> Result<(), ErrorEnvelope> {
        let command = request.command()?;
        if command.owner() == module.as_str() {
            Ok(())
        } else {
            Err(ErrorEnvelope::command_name_mismatch(
                module.as_str(),
                command.as_str(),
            ))
        }
    }

    pub fn snapshot(&self) -> Vec<Task> {
        self.tasks.snapshots()
    }

    pub fn request_cancel(&self, request: &CancelTaskRequest) -> CancelTaskOutcome {
        self.tasks.request_cancel(request)
    }

    pub fn module_ids(&self) -> Vec<ModuleId> {
        self.registry.ids().collect()
    }

    pub fn task_service(&self) -> &Arc<dyn TaskService> {
        &self.tasks
    }

    pub fn event_sink(&self) -> &Arc<dyn EventSink> {
        &self.events
    }

    pub fn dirs(&self) -> &PlatformDirs {
        &self.dirs
    }
}

impl std::fmt::Debug for CoreApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreApp")
            .field("dirs", &self.dirs.root)
            .field("modules", &self.registry.len())
            .finish_non_exhaustive()
    }
}

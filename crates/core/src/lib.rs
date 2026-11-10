//! Core module boundary.
//!
//! Each business module (01—08) implements [`CoreModule`] inside its own
//! directory and the application layer registers it. The registry is the only
//! dispatch point in this crate; it does not schedule work, hold credentials or
//! perform module business. Task scheduling and event sequencing live in
//! [`tasks`] and are handed to modules through [`ModuleContext`].
//!
//! Frozen integration pattern:
//! 1. the app layer builds [`CoreApp`] with the platform directories,
//! 2. each module is constructed with `app.context()` and then registered,
//! 3. long operations return `CommandResponse::Accepted` after
//!    [`tasks::TaskService::accept`]; progress and results travel as events,
//! 4. cancellation enters only through [`tasks::TaskService::request_cancel`].

pub mod accounts;
pub mod app;
pub mod content;
pub mod context;
pub mod diagnostics;
// Research branch (module 09). Not registered in the app and not depended on by
// any shipped module; see `distribution::WIRED_INTO_PRODUCT`. Declared here only
// so its PoC and measurements compile and run.
pub mod distribution;
pub mod downloads;
pub mod instances;
pub mod launch;
pub mod registry;
pub mod rooms;
pub mod servers;
pub mod tasks;

use shtmcl_domain::{CommandRequest, CommandResponse, ErrorEnvelope, ModuleId};

/// A long-lived module service. Implementations must be thread-safe and must
/// return an `accepted` response for work that outlives the request.
pub trait CoreModule: Send + Sync {
    fn id(&self) -> ModuleId;

    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope>;
}

pub use app::CoreApp;
pub use context::ModuleContext;
pub use registry::{ModuleRegistry, RegistryError};
pub use tasks::{
    bytes_progress, EventObserver, EventRequest, EventSink, InMemoryEvents, InMemoryTasks,
    TaskService, TaskSpec,
};

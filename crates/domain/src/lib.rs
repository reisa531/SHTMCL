//! Shared domain contract for SHTMCL (contract version 1).
//!
//! This crate is the single owner of cross-module state definitions. Modules may
//! depend on these types, but must not copy them into their own crates.
//!
//! Encoding conventions:
//! - JSON fields are camelCase.
//! - Enum values are snake_case unless documented otherwise.
//! - `revision`, `attempt_generation` and `sequence` are distinct concepts and
//!   must not be substituted for one another.

pub mod cancel;
pub mod command;
pub mod error;
pub mod event;
pub mod ids;
pub mod module;
pub mod revision;
pub mod task;
pub mod view;

pub use cancel::{CancelDecision, CancelTaskOutcome, CancelTaskRequest};
pub use command::{CommandKind, CommandName, CommandNameError, CommandRequest, CommandResponse};
pub use error::{
    ErrorCategory, ErrorCode, ErrorCodeError, ErrorEnvelope, UserAction,
    USER_ACTION_CHOOSE_INSTANCE, USER_ACTION_OPEN_DETAILS, USER_ACTION_REAUTHENTICATE,
    USER_ACTION_RELOAD, USER_ACTION_RETRY,
};
pub use event::{
    EventEnvelope, EventError, EventKind, SchemaVersion, StreamCursor, TaskProgressPayload,
    TaskResultPayload, TaskStatePayload, SUPPORTED_SCHEMA_VERSION,
};
pub use ids::{
    AccountId, ArtifactId, EntityId, EventId, InstanceId, LaunchSessionId, OperationId, RequestId,
    RoomId, ServerId, TaskId,
};
pub use module::ModuleId;
pub use revision::{AttemptGeneration, Revision, Sequence};
pub use task::{ProgressUnit, Task, TaskError, TaskPhase, TaskProgress, TaskState};
pub use view::{
    ActionAvailability, AvailabilityProjection, AvailabilityState, CapabilityId, MessageAction,
    MessageActionKind, MessageActionRole, MessageCategory, UserMessage, ViewError,
};

/// Version of the public command/event/task schema in this workspace.
pub const CONTRACT_VERSION: u16 = 1;

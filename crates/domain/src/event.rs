//! Event envelope and stream cursor contract.
//!
//! `sequence` is monotonic only within the stream identified by
//! `operation_id`; it is not a global ordering across modules. Consumers
//! deduplicate by `event_id` and, after a gap or reconnect, read a snapshot plus
//! the last applied cursor before resuming.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ErrorEnvelope;
use crate::ids::{EntityId, EventId, OperationId, TaskId};
use crate::revision::{AttemptGeneration, Revision, Sequence};
use crate::task::{ProgressUnit, TaskPhase, TaskState};

pub const SUPPORTED_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SchemaVersion(u16);

impl SchemaVersion {
    pub const V1: Self = Self(SUPPORTED_SCHEMA_VERSION);

    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u16 {
        self.0
    }

    pub const fn is_supported(self) -> bool {
        self.0 == SUPPORTED_SCHEMA_VERSION
    }
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventKind(String);

impl EventKind {
    pub const TASK_STATE: &'static str = "task.state";
    pub const TASK_PROGRESS: &'static str = "task.progress";
    pub const TASK_RESULT: &'static str = "task.result";

    pub fn new(value: impl Into<String>) -> Result<Self, EventError> {
        let value = value.into();
        if value.is_empty() || value.len() > 64 || value.chars().any(char::is_control) {
            return Err(EventError::InvalidKind);
        }
        Ok(Self(value))
    }

    pub fn from_static(value: &'static str) -> Self {
        debug_assert!(
            Self::new(value).is_ok(),
            "invalid static event kind: {value}"
        );
        Self(value.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    pub event_id: EventId,
    pub schema_version: SchemaVersion,
    pub operation_id: OperationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<EntityId>,
    pub revision: Revision,
    pub sequence: Sequence,
    pub attempt_generation: AttemptGeneration,
    pub kind: EventKind,
    pub payload: Value,
}

impl EventEnvelope {
    pub fn validate(&self) -> Result<(), EventError> {
        if !self.schema_version.is_supported() {
            return Err(EventError::UnsupportedSchemaVersion {
                version: self.schema_version.get(),
            });
        }
        if self.kind.as_str().is_empty() || self.kind.as_str().len() > 64 {
            return Err(EventError::InvalidKind);
        }
        Ok(())
    }
}

/// Cursor persisted by a consumer so it can resume a single operation stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamCursor {
    pub operation_id: OperationId,
    pub sequence: Sequence,
}

/// Payload of `task.state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatePayload {
    pub task_id: TaskId,
    pub state: TaskState,
    pub phase: TaskPhase,
    pub cancel_requested: bool,
}

/// Payload of `task.progress`. `total` is `null` while the module cannot
/// estimate it, and `current` must never exceed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskProgressPayload {
    pub task_id: TaskId,
    pub current: u64,
    pub total: Option<u64>,
    pub unit: ProgressUnit,
}

/// Payload of `task.result`. Exactly one of `result` / `error` is present for a
/// terminal state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskResultPayload {
    pub task_id: TaskId,
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorEnvelope>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventError {
    UnsupportedSchemaVersion { version: u16 },
    InvalidKind,
}

impl fmt::Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion { version } => {
                write!(f, "unsupported event schema version {version}")
            }
            Self::InvalidKind => {
                f.write_str("event kind must be non-empty, at most 64 bytes and control-free")
            }
        }
    }
}

impl std::error::Error for EventError {}

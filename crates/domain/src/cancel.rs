//! Cancellation request and decision contract.
//!
//! Cancellation is cooperative: requesting it only sets `Task::cancel_requested`.
//! A module may emit `cancelled` only after reaching a safe stop/rollback point.

use serde::{Deserialize, Serialize};

use crate::ids::TaskId;
use crate::revision::AttemptGeneration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelTaskRequest {
    pub task_id: TaskId,
    pub attempt_generation: AttemptGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelDecision {
    /// Cancellation was recorded for the current attempt.
    Requested,
    /// The same attempt already has a pending cancellation request.
    AlreadyRequested,
    /// The task is already in a terminal state.
    NotCancellable,
    /// The generation is stale; the caller must not act on the current task.
    StaleGeneration,
    /// No task with this identifier exists in this process snapshot.
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelTaskOutcome {
    pub task_id: TaskId,
    pub decision: CancelDecision,
}

//! Task state, progress and the shared cancellation/retry state machine.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cancel::CancelDecision;
use crate::error::ErrorEnvelope;
use crate::ids::{InstanceId, OperationId, TaskId};
use crate::revision::{AttemptGeneration, Revision};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    Paused,
    WaitingUser,
    Succeeded,
    Failed,
    Cancelled,
}

impl TaskState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    /// Allowed forward transitions. Reaching a terminal state is irreversible;
    /// a retry creates a new attempt generation rather than moving backwards.
    pub const fn can_transition_to(self, next: Self) -> bool {
        match (self, next) {
            (Self::Queued, Self::Running | Self::Cancelled | Self::Failed) => true,
            (
                Self::Running,
                Self::Paused | Self::WaitingUser | Self::Succeeded | Self::Failed | Self::Cancelled,
            ) => true,
            (Self::Paused, Self::Running | Self::Cancelled | Self::Failed) => true,
            (Self::WaitingUser, Self::Running | Self::Cancelled | Self::Failed) => true,
            _ => false,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::WaitingUser => "waiting_user",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Module-defined stable phase identifier, e.g. `download`, `verify`, `install`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskPhase(String);

impl TaskPhase {
    pub fn new(value: impl Into<String>) -> Result<Self, TaskError> {
        let value = value.into();
        if value.is_empty() || value.len() > 64 || value.chars().any(char::is_control) {
            return Err(TaskError::InvalidPhase);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn queued() -> Self {
        Self("queued".to_string())
    }
}

impl fmt::Display for TaskPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressUnit {
    Bytes,
    Items,
    Steps,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskProgress {
    pub current: u64,
    /// `None` means the module cannot estimate a total yet.
    pub total: Option<u64>,
    pub unit: ProgressUnit,
}

impl TaskProgress {
    pub fn new(current: u64, total: Option<u64>, unit: ProgressUnit) -> Result<Self, TaskError> {
        let progress = Self {
            current,
            total,
            unit,
        };
        progress.validate()?;
        Ok(progress)
    }

    pub fn validate(&self) -> Result<(), TaskError> {
        if let Some(total) = self.total {
            if self.current > total {
                return Err(TaskError::ProgressOutOfRange {
                    current: self.current,
                    total: Some(total),
                });
            }
        }
        Ok(())
    }

    pub fn percent(&self) -> Option<f64> {
        self.total
            .filter(|total| *total > 0)
            .map(|total| (self.current as f64 / total as f64) * 100.0)
            .map(|percent| percent.min(100.0))
    }
}

/// Task snapshot exposed by the application layer.
///
/// `phase` and `state` are separate: a download may be `running` in phase
/// `verify`, and a task may be `waiting_user` in phase `auth`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: TaskId,
    pub operation_id: OperationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<InstanceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_revision: Option<Revision>,
    pub attempt_generation: AttemptGeneration,
    pub state: TaskState,
    pub phase: TaskPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<TaskProgress>,
    pub cancel_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorEnvelope>,
    /// Evidence markers the module actually produced (for example
    /// `resumable_ranges`, `original_file_preserved`). Copy that asserts an
    /// outcome may only be shown when its marker is present here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

impl Task {
    pub fn new(
        id: TaskId,
        operation_id: OperationId,
        attempt_generation: AttemptGeneration,
    ) -> Self {
        Self {
            id,
            operation_id,
            instance_id: None,
            input_revision: None,
            attempt_generation,
            state: TaskState::Queued,
            phase: TaskPhase::queued(),
            progress: None,
            cancel_requested: false,
            result: None,
            error: None,
            evidence: Vec::new(),
        }
    }

    /// Attach one evidence marker. Duplicates are ignored so a module can report
    /// the same fact from several code paths.
    pub fn add_evidence(&mut self, marker: impl Into<String>) {
        let marker = marker.into();
        if !marker.is_empty() && !self.evidence.contains(&marker) {
            self.evidence.push(marker);
        }
    }

    pub fn validate(&self) -> Result<(), TaskError> {
        if self.attempt_generation.get() == 0 {
            return Err(TaskError::InvalidAttemptGeneration);
        }
        TaskPhase::new(self.phase.as_str().to_string())?;
        if let Some(progress) = &self.progress {
            progress.validate()?;
        }
        Ok(())
    }

    pub fn accepts_generation(&self, generation: AttemptGeneration) -> bool {
        generation == self.attempt_generation
    }

    fn ensure_generation(&self, actual: AttemptGeneration) -> Result<(), TaskError> {
        if actual == self.attempt_generation {
            Ok(())
        } else {
            Err(TaskError::StaleGeneration {
                expected: self.attempt_generation,
                actual,
            })
        }
    }

    pub fn transition(
        &mut self,
        next: TaskState,
        generation: AttemptGeneration,
    ) -> Result<(), TaskError> {
        self.ensure_generation(generation)?;
        if !self.state.can_transition_to(next) {
            return Err(TaskError::InvalidTransition {
                from: self.state,
                to: next,
            });
        }
        if next == TaskState::Cancelled && !self.cancel_requested {
            return Err(TaskError::CancelNotRequested);
        }
        self.state = next;
        Ok(())
    }

    pub fn set_phase(
        &mut self,
        phase: TaskPhase,
        generation: AttemptGeneration,
    ) -> Result<(), TaskError> {
        self.ensure_generation(generation)?;
        if self.state.is_terminal() {
            return Err(TaskError::TerminalTask { state: self.state });
        }
        self.phase = phase;
        Ok(())
    }

    pub fn set_progress(
        &mut self,
        progress: TaskProgress,
        generation: AttemptGeneration,
    ) -> Result<(), TaskError> {
        self.ensure_generation(generation)?;
        if !matches!(
            self.state,
            TaskState::Running | TaskState::Paused | TaskState::WaitingUser
        ) {
            return Err(TaskError::ProgressNotAllowed { state: self.state });
        }
        progress.validate()?;
        self.progress = Some(progress);
        Ok(())
    }

    /// Record a cancellation request. The state is not changed here.
    pub fn request_cancel(&mut self, generation: AttemptGeneration) -> CancelDecision {
        if generation != self.attempt_generation {
            return CancelDecision::StaleGeneration;
        }
        if self.state.is_terminal() {
            return CancelDecision::NotCancellable;
        }
        if self.cancel_requested {
            return CancelDecision::AlreadyRequested;
        }
        self.cancel_requested = true;
        CancelDecision::Requested
    }

    /// Confirm cancellation only at a safe point after `request_cancel`.
    pub fn mark_cancelled(&mut self, generation: AttemptGeneration) -> Result<(), TaskError> {
        self.transition(TaskState::Cancelled, generation)
    }

    pub fn succeed(
        &mut self,
        result: Option<Value>,
        generation: AttemptGeneration,
    ) -> Result<(), TaskError> {
        self.transition(TaskState::Succeeded, generation)?;
        self.result = result;
        self.error = None;
        Ok(())
    }

    pub fn fail(
        &mut self,
        error: ErrorEnvelope,
        generation: AttemptGeneration,
    ) -> Result<(), TaskError> {
        self.transition(TaskState::Failed, generation)?;
        self.error = Some(error);
        Ok(())
    }

    /// Start a new attempt after a terminal failure/cancellation. The increased
    /// generation fences late responses and events from the previous attempt.
    pub fn begin_retry(&mut self) -> Result<AttemptGeneration, TaskError> {
        if !matches!(self.state, TaskState::Failed | TaskState::Cancelled) {
            return Err(TaskError::RetryNotAllowed { state: self.state });
        }
        let next = self
            .attempt_generation
            .checked_next()
            .ok_or(TaskError::GenerationExhausted)?;
        self.attempt_generation = next;
        self.state = TaskState::Queued;
        self.phase = TaskPhase::queued();
        self.progress = None;
        self.cancel_requested = false;
        self.result = None;
        self.error = None;
        self.evidence.clear();
        Ok(next)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskError {
    StaleGeneration {
        expected: AttemptGeneration,
        actual: AttemptGeneration,
    },
    InvalidTransition {
        from: TaskState,
        to: TaskState,
    },
    CancelNotRequested,
    RetryNotAllowed {
        state: TaskState,
    },
    GenerationExhausted,
    TerminalTask {
        state: TaskState,
    },
    ProgressNotAllowed {
        state: TaskState,
    },
    ProgressOutOfRange {
        current: u64,
        total: Option<u64>,
    },
    InvalidPhase,
    InvalidAttemptGeneration,
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleGeneration { expected, actual } => {
                write!(
                    f,
                    "stale attempt generation: expected {expected}, got {actual}"
                )
            }
            Self::InvalidTransition { from, to } => {
                write!(f, "invalid task transition: {from} -> {to}")
            }
            Self::CancelNotRequested => {
                f.write_str("cancellation was not confirmed at a safe point")
            }
            Self::RetryNotAllowed { state } => write!(f, "cannot retry task in state {state}"),
            Self::GenerationExhausted => f.write_str("attempt generation exhausted"),
            Self::TerminalTask { state } => write!(f, "task is terminal ({state})"),
            Self::ProgressNotAllowed { state } => {
                write!(f, "progress is not allowed in state {state}")
            }
            Self::ProgressOutOfRange { current, total } => {
                write!(f, "progress {current} exceeds total {total:?}")
            }
            Self::InvalidPhase => f.write_str("task phase must be non-empty and at most 64 bytes"),
            Self::InvalidAttemptGeneration => f.write_str("attempt generation must be at least 1"),
        }
    }
}

impl std::error::Error for TaskError {}

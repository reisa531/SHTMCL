//! Game sessions: preparation, process, run and exit.
//!
//! A session is module 02's own state machine and is deliberately *not* the
//! task state. "A process exists" and "the game reached a menu" are different
//! facts, so `state == ProcessRunning` never means playable: `game_reached`
//! stays `None` until a probe produces real evidence.
//!
//! Idempotency: one click → one session. A repeated `launch.prepare` with the
//! same idempotency key returns the live session instead of creating a second
//! one. A second *concurrent* session needs the explicit `launch.start_again`
//! entry, and only when the frozen snapshot allows multi-open.

use std::collections::BTreeMap;

use serde_json::Value;
use shtmcl_domain::{
    AttemptGeneration, ErrorEnvelope, InstanceId, LaunchSessionId, OperationId, Revision, TaskId,
};

use crate::launch::errors;

/// Session lifecycle. Separate from `TaskState` on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchSessionState {
    /// Snapshot/auth/runtime/files checks are running.
    Preparing,
    /// The process is being created (cancellation is serialized here).
    Spawning,
    /// The process exists. Not evidence that the game is playable.
    ProcessRunning,
    /// The process ended.
    Exited,
    /// Preparation or process creation failed.
    Failed,
    /// Cancelled at a safe point.
    Cancelled,
}

impl LaunchSessionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Spawning => "spawning",
            Self::ProcessRunning => "process_running",
            Self::Exited => "exited",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Exited | Self::Failed | Self::Cancelled)
    }

    pub const fn is_live(self) -> bool {
        !self.is_terminal()
    }

    /// Allowed forward transitions; terminal states never move again.
    pub const fn can_transition_to(self, next: Self) -> bool {
        match (self, next) {
            (Self::Preparing, Self::Spawning | Self::Failed | Self::Cancelled) => true,
            (Self::Spawning, Self::ProcessRunning | Self::Failed | Self::Cancelled) => true,
            (Self::ProcessRunning, Self::Exited | Self::Failed | Self::Cancelled) => true,
            _ => false,
        }
    }
}

/// Process identity: pid plus a token that makes pid reuse detectable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Creation time of *our* handle, used to reject a recycled pid.
    pub started_at_unix_ms: u64,
    /// Opaque platform-handle identity surrogate (`session:pid:attempt`).
    pub token: String,
}

impl ProcessIdentity {
    pub fn new(pid: u32, started_at_unix_ms: u64, token: impl Into<String>) -> Self {
        Self {
            pid,
            started_at_unix_ms,
            token: token.into(),
        }
    }

    pub fn matches(&self, other: &ProcessIdentity) -> bool {
        self.pid == other.pid && self.token == other.token
    }
}

pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// One launch session.
#[derive(Debug, Clone)]
pub struct LaunchSession {
    pub id: LaunchSessionId,
    pub instance_id: InstanceId,
    pub operation_id: OperationId,
    pub task_id: TaskId,
    pub attempt_generation: AttemptGeneration,
    /// Key that makes a repeated click resolve to this session.
    pub idempotency_key: String,
    /// Instance revision the session was frozen against (`0` before the
    /// snapshot is read). Session events carry it as their `revision`.
    pub instance_revision: Revision,
    pub plan_id: Option<String>,
    pub runtime_id: Option<String>,
    pub java_major: Option<u32>,
    pub state: LaunchSessionState,
    pub identity: Option<ProcessIdentity>,
    pub exit_code: Option<i32>,
    /// `None` = unknown. Only a probe can set `Some(true)`.
    pub game_reached: Option<bool>,
    pub error: Option<ErrorEnvelope>,
    pub evidence: Vec<String>,
    pub multi_open: bool,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl LaunchSession {
    pub fn new(
        id: LaunchSessionId,
        instance_id: InstanceId,
        operation_id: OperationId,
        task_id: TaskId,
        attempt_generation: AttemptGeneration,
        idempotency_key: impl Into<String>,
        multi_open: bool,
    ) -> Self {
        let now = now_unix_ms();
        Self {
            id,
            instance_id,
            operation_id,
            task_id,
            attempt_generation,
            idempotency_key: idempotency_key.into(),
            instance_revision: Revision::ZERO,
            plan_id: None,
            runtime_id: None,
            java_major: None,
            state: LaunchSessionState::Preparing,
            identity: None,
            exit_code: None,
            game_reached: None,
            error: None,
            evidence: Vec::new(),
            multi_open,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
        }
    }

    pub fn add_evidence(&mut self, marker: impl Into<String>) {
        let marker = marker.into();
        if !marker.is_empty() && !self.evidence.contains(&marker) {
            self.evidence.push(marker);
        }
    }

    pub fn has_evidence(&self, marker: &str) -> bool {
        self.evidence.iter().any(|existing| existing == marker)
    }

    pub fn transition(&mut self, next: LaunchSessionState) -> Result<(), ErrorEnvelope> {
        if !self.state.can_transition_to(next) {
            return Err(errors::with_detail(
                errors::codes::SESSION_CONFLICT,
                shtmcl_domain::ErrorCategory::Conflict,
                false,
                format!(
                    "session:{}:{}->{}",
                    self.id.as_str(),
                    self.state.as_str(),
                    next.as_str()
                ),
            ));
        }
        self.state = next;
        self.updated_at_unix_ms = now_unix_ms();
        Ok(())
    }

    pub fn attach_process(&mut self, identity: ProcessIdentity) {
        self.identity = Some(identity);
        self.updated_at_unix_ms = now_unix_ms();
    }

    /// Redacted snapshot for commands, events and the UI.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "sessionId": self.id.as_str(),
            "instanceId": self.instance_id.as_str(),
            "operationId": self.operation_id.as_str(),
            "taskId": self.task_id.as_str(),
            "attemptGeneration": self.attempt_generation.get(),
            "instanceRevision": self.instance_revision.get(),
            "state": self.state.as_str(),
            "planId": self.plan_id,
            "runtimeId": self.runtime_id,
            "javaMajor": self.java_major,
            "pid": self.identity.as_ref().map(|identity| identity.pid),
            "processIdentity": self.identity.as_ref().map(|identity| serde_json::json!({
                "pid": identity.pid,
                "startedAt": identity.started_at_unix_ms,
                "token": identity.token,
            })),
            "exitCode": self.exit_code,
            "gameReached": self.game_reached,
            "error": self.error.as_ref().and_then(|error| serde_json::to_value(error).ok()),
            "evidence": self.evidence,
            "multiOpen": self.multi_open,
            "createdAt": self.created_at_unix_ms,
            "updatedAt": self.updated_at_unix_ms,
        })
    }
}

/// How a `launch.prepare` request resolved.
#[derive(Debug, Clone)]
pub enum SessionReservation {
    /// A brand new session; the caller must run the preparation job.
    Created(LaunchSession),
    /// A live session already covers this click; the caller must not start a
    /// second job and should return the existing task instead.
    Existing(LaunchSession),
}

/// Bounded registry of sessions plus the idempotency index.
#[derive(Debug)]
pub struct LaunchSessionRegistry {
    sessions: BTreeMap<LaunchSessionId, LaunchSession>,
    live: BTreeMap<(String, String), LaunchSessionId>,
    order: Vec<LaunchSessionId>,
    next_id: u64,
    capacity: usize,
}

impl Default for LaunchSessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl LaunchSessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
            live: BTreeMap::new(),
            order: Vec::new(),
            next_id: 0,
            capacity: 64,
        }
    }

    pub fn allocate_id(&mut self) -> LaunchSessionId {
        self.next_id += 1;
        LaunchSessionId::new(format!("launch-{}", self.next_id))
            .expect("generated session id is valid")
    }

    /// Find a live session for the same click (idempotent repeat).
    pub fn find_live(&self, instance_id: &InstanceId, key: &str) -> Option<&LaunchSession> {
        let id = self
            .live
            .get(&(instance_id.as_str().to_string(), key.to_string()))?;
        self.sessions
            .get(id)
            .filter(|session| session.state.is_live())
    }

    pub fn live_for_instance(&self, instance_id: &InstanceId) -> Vec<&LaunchSession> {
        self.sessions
            .values()
            .filter(|session| session.instance_id == *instance_id && session.state.is_live())
            .collect()
    }

    pub fn insert(&mut self, session: LaunchSession) {
        let id = session.id.clone();
        if session.state.is_live() {
            self.live.insert(
                (
                    session.instance_id.as_str().to_string(),
                    session.idempotency_key.clone(),
                ),
                id.clone(),
            );
        }
        self.sessions.insert(id.clone(), session);
        self.order.push(id);
        self.evict();
    }

    fn evict(&mut self) {
        while self.order.len() > self.capacity {
            let Some(position) = self.order.iter().position(|id| {
                self.sessions
                    .get(id)
                    .map(|session| session.state.is_terminal())
                    .unwrap_or(true)
            }) else {
                // Everything is live: keep the newest and stop growing.
                break;
            };
            let id = self.order.remove(position);
            if let Some(session) = self.sessions.remove(&id) {
                self.live.remove(&(
                    session.instance_id.as_str().to_string(),
                    session.idempotency_key,
                ));
            }
        }
    }

    pub fn get(&self, id: &LaunchSessionId) -> Option<&LaunchSession> {
        self.sessions.get(id)
    }

    pub fn apply(
        &mut self,
        id: &LaunchSessionId,
        mutate: impl FnOnce(&mut LaunchSession),
    ) -> Option<LaunchSession> {
        let mut session = self.sessions.get(id)?.clone();
        mutate(&mut session);
        session.updated_at_unix_ms = now_unix_ms();
        let key = (
            session.instance_id.as_str().to_string(),
            session.idempotency_key.clone(),
        );
        if session.state.is_terminal() {
            self.live.remove(&key);
        } else {
            self.live.insert(key, id.clone());
        }
        self.sessions.insert(id.clone(), session.clone());
        Some(session)
    }

    pub fn all(&self) -> Vec<LaunchSession> {
        self.order
            .iter()
            .filter_map(|id| self.sessions.get(id).cloned())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, key: &str) -> LaunchSession {
        LaunchSession::new(
            LaunchSessionId::new(id).expect("id"),
            InstanceId::new("instance-1").expect("instance"),
            OperationId::new("op-1").expect("operation"),
            TaskId::new("task-1").expect("task"),
            AttemptGeneration::INITIAL,
            key,
            false,
        )
    }

    #[test]
    fn terminal_states_do_not_move_again() {
        let mut session = session("launch-1", "k");
        assert!(session.transition(LaunchSessionState::Spawning).is_ok());
        assert!(session
            .transition(LaunchSessionState::ProcessRunning)
            .is_ok());
        assert!(session.transition(LaunchSessionState::Exited).is_ok());
        assert!(session
            .transition(LaunchSessionState::ProcessRunning)
            .is_err());
        assert!(
            LaunchSessionState::Preparing.can_transition_to(LaunchSessionState::ProcessRunning)
                == false
        );
    }

    #[test]
    fn registry_indexes_live_sessions_only() {
        let mut registry = LaunchSessionRegistry::new();
        let instance = InstanceId::new("instance-1").expect("instance");
        let created = session("launch-1", "instance:instance-1");
        registry.insert(created.clone());
        assert!(registry
            .find_live(&instance, "instance:instance-1")
            .is_some());

        registry.apply(&created.id, |session| {
            session.state = LaunchSessionState::Exited;
        });
        assert!(registry
            .find_live(&instance, "instance:instance-1")
            .is_none());
        assert!(registry.live_for_instance(&instance).is_empty());
    }

    #[test]
    fn identity_rejects_a_recycled_pid() {
        let ours = ProcessIdentity::new(42, 1_000, "launch-1:42:1");
        let reused = ProcessIdentity::new(42, 9_999, "launch-2:42:1");
        assert!(ours.matches(&ours));
        assert!(!ours.matches(&reused));
    }
}

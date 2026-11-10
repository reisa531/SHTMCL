//! Application-owned task scheduling and event stream (contract v1).
//!
//! This module is the *only* place a long-running module operation becomes a
//! [`Task`]. Modules own their business state; they do not own a scheduler, a
//! retry loop or an event counter. They call [`TaskService`] and
//! [`EventSink`] supplied through [`crate::ModuleContext`].
//!
//! Guarantees implemented here:
//! - every mutation is fenced by `attempt_generation`; a stale caller cannot
//!   write into a newer attempt,
//! - `cancelled` is only reachable after `request_cancel`, so a task can finish
//!   at a safe point instead of being killed mid-write,
//! - `event_id` is unique and `sequence` is monotonic per operation stream,
//! - terminal tasks are retained (bounded) so the UI can still read the result.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use shtmcl_domain::{
    AttemptGeneration, CancelDecision, CancelTaskOutcome, CancelTaskRequest, EntityId, ErrorCode,
    ErrorEnvelope, EventEnvelope, EventId, EventKind, InstanceId, OperationId, ProgressUnit,
    Revision, SchemaVersion, Sequence, StreamCursor, Task, TaskError, TaskId, TaskPhase,
    TaskProgress, TaskProgressPayload, TaskResultPayload, TaskState, TaskStatePayload,
};

/// Maximum number of tasks kept in the in-memory snapshot.
const TASK_CAPACITY: usize = 256;
/// Maximum number of events retained per operation stream.
const STREAM_CAPACITY: usize = 512;

/// One event a module wants to publish. The sink owns `event_id` and
/// `sequence`; the module owns the semantics of `kind` and `payload`.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRequest {
    pub operation_id: OperationId,
    pub entity_id: Option<EntityId>,
    pub revision: Revision,
    pub attempt_generation: AttemptGeneration,
    pub kind: EventKind,
    pub payload: Value,
}

impl EventRequest {
    pub fn new(
        operation_id: OperationId,
        attempt_generation: AttemptGeneration,
        kind: EventKind,
    ) -> Self {
        Self {
            operation_id,
            entity_id: None,
            revision: Revision::ZERO,
            attempt_generation,
            kind,
            payload: Value::Null,
        }
    }

    pub fn with_entity(mut self, entity_id: Option<EntityId>) -> Self {
        self.entity_id = entity_id;
        self
    }

    pub fn with_revision(mut self, revision: Revision) -> Self {
        self.revision = revision;
        self
    }

    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }
}

pub trait EventSink: Send + Sync {
    fn publish(&self, request: EventRequest) -> Result<EventEnvelope, ErrorEnvelope>;

    /// Events of one operation stream after `after`, oldest first.
    fn since(&self, operation_id: &OperationId, after: Option<Sequence>) -> Vec<EventEnvelope>;

    /// Next cursor a consumer should persist after applying `events`.
    fn cursor_after(&self, operation_id: &OperationId) -> Option<StreamCursor>;

    /// Register the single downstream observer (the application bridge). It is
    /// called after the event is stored, so a slow observer never reorders or
    /// drops the retained stream.
    fn observe(&self, observer: Arc<dyn EventObserver>);
}

/// Downstream consumer of published events, typically the desktop bridge that
/// forwards them to the webview.
pub trait EventObserver: Send + Sync {
    fn on_event(&self, event: &EventEnvelope);
}

impl<F> EventObserver for F
where
    F: Fn(&EventEnvelope) + Send + Sync,
{
    fn on_event(&self, event: &EventEnvelope) {
        self(event);
    }
}

/// Bounded, in-process event log. The application layer forwards these to the
/// webview; a module never pushes to the UI directly.
#[derive(Default)]
pub struct InMemoryEvents {
    streams: Mutex<BTreeMap<OperationId, OperationStream>>,
    observer: Mutex<Option<Arc<dyn EventObserver>>>,
    next_event: AtomicU64,
}

#[derive(Default)]
struct OperationStream {
    sequence: u64,
    events: Vec<EventEnvelope>,
}

impl InMemoryEvents {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn retained(&self, operation_id: &OperationId) -> usize {
        self.streams
            .lock()
            .map(|streams| {
                streams
                    .get(operation_id)
                    .map_or(0, |stream| stream.events.len())
            })
            .unwrap_or(0)
    }
}

impl EventSink for InMemoryEvents {
    fn publish(&self, request: EventRequest) -> Result<EventEnvelope, ErrorEnvelope> {
        let mut streams = self
            .streams
            .lock()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        let stream = streams.entry(request.operation_id.clone()).or_default();

        stream.sequence = stream
            .sequence
            .checked_add(1)
            .ok_or_else(|| ErrorEnvelope::internal_unavailable())?;

        let event_number = self.next_event.fetch_add(1, Ordering::Relaxed) + 1;
        let envelope = EventEnvelope {
            event_id: EventId::new(format!("evt-{event_number}"))
                .map_err(|_| ErrorEnvelope::internal_unavailable())?,
            schema_version: SchemaVersion::V1,
            operation_id: request.operation_id,
            entity_id: request.entity_id,
            revision: request.revision,
            // `sequence` orders this stream only; it is never a global clock.
            sequence: Sequence::new(stream.sequence),
            attempt_generation: request.attempt_generation,
            kind: request.kind,
            payload: request.payload,
        };

        if stream.events.len() >= STREAM_CAPACITY {
            stream.events.remove(0);
        }
        stream.events.push(envelope.clone());
        drop(streams);

        // Notify after releasing the stream lock so an observer that reads the
        // stream back (for example to resync) cannot deadlock or see a gap.
        if let Ok(observer) = self.observer.lock() {
            if let Some(observer) = observer.as_ref() {
                observer.on_event(&envelope);
            }
        }

        Ok(envelope)
    }

    fn since(&self, operation_id: &OperationId, after: Option<Sequence>) -> Vec<EventEnvelope> {
        let after = after.map(|sequence| sequence.get());
        self.streams
            .lock()
            .map(|streams| {
                streams
                    .get(operation_id)
                    .map(|stream| {
                        stream
                            .events
                            .iter()
                            .filter(|event| {
                                after.map_or(true, |value| event.sequence.get() > value)
                            })
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    fn cursor_after(&self, operation_id: &OperationId) -> Option<StreamCursor> {
        self.streams.lock().ok().and_then(|streams| {
            streams.get(operation_id).map(|stream| StreamCursor {
                operation_id: operation_id.clone(),
                sequence: Sequence::new(stream.sequence),
            })
        })
    }

    fn observe(&self, observer: Arc<dyn EventObserver>) {
        if let Ok(mut slot) = self.observer.lock() {
            *slot = Some(observer);
        }
    }
}

/// Input for a new task. The module decides the phase name and the revision its
/// plan was computed from.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskSpec {
    pub operation_id: OperationId,
    pub instance_id: Option<InstanceId>,
    pub input_revision: Option<Revision>,
    pub phase: TaskPhase,
}

impl TaskSpec {
    pub fn new(operation_id: OperationId, phase: TaskPhase) -> Self {
        Self {
            operation_id,
            instance_id: None,
            input_revision: None,
            phase,
        }
    }

    pub fn with_instance(mut self, instance_id: Option<InstanceId>) -> Self {
        self.instance_id = instance_id;
        self
    }

    pub fn with_input_revision(mut self, revision: Option<Revision>) -> Self {
        self.input_revision = revision;
        self
    }
}

pub trait TaskService: Send + Sync {
    fn accept(&self, spec: TaskSpec) -> Result<Task, ErrorEnvelope>;
    fn transition(
        &self,
        task_id: &TaskId,
        generation: AttemptGeneration,
        state: TaskState,
    ) -> Result<Task, ErrorEnvelope>;
    fn set_phase(
        &self,
        task_id: &TaskId,
        generation: AttemptGeneration,
        phase: TaskPhase,
    ) -> Result<Task, ErrorEnvelope>;
    fn set_progress(
        &self,
        task_id: &TaskId,
        generation: AttemptGeneration,
        progress: TaskProgress,
    ) -> Result<Task, ErrorEnvelope>;
    fn add_evidence(&self, task_id: &TaskId, marker: &str) -> Result<Task, ErrorEnvelope>;
    fn succeed(
        &self,
        task_id: &TaskId,
        generation: AttemptGeneration,
        result: Option<Value>,
    ) -> Result<Task, ErrorEnvelope>;
    fn fail(
        &self,
        task_id: &TaskId,
        generation: AttemptGeneration,
        error: ErrorEnvelope,
    ) -> Result<Task, ErrorEnvelope>;
    /// Confirm cancellation at a safe stop/rollback point.
    fn cancel_now(
        &self,
        task_id: &TaskId,
        generation: AttemptGeneration,
    ) -> Result<Task, ErrorEnvelope>;
    fn request_cancel(&self, request: &CancelTaskRequest) -> CancelTaskOutcome;
    /// Cooperative check performed by the module between steps.
    fn is_cancel_requested(&self, task_id: &TaskId, generation: AttemptGeneration) -> bool;
    fn snapshot(&self, task_id: &TaskId) -> Option<Task>;
    fn snapshots(&self) -> Vec<Task>;
    /// Start a new attempt after `failed` / `cancelled`; the generation increases
    /// so late responses of the previous attempt are fenced.
    fn begin_retry(&self, task_id: &TaskId) -> Result<Task, ErrorEnvelope>;
}

pub struct InMemoryTasks {
    tasks: Mutex<TaskTable>,
    events: Arc<dyn EventSink>,
    next_id: AtomicU64,
}

#[derive(Default)]
struct TaskTable {
    order: Vec<TaskId>,
    tasks: BTreeMap<TaskId, Task>,
}

impl InMemoryTasks {
    pub fn new(events: Arc<dyn EventSink>) -> Self {
        Self {
            tasks: Mutex::new(TaskTable::default()),
            events,
            next_id: AtomicU64::new(0),
        }
    }

    fn publish_state(&self, task: &Task) {
        let payload = TaskStatePayload {
            task_id: task.id.clone(),
            state: task.state,
            phase: task.phase.clone(),
            cancel_requested: task.cancel_requested,
        };
        let _ = self.events.publish(
            EventRequest::new(
                task.operation_id.clone(),
                task.attempt_generation,
                EventKind::from_static(EventKind::TASK_STATE),
            )
            .with_entity(entity_of(task))
            .with_revision(task_revision(task))
            .with_payload(serde_json::to_value(payload).unwrap_or(Value::Null)),
        );
    }

    fn publish_progress(&self, task: &Task, progress: &TaskProgress) {
        let payload = TaskProgressPayload {
            task_id: task.id.clone(),
            current: progress.current,
            total: progress.total,
            unit: progress.unit,
        };
        let _ = self.events.publish(
            EventRequest::new(
                task.operation_id.clone(),
                task.attempt_generation,
                EventKind::from_static(EventKind::TASK_PROGRESS),
            )
            .with_entity(entity_of(task))
            .with_revision(task_revision(task))
            .with_payload(serde_json::to_value(payload).unwrap_or(Value::Null)),
        );
    }

    fn publish_result(&self, task: &Task) {
        let payload = TaskResultPayload {
            task_id: task.id.clone(),
            state: task.state,
            result: task.result.clone(),
            error: task.error.clone(),
        };
        let _ = self.events.publish(
            EventRequest::new(
                task.operation_id.clone(),
                task.attempt_generation,
                EventKind::from_static(EventKind::TASK_RESULT),
            )
            .with_entity(entity_of(task))
            .with_revision(task_revision(task))
            .with_payload(serde_json::to_value(payload).unwrap_or(Value::Null)),
        );
    }

    fn with_task<T>(
        &self,
        task_id: &TaskId,
        mutate: impl FnOnce(&mut Task) -> Result<T, TaskError>,
    ) -> Result<(Task, T), ErrorEnvelope> {
        let mut table = self
            .tasks
            .lock()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        let task = table
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| ErrorEnvelope::task_not_found(task_id, None))?;
        let value = mutate(&mut *task).map_err(|error| task_error_envelope(error, task))?;
        Ok((task.clone(), value))
    }

    fn evict_if_needed(table: &mut TaskTable) {
        while table.order.len() > TASK_CAPACITY {
            let index = table
                .order
                .iter()
                .position(|id| {
                    table
                        .tasks
                        .get(id)
                        .map(|task| task.state.is_terminal())
                        .unwrap_or(true)
                })
                .unwrap_or(0);
            let id = table.order.remove(index);
            table.tasks.remove(&id);
        }
    }
}

fn task_revision(task: &Task) -> Revision {
    // Task events describe operation progress, not an entity mutation, so they
    // carry the revision the plan was computed from (0 when there is none).
    task.input_revision.unwrap_or(Revision::ZERO)
}

fn entity_of(task: &Task) -> Option<EntityId> {
    task.instance_id
        .as_ref()
        .and_then(|instance| EntityId::new(instance.as_str().to_string()).ok())
}

fn task_error_envelope(error: TaskError, task: &Task) -> ErrorEnvelope {
    match error {
        TaskError::StaleGeneration { expected, actual } => {
            ErrorEnvelope::stale_generation(expected, actual, Some(task.operation_id.clone()))
        }
        TaskError::InvalidTransition { from, to } => ErrorEnvelope::new(
            ErrorCode::from_static(ErrorCode::TASK_INVALID_TRANSITION),
            shtmcl_domain::ErrorCategory::Conflict,
            false,
        )
        .with_detail_ref(format!("task:{}:{}->{}", task.id.as_str(), from, to))
        .with_operation(task.operation_id.clone()),
        TaskError::CancelNotRequested => ErrorEnvelope::new(
            ErrorCode::from_static(ErrorCode::TASK_NOT_CANCELLABLE),
            shtmcl_domain::ErrorCategory::Conflict,
            false,
        )
        .with_detail_ref(format!("task:{}", task.id.as_str())),
        TaskError::TerminalTask { state } => ErrorEnvelope::new(
            ErrorCode::from_static(ErrorCode::TASK_NOT_CANCELLABLE),
            shtmcl_domain::ErrorCategory::Conflict,
            false,
        )
        .with_detail_ref(format!("task:{}:{}", task.id.as_str(), state)),
        other => ErrorEnvelope::new(
            ErrorCode::from_static(ErrorCode::TASK_INVALID_TRANSITION),
            shtmcl_domain::ErrorCategory::InvalidRequest,
            false,
        )
        .with_detail_ref(format!("task:{}:{}", task.id.as_str(), other)),
    }
}

impl TaskService for InMemoryTasks {
    fn accept(&self, spec: TaskSpec) -> Result<Task, ErrorEnvelope> {
        let number = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let task_id = TaskId::new(format!("task-{number}"))
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;

        let mut task = Task::new(
            task_id.clone(),
            spec.operation_id,
            AttemptGeneration::INITIAL,
        );
        task.instance_id = spec.instance_id;
        task.input_revision = spec.input_revision;
        task.phase = spec.phase;
        task.validate()
            .map_err(|error| task_error_envelope(error, &task))?;

        let mut table = self
            .tasks
            .lock()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        table.order.push(task_id);
        table.tasks.insert(task.id.clone(), task.clone());
        Self::evict_if_needed(&mut table);
        drop(table);

        self.publish_state(&task);
        Ok(task)
    }

    fn transition(
        &self,
        task_id: &TaskId,
        generation: AttemptGeneration,
        state: TaskState,
    ) -> Result<Task, ErrorEnvelope> {
        let (task, ()) = self.with_task(task_id, |task| task.transition(state, generation))?;
        self.publish_state(&task);
        Ok(task)
    }

    fn set_phase(
        &self,
        task_id: &TaskId,
        generation: AttemptGeneration,
        phase: TaskPhase,
    ) -> Result<Task, ErrorEnvelope> {
        let (task, ()) = self.with_task(task_id, |task| task.set_phase(phase, generation))?;
        self.publish_state(&task);
        Ok(task)
    }

    fn set_progress(
        &self,
        task_id: &TaskId,
        generation: AttemptGeneration,
        progress: TaskProgress,
    ) -> Result<Task, ErrorEnvelope> {
        let (task, ()) = self.with_task(task_id, |task| {
            task.set_progress(progress.clone(), generation)
        })?;
        self.publish_progress(&task, &progress);
        Ok(task)
    }

    fn add_evidence(&self, task_id: &TaskId, marker: &str) -> Result<Task, ErrorEnvelope> {
        let (task, ()) = self.with_task(task_id, |task| {
            task.add_evidence(marker);
            Ok(())
        })?;
        Ok(task)
    }

    fn succeed(
        &self,
        task_id: &TaskId,
        generation: AttemptGeneration,
        result: Option<Value>,
    ) -> Result<Task, ErrorEnvelope> {
        let (task, ()) = self.with_task(task_id, |task| task.succeed(result, generation))?;
        self.publish_result(&task);
        Ok(task)
    }

    fn fail(
        &self,
        task_id: &TaskId,
        generation: AttemptGeneration,
        error: ErrorEnvelope,
    ) -> Result<Task, ErrorEnvelope> {
        let (task, ()) = self.with_task(task_id, |task| task.fail(error, generation))?;
        self.publish_result(&task);
        Ok(task)
    }

    fn cancel_now(
        &self,
        task_id: &TaskId,
        generation: AttemptGeneration,
    ) -> Result<Task, ErrorEnvelope> {
        let (task, ()) = self.with_task(task_id, |task| task.mark_cancelled(generation))?;
        self.publish_result(&task);
        Ok(task)
    }

    fn request_cancel(&self, request: &CancelTaskRequest) -> CancelTaskOutcome {
        let decision = match self.tasks.lock() {
            Ok(mut table) => match table.tasks.get_mut(&request.task_id) {
                Some(task) => task.request_cancel(request.attempt_generation),
                None => CancelDecision::NotFound,
            },
            Err(_) => CancelDecision::NotFound,
        };
        CancelTaskOutcome {
            task_id: request.task_id.clone(),
            decision,
        }
    }

    fn is_cancel_requested(&self, task_id: &TaskId, generation: AttemptGeneration) -> bool {
        self.tasks
            .lock()
            .ok()
            .and_then(|table| {
                table
                    .tasks
                    .get(task_id)
                    .map(|task| task.cancel_requested && task.accepts_generation(generation))
            })
            .unwrap_or(false)
    }

    fn snapshot(&self, task_id: &TaskId) -> Option<Task> {
        self.tasks
            .lock()
            .ok()
            .and_then(|table| table.tasks.get(task_id).cloned())
    }

    fn snapshots(&self) -> Vec<Task> {
        self.tasks
            .lock()
            .map(|table| {
                table
                    .order
                    .iter()
                    .filter_map(|id| table.tasks.get(id).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn begin_retry(&self, task_id: &TaskId) -> Result<Task, ErrorEnvelope> {
        let (task, generation) = self.with_task(task_id, |task| task.begin_retry())?;
        debug_assert_eq!(task.attempt_generation, generation);
        self.publish_state(&task);
        Ok(task)
    }
}

impl fmt::Debug for InMemoryTasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let len = self
            .tasks
            .lock()
            .map(|table| table.tasks.len())
            .unwrap_or(0);
        f.debug_struct("InMemoryTasks")
            .field("tasks", &len)
            .finish()
    }
}

/// Convenience: a bytes progress value used by download-shaped phases.
pub fn bytes_progress(current: u64, total: Option<u64>) -> Result<TaskProgress, ErrorEnvelope> {
    TaskProgress::new(current, total, ProgressUnit::Bytes)
        .map_err(|_| ErrorEnvelope::internal_unavailable())
}

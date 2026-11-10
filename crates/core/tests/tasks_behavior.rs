//! Behaviour of the application-owned task/event port.
//!
//! These tests exist to protect the frozen contract other modules build on:
//! generation fencing, cooperative cancellation, retry, event sequencing and
//! bounded retention. They are not a substitute for module-level tests.

use std::sync::Arc;

use shtmcl_core::tasks::{
    EventRequest, EventSink, InMemoryEvents, InMemoryTasks, TaskService, TaskSpec,
};
use shtmcl_core::ModuleContext;
use shtmcl_domain::{
    AttemptGeneration, CancelDecision, CancelTaskRequest, ErrorCategory, ErrorCode, ErrorEnvelope,
    EventKind, OperationId, ProgressUnit, Revision, Sequence, TaskError, TaskId, TaskPhase,
    TaskProgress, TaskState,
};
use shtmcl_platform::PlatformDirs;

fn operation(id: &str) -> OperationId {
    OperationId::new(id).expect("valid operation id")
}

fn phase(value: &str) -> TaskPhase {
    TaskPhase::new(value).expect("valid phase")
}

fn service() -> (Arc<InMemoryTasks>, Arc<InMemoryEvents>) {
    let events = Arc::new(InMemoryEvents::new());
    let tasks = Arc::new(InMemoryTasks::new(events.clone()));
    (tasks, events)
}

fn accept(tasks: &InMemoryTasks) -> shtmcl_domain::Task {
    tasks
        .accept(
            TaskSpec::new(operation("op-1"), phase("prepare"))
                .with_input_revision(Some(Revision::new(7))),
        )
        .expect("accept task")
}

#[test]
fn accepted_task_starts_queued_with_a_first_generation() {
    let (tasks, _events) = service();
    let task = accept(&tasks);
    assert_eq!(task.state, TaskState::Queued);
    assert_eq!(task.attempt_generation, AttemptGeneration::INITIAL);
    assert_eq!(task.phase.as_str(), "prepare");
    assert!(!task.cancel_requested);
    assert!(task.validate().is_ok());
}

#[test]
fn progress_is_bounded_and_published_in_order() {
    let (tasks, events) = service();
    let task = accept(&tasks);
    tasks
        .transition(&task.id, task.attempt_generation, TaskState::Running)
        .expect("run");

    let progress = TaskProgress::new(512, Some(1024), ProgressUnit::Bytes).expect("progress");
    tasks
        .set_progress(&task.id, task.attempt_generation, progress)
        .expect("progress accepted");

    let stream = events.since(&task.operation_id, None);
    // accept published task.state, transition published task.state,
    // set_progress published task.progress.
    assert_eq!(stream.len(), 3);
    let sequences: Vec<u64> = stream.iter().map(|event| event.sequence.get()).collect();
    assert_eq!(sequences, vec![1, 2, 3], "sequence is monotonic per stream");
    assert_eq!(stream[2].kind.as_str(), EventKind::TASK_PROGRESS);
    assert_eq!(stream[2].payload["current"], 512);
    assert_eq!(stream[2].payload["total"], 1024);

    let over = TaskProgress::new(2048, Some(1024), ProgressUnit::Bytes);
    assert!(over.is_err(), "current must not exceed total");
}

#[test]
fn stale_generation_cannot_write_into_a_newer_attempt() {
    let (tasks, _events) = service();
    let task = accept(&tasks);
    tasks
        .transition(&task.id, task.attempt_generation, TaskState::Running)
        .expect("run");
    let stale = AttemptGeneration::new(task.attempt_generation.get() + 1);

    let error = tasks
        .transition(&task.id, stale, TaskState::Succeeded)
        .expect_err("stale generation must be rejected");
    assert_eq!(error.code.as_str(), ErrorCode::TASK_STALE_GENERATION);
    assert_eq!(error.category, ErrorCategory::Conflict);
}

#[test]
fn cancellation_is_cooperative_and_fenced() {
    let (tasks, _events) = service();
    let task = accept(&tasks);
    tasks
        .transition(&task.id, task.attempt_generation, TaskState::Running)
        .expect("run");

    // Confirming cancellation before it was requested is a contract error.
    let error = tasks
        .cancel_now(&task.id, task.attempt_generation)
        .expect_err("cancel must be requested first");
    assert_eq!(error.code.as_str(), ErrorCode::TASK_NOT_CANCELLABLE);

    let outcome = tasks.request_cancel(&CancelTaskRequest {
        task_id: task.id.clone(),
        attempt_generation: task.attempt_generation,
    });
    assert_eq!(outcome.decision, CancelDecision::Requested);
    assert!(tasks.is_cancel_requested(&task.id, task.attempt_generation));
    assert!(!tasks.is_cancel_requested(&task.id, AttemptGeneration::new(9)));

    // The state does not move on request alone.
    let snapshot = tasks.snapshot(&task.id).expect("task");
    assert_eq!(snapshot.state, TaskState::Running);
    assert!(snapshot.cancel_requested);

    let cancelled = tasks
        .cancel_now(&task.id, task.attempt_generation)
        .expect("safe stop");
    assert_eq!(cancelled.state, TaskState::Cancelled);

    let again = tasks.request_cancel(&CancelTaskRequest {
        task_id: task.id.clone(),
        attempt_generation: task.attempt_generation,
    });
    assert_eq!(again.decision, CancelDecision::NotCancellable);

    let missing = tasks.request_cancel(&CancelTaskRequest {
        task_id: TaskId::new("task-missing").expect("id"),
        attempt_generation: AttemptGeneration::INITIAL,
    });
    assert_eq!(missing.decision, CancelDecision::NotFound);
}

#[test]
fn retry_increments_the_generation_and_clears_the_previous_attempt() {
    let (tasks, _events) = service();
    let task = accept(&tasks);
    tasks
        .transition(&task.id, task.attempt_generation, TaskState::Running)
        .expect("run");
    tasks
        .add_evidence(&task.id, "resumable_ranges")
        .expect("evidence");
    tasks
        .fail(
            &task.id,
            task.attempt_generation,
            ErrorEnvelope::new(
                ErrorCode::from_static("download.source.unavailable"),
                ErrorCategory::Network,
                true,
            ),
        )
        .expect("fail");

    let retried = tasks.begin_retry(&task.id).expect("retry");
    assert_eq!(retried.attempt_generation.get(), 2);
    assert_eq!(retried.state, TaskState::Queued);
    assert!(retried.error.is_none());
    assert!(retried.evidence.is_empty(), "evidence is per attempt");

    let error = tasks
        .begin_retry(&task.id)
        .expect_err("a queued task cannot be retried");
    assert!(matches!(
        error.code.as_str(),
        ErrorCode::TASK_INVALID_TRANSITION
    ));
}

#[test]
fn evidence_is_deduplicated_and_survives_until_retry() {
    let (tasks, _events) = service();
    let task = accept(&tasks);
    tasks
        .add_evidence(&task.id, "binding_committed")
        .expect("add");
    let updated = tasks
        .add_evidence(&task.id, "binding_committed")
        .expect("add");
    assert_eq!(updated.evidence, vec!["binding_committed".to_string()]);
}

#[test]
fn terminal_results_are_published_as_task_result_events() {
    let (tasks, events) = service();
    let task = accept(&tasks);
    tasks
        .transition(&task.id, task.attempt_generation, TaskState::Running)
        .expect("run");
    tasks
        .succeed(
            &task.id,
            task.attempt_generation,
            Some(serde_json::json!({ "sessionId": "launch-1" })),
        )
        .expect("succeed");

    let stream = events.since(&task.operation_id, None);
    let last = stream.last().expect("events");
    assert_eq!(last.kind.as_str(), EventKind::TASK_RESULT);
    assert_eq!(last.payload["state"], "succeeded");
    assert_eq!(last.payload["result"]["sessionId"], "launch-1");

    let cursor = events
        .cursor_after(&task.operation_id)
        .expect("cursor after events");
    assert_eq!(cursor.sequence, last.sequence);
    assert!(events
        .since(&task.operation_id, Some(cursor.sequence))
        .is_empty());
}

#[test]
fn streams_are_independent_and_kinds_are_module_defined() {
    let events = Arc::new(InMemoryEvents::new());
    let first = operation("op-1");
    let second = operation("op-2");
    let kind = EventKind::new("room.membership.changed").expect("kind");

    events
        .publish(EventRequest::new(
            first.clone(),
            AttemptGeneration::INITIAL,
            kind.clone(),
        ))
        .expect("publish");
    events
        .publish(EventRequest::new(
            second.clone(),
            AttemptGeneration::INITIAL,
            kind,
        ))
        .expect("publish");

    let first_stream = events.since(&first, None);
    let second_stream = events.since(&second, None);
    assert_eq!(first_stream.len(), 1);
    assert_eq!(second_stream.len(), 1);
    assert_eq!(
        first_stream[0].sequence, second_stream[0].sequence,
        "sequence restarts per operation stream; it is not a global order"
    );
    assert_ne!(first_stream[0].event_id, second_stream[0].event_id);
    assert_eq!(first_stream[0].schema_version.get(), 1);
    assert_eq!(
        events.since(&first, Some(Sequence::new(1))).len(),
        0,
        "cursor excludes already-applied events"
    );
}

#[test]
fn unknown_task_returns_a_stable_not_found_error() {
    let (tasks, _events) = service();
    let missing = TaskId::new("task-missing").expect("id");
    let error = tasks
        .transition(&missing, AttemptGeneration::INITIAL, TaskState::Running)
        .expect_err("missing task");
    assert_eq!(error.code.as_str(), ErrorCode::TASK_NOT_FOUND);
    assert!(matches!(
        tasks.snapshot(&missing).map(|task| task.state),
        None
    ));
}

#[test]
fn module_context_shares_one_task_table_and_event_stream() {
    let dirs = PlatformDirs::under("C:\\shtmcl-test-root");
    let events = Arc::new(InMemoryEvents::new());
    let tasks = Arc::new(InMemoryTasks::new(events.clone()));
    let context = ModuleContext::new(tasks.clone(), events.clone(), dirs.clone());

    let task = context
        .tasks()
        .accept(TaskSpec::new(operation("op-ctx"), phase("prepare")))
        .expect("accept through context");
    assert_eq!(
        tasks.snapshot(&task.id).map(|snapshot| snapshot.state),
        Some(TaskState::Queued)
    );
    assert_eq!(events.since(&task.operation_id, None).len(), 1);
    assert_eq!(context.dirs().root, dirs.root);
}

#[test]
fn task_errors_surface_as_contract_error_envelopes() {
    let (tasks, _events) = service();
    let task = accept(&tasks);
    let error = tasks
        .transition(&task.id, task.attempt_generation, TaskState::Paused)
        .expect_err("queued -> paused is not allowed");

    assert_eq!(error.category, ErrorCategory::Conflict);
    assert_eq!(error.code.as_str(), ErrorCode::TASK_INVALID_TRANSITION);
    assert!(!error.retryable);
    // The detail reference names the task and the rejected transition.
    assert!(error
        .detail_ref
        .as_deref()
        .unwrap_or_default()
        .contains("queued->paused"));

    // Domain-level status is unchanged by a rejected call.
    let snapshot = tasks.snapshot(&task.id).expect("task");
    assert_eq!(snapshot.state, TaskState::Queued);
    let _ = TaskError::CancelNotRequested;
}

#[test]
fn retention_is_bounded() {
    let (tasks, _events) = service();
    for index in 0..260 {
        tasks
            .accept(
                TaskSpec::new(operation(&format!("op-{index}")), phase("prepare"))
                    .with_input_revision(None),
            )
            .expect("accept");
    }
    // The snapshot is capped so a long session cannot grow without bound.
    assert_eq!(tasks.snapshots().len(), 256);
}

#[test]
fn dispatch_rejects_a_command_from_another_module_namespace() {
    use shtmcl_core::{CoreApp, CoreModule};
    use shtmcl_domain::{CommandResponse, ModuleId, RequestId};

    struct Echo;

    impl CoreModule for Echo {
        fn id(&self) -> ModuleId {
            ModuleId::Content
        }

        fn handle(
            &self,
            _request: &shtmcl_domain::CommandRequest,
        ) -> Result<CommandResponse, shtmcl_domain::ErrorEnvelope> {
            Ok(CommandResponse::Completed {
                result: serde_json::Value::Null,
            })
        }
    }

    let mut app = CoreApp::new(PlatformDirs::under("C:\\shtmcl-test-root"));
    app.register(Arc::new(Echo)).expect("register");

    let foreign = shtmcl_domain::CommandRequest {
        request_id: RequestId::new("req-1").expect("id"),
        operation_id: Some(OperationId::new("op-1").expect("id")),
        entity_id: None,
        expected_revision: None,
        payload: serde_json::json!({ "command": "launch.start" }),
    };
    let error = app
        .dispatch_mutation(ModuleId::Content, &foreign)
        .expect_err("launch.* must not dispatch to the content module");
    assert_eq!(error.code.as_str(), ErrorCode::COMMAND_NAME_MISMATCH);

    let own = shtmcl_domain::CommandRequest {
        payload: serde_json::json!({ "command": "content.plan" }),
        ..foreign
    };
    assert!(app.dispatch_mutation(ModuleId::Content, &own).is_ok());
}

#[test]
fn dispatch_enforces_operation_id_for_mutations() {
    use shtmcl_core::{CoreApp, CoreModule};
    use shtmcl_domain::{CommandResponse, ModuleId, RequestId};

    struct Echo;

    impl CoreModule for Echo {
        fn id(&self) -> ModuleId {
            ModuleId::Launch
        }

        fn handle(
            &self,
            _request: &shtmcl_domain::CommandRequest,
        ) -> Result<CommandResponse, shtmcl_domain::ErrorEnvelope> {
            Ok(CommandResponse::Completed {
                result: serde_json::Value::Null,
            })
        }
    }

    let mut app = CoreApp::new(PlatformDirs::under("C:\\shtmcl-test-root"));
    app.register(Arc::new(Echo)).expect("register");

    let read = shtmcl_domain::CommandRequest {
        request_id: RequestId::new("req-1").expect("id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        // A read must still name its command: dispatch validates the namespace.
        payload: serde_json::json!({ "command": "launch.echo" }),
    };
    assert!(app.dispatch_read(ModuleId::Launch, &read).is_ok());

    // The mutation path rejects the missing operation id before anything else,
    // so the same payload is still a useful fixture here.
    let error = app
        .dispatch_mutation(ModuleId::Launch, &read)
        .expect_err("mutation without operation id must be rejected");
    assert_eq!(error.code.as_str(), ErrorCode::COMMAND_OPERATION_REQUIRED);
}

#[test]
fn observer_sees_events_after_they_are_retained() {
    use std::sync::Mutex;

    let events = Arc::new(InMemoryEvents::new());
    let tasks = InMemoryTasks::new(events.clone());
    let seen: Arc<Mutex<Vec<(u64, String)>>> = Arc::new(Mutex::new(Vec::new()));

    let sink = seen.clone();
    let retained = events.clone();
    events.observe(Arc::new(move |event: &shtmcl_domain::EventEnvelope| {
        // The event must already be readable from the retained stream when the
        // observer runs; otherwise a live push could outrun the cursor.
        assert!(!retained.since(&event.operation_id, None).is_empty());
        sink.lock()
            .expect("lock")
            .push((event.sequence.get(), event.kind.as_str().to_string()));
    }));

    let task = tasks
        .accept(
            TaskSpec::new(operation("op-obs"), phase("prepare"))
                .with_input_revision(Some(Revision::new(3))),
        )
        .expect("accept");
    tasks
        .transition(&task.id, task.attempt_generation, TaskState::Running)
        .expect("run");

    let recorded = seen.lock().expect("lock").clone();
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0], (1, EventKind::TASK_STATE.to_string()));
    assert_eq!(recorded[1].0, 2, "observer order follows sequence order");
}

use shtmcl_domain::cancel::CancelDecision;
use shtmcl_domain::command::{CommandKind, CommandRequest};
use shtmcl_domain::error::ErrorCode;
use shtmcl_domain::ids::{OperationId, RequestId, TaskId};
use shtmcl_domain::revision::{AttemptGeneration, Sequence};
use shtmcl_domain::task::{ProgressUnit, Task, TaskProgress, TaskState};

fn task() -> Task {
    Task::new(
        TaskId::new("task-1").expect("valid task id"),
        OperationId::new("op-1").expect("valid operation id"),
        AttemptGeneration::new(1),
    )
}

#[test]
fn transitions_follow_the_frozen_state_machine() {
    assert!(TaskState::Queued.can_transition_to(TaskState::Running));
    assert!(TaskState::Running.can_transition_to(TaskState::Succeeded));
    assert!(!TaskState::Succeeded.can_transition_to(TaskState::Running));
    assert!(!TaskState::Cancelled.can_transition_to(TaskState::Queued));

    let mut task = task();
    let generation = task.attempt_generation;
    task.transition(TaskState::Running, generation)
        .expect("queued -> running");
    task.succeed(None, generation)
        .expect("running -> succeeded");
    assert_eq!(task.state, TaskState::Succeeded);
    assert!(
        task.begin_retry().is_err(),
        "a succeeded task must not retry silently"
    );
}

#[test]
fn cancellation_is_cooperative_and_generation_fenced() {
    let mut task = task();
    let generation = task.attempt_generation;

    assert_eq!(
        task.request_cancel(AttemptGeneration::new(2)),
        CancelDecision::StaleGeneration
    );
    assert_eq!(task.request_cancel(generation), CancelDecision::Requested);
    assert_eq!(
        task.request_cancel(generation),
        CancelDecision::AlreadyRequested
    );

    task.mark_cancelled(generation)
        .expect("safe-point cancellation");
    assert_eq!(task.state, TaskState::Cancelled);
    assert_eq!(
        task.request_cancel(generation),
        CancelDecision::NotCancellable
    );

    let next = task.begin_retry().expect("retry after cancellation");
    assert_eq!(next.get(), 2);
    assert!(!task.accepts_generation(generation));
    assert!(task.accepts_generation(next));
    assert_eq!(task.state, TaskState::Queued);
    assert!(!task.cancel_requested);
}

#[test]
fn cancellation_cannot_be_confirmed_without_a_request() {
    let mut task = task();
    let generation = task.attempt_generation;
    task.transition(TaskState::Running, generation)
        .expect("queued -> running");
    assert!(task.mark_cancelled(generation).is_err());
    assert_eq!(task.state, TaskState::Running);
}

#[test]
fn progress_is_bounded_and_only_while_active() {
    let mut task = task();
    let generation = task.attempt_generation;

    assert!(TaskProgress::new(2, Some(1), ProgressUnit::Bytes).is_err());
    assert!(task
        .set_progress(
            TaskProgress::new(1, Some(2), ProgressUnit::Bytes).expect("valid progress"),
            generation,
        )
        .is_err());

    task.transition(TaskState::Running, generation)
        .expect("queued -> running");
    task.set_progress(
        TaskProgress::new(1, Some(2), ProgressUnit::Bytes).expect("valid progress"),
        generation,
    )
    .expect("running task accepts progress");
    assert_eq!(
        task.progress
            .as_ref()
            .and_then(|progress| progress.percent()),
        Some(50.0)
    );
}

#[test]
fn counters_do_not_wrap() {
    assert_eq!(Sequence::new(u64::MAX).checked_next(), None);
    assert_eq!(AttemptGeneration::new(u32::MAX).checked_next(), None);
}

#[test]
fn mutation_commands_require_an_operation_id() {
    let request = CommandRequest {
        request_id: RequestId::new("req-1").expect("valid request id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload: serde_json::json!({}),
    };
    assert!(request.validate(CommandKind::Read).is_ok());
    assert!(request.validate(CommandKind::Mutate).is_err());
    assert_eq!(
        request
            .validate(CommandKind::Mutate)
            .expect_err("mutation must fail")
            .code
            .as_str(),
        ErrorCode::COMMAND_OPERATION_REQUIRED
    );
}

#[test]
fn error_codes_are_stable_and_machine_readable() {
    assert!(ErrorCode::new("task.not_found").is_ok());
    assert!(ErrorCode::new("Task.NotFound").is_err());
    assert!(ErrorCode::new("bad code").is_err());
    assert!(ErrorCode::new("ab").is_err());
}

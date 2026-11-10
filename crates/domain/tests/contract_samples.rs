use shtmcl_domain::cancel::CancelTaskRequest;
use shtmcl_domain::error::{ErrorCategory, ErrorEnvelope};
use shtmcl_domain::event::EventEnvelope;
use shtmcl_domain::task::{Task, TaskState};
use shtmcl_domain::view::{
    ActionAvailability, AvailabilityProjection, AvailabilityState, MessageAction,
    MessageActionKind, MessageActionRole, MessageCategory, UserMessage,
};
use shtmcl_domain::CONTRACT_VERSION;

#[test]
fn task_sample_matches_the_v1_contract() {
    let task: Task = serde_json::from_str(include_str!("../../../contracts/samples/task.json"))
        .expect("task sample must deserialize");
    task.validate().expect("task sample invariants");
    assert_eq!(task.state, TaskState::Running);
    assert_eq!(task.attempt_generation.get(), 1);
    assert_eq!(task.input_revision.map(|revision| revision.get()), Some(7));
    assert_eq!(
        task.progress.as_ref().and_then(|progress| progress.total),
        Some(1024)
    );
    assert_eq!(task.evidence, vec!["resumable_ranges".to_string()]);
}

#[test]
fn event_sample_matches_the_v1_contract() {
    let event: EventEnvelope =
        serde_json::from_str(include_str!("../../../contracts/samples/event.json"))
            .expect("event sample must deserialize");
    event.validate().expect("event sample invariants");
    assert_eq!(event.schema_version.get(), CONTRACT_VERSION);
    assert_eq!(event.sequence.get(), 12);
    assert_eq!(event.kind.as_str(), "task.progress");
}

#[test]
fn error_sample_matches_the_v1_contract() {
    let error: ErrorEnvelope =
        serde_json::from_str(include_str!("../../../contracts/samples/error.json"))
            .expect("error sample must deserialize");
    assert_eq!(error.code.as_str(), "task.not_found");
    assert_eq!(error.category, ErrorCategory::NotFound);
    assert!(!error.retryable);
    assert_eq!(
        error.user_action.as_ref().map(|action| action.as_str()),
        Some("reload")
    );
}

#[test]
fn cancel_sample_matches_the_v1_contract() {
    let cancel: CancelTaskRequest =
        serde_json::from_str(include_str!("../../../contracts/samples/cancel.json"))
            .expect("cancel sample must deserialize");
    assert_eq!(cancel.task_id.as_str(), "task-m0-sample");
    assert_eq!(cancel.attempt_generation.get(), 2);
}

#[test]
fn command_sample_matches_the_v1_contract() {
    let request: shtmcl_domain::CommandRequest =
        serde_json::from_str(include_str!("../../../contracts/samples/command.json"))
            .expect("command sample must deserialize");
    let name = request.command().expect("command name");
    assert_eq!(name.as_str(), "launch.start");
    assert_eq!(name.owner(), "launch");
    request
        .validate(shtmcl_domain::CommandKind::Mutate)
        .expect("mutation carries an operation id");
    assert_eq!(
        request.expected_revision.map(|revision| revision.get()),
        Some(7)
    );
    assert_eq!(
        request.entity_id.as_ref().map(|entity| entity.as_str()),
        Some("instance-m0-sample")
    );
}

#[test]
fn a_request_without_a_command_name_is_rejected() {
    let request: shtmcl_domain::CommandRequest = serde_json::from_value(serde_json::json!({
        "requestId": "req-1",
        "payload": { "taskId": "task-1" }
    }))
    .expect("deserialize");
    let error = request.command().expect_err("no command name");
    assert_eq!(
        error.code.as_str(),
        shtmcl_domain::ErrorCode::COMMAND_NAME_REQUIRED
    );
}

#[test]
fn user_message_sample_matches_the_v1_contract() {
    let message: UserMessage =
        serde_json::from_str(include_str!("../../../contracts/samples/user-message.json"))
            .expect("user message sample must deserialize");
    assert_eq!(message.code.as_str(), "download.interrupted");
    assert!(message.code_known);
    assert_eq!(message.category, MessageCategory::Network);
    assert_eq!(message.evidence, vec!["resumable_ranges".to_string()]);
    assert_eq!(message.actions.len(), 2);
    assert_eq!(message.actions[0].role, MessageActionRole::Primary);
    assert_eq!(message.actions[1].kind, Some(MessageActionKind::Detail));
    assert_eq!(message.actions[0].capability.as_str(), "download.resume");

    // Round-trip keeps the wire shape stable for the TypeScript mirror.
    let json = serde_json::to_value(&message).expect("serialize");
    assert_eq!(json["detailRef"], "task:task-m0-sample");
    assert_eq!(json["contentVersion"], "copy.v1");
}

#[test]
fn availability_sample_matches_the_v1_contract() {
    let projection: AvailabilityProjection =
        serde_json::from_str(include_str!("../../../contracts/samples/availability.json"))
            .expect("availability sample must deserialize");
    assert_eq!(
        projection.schema_version,
        AvailabilityProjection::SCHEMA_VERSION
    );
    assert!(projection.requires_recheck_at_execution);
    assert_eq!(projection.actions.len(), 2);
    assert_eq!(projection.enabled.len(), 1);

    let start: &ActionAvailability = &projection.actions[1];
    assert_eq!(start.capability.as_str(), "launch.start");
    assert!(!start.available);
    assert_eq!(start.state, AvailabilityState::Unavailable);
    assert_eq!(start.condition.as_deref(), Some("game_running"));
    assert_eq!(
        start.next_step.as_ref().map(|step| step.as_str()),
        Some("launch.stop_game")
    );

    // A projection is never an authorization result.
    assert!(projection.requires_recheck_at_execution);
}

#[test]
fn message_actions_reject_invalid_capability_ids() {
    let json = serde_json::json!({
        "label": "Continue",
        "capability": "Not A Capability",
        "role": "primary"
    });
    assert!(serde_json::from_value::<MessageAction>(json).is_err());
}

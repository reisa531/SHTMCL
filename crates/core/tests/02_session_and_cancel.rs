//! 02 · sessions: idempotent clicks, explicit multi-open, cancellation at a safe
//! point, crash recording and spawn failure.
//!
//! Session identity, the state machine, the cancellation protocol and the
//! "never restart automatically" rule are the real implementation; instance,
//! account and process I/O are doubles.

#[path = "02_support/mod.rs"]
mod support;

use std::time::Duration;

use shtmcl_core::launch::session::LaunchSessionState;
use shtmcl_core::launch::{commands, evidence};
use shtmcl_core::tasks::TaskService;
use shtmcl_core::CoreModule;
use shtmcl_domain::{AttemptGeneration, CancelTaskRequest, TaskState};

use support::*;

const GATE_TIMEOUT: Duration = Duration::from_secs(10);

fn prepare_request() -> shtmcl_domain::CommandRequest {
    mutation(
        commands::LAUNCH_PREPARE,
        serde_json::json!({ "instanceId": "instance-1" }),
    )
}

#[test]
fn identity_frozen_before_runtime_preparation_cannot_silently_change() {
    let mut snapshot = sample_snapshot("1.20.4");
    snapshot.offline_username = Some("Bob".into());
    let fixture = Fixture::new("identity-changed-during-runtime", snapshot);
    let mut request = prepare_request();
    request.payload["expectedOfflineUsername"] = serde_json::json!("Alice");
    let (_, task) = accepted(fixture.module.handle(&request).unwrap());
    fixture.module.join_jobs();
    let task = fixture.task(&task);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(task.error.unwrap().code.as_str(), "launch.identity.changed");
    assert_eq!(
        fixture
            .accounts
            .calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[test]
fn a_repeated_click_resolves_to_the_same_session_and_task() {
    let fixture = Fixture::with_gate(
        "session-idempotent",
        sample_snapshot("1.20.4"),
        Some("auth"),
    );
    fixture.process.set_java(&java_banner(21));

    let (first_operation, first_task) = accepted(
        fixture
            .module
            .handle(&prepare_request())
            .expect("first click"),
    );
    assert!(
        fixture.steps.wait_blocked(GATE_TIMEOUT),
        "the preparation job must reach the auth phase"
    );

    // Second click while the first is still preparing.
    let (second_operation, second_task) = accepted(
        fixture
            .module
            .handle(&prepare_request())
            .expect("second click"),
    );
    assert_eq!(second_task, first_task, "the repeated click joins the task");
    assert_eq!(second_operation, first_operation);
    assert_eq!(fixture.module.sessions().len(), 1, "one session only");
    assert_eq!(fixture.tasks.snapshots().len(), 1, "one task only");

    fixture.steps.release();
    fixture.process.exit_game_now(0);
    fixture.module.join_jobs();
    assert_eq!(fixture.task(&first_task).state, TaskState::Succeeded);
}

#[test]
fn a_second_session_requires_the_explicit_entry_and_multi_open() {
    // Multi-open disabled: the explicit entry still refuses.
    let single = Fixture::with_gate("session-single", sample_snapshot("1.20.4"), Some("auth"));
    single.process.set_java(&java_banner(21));
    let (_, first_task) = accepted(single.module.handle(&prepare_request()).expect("prepare"));
    assert!(single.steps.wait_blocked(GATE_TIMEOUT));

    let (_, again_task) = accepted(
        single
            .module
            .handle(&mutation(
                commands::LAUNCH_START_AGAIN,
                serde_json::json!({ "instanceId": "instance-1" }),
            ))
            .expect("start again is a valid command"),
    );
    single.steps.release();

    let again = single.wait_task_terminal(&again_task, GATE_TIMEOUT);
    assert_eq!(again.state, TaskState::Failed);
    assert_eq!(
        again
            .error
            .as_ref()
            .map(|error| error.code.as_str().to_string()),
        Some("launch.multi_open.disabled".to_string())
    );
    assert_eq!(
        single.module.sessions().len(),
        2,
        "the attempt has its own session"
    );
    let first_session_id = single.session_by_task(&first_task).expect("session").id;
    assert!(
        single.wait_session_state(
            &first_session_id,
            LaunchSessionState::ProcessRunning,
            GATE_TIMEOUT
        ),
        "the first session keeps preparing/running undisturbed"
    );
    assert_eq!(single.process.game_spawns().len(), 1);

    single.process.exit_game_now(0);
    single.module.join_jobs();

    // Multi-open allowed: two independent sessions and two processes.
    let mut snapshot = sample_snapshot("1.20.4");
    snapshot.multi_open = true;
    let multi = Fixture::with_gate("session-multi", snapshot, Some("auth"));
    multi.process.set_java(&java_banner(21));
    let (_, first) = accepted(multi.module.handle(&prepare_request()).expect("prepare"));
    assert!(multi.steps.wait_blocked(GATE_TIMEOUT));
    let (_, second) = accepted(
        multi
            .module
            .handle(&mutation(
                commands::LAUNCH_START_AGAIN,
                serde_json::json!({ "instanceId": "instance-1" }),
            ))
            .expect("start again"),
    );
    multi.steps.release();
    assert_ne!(first, second);

    let first_id = multi.session_by_task(&first).expect("first session").id;
    let second_id = multi.session_by_task(&second).expect("second session").id;
    assert_ne!(first_id, second_id, "two independent sessions");
    assert!(
        multi.wait_session_state(&first_id, LaunchSessionState::ProcessRunning, GATE_TIMEOUT),
        "the first session runs"
    );
    assert!(
        multi.wait_session_state(&second_id, LaunchSessionState::ProcessRunning, GATE_TIMEOUT),
        "the second session runs"
    );
    let first_pid = multi
        .module
        .session(&first_id)
        .and_then(|session| session.identity.map(|identity| identity.pid));
    let second_pid = multi
        .module
        .session(&second_id)
        .and_then(|session| session.identity.map(|identity| identity.pid));
    assert!(first_pid.is_some() && second_pid.is_some());
    assert_ne!(first_pid, second_pid, "each session owns its own process");
    assert_eq!(multi.process.game_spawns().len(), 2);

    multi.process.exit_game_now(0);
    multi.module.join_jobs();
    assert_eq!(
        multi.module.session(&first_id).map(|session| session.state),
        Some(LaunchSessionState::Exited)
    );
    assert_eq!(
        multi
            .module
            .session(&second_id)
            .map(|session| session.state),
        Some(LaunchSessionState::Exited)
    );
}

#[test]
fn cancellation_during_preparation_stops_at_a_safe_point() {
    let fixture = Fixture::with_gate("session-cancel", sample_snapshot("1.20.4"), Some("auth"));
    fixture.process.set_java(&java_banner(21));

    let (_, task_id) = accepted(fixture.module.handle(&prepare_request()).expect("prepare"));
    assert!(fixture.steps.wait_blocked(GATE_TIMEOUT));

    let outcome = fixture.tasks.request_cancel(&CancelTaskRequest {
        task_id: task_id.clone(),
        attempt_generation: AttemptGeneration::INITIAL,
    });
    assert_eq!(outcome.decision, shtmcl_domain::CancelDecision::Requested);
    // Nothing is cancelled until the module reaches the safe point.
    assert_eq!(fixture.task(&task_id).state, TaskState::Running);

    fixture.steps.release();
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Cancelled);
    assert!(
        task.evidence
            .iter()
            .any(|marker| marker == evidence::SAFE_STOP_REACHED),
        "the safe stop is proven by evidence: {:?}",
        task.evidence
    );
    let session = fixture.session_by_task(&task_id).expect("session");
    assert_eq!(session.state, LaunchSessionState::Cancelled);
    assert!(session.identity.is_none());
    assert!(
        fixture.process.game_spawns().is_empty(),
        "a cancelled preparation never creates a process"
    );
}

#[test]
fn a_crash_is_recorded_and_never_restarted() {
    let fixture = Fixture::new("session-crash", sample_snapshot("1.20.4"));
    fixture.process.set_java(&java_banner(21));
    fixture.process.exit_game_now(1);

    let (_, task_id) = accepted(fixture.module.handle(&prepare_request()).expect("prepare"));
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(
        task.state,
        TaskState::Succeeded,
        "the launch itself succeeded: the process was created"
    );
    let session = fixture.session_by_task(&task_id).expect("session");
    assert_eq!(session.state, LaunchSessionState::Exited);
    assert_eq!(session.exit_code, Some(1));
    assert_eq!(
        session
            .error
            .as_ref()
            .map(|error| error.code.as_str().to_string()),
        Some("launch.process.exited_early".to_string())
    );
    assert!(session.has_evidence("process_exited_early"));
    assert_eq!(
        session.game_reached, None,
        "a pid is not evidence that the game started"
    );
    assert!(
        !session.has_evidence(evidence::GAME_REACHED),
        "the module never claims the game was reached without evidence"
    );
    assert_eq!(
        fixture.process.game_spawns().len(),
        1,
        "a crashed game is not restarted automatically"
    );
    assert_eq!(fixture.module.sessions().len(), 1, "no replacement session");
}

#[test]
fn a_spawn_failure_fails_the_session_without_inventing_a_process() {
    let fixture = Fixture::new("session-spawn-fail", sample_snapshot("1.20.4"));
    fixture.process.set_java(&java_banner(21));
    fixture
        .process
        .fail_game_spawn
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let (_, task_id) = accepted(fixture.module.handle(&prepare_request()).expect("prepare"));
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Failed);
    let error = task.error.expect("failure envelope");
    assert_eq!(error.code.as_str(), "launch.spawn.failed");
    assert_eq!(error.category, shtmcl_domain::ErrorCategory::Internal);

    let session = fixture.session_by_task(&task_id).expect("session");
    assert_eq!(session.state, LaunchSessionState::Failed);
    assert!(session.identity.is_none());
    assert!(
        session.plan_id.is_some(),
        "the plan was built before spawning"
    );
}

#[test]
fn launch_stop_is_identity_checked_and_recorded() {
    let fixture = Fixture::new("session-stop", sample_snapshot("1.20.4"));
    fixture.process.set_java(&java_banner(21));

    let (_, task_id) = accepted(fixture.module.handle(&prepare_request()).expect("prepare"));
    let session_id = fixture.session_by_task(&task_id).expect("session").id;
    assert!(
        fixture.wait_session_state(
            &session_id,
            LaunchSessionState::ProcessRunning,
            GATE_TIMEOUT
        ),
        "the session must reach process_running"
    );
    let session = fixture.module.session(&session_id).expect("session");

    // A stale pid is refused instead of stopping an unrelated process.
    let (_, wrong_task) = accepted(
        fixture
            .module
            .handle(&mutation(
                commands::LAUNCH_STOP,
                serde_json::json!({ "sessionId": session.id.as_str(), "expectedPid": 999_u64 }),
            ))
            .expect("stop accepted"),
    );
    let wrong = fixture.wait_task_terminal(&wrong_task, GATE_TIMEOUT);
    assert_eq!(wrong.state, TaskState::Failed);
    assert_eq!(
        wrong
            .error
            .as_ref()
            .map(|error| error.code.as_str().to_string()),
        Some("launch.process.mismatch".to_string())
    );

    let pid = session.identity.as_ref().expect("pid").pid;
    let (_, stop_task) = accepted(
        fixture
            .module
            .handle(&mutation(
                commands::LAUNCH_STOP,
                serde_json::json!({ "sessionId": session.id.as_str(), "expectedPid": pid }),
            ))
            .expect("stop accepted"),
    );
    fixture.module.join_jobs();

    assert_eq!(fixture.task(&stop_task).state, TaskState::Succeeded);
    let stopped = fixture.module.session(&session.id).expect("session");
    assert!(stopped.has_evidence(evidence::STOP_REQUESTED));
    assert!(
        stopped.error.is_none(),
        "an explicit stop is not an early-exit failure"
    );
    assert_eq!(
        fixture.process.stops.lock().expect("stops").as_slice(),
        &[pid],
        "the supervisor received exactly one stop for the recorded pid"
    );
    assert_eq!(
        stopped.state,
        LaunchSessionState::Exited,
        "the exit watcher closes the session after the process ends"
    );
}

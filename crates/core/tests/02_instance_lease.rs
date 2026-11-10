//! Protection spans preparation and process lifetime, including failure paths.
#[path = "02_support/mod.rs"]
mod support;

use shtmcl_core::launch::commands;
use shtmcl_core::tasks::TaskService;
use shtmcl_core::CoreModule;
use shtmcl_domain::{AttemptGeneration, CancelTaskRequest, TaskState};
use std::sync::atomic::Ordering;
use std::time::Duration;
use support::*;

fn prepare() -> shtmcl_domain::CommandRequest {
    mutation(
        commands::LAUNCH_PREPARE,
        serde_json::json!({"instanceId":"instance-1"}),
    )
}

#[test]
fn process_binding_failure_fails_task_but_protects_game_until_confirmed_exit() {
    let fixture = Fixture::new("lease-bind-failure", sample_snapshot("1.20.4"));
    fixture.leases.fail_bind.store(true, Ordering::SeqCst);
    fixture.process.stop_fails.store(true, Ordering::SeqCst);
    let (_, task) = accepted(fixture.module.handle(&prepare()).unwrap());
    assert_eq!(
        fixture
            .wait_task_terminal(&task, Duration::from_secs(10))
            .state,
        TaskState::Failed
    );
    assert_eq!(fixture.process.game_spawns().len(), 1);
    assert_eq!(fixture.leases.active.load(Ordering::SeqCst), 1);
    let observed = fixture.process.exited.lock().unwrap().clone();
    fixture.process.exited.lock().unwrap().clear();
    // Unknown from the platform must not be treated as process exit.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(fixture.leases.active.load(Ordering::SeqCst), 1);
    *fixture.process.exited.lock().unwrap() = observed;
    fixture.process.exit_game_now(0);
    fixture.module.join_jobs();
    assert_eq!(fixture.leases.active.load(Ordering::SeqCst), 0);
}

#[test]
fn lease_survives_completed_launch_task_until_process_exit() {
    let fixture = Fixture::with_gate("lease-lifetime", sample_snapshot("1.20.4"), Some("auth"));
    let (_, task) = accepted(fixture.module.handle(&prepare()).unwrap());
    assert!(fixture.steps.wait_blocked(Duration::from_secs(10)));
    assert_eq!(fixture.leases.active.load(Ordering::SeqCst), 1);
    fixture.steps.release();
    assert_eq!(
        fixture
            .wait_task_terminal(&task, Duration::from_secs(10))
            .state,
        TaskState::Succeeded
    );
    assert_eq!(
        fixture.leases.active.load(Ordering::SeqCst),
        1,
        "a completed launch task still has a live game"
    );
    assert!(fixture.leases.renewals.load(Ordering::SeqCst) >= 1);
    fixture.process.exit_game_now(0);
    fixture.module.join_jobs();
    assert_eq!(fixture.leases.active.load(Ordering::SeqCst), 0);
}

#[test]
fn lost_lease_before_spawn_refuses_process_and_releases_guard() {
    let fixture = Fixture::with_gate(
        "lease-renew-failure",
        sample_snapshot("1.20.4"),
        Some("spawn"),
    );
    let (_, task) = accepted(fixture.module.handle(&prepare()).unwrap());
    assert!(fixture.steps.wait_blocked(Duration::from_secs(10)));
    fixture.leases.fail_renew.store(true, Ordering::SeqCst);
    fixture.steps.release();
    fixture.module.join_jobs();
    assert_eq!(fixture.task(&task).state, TaskState::Failed);
    assert!(fixture.process.game_spawns().is_empty());
    assert_eq!(fixture.leases.active.load(Ordering::SeqCst), 0);
}

#[test]
fn preparation_failure_releases_lease_without_spawning() {
    let mut snapshot = sample_snapshot("1.20.4");
    snapshot.offline_username = Some("Bob".into());
    let fixture = Fixture::new("lease-snapshot-failure", snapshot);
    let mut request = prepare();
    request.payload["expectedOfflineUsername"] = serde_json::json!("Alice");
    let (_, task) = accepted(fixture.module.handle(&request).unwrap());
    fixture.module.join_jobs();
    assert_eq!(fixture.task(&task).state, TaskState::Failed);
    assert!(fixture.process.game_spawns().is_empty());
    assert_eq!(fixture.leases.active.load(Ordering::SeqCst), 0);
}

#[test]
fn cancellation_releases_only_after_preparation_safe_point() {
    let fixture = Fixture::with_gate("lease-cancel", sample_snapshot("1.20.4"), Some("auth"));
    let (_, task) = accepted(fixture.module.handle(&prepare()).unwrap());
    assert!(fixture.steps.wait_blocked(Duration::from_secs(10)));
    fixture.tasks.request_cancel(&CancelTaskRequest {
        task_id: task.clone(),
        attempt_generation: AttemptGeneration::INITIAL,
    });
    assert_eq!(fixture.leases.active.load(Ordering::SeqCst), 1);
    fixture.steps.release();
    fixture.module.join_jobs();
    assert_eq!(fixture.task(&task).state, TaskState::Cancelled);
    assert_eq!(fixture.leases.active.load(Ordering::SeqCst), 0);
    assert!(fixture.process.game_spawns().is_empty());
}

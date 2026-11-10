//! 04 · dependency solver behaviour: complete solutions, proven unsatisfiability,
//! budget exhaustion, cancellation through the shared task port.
//!
//! These are integration tests (external API only) so they also protect the frozen
//! `TaskService` cancellation contract that 04 relies on.

use std::sync::Arc;
use std::time::Duration;

use shtmcl_core::content::loader::{GameTarget, LoaderKind};
use shtmcl_core::content::ports::SourceKind;
use shtmcl_core::content::resolve::{
    candidate_from_value, solve, BudgetLimit, Candidate, ConflictReason, Dependency, SolveBudget,
    SolveOutcome, SolveRequest,
};
use shtmcl_core::content::version::{Version, VersionRange};
use shtmcl_core::tasks::{InMemoryEvents, InMemoryTasks, TaskService, TaskSpec};
use shtmcl_domain::{CancelTaskRequest, OperationId, TaskId, TaskPhase, TaskState};

fn version(value: &str) -> Version {
    Version::parse(value).expect("valid version")
}

fn range(value: &str) -> VersionRange {
    VersionRange::parse(value).expect("valid range")
}

fn target() -> GameTarget {
    GameTarget::new("1.20.1", LoaderKind::Fabric)
}

fn candidate(project: &str, file: &str, version_value: &str) -> Candidate {
    Candidate::new(SourceKind::Modrinth, project, file, version(version_value))
        .for_target("1.20.1", LoaderKind::Fabric)
}

fn solved_pairs(request: &SolveRequest) -> Vec<(String, String)> {
    match &solve(request, &|| false).outcome {
        SolveOutcome::Solved { assignments } => assignments
            .iter()
            .map(|item| (item.project_id.clone(), item.version.clone()))
            .collect(),
        other => panic!("expected a complete solution, got {other:?}"),
    }
}

#[test]
fn resolves_transitive_dependencies_in_a_deterministic_order() {
    let request = SolveRequest {
        target: target(),
        roots: vec!["sodium".to_string()],
        candidates: vec![
            candidate("sodium", "f-1", "0.5.3")
                .depends_on(Dependency::new("fabric-api", range(">=0.90"))),
            candidate("fabric-api", "f-3", "0.90.0"),
            candidate("fabric-api", "f-2", "0.92.0"),
        ],
        budget: SolveBudget::default(),
    };

    let first = solved_pairs(&request);
    assert_eq!(
        first,
        vec![
            ("fabric-api".to_string(), "0.92.0".to_string()),
            ("sodium".to_string(), "0.5.3".to_string()),
        ],
        "highest satisfying file wins and output is sorted by project id"
    );
    // Same input, same output: no map-iteration or hash order leaks into the plan.
    assert_eq!(solved_pairs(&request), first);

    let reversed = SolveRequest {
        candidates: request.candidates.iter().rev().cloned().collect(),
        ..request
    };
    assert_eq!(solved_pairs(&reversed), first);
}

#[test]
fn proves_unsatisfiable_with_the_conflict_chain() {
    let request = SolveRequest {
        target: target(),
        roots: vec!["create".to_string()],
        candidates: vec![
            candidate("create", "c-1", "0.5.1")
                .depends_on(Dependency::new("flywheel", range(">=0.6.10"))),
            candidate("flywheel", "fw-1", "0.6.8"),
        ],
        budget: SolveBudget::default(),
    };
    let result = solve(&request, &|| false);
    match &result.outcome {
        SolveOutcome::Unsatisfiable { conflicts } => {
            let entry = conflicts
                .iter()
                .find(|conflict| conflict.project_id == "flywheel")
                .expect("a conflict entry for flywheel");
            assert_eq!(entry.reason, ConflictReason::RangeUnmet);
            assert_eq!(entry.requested_by.len(), 1);
            assert_eq!(entry.requested_by[0].project_id, "create");
            assert_eq!(entry.requested_by[0].range, ">=0.6.10");
            assert_eq!(entry.available, vec!["0.6.8".to_string()]);
        }
        other => panic!("expected unsatisfiable, got {other:?}"),
    }
    assert!(!result.outcome.is_installable());
}

#[test]
fn budget_exhaustion_is_reported_separately_from_unsatisfiability() {
    // Every candidate of "a" needs a project that does not exist, so the search
    // must try each one: 3 states before the state budget runs out.
    let mut candidates = Vec::new();
    for index in 0..6 {
        candidates.push(
            candidate("a", &format!("a{index}"), &format!("1.{index}"))
                .depends_on(Dependency::new("missing", range(">=1.0"))),
        );
    }
    let exhausted = solve(
        &SolveRequest {
            target: target(),
            roots: vec!["a".to_string()],
            candidates: candidates.clone(),
            budget: SolveBudget::new(3, Duration::from_secs(30)),
        },
        &|| false,
    );
    match exhausted.outcome {
        SolveOutcome::BudgetExhausted { limit, used_states } => {
            assert_eq!(limit, BudgetLimit::States);
            assert!(used_states <= 3, "the budget caps the visited states");
            assert!(exhausted.states_visited <= 3);
        }
        other => panic!("expected budget exhaustion, got {other:?}"),
    }

    // The very same graph finishes and is *proven* unsatisfiable with a bigger
    // budget: a partial search must never be presented as "no compatible version".
    let complete = solve(
        &SolveRequest {
            target: target(),
            roots: vec!["a".to_string()],
            candidates,
            budget: SolveBudget::new(10_000, Duration::from_secs(30)),
        },
        &|| false,
    );
    match complete.outcome {
        SolveOutcome::Unsatisfiable { conflicts } => {
            assert!(conflicts
                .iter()
                .any(|conflict| conflict.project_id == "missing"
                    && conflict.reason == ConflictReason::MissingProject));
        }
        other => panic!("expected a proof of unsatisfiability, got {other:?}"),
    }
}

#[test]
fn wall_clock_budget_stops_the_search() {
    let result = solve(
        &SolveRequest {
            target: target(),
            roots: vec!["a".to_string()],
            candidates: vec![candidate("a", "a1", "1.0")],
            budget: SolveBudget::new(10_000, Duration::from_millis(0)),
        },
        &|| false,
    );
    match result.outcome {
        SolveOutcome::BudgetExhausted { limit, .. } => {
            assert_eq!(limit, BudgetLimit::WallClock);
        }
        other => panic!("expected a wall-clock budget stop, got {other:?}"),
    }
}

#[test]
fn cancellation_enters_only_through_the_task_service() {
    let events = Arc::new(InMemoryEvents::new());
    let tasks: Arc<InMemoryTasks> = Arc::new(InMemoryTasks::new(events));
    let task = tasks
        .accept(TaskSpec::new(
            OperationId::new("op-solve").expect("operation"),
            TaskPhase::new("content.plan").expect("phase"),
        ))
        .expect("accept");
    tasks
        .transition(&task.id, task.attempt_generation, TaskState::Running)
        .expect("running");

    let request = SolveRequest {
        target: target(),
        roots: vec!["a".to_string()],
        candidates: vec![candidate("a", "a1", "1.0")],
        budget: SolveBudget::default(),
    };

    // No cancellation yet: the solve completes.
    assert!(solve(&request, &|| tasks
        .is_cancel_requested(&task.id, task.attempt_generation))
    .outcome
    .is_installable());

    let decision = tasks
        .request_cancel(&CancelTaskRequest {
            task_id: task.id.clone(),
            attempt_generation: task.attempt_generation,
        })
        .decision;
    assert_eq!(decision, shtmcl_domain::CancelDecision::Requested);
    assert!(tasks.is_cancel_requested(&task.id, task.attempt_generation));

    let cancelled = solve(&request, &|| {
        tasks.is_cancel_requested(&task.id, task.attempt_generation)
    });
    assert!(matches!(cancelled.outcome, SolveOutcome::Cancelled { .. }));
    assert!(!cancelled.outcome.is_installable());

    // Cancellation is only confirmed at a safe point.
    let stopped = tasks
        .cancel_now(&task.id, task.attempt_generation)
        .expect("safe stop");
    assert_eq!(stopped.state, TaskState::Cancelled);
}

#[test]
fn dependency_cycles_are_not_automatically_unsatisfiable() {
    let request = SolveRequest {
        target: target(),
        roots: vec!["a".to_string()],
        candidates: vec![
            candidate("a", "a1", "1.0").depends_on(Dependency::new("b", range("*"))),
            candidate("b", "b1", "1.0").depends_on(Dependency::new("a", range(">=1.0"))),
        ],
        budget: SolveBudget::default(),
    };
    assert_eq!(
        solved_pairs(&request),
        vec![
            ("a".to_string(), "1.0".to_string()),
            ("b".to_string(), "1.0".to_string()),
        ]
    );
}

#[test]
fn declared_incompatibility_is_explained_with_both_projects() {
    let request = SolveRequest {
        target: target(),
        roots: vec!["a".to_string(), "b".to_string()],
        candidates: vec![
            candidate("a", "a1", "1.0").incompatible_with(Dependency::new("b", range("*"))),
            candidate("b", "b1", "1.0"),
        ],
        budget: SolveBudget::default(),
    };
    match solve(&request, &|| false).outcome {
        SolveOutcome::Unsatisfiable { conflicts } => {
            let entry = conflicts
                .iter()
                .find(|conflict| conflict.reason == ConflictReason::IncompatiblePair)
                .expect("an incompatible pair entry");
            assert!(!entry.requested_by.is_empty());
        }
        other => panic!("expected unsatisfiable, got {other:?}"),
    }
}

#[test]
fn candidates_for_another_target_are_filtered_before_search() {
    let mut wrong_loader = candidate("a", "a1", "1.0");
    wrong_loader.loaders = vec![LoaderKind::Forge];
    let request = SolveRequest {
        target: target(),
        roots: vec!["a".to_string()],
        candidates: vec![wrong_loader],
        budget: SolveBudget::default(),
    };
    let result = solve(&request, &|| false);
    assert_eq!(result.filtered_out, 1);
    match result.outcome {
        SolveOutcome::Unsatisfiable { conflicts } => {
            assert_eq!(conflicts[0].reason, ConflictReason::MissingProject);
        }
        other => panic!("expected unsatisfiable, got {other:?}"),
    }
}

#[test]
fn json_candidates_parse_with_the_documented_field_names() {
    let value = serde_json::json!({
        "provider": "modrinth",
        "projectId": "sodium",
        "fileId": "abc",
        "version": "0.5.3",
        "gameVersions": ["1.20.1"],
        "loaders": ["fabric"],
        "dependencies": [{ "projectId": "fabric-api", "range": ">=0.90" }],
    });
    let parsed = candidate_from_value(&value).expect("parse");
    assert_eq!(parsed.project_id, "sodium");
    assert_eq!(parsed.loaders, vec![LoaderKind::Fabric]);
    assert_eq!(parsed.dependencies.len(), 1);
    assert_eq!(parsed.dependencies[0].range.as_str(), ">=0.90");

    let bad = serde_json::json!({ "projectId": "x" });
    assert!(candidate_from_value(&bad).is_err(), "version is required");

    let _ = TaskId::new("task-unused").expect("id");
}

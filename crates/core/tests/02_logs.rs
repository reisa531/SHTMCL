//! 02 · game log pipeline: bounded retention, truncation, flood behaviour.
//!
//! 02 owns the only reader of the child pipes. The pump, the bounded ring, the
//! cursor/page contract and the truncation rules are the real implementation;
//! the process output and the LogStore are doubles.

#[path = "02_support/mod.rs"]
mod support;

use std::sync::atomic::Ordering;

use shtmcl_core::launch::{commands, logs};
use shtmcl_core::CoreModule;
use shtmcl_domain::TaskState;

use support::*;

fn lines(count: usize, width: usize) -> String {
    let mut out = String::with_capacity(count * (width + 12));
    for index in 0..count {
        out.push_str(&format!("[12:00:00] [main/INFO] line {index:05} "));
        out.push_str(&"x".repeat(width.saturating_sub(32)));
        out.push('\n');
    }
    out
}

fn launch_with_output(fixture: &Fixture) -> shtmcl_domain::TaskId {
    fixture.process.set_java(&java_banner(21));
    fixture.process.exit_game_now(0);
    let (_, task_id) = accepted(
        fixture
            .module
            .handle(&mutation(
                commands::LAUNCH_PREPARE,
                serde_json::json!({ "instanceId": "instance-1" }),
            ))
            .expect("prepare"),
    );
    task_id
}

#[test]
fn log_retention_is_bounded_and_reports_dropped_bytes() {
    let fixture = Fixture::new("logs-flood", sample_snapshot("1.20.4"));
    // 6,000 lines is above the 5,000-line retention cap, so the oldest go.
    fixture.process.set_game_output(&lines(6_000, 100), "");
    let task_id = launch_with_output(&fixture);
    fixture.module.join_jobs();

    assert_eq!(fixture.task(&task_id).state, TaskState::Succeeded);
    let session = fixture.session_by_task(&task_id).expect("session");
    let (records, bytes) = fixture.logs().retained(&session.id);
    assert!(
        records <= logs::DEFAULT_MAX_RECORDS,
        "records bounded: {records}"
    );
    assert!(bytes <= logs::DEFAULT_MAX_BYTES, "bytes bounded: {bytes}");
    let (dropped_bytes, dropped_lines) = fixture.logs().dropped(&session.id);
    assert!(dropped_lines > 0, "old lines are dropped and counted");
    assert!(dropped_bytes > 0);

    // The page is bounded and its cursor advances.
    let first = completed(
        fixture
            .module
            .handle(&request(
                commands::LAUNCH_LOGS,
                serde_json::json!({ "sessionId": session.id.as_str(), "limit": 50 }),
            ))
            .expect("page"),
    );
    let page = first["records"].as_array().expect("records");
    assert_eq!(page.len(), 50);
    assert_eq!(first["hasMore"], serde_json::json!(true));
    assert!(
        first["droppedLines"].as_u64().unwrap_or(0) > 0,
        "the page reports the loss so diagnostics can warn"
    );

    let cursor = first["nextCursor"].as_u64().expect("cursor");
    let second = completed(
        fixture
            .module
            .handle(&request(
                commands::LAUNCH_LOGS,
                serde_json::json!({
                    "sessionId": session.id.as_str(),
                    "limit": 50,
                    "cursor": cursor,
                }),
            ))
            .expect("page"),
    );
    let next_first = second["records"][0]["sequence"].as_u64().expect("sequence");
    assert!(
        next_first > cursor,
        "the next page continues after the cursor instead of repeating"
    );
    // The newest line is what an operator needs; it is never evicted first.
    let last_page = completed(
        fixture
            .module
            .handle(&request(
                commands::LAUNCH_LOGS,
                serde_json::json!({ "sessionId": session.id.as_str(), "limit": 1, "cursor": u64::MAX - 10 }),
            ))
            .expect("page"),
    );
    assert_eq!(last_page["records"].as_array().map(Vec::len), Some(0));
    assert!(fixture.logs().last_sequence(&session.id) >= 6_000);
}

#[test]
fn a_single_huge_line_is_truncated_and_counted() {
    let fixture = Fixture::new("logs-long-line", sample_snapshot("1.20.4"));
    let mut output = "[12:00:00] [main/INFO] ".to_string();
    output.push_str(&"y".repeat(300_000));
    output.push('\n');
    fixture.process.set_game_output(&output, "");
    let task_id = launch_with_output(&fixture);
    fixture.module.join_jobs();

    let session = fixture.session_by_task(&task_id).expect("session");
    let page = completed(
        fixture
            .module
            .handle(&request(
                commands::LAUNCH_LOGS,
                serde_json::json!({ "sessionId": session.id.as_str(), "limit": 10 }),
            ))
            .expect("page"),
    );
    let record = &page["records"][0];
    let line = record["line"].as_str().expect("line");
    assert!(
        line.len() <= logs::DEFAULT_MAX_LINE_BYTES,
        "one line cannot exceed the cap: {} bytes",
        line.len()
    );
    assert!(line.is_char_boundary(line.len()));
    assert!(
        record["omittedBytes"].as_u64().unwrap_or(0) > 0,
        "the omitted byte count is recorded"
    );
}

#[test]
fn a_rejecting_sink_never_blocks_the_game_pipes() {
    let fixture = Fixture::new("logs-rejecting-sink", sample_snapshot("1.20.4"));
    // Simulate a full disk / unavailable LogStore: the sink accepts nothing.
    fixture.sink.reject_all.store(true, Ordering::SeqCst);
    fixture
        .process
        .set_game_output(&lines(3_000, 80), &lines(500, 60));
    let task_id = launch_with_output(&fixture);
    fixture.module.join_jobs();

    let session = fixture.session_by_task(&task_id).expect("session");
    assert_eq!(
        fixture.logs().last_sequence(&session.id),
        3_500,
        "every line from both streams was still drained"
    );
    assert_eq!(fixture.sink.line_count(), 0, "the sink stored nothing");
    assert!(
        fixture.sink.append_calls.load(Ordering::Relaxed) >= 3_500,
        "the pump kept handing lines to the sink instead of stopping"
    );
    // The in-memory ring still holds a usable, bounded window.
    let (records, bytes) = fixture.logs().retained(&session.id);
    assert!(records > 0 && records <= logs::DEFAULT_MAX_RECORDS);
    assert!(bytes <= logs::DEFAULT_MAX_BYTES);
}

#[test]
fn an_unknown_session_has_no_log_page() {
    let fixture = Fixture::new("logs-unknown", sample_snapshot("1.20.4"));
    let error = fixture
        .module
        .handle(&request(
            commands::LAUNCH_LOGS,
            serde_json::json!({ "sessionId": "launch-does-not-exist" }),
        ))
        .expect_err("no page for an unknown session");
    assert_eq!(error.code.as_str(), "launch.logs.unavailable");
    assert_eq!(error.category, shtmcl_domain::ErrorCategory::NotFound);
}

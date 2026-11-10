//! 08 - bounded log store, diagnostic rules, repair plan, export, help.
//!
//! Everything here drives the real implementation: the module is built with
//! fail-closed ports plus the named `Fixture*` doubles from 04, the log store is
//! the shipped one (with small budgets so the ring/queue limits are reachable), and
//! the export really writes and really replaces a file on disk.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use shtmcl_core::content::plan::InstanceFile;
use shtmcl_core::content::ports::FixtureInstanceWriter;
use shtmcl_core::diagnostics::export::{ExportSelection, Exporter};
use shtmcl_core::diagnostics::logstore::LogLimits;
use shtmcl_core::diagnostics::rules::{rule_ids, rules as rule_names};
use shtmcl_core::diagnostics::{module_with_ports, DiagnosticsModule, DiagnosticsPorts};
use shtmcl_core::launch::logs::{LogRecord, LogSink, LogStream};
use shtmcl_core::tasks::{EventSink, InMemoryEvents, InMemoryTasks, TaskService};
use shtmcl_core::{CoreModule, ModuleContext};
use shtmcl_domain::{
    CancelTaskRequest, CommandRequest, CommandResponse, LaunchSessionId, OperationId, RequestId,
    Revision, TaskId, TaskState,
};
use shtmcl_platform::PlatformDirs;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "shtmcl-08-core-{name}-{}-{}",
        std::process::id(),
        unique()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp root");
    root
}

#[allow(dead_code)]
struct Fixture {
    root: PathBuf,
    module: Arc<DiagnosticsModule>,
    tasks: Arc<dyn TaskService>,
    events: Arc<dyn EventSink>,
}

impl Fixture {
    fn new(name: &str, ports: DiagnosticsPorts) -> Self {
        let root = temp_root(name);
        let dirs = PlatformDirs::under(&root);
        dirs.create_all().expect("dirs");
        let events: Arc<dyn EventSink> = Arc::new(InMemoryEvents::new());
        let tasks: Arc<dyn TaskService> = Arc::new(InMemoryTasks::new(events.clone()));
        let ctx = ModuleContext::new(tasks.clone(), events.clone(), dirs);
        let module = module_with_ports(ctx, ports);
        Self {
            root,
            module,
            tasks,
            events,
        }
    }

    fn with_limits(name: &str, limits: LogLimits) -> Self {
        let mut ports = DiagnosticsPorts::unavailable();
        ports.log_limits = limits;
        Self::new(name, ports)
    }

    fn handle(
        &self,
        request: CommandRequest,
    ) -> Result<CommandResponse, shtmcl_domain::ErrorEnvelope> {
        self.module.handle(&request)
    }

    fn feed(&self, session: &LaunchSessionId, lines: &[&str]) {
        self.module.log_store().register_session(session, None);
        for (index, line) in lines.iter().enumerate() {
            self.module.log_store().append(
                session,
                &[LogRecord {
                    sequence: index as u64 + 1,
                    stream: LogStream::Stdout,
                    line: (*line).to_string(),
                    omitted_bytes: 0,
                }],
            );
        }
    }

    fn feed_raw(&self, session: &LaunchSessionId, stream: LogStream, line: &str, omitted: usize) {
        self.module.log_store().append(
            session,
            &[LogRecord {
                sequence: 0,
                stream,
                line: line.to_string(),
                omitted_bytes: omitted,
            }],
        );
    }

    fn task(&self, task_id: &TaskId) -> shtmcl_domain::Task {
        self.tasks.snapshot(task_id).expect("task snapshot")
    }

    fn finish(&self, task_id: &TaskId) -> Value {
        self.module.join_jobs();
        let task = self.task(task_id);
        assert_eq!(
            task.state,
            TaskState::Succeeded,
            "task did not succeed: {:?}",
            task.error
        );
        task.result.clone().unwrap_or(Value::Null)
    }
}

fn request(command: &str, payload: Value) -> CommandRequest {
    let mut payload = payload;
    if let Some(object) = payload.as_object_mut() {
        object.insert("command".to_string(), Value::String(command.to_string()));
    }
    CommandRequest {
        request_id: RequestId::new(format!("req-{}-{}", command, unique())).expect("request id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload,
    }
}

fn mutation(command: &str, payload: Value) -> CommandRequest {
    let mut request = request(command, payload);
    request.operation_id =
        Some(OperationId::new(format!("op-{}-{}", command, unique())).expect("operation id"));
    request
}

fn accepted(response: CommandResponse) -> TaskId {
    match response {
        CommandResponse::Accepted { task_id, .. } => task_id,
        other => panic!("expected accepted, got {other:?}"),
    }
}

fn completed(response: CommandResponse) -> Value {
    match response {
        CommandResponse::Completed { result } => result,
        other => panic!("expected completed, got {other:?}"),
    }
}

fn session(id: &str) -> LaunchSessionId {
    LaunchSessionId::new(id).expect("session id")
}

// ---------------------------------------------------------------------------
// log store
// ---------------------------------------------------------------------------

#[test]
fn a_flood_is_bounded_truncated_and_still_reports_the_newest_lines() {
    let fixture = Fixture::with_limits(
        "flood",
        LogLimits {
            ring_records: 5_000,
            ring_bytes: 8 * 1024 * 1024,
            line_bytes: 64 * 1024,
            ..LogLimits::default()
        },
    );
    let session = session("session-flood");
    fixture.feed(&session, &["[12:00:00] [main/INFO] first line"]);
    for index in 0..7_000u32 {
        let line = format!("[12:00:00] [main/INFO] flood {index:05} {}", "x".repeat(40));
        fixture.feed_raw(&session, LogStream::Stdout, &line, 0);
    }
    // One hostile line far above the per-line cap, as the newest line so the
    // bounded window keeps it.
    fixture.feed_raw(
        &session,
        LogStream::Stderr,
        &format!("[12:00:00] [main/ERROR] {}", "y".repeat(200_000)),
        0,
    );

    let status = fixture
        .module
        .log_store()
        .status(&session)
        .expect("session status");
    assert!(
        status.records <= 5_000,
        "retention is bounded: {} records",
        status.records
    );
    assert!(
        status.ring_bytes <= 8 * 1024 * 1024,
        "ring bytes bounded: {}",
        status.ring_bytes
    );
    assert!(status.ring_dropped_lines > 0, "old lines are dropped");
    assert_eq!(status.last_sequence, 7_002, "every line was retained");

    // The page near the tail carries the truncated line, cut on a char boundary
    // with the omission recorded.
    let page = completed(
        fixture
            .handle(request(
                "diagnostics.logs",
                json!({
                    "sessionId": session.as_str(),
                    "limit": 10,
                    "cursor": status.last_sequence - 5,
                }),
            ))
            .expect("logs page"),
    );
    let records = page["records"].as_array().expect("records");
    let long = records
        .iter()
        .find(|record| record["omittedBytes"].as_u64().unwrap_or(0) > 0)
        .expect("the truncated line is retained in the window");
    let line = long["line"].as_str().expect("line");
    assert!(
        line.len() <= 64 * 1024,
        "one line cannot exceed the cap: {}",
        line.len()
    );
    assert!(line.is_char_boundary(line.len()));
    assert!(long["omittedBytes"].as_u64().unwrap_or(0) > 130_000);
    assert!(line.starts_with("[12:00:00] [main/ERROR]"));

    // A full page from the oldest retained line is still bounded.
    let head = completed(
        fixture
            .handle(request(
                "diagnostics.logs",
                json!({ "sessionId": session.as_str(), "limit": 1000 }),
            ))
            .expect("logs page"),
    );
    assert_eq!(head["records"].as_array().map(Vec::len), Some(1000));
    assert_eq!(head["hasMore"], json!(true));
    assert!(head["droppedLines"].as_u64().unwrap_or(0) > 0);
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn a_cursor_continues_without_repeating_or_skipping() {
    let fixture = Fixture::new("cursor", DiagnosticsPorts::unavailable());
    let session = session("session-cursor");
    let lines: Vec<String> = (0..600).map(|index| format!("line-{index:04}")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    fixture.feed(&session, &refs);

    let mut cursor: Option<u64> = None;
    let mut seen: Vec<u64> = Vec::new();
    loop {
        let page = completed(
            fixture
                .handle(request(
                    "diagnostics.logs",
                    json!({
                        "sessionId": session.as_str(),
                        "limit": 100,
                        "cursor": cursor,
                    }),
                ))
                .expect("logs page"),
        );
        let records = page["records"].as_array().expect("records");
        if records.is_empty() {
            break;
        }
        for record in records {
            seen.push(record["sequence"].as_u64().expect("sequence"));
        }
        cursor = Some(page["nextCursor"].as_u64().expect("cursor"));
        if page["hasMore"] != json!(true) {
            break;
        }
    }
    assert_eq!(seen.len(), 600, "no line is skipped");
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 600, "no line is repeated");
    assert_eq!(sorted.first().copied(), Some(1));
    assert_eq!(sorted.last().copied(), Some(600));
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn a_lagging_disk_drops_fragments_and_keeps_draining() {
    let fixture = Fixture::with_limits(
        "disk",
        LogLimits {
            queue_bytes: 4 * 1024,
            ring_records: 5_000,
            ..LogLimits::default()
        },
    );
    let session = session("session-disk");
    fixture.module.log_store().set_disk_stalled(true);
    for index in 0..2_000u32 {
        let line = format!(
            "[12:00:00] [main/INFO] stalled {index:04} {}",
            "z".repeat(60)
        );
        fixture.feed_raw(&session, LogStream::Stdout, &line, 0);
    }
    let status = fixture
        .module
        .log_store()
        .status(&session)
        .expect("session status");
    assert!(
        status.disk_dropped_lines > 0,
        "a jammed disk must be counted, not hidden"
    );
    assert!(status.not_persisted);
    assert_eq!(
        status.last_sequence, 2_000,
        "the pump kept delivering: nothing back-pressured the game"
    );
    assert!(fixture.module.log_store().queued_bytes() <= 4 * 1024);

    // The page carries the loss so diagnostics can warn instead of implying health.
    let page = completed(
        fixture
            .handle(request(
                "diagnostics.logs",
                json!({ "sessionId": session.as_str(), "limit": 10 }),
            ))
            .expect("logs page"),
    );
    assert_eq!(page["session"]["notPersisted"], json!(true));
    assert_eq!(page["session"]["lossy"], json!(true));
    assert!(page["session"]["diskDroppedLines"].as_u64().unwrap_or(0) > 0);

    fixture.module.log_store().set_disk_stalled(false);
    fixture
        .module
        .log_store()
        .flush_now(std::time::Duration::from_secs(5))
        .expect("drained");
    assert!(
        fixture.module.log_store().persisted_bytes(&session) > 0,
        "what fit in the queue really reached the file"
    );
    let _ = std::fs::remove_dir_all(&fixture.root);
}

// ---------------------------------------------------------------------------
// rules and report
// ---------------------------------------------------------------------------

fn analyze(fixture: &Fixture, session: &LaunchSessionId, payload: Value) -> Value {
    let mut payload = payload;
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "sessionId".to_string(),
            Value::String(session.as_str().to_string()),
        );
    }
    let task_id = accepted(
        fixture
            .handle(mutation("diagnostics.analyze", payload))
            .expect("analyze accepted"),
    );
    fixture.finish(&task_id)
}

#[test]
fn all_five_rules_find_their_condition_and_a_clean_log_stays_clean() {
    let fixture = Fixture::new("rules", DiagnosticsPorts::unavailable());
    let broken = session("session-broken");
    fixture.feed(
        &broken,
        &[
            "[12:00:00] [main/INFO] Using Java 17 (17.0.9) from C:\\Java\\jdk-17",
            "[12:00:00] [main/ERROR] java.lang.UnsupportedClassVersionError: net/fabricmc/loader/impl/FabricLoader has been compiled by a more recent version of the Java Runtime (class file version 65.0)",
            "[12:00:00] [main/ERROR] Mod resolution encountered an error: mod sodium requires any version of fabric-api, which is missing",
            "[12:00:00] [main/WARN] Duplicate mods found: sodium 0.5.8 and sodium 0.5.3",
            "[12:00:00] [main/ERROR] java.nio.file.FileSystemException: mods/sodium.jar: The process cannot access the file because it is being used by another process",
            "[12:00:00] [main/ERROR] java.lang.OutOfMemoryError: Java heap space",
        ],
    );
    let report = analyze(&fixture, &broken, json!({ "instanceRevision": 4 }));
    assert_eq!(report["status"], json!("completed"));
    assert_eq!(report["verdict"], json!("findings"));
    assert_eq!(report["conclusionsPossible"], json!(true));
    let found: Vec<String> = report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .map(|finding| finding["ruleId"].as_str().expect("ruleId").to_string())
        .collect();
    for rule in rule_ids() {
        assert!(
            found.iter().any(|id| id == rule),
            "rule {rule} should match its positive sample: {found:?}"
        );
    }
    let java = report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .find(|finding| finding["ruleId"] == json!(rule_names::JAVA_MISMATCH))
        .expect("java finding");
    assert_eq!(java["confidence"], json!("confirmed"));
    assert_eq!(java["action"]["kind"], json!("choose_runtime"));
    assert!(
        java["evidenceRefs"][0]["fromSequence"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    );
    assert_eq!(
        java["sessionId"],
        json!(broken.as_str()),
        "a finding references the session, not a copied log line"
    );

    let clean = session("session-clean");
    fixture.feed(
        &clean,
        &[
            "[12:00:00] [main/INFO] Using Java 17 (17.0.9) from C:\\Java\\jdk-17",
            "[12:00:00] [Worker-Main-1/INFO] Loading 42 mods",
            "[12:00:00] [main/INFO] Memory usage: 512 MB / 2048 MB",
            "[12:00:00] [main/INFO] Wrote options.txt",
        ],
    );
    let report = analyze(&fixture, &clean, json!({}));
    assert_eq!(report["status"], json!("no_matches"));
    assert_eq!(report["findingCount"], json!(0));
    assert_eq!(report["conclusionsPossible"], json!(true));
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn a_timed_out_analysis_returns_partial_results_and_never_says_the_log_is_fine() {
    let fixture = Fixture::new("timeout", DiagnosticsPorts::unavailable());
    let session = session("session-timeout");
    let mut lines: Vec<String> = Vec::new();
    for index in 0..200 {
        lines.push(format!("[12:00:00] [main/INFO] warmup {index:03}"));
    }
    lines.push("[12:00:00] [main/ERROR] java.lang.OutOfMemoryError: Java heap space".to_string());
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    fixture.feed(&session, &refs);

    // Deterministic budget clock: every probe advances 900 ms of a 2,000 ms budget.
    fixture
        .module
        .engine()
        .set_budget_step(Some(std::time::Duration::from_millis(900)));
    let report = analyze(&fixture, &session, json!({}));

    assert_eq!(report["status"], json!("timed_out"));
    assert_eq!(report["verdict"], json!("inconclusive"));
    assert_eq!(report["budget"]["timedOut"], json!(true));
    assert_eq!(report["budget"]["budgetMs"], json!(2000));
    assert_eq!(report["conclusionsPossible"], json!(false));
    let scanned = report["scannedRules"].as_array().expect("scanned").len();
    let skipped = report["skippedRules"].as_array().expect("skipped").len();
    assert!(scanned > 0, "finished rules are kept");
    assert!(skipped > 0, "rules that did not run are named");
    assert_eq!(scanned + skipped, rule_ids().len());

    // The partial report is still readable and the task says it is partial.
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn a_second_run_reads_only_the_increment() {
    let fixture = Fixture::new("increment", DiagnosticsPorts::unavailable());
    let session = session("session-increment");
    fixture.feed(&session, &["[12:00:00] [main/INFO] Loading 42 mods"]);
    let first = analyze(&fixture, &session, json!({}));
    assert_eq!(first["cached"], json!(false));
    let second = analyze(&fixture, &session, json!({}));
    assert_eq!(second["cached"], json!(true));

    fixture.feed(
        &session,
        &["[12:00:00] [main/WARN] Duplicate mods found: sodium"],
    );
    let third = analyze(&fixture, &session, json!({}));
    assert_eq!(third["cached"], json!(false));
    assert!(
        third["logRange"]["to"].as_u64().unwrap_or(0)
            > second["logRange"]["to"].as_u64().unwrap_or(0)
    );
    assert_eq!(third["findingCount"], json!(1));
    let key = third["cacheKey"].as_str().expect("cache key");
    assert!(
        key.contains("diagnostics-rules.v1"),
        "rules version in the key"
    );
    assert!(key.contains(session.as_str()), "session in the key");
    let _ = std::fs::remove_dir_all(&fixture.root);
}

// ---------------------------------------------------------------------------
// repair plan
// ---------------------------------------------------------------------------

fn instance_fixture() -> Arc<FixtureInstanceWriter> {
    let writer = Arc::new(FixtureInstanceWriter::new(Revision::new(3)));
    writer.seed(vec![
        InstanceFile::new("mods/sodium-0.5.8.jar", "sha1:aaa", 100),
        InstanceFile::new("mods/sodium-0.5.3.jar", "sha1:aaa", 100),
    ]);
    writer
}

#[test]
fn a_repair_plan_uses_the_content_plan_shape_and_never_writes_the_game_directory() {
    let instances = instance_fixture();
    let fixture = Fixture::new(
        "repair",
        DiagnosticsPorts {
            instances: instances.clone(),
            update: shtmcl_core::diagnostics::UpdatePorts::unavailable(),
            log_limits: LogLimits::default(),
        },
    );
    let session = session("session-repair");
    fixture.feed(
        &session,
        &["[12:00:00] [main/WARN] Duplicate mods found: sodium 0.5.8 and sodium 0.5.3"],
    );
    let report = analyze(&fixture, &session, json!({ "instanceId": "instance-1" }));
    let finding_id = report["findings"][0]["findingId"]
        .as_str()
        .expect("finding id")
        .to_string();
    assert_eq!(
        report["findings"][0]["action"]["kind"],
        json!("delete_file")
    );

    let plan = completed(
        fixture
            .handle(request(
                "diagnostics.repair.plan",
                json!({
                    "sessionId": session.as_str(),
                    "findingId": finding_id,
                    "target": "mods/sodium-0.5.3.jar",
                    "instanceRevision": 3,
                }),
            ))
            .expect("plan"),
    );
    assert_eq!(plan["schema"], json!("diagnostics-repair.v1"));
    let entries = plan["plan"]["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["action"], json!("delete"));
    assert_eq!(entries[0]["path"], json!("mods/sodium-0.5.3.jar"));
    assert_eq!(plan["plan"]["baseRevision"], json!(3));
    assert_eq!(plan["plan"]["schema"], json!("content-plan.v1"));
    assert!(plan["receipt"].is_null(), "planning does not write");
    assert!(
        instances.commits().is_empty(),
        "the instance owner was not asked to commit"
    );
    let instances_dir = fixture.root.join("instances");
    let leftovers = std::fs::read_dir(&instances_dir)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "08 never writes the game directory");
    assert_eq!(
        plan["recheck"]["command"],
        json!("diagnostics.analyze"),
        "a repair is only done after a re-check"
    );

    // Committing goes through the instance port, not through this module.
    let committed = completed(
        fixture
            .handle(mutation(
                "diagnostics.repair.plan",
                json!({
                    "sessionId": session.as_str(),
                    "findingId": finding_id,
                    "target": "mods/sodium-0.5.3.jar",
                    "instanceRevision": 3,
                    "commit": true,
                }),
            ))
            .expect("commit"),
    );
    assert_eq!(committed["receipt"]["revision"], json!(4));
    assert_eq!(instances.commits().len(), 1);
    assert_eq!(instances.files().len(), 1);
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn an_unplannable_finding_and_a_default_port_fail_closed() {
    let fixture = Fixture::new("repair-refused", DiagnosticsPorts::unavailable());
    let session = session("session-lock");
    fixture.feed(
        &session,
        &["[12:00:00] [main/ERROR] Unable to write mods/x.jar: The process cannot access the file because it is being used by another process"],
    );
    let report = analyze(&fixture, &session, json!({ "instanceId": "instance-1" }));
    let finding = &report["findings"][0];
    assert_eq!(finding["action"]["kind"], json!("close_process"));
    let finding_id = finding["findingId"]
        .as_str()
        .expect("finding id")
        .to_string();

    let error = fixture
        .handle(request(
            "diagnostics.repair.plan",
            json!({ "sessionId": session.as_str(), "findingId": finding_id }),
        ))
        .expect_err("a process repair is not a file change");
    assert_eq!(error.code.as_str(), "diagnostics.repair.unsupported");

    let error = fixture
        .handle(request(
            "diagnostics.repair.plan",
            json!({ "sessionId": session.as_str(), "findingId": "finding-does-not-exist" }),
        ))
        .expect_err("unknown finding");
    assert_eq!(error.code.as_str(), "diagnostics.analysis.not_found");
    let _ = std::fs::remove_dir_all(&fixture.root);
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

const CREDENTIAL_LOG: &str =
    include_str!("../../../tests/modules/08/fixtures/credentials-sample.log");
const REDACTION_CASES: &str = include_str!("../../../tests/modules/08/redaction-cases.json");

/// Longest run of hex digits in `text`, for the strict-export scan.
fn longest_hex_run(text: &str) -> usize {
    let mut longest = 0usize;
    let mut current = 0usize;
    for byte in text.bytes() {
        if byte.is_ascii_hexdigit() {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

#[test]
fn the_redaction_regression_sample_never_leaks_a_credential() {
    let cases: Value = serde_json::from_str(REDACTION_CASES).expect("cases json");
    let cases = cases["cases"].as_array().expect("cases");
    assert!(cases.len() >= 5, "the sample set covers every category");
    for case in cases {
        let id = case["id"].as_str().expect("case id");
        let line = case["line"].as_str().expect("case line");
        let (redacted, count) = shtmcl_core::diagnostics::export::redact_line(line);
        for forbidden in case["mustNotContain"].as_array().expect("mustNotContain") {
            let needle = forbidden.as_str().expect("string");
            assert!(
                !redacted.contains(needle),
                "{id}: {needle} survived redaction: {redacted}"
            );
        }
        for required in case["mustContain"].as_array().expect("mustContain") {
            let needle = required.as_str().expect("string");
            assert!(
                redacted.contains(needle),
                "{id}: {needle} must be kept: {redacted}"
            );
        }
        if !case["mustNotContain"].as_array().expect("list").is_empty() {
            assert!(count >= 1, "{id} redacted nothing");
        }
    }
}

#[test]
fn an_export_previews_its_fields_and_writes_a_redacted_bundle() {
    let fixture = Fixture::new("export", DiagnosticsPorts::unavailable());
    let session = session("session-export");
    let lines: Vec<&str> = CREDENTIAL_LOG.lines().collect();
    fixture.feed(&session, &lines);

    let preview = completed(
        fixture
            .handle(request(
                "diagnostics.export",
                json!({ "mode": "preview", "sessions": [session.as_str()] }),
            ))
            .expect("preview"),
    );
    assert_eq!(preview["mode"], json!("preview"));
    let fields: Vec<&str> = preview["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(fields.contains(&"session"));
    assert!(fields.contains(&"line"));
    assert!(fields.contains(&"findings"));
    assert!(!preview["redactionRules"]
        .as_array()
        .expect("rules")
        .is_empty());
    assert_eq!(preview["withinBudget"], json!(true));

    let exports = fixture.root.join("exports");
    std::fs::create_dir_all(&exports).expect("exports dir");
    let destination = exports.join("diagnostics.txt");
    let task_id = accepted(
        fixture
            .handle(mutation(
                "diagnostics.export",
                json!({
                    "mode": "write",
                    "sessions": [session.as_str()],
                    "destination": destination.to_string_lossy(),
                }),
            ))
            .expect("export accepted"),
    );
    let result = fixture.finish(&task_id);
    assert_eq!(result["cancelled"], json!(false));
    assert!(result["records"].as_u64().unwrap_or(0) > 0);
    assert!(result["redactions"].as_u64().unwrap_or(0) >= 5);
    assert_eq!(result["replacedExisting"], json!(false));

    let written = std::fs::read_to_string(&destination).expect("bundle");
    for secret in [
        "0f1e2d3c4b5a69788796a5b4c3d2e1f0",
        "M.R3_BAY.abc123",
        "rt-1122334455",
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
        "Sup3rSecret!",
        "cs_live_9x8y7z",
        "8f7e6d5c",
        // The command-line form an independent reviewer reproduced: it survived
        // before D08-1 and must never survive again.
        "eyJhbGciOi-argv-token",
    ] {
        assert!(
            !written.contains(secret),
            "a credential reached the bundle: {secret}"
        );
    }
    assert!(written.contains("<redacted>"));

    // No JWT survives, whatever the separator or key spelling was.
    assert!(
        !written.contains("eyJ"),
        "a JWT-shaped value reached the bundle"
    );
    // A file digest is diagnostic material and stays in the default bundle; the
    // stricter form (`redactDigests`) is checked below.
    assert!(
        written.contains("0a1b2c3d4e5f60718293a4b5c6d7e8f901234567"),
        "the file digest is diagnostic material"
    );
    assert!(
        written.contains("sodium"),
        "the useful context survives redaction"
    );
    assert!(
        written.contains("Steve"),
        "the player name is not a credential"
    );
    // No temporary or backup file is left behind.
    let leftovers: Vec<String> = std::fs::read_dir(&exports)
        .expect("exports dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(leftovers, vec!["diagnostics.txt".to_string()]);
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn a_strict_export_also_scans_digest_shapes() {
    // The opt-in form for a bundle that leaves the machine: nothing token-shaped
    // survives, whatever the key spelling was.
    let fixture = Fixture::new("export-strict", DiagnosticsPorts::unavailable());
    let session = session("session-strict");
    let lines: Vec<&str> = CREDENTIAL_LOG.lines().collect();
    fixture.feed(&session, &lines);
    let exports = fixture.root.join("exports");
    std::fs::create_dir_all(&exports).expect("exports dir");
    let destination = exports.join("strict.txt");
    let task_id = accepted(
        fixture
            .handle(mutation(
                "diagnostics.export",
                json!({
                    "mode": "write",
                    "sessions": [session.as_str()],
                    "destination": destination.to_string_lossy(),
                    "redactDigests": true,
                }),
            ))
            .expect("accepted"),
    );
    fixture.finish(&task_id);
    let written = std::fs::read_to_string(&destination).expect("bundle");
    assert!(
        longest_hex_run(&written) < 32,
        "a digest-shaped run survived the strict scan: {} hex digits",
        longest_hex_run(&written)
    );
    assert!(!written.contains("eyJ"));
    assert!(!written.contains("eyJhbGciOi-argv-token"));
    assert!(written.contains("sodium"), "the mod name is still useful");
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn a_cancelled_export_removes_its_temporary_and_keeps_the_existing_file() {
    let fixture = Fixture::new("export-cancel", DiagnosticsPorts::unavailable());
    let session = session("session-cancel");
    let lines: Vec<&str> = CREDENTIAL_LOG.lines().collect();
    fixture.feed(&session, &lines);

    let exports = fixture.root.join("exports");
    std::fs::create_dir_all(&exports).expect("exports dir");
    let destination = exports.join("diagnostics.txt");
    std::fs::write(&destination, b"previous bundle").expect("seed destination");

    let store = fixture.module.log_store().clone();
    let exporter = Exporter::new(store);
    let selection = ExportSelection {
        sessions: vec![session.clone()],
        ..ExportSelection::default()
    };
    let result = exporter
        .export(&selection, None, &destination, &|| true)
        .expect("cancelled export");
    assert!(result.cancelled);
    assert!(result.temporary_removed);
    assert_eq!(
        std::fs::read_to_string(&destination).expect("destination"),
        "previous bundle",
        "an existing target is never damaged by a cancelled export"
    );
    let leftovers: Vec<String> = std::fs::read_dir(&exports)
        .expect("exports dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        leftovers,
        vec!["diagnostics.txt".to_string()],
        "temporary removed"
    );
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn a_successful_export_replaces_the_previous_file_atomically() {
    let fixture = Fixture::new("export-replace", DiagnosticsPorts::unavailable());
    let session = session("session-replace");
    fixture.feed(&session, &["[12:00:00] [main/INFO] hello"]);
    let exports = fixture.root.join("exports");
    std::fs::create_dir_all(&exports).expect("exports dir");
    let destination = exports.join("diagnostics.txt");
    std::fs::write(&destination, b"old bundle").expect("seed");

    let exporter = Exporter::new(fixture.module.log_store().clone());
    let selection = ExportSelection {
        sessions: vec![session.clone()],
        ..ExportSelection::default()
    };
    let result = exporter
        .export(&selection, None, &destination, &|| false)
        .expect("export");
    assert!(result.replaced_existing);
    let written = std::fs::read_to_string(&destination).expect("destination");
    assert!(written.contains("hello"));
    assert!(!written.contains("old bundle"));
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn an_export_over_its_budget_fails_instead_of_truncating() {
    let fixture = Fixture::new("export-budget", DiagnosticsPorts::unavailable());
    let session = session("session-budget");
    let mut lines: Vec<String> = Vec::new();
    for index in 0..300 {
        lines.push(format!(
            "[12:00:00] [main/INFO] padding {index:03} {}",
            "p".repeat(80)
        ));
    }
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    fixture.feed(&session, &refs);
    let exports = fixture.root.join("exports");
    std::fs::create_dir_all(&exports).expect("exports dir");
    let destination = exports.join("too-big.txt");

    let task_id = accepted(
        fixture
            .handle(mutation(
                "diagnostics.export",
                json!({
                    "mode": "write",
                    "sessions": [session.as_str()],
                    "destination": destination.to_string_lossy(),
                    "maxBytes": 4096,
                }),
            ))
            .expect("accepted"),
    );
    fixture.module.join_jobs();
    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(
        task.error
            .as_ref()
            .map(|error| error.code.as_str().to_string()),
        Some("diagnostics.export.too_large".to_string())
    );
    assert!(
        !destination.exists(),
        "a refused export leaves no partial file"
    );
    let leftovers: Vec<String> = std::fs::read_dir(&exports)
        .expect("exports dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        leftovers.is_empty(),
        "no temporary file survives: {leftovers:?}"
    );
    let _ = std::fs::remove_dir_all(&fixture.root);
}

// ---------------------------------------------------------------------------
// help and routing
// ---------------------------------------------------------------------------

#[test]
fn help_entries_are_local_and_carry_a_next_step() {
    let fixture = Fixture::new("help", DiagnosticsPorts::unavailable());
    let value = completed(
        fixture
            .handle(request(
                "diagnostics.help",
                json!({ "entry": "out_of_memory" }),
            ))
            .expect("help"),
    );
    assert_eq!(value["schema"], json!("diagnostics-help.v1"));
    assert_eq!(value["offline"], json!(true));
    assert_eq!(value["matched"], json!(true));
    let entry = &value["entries"][0];
    assert_eq!(entry["id"], json!("out_of_memory"));
    assert!(!entry["nextStep"].as_str().unwrap_or_default().is_empty());

    let by_code = completed(
        fixture
            .handle(request(
                "diagnostics.help",
                json!({ "code": "diagnostics.update.signature_unverified" }),
            ))
            .expect("help"),
    );
    assert_eq!(by_code["category"], json!("delivery"));
    assert_eq!(by_code["matched"], json!(true));
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn unknown_and_malformed_commands_get_stable_codes() {
    let fixture = Fixture::new("routing", DiagnosticsPorts::unavailable());
    let error = fixture
        .handle(request("diagnostics.nonsense", json!({})))
        .expect_err("unknown command");
    assert_eq!(error.code.as_str(), "diagnostics.command.unknown");
    assert_eq!(error.category, shtmcl_domain::ErrorCategory::InvalidRequest);

    let mut missing = request("diagnostics.logs", json!({ "sessionId": "s" }));
    if let Some(object) = missing.payload.as_object_mut() {
        object.remove("command");
    }
    let error = fixture.handle(missing).expect_err("no command name");
    assert_eq!(error.code.as_str(), "command.name_required");

    let error = fixture
        .handle(request("diagnostics.logs", json!({ "sessionId": "nope" })))
        .expect_err("unknown session");
    assert_eq!(error.code.as_str(), "diagnostics.logs.not_found");
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn the_snapshot_reports_the_store_and_the_update_state() {
    let fixture = Fixture::new("snapshot", DiagnosticsPorts::unavailable());
    let session = session("session-snapshot");
    fixture.feed(&session, &["[12:00:00] [main/INFO] hi"]);
    let value = completed(
        fixture
            .handle(request("diagnostics.snapshot", json!({})))
            .expect("snapshot"),
    );
    assert_eq!(
        value["logStore"]["sessions"][0]["sessionId"],
        json!(session.as_str())
    );
    assert!(
        value["logStore"]["sessions"][0]["records"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    );
    assert!(value["updates"]["transactions"].is_array());
    assert!(value["helpCategories"]
        .as_array()
        .map(|list| !list.is_empty())
        .unwrap_or(false));
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn the_default_wiring_refuses_to_read_an_instance() {
    // `DiagnosticsPorts::unavailable()` is what `diagnostics::module(ctx)` uses.
    let fixture = Fixture::new("fail-closed", DiagnosticsPorts::unavailable());
    let session = session("session-closed");
    fixture.feed(
        &session,
        &["[12:00:00] [main/WARN] Duplicate mods found: sodium"],
    );
    let report = analyze(&fixture, &session, json!({ "instanceId": "instance-1" }));
    let finding_id = report["findings"][0]["findingId"]
        .as_str()
        .expect("finding id")
        .to_string();
    let error = fixture
        .handle(request(
            "diagnostics.repair.plan",
            json!({
                "sessionId": session.as_str(),
                "findingId": finding_id,
                "target": "mods/sodium-0.5.3.jar",
            }),
        ))
        .expect_err("no instance owner");
    assert_eq!(error.code.as_str(), "diagnostics.owner.unavailable");
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn the_sink_trait_object_pages_like_the_command_and_puts_the_pump_in_secondary_mode() {
    // 00's ruling: the LogStore is the single diagnostic store. The object 08 hands
    // to 02 (`log_sink()`) must therefore serve pages itself, so 02 keeps only a
    // small tail and the per-session memory budget does not double.
    let fixture = Fixture::new("sink-page", DiagnosticsPorts::unavailable());
    let session = session("session-sink");
    let lines: Vec<String> = (0..250).map(|index| format!("sink-{index:03}")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    fixture.feed(&session, &refs);

    let sink: Arc<dyn LogSink> = fixture.module.log_sink();
    assert!(
        sink.supports_paging(),
        "the store is the primary log source, so the pump must not keep a full ring"
    );
    // 02 asks the same question when it builds the pipeline.
    let pipeline = shtmcl_core::launch::logs::LogPipeline::new(sink.clone());
    assert!(
        pipeline.is_secondary(),
        "02 enters its tail-only mode with this sink"
    );
    let (tail_records, tail_bytes) = pipeline.local_capacity();
    assert!(
        tail_records < shtmcl_core::launch::logs::DEFAULT_MAX_RECORDS
            && tail_bytes < shtmcl_core::launch::logs::DEFAULT_MAX_BYTES,
        "one session no longer holds two 8 MiB rings: {tail_records} records / {tail_bytes} bytes"
    );

    let mut cursor = None;
    let mut seen = Vec::new();
    loop {
        // Sink-shaped read through the trait object, exactly as 02 will call it.
        let page = sink.page(&session, cursor, 40).expect("sink page");
        if page.records.is_empty() {
            break;
        }
        // The same cursor through the command surface returns the same records.
        let requested = completed(
            fixture
                .handle(request(
                    "diagnostics.logs",
                    json!({
                        "sessionId": session.as_str(),
                        "limit": 40,
                        "cursor": cursor,
                    }),
                ))
                .expect("logs page"),
        );
        let command_records = requested["records"].as_array().expect("records");
        assert_eq!(command_records.len(), page.records.len());
        for (record, value) in page.records.iter().zip(command_records) {
            assert_eq!(value["sequence"].as_u64(), Some(record.sequence));
            assert_eq!(value["line"].as_str(), Some(record.line.as_str()));
            seen.push(record.sequence);
        }
        cursor = Some(page.next_cursor);
        if !page.has_more {
            break;
        }
    }
    assert_eq!(
        seen.len(),
        250,
        "no line is lost through the sink trait object"
    );
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        250,
        "no line is repeated through the sink trait object"
    );
    let missing = LaunchSessionId::new("session-that-does-not-exist").expect("id");
    assert!(
        sink.page(&missing, None, 10).is_none(),
        "an unknown session is None, never an empty fake page"
    );
    assert_eq!(sink.append(&session, &[]), 0);
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn read_all_returns_the_whole_retained_window() {
    let fixture = Fixture::new("read-all", DiagnosticsPorts::unavailable());
    let session = session("session-all");
    // 2,500 lines is above one page (MAX_PAGE_LIMIT = 1,000) and inside the ring.
    let lines: Vec<String> = (0..2_500).map(|index| format!("all-{index:04}")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    fixture.feed(&session, &refs);
    let all = fixture
        .module
        .log_store()
        .read_all(&session)
        .expect("read_all");
    assert_eq!(all.len(), 2_500, "the whole window, not one capped page");
    assert_eq!(all.first().map(|record| record.sequence), Some(1));
    assert_eq!(all.last().map(|record| record.sequence), Some(2_500));
    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[test]
fn a_cancel_request_does_not_fake_a_finished_analysis() {
    let fixture = Fixture::new("cancel", DiagnosticsPorts::unavailable());
    let session = session("session-cancel-analyze");
    let mut lines: Vec<String> = Vec::new();
    for index in 0..400 {
        lines.push(format!("[12:00:00] [main/INFO] line {index:03}"));
    }
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    fixture.feed(&session, &refs);
    let task_id = accepted(
        fixture
            .handle(mutation(
                "diagnostics.analyze",
                json!({ "sessionId": session.as_str() }),
            ))
            .expect("accepted"),
    );
    let task = fixture.task(&task_id);
    let outcome = fixture.tasks.request_cancel(&CancelTaskRequest {
        task_id: task_id.clone(),
        attempt_generation: task.attempt_generation,
    });
    fixture.module.join_jobs();
    let settled = fixture.task(&task_id);
    // Either the analysis finished first (succeeded) or the cancellation was
    // confirmed at a safe point; a request alone never produces `cancelled`.
    match settled.state {
        TaskState::Cancelled => {
            assert_eq!(
                outcome.decision,
                shtmcl_domain::CancelDecision::Requested,
                "cancelled requires a confirmed request"
            );
            assert!(settled.result.is_none());
        }
        TaskState::Succeeded => {
            let report = settled.result.unwrap_or(Value::Null);
            assert!(report["findingCount"].is_number());
        }
        other => panic!("unexpected state {other}"),
    }
    let _ = std::fs::remove_dir_all(&fixture.root);
}

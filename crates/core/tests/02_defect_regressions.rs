//! Regression tests for the core evaluation defects
//! (`docs/development/handoffs/02/EVALUATION-core.md`).
//!
//! Each test names its defect id so a re-reviewer can map fix ↔ proof:
//! - D02-1 arm64 must not be read as x64 from an ambiguous banner
//! - D02-2 a credential in any argument family (including `jvm_args`) is redacted
//! - D02-4 the Quick Play auth gate is identical on every version row
//! - D02-5 `omittedBytes` equals the bytes actually dropped
//! - D02-6 a paging store owns retention, 02 keeps only a tail
//! - O02-1 `-cp` belongs to exactly one side

#[path = "02_support/mod.rs"]
mod support;

use std::sync::atomic::Ordering;

use shtmcl_core::launch::java::{self, Arch, JavaRequirement, RuntimeConstraint};
use shtmcl_core::launch::logs::{self, BoundedLogRing, LogPipeline, LogStream, NoopLogSink};
use shtmcl_core::launch::plan::LoaderKind;
use shtmcl_core::launch::ports::AuthRequirement;
use shtmcl_core::launch::quickplay::{reasons, CapabilityKey, QuickPlayCapability, QuickPlayTable};
use shtmcl_core::launch::{commands, evidence};
use shtmcl_core::tasks::EventSink;
use shtmcl_core::CoreModule;
use shtmcl_domain::{LaunchSessionId, TaskState};

use support::*;

/// The real aarch64 banner shape: identical to x86_64 apart from the build.
const ARM64_BANNER: &str = "openjdk version \"21.0.2\" 2024-01-16 LTS\nOpenJDK Runtime Environment Temurin-21.0.2+13 (build 21.0.2+13-LTS)\nOpenJDK 64-Bit Server VM Temurin-21.0.2+13 (build 21.0.2+13-LTS, mixed mode, sharing)\n";

fn prepare(fixture: &Fixture) -> shtmcl_domain::TaskId {
    let (_, task_id) = accepted(
        fixture
            .module
            .handle(&mutation(
                commands::LAUNCH_PREPARE,
                serde_json::json!({ "instanceId": "instance-1" }),
            ))
            .expect("prepare accepted"),
    );
    task_id
}

// ---------------------------------------------------------------------------
// D02-1
// ---------------------------------------------------------------------------

#[test]
fn d02_1_an_ambiguous_bitness_banner_is_never_guessed_as_x64() {
    // A 64-bit JVM banner carries no architecture: x86_64 and aarch64 print the
    // same line, so a bare parse must stay Unknown instead of guessing.
    assert_eq!(java::parse_arch(ARM64_BANNER), Arch::Unknown);

    // With the host as evidence (the probe ran here) it resolves correctly on
    // both architectures, so an arm64 host no longer rejects a correct runtime.
    assert_eq!(
        java::parse_arch_with_host(ARM64_BANNER, Arch::Arm64),
        Arch::Arm64
    );
    assert_eq!(
        java::parse_arch_with_host(ARM64_BANNER, Arch::X64),
        Arch::X64
    );
    assert_eq!(
        java::parse_arch_with_host(ARM64_BANNER, Arch::Unknown),
        Arch::Unknown
    );
}

#[test]
fn d02_1_an_unknown_architecture_is_unverified_not_rejected() {
    let host = java::HostPlatform::current();
    let mut constraint = RuntimeConstraint::host(JavaRequirement::AtLeast(17));
    constraint.platform = Some(host);
    constraint.arch = Some(Arch::Arm64);

    // Unknown arch: accepted, but flagged so the UI cannot claim a verified match.
    let unknown = vec![runtime_at("C:/jdk/bin/java.exe", 21, Arch::Unknown)];
    let selection = java::select(&unknown, &constraint, true);
    assert!(
        selection.selected.is_some(),
        "an unreadable architecture must not reject a working runtime"
    );
    assert!(selection.arch_unverified);
    assert!(selection.rejected.is_empty());

    // The same runtime resolved through the host evidence matches arm64 exactly.
    let resolved = java::parse_arch_with_host(ARM64_BANNER, Arch::Arm64);
    let arm64 = vec![runtime_at("C:/jdk/bin/java.exe", 21, resolved)];
    let selection = java::select(&arm64, &constraint, true);
    assert!(selection.selected.is_some());
    assert!(
        !selection.arch_unverified,
        "a resolved architecture is a verified match"
    );

    // And a genuinely mismatched architecture is still reported, not silently used.
    let x64 = vec![runtime_at("C:/jdk/bin/java.exe", 21, Arch::X64)];
    let selection = java::select(&x64, &constraint, true);
    assert!(selection.selected.is_none());
    assert_eq!(
        selection.rejected[0].reason.code(),
        "launch.java.arch_mismatch"
    );
}

// ---------------------------------------------------------------------------
// D02-2
// ---------------------------------------------------------------------------

#[test]
fn d02_2_a_credential_in_any_argument_family_is_redacted() {
    let mut snapshot = sample_snapshot("1.20.4");
    // Both families, plus a credential in the middle of a property value.
    snapshot.jvm_args = vec![
        "-Dminecraft.api.accessToken=${auth_access_token}".to_string(),
        "-Dmap_prefix=a=${auth_session}-b=${game_directory}".to_string(),
        "-Xmx1G".to_string(),
    ];
    let fixture = Fixture::new("d02-2-token", snapshot);
    fixture.process.set_java(&java_banner(21));
    fixture.process.exit_game_now(0);

    let task_id = prepare(&fixture);
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Succeeded, "error: {:?}", task.error);
    let result = task.result.clone().expect("result");
    assert_eq!(
        result["sensitiveArgs"],
        serde_json::json!(3),
        "every argument built from a credential template is marked sensitive"
    );

    for surface in [
        serde_json::to_string(&result).expect("result json"),
        serde_json::to_string(
            &fixture
                .module
                .sessions()
                .iter()
                .map(|session| session.to_json())
                .collect::<Vec<_>>(),
        )
        .expect("sessions json"),
        serde_json::to_string(
            &fixture
                .events
                .since(
                    &shtmcl_domain::OperationId::new("op-launch.prepare").expect("op"),
                    None,
                )
                .iter()
                .map(|event| event.payload.clone())
                .collect::<Vec<_>>(),
        )
        .expect("events json"),
    ] {
        assert!(
            !surface.contains("token-value-that-must-not-leak"),
            "no projection may contain the credential"
        );
    }

    // The credential is still passed to the game, once per argument that needs it.
    let spawns = fixture.process.game_spawns();
    assert_eq!(spawns.len(), 1);
    assert_eq!(
        spawns[0]
            .args
            .iter()
            .filter(|arg| arg.contains("token-value-that-must-not-leak"))
            .count(),
        3,
        "argv: {:?}",
        spawns[0].args
    );
    assert!(spawns[0].environment.is_empty(), "no credential in env");
}

// ---------------------------------------------------------------------------
// D02-4
// ---------------------------------------------------------------------------

#[test]
fn d02_4_the_auth_gate_is_identical_on_every_version_row() {
    let cases = [
        ("1.16.5", QuickPlayCapability::LaunchAndJoin),
        ("1.19.4", QuickPlayCapability::LaunchAndJoin),
        ("1.20.4", QuickPlayCapability::DirectJoin),
        ("1.21.4", QuickPlayCapability::DirectJoin),
    ];
    for (version, version_capability) in cases {
        let offline = QuickPlayTable::lookup(&CapabilityKey::new(
            version,
            LoaderKind::Vanilla,
            AuthRequirement::Offline,
        ));
        assert_eq!(
            offline.capability,
            QuickPlayCapability::Unsupported,
            "{version}: an offline identity never joins a server"
        );
        assert!(!offline.available, "{version}");
        assert_eq!(offline.reason_code, reasons::ONLINE_SESSION_REQUIRED);

        let online = QuickPlayTable::lookup(&CapabilityKey::new(
            version,
            LoaderKind::Vanilla,
            AuthRequirement::Online,
        ));
        assert_eq!(online.capability, version_capability, "{version}");
        assert!(online.available, "{version}");

        let unknown_auth = QuickPlayTable::lookup(&CapabilityKey::new(
            version,
            LoaderKind::Vanilla,
            AuthRequirement::Any,
        ));
        assert_eq!(unknown_auth.capability, version_capability, "{version}");
        assert!(
            !unknown_auth.available,
            "{version}: an unknown account state is never a yes"
        );
        assert_eq!(unknown_auth.reason_code, reasons::AUTH_UNVERIFIED);
    }
}

// ---------------------------------------------------------------------------
// D02-5
// ---------------------------------------------------------------------------

#[test]
fn d02_5_the_omitted_count_is_exact_for_a_multibyte_line() {
    let mut raw = String::new();
    while raw.len() < 300_000 {
        raw.push('あ');
    }
    let total = raw.len();

    // Ring path.
    let mut ring = BoundedLogRing::with_limits(4, 1 << 20, 64 * 1024);
    let record = ring.push_line(LogStream::Stdout, &raw);
    assert!(record.line.is_char_boundary(record.line.len()));
    assert_eq!(
        record.line.len() + record.omitted_bytes,
        total,
        "kept + omitted must equal the input"
    );

    // Pump path: a line longer than the pending buffer, where part of the loss is
    // reported by the reader as `extra_omitted`.
    let pipeline = LogPipeline::new(std::sync::Arc::new(NoopLogSink));
    let session = LaunchSessionId::new("d02-5-session").expect("id");
    pipeline.ensure(&session);
    let max_line = pipeline.max_line_bytes();
    let mut input: &[u8] = raw.as_bytes();
    logs::drain_reader(&mut input, max_line, |line, omitted| {
        pipeline.ingest(&session, LogStream::Stdout, line, omitted);
    });
    let page = pipeline.page(&session, None, 8).expect("page");
    assert_eq!(page.records.len(), 1);
    assert_eq!(
        page.records[0].line.len() + page.records[0].omitted_bytes,
        total,
        "the pump path accounts for every byte exactly once"
    );
}

// ---------------------------------------------------------------------------
// D02-6
// ---------------------------------------------------------------------------

#[test]
fn d02_6_a_paging_store_owns_retention_and_02_keeps_only_a_tail() {
    let fixture = Fixture::with_paging_sink("d02-6-paging", sample_snapshot("1.20.4"));
    fixture.process.set_java(&java_banner(21));
    fixture.process.set_game_output(&bulk_lines(6_000, 100), "");
    fixture.process.exit_game_now(0);

    let task_id = prepare(&fixture);
    fixture.module.join_jobs();

    assert_eq!(fixture.task(&task_id).state, TaskState::Succeeded);
    let session = fixture.session_by_task(&task_id).expect("session");
    let pipeline = fixture.logs();
    assert!(
        pipeline.is_secondary(),
        "a paging sink is the primary store"
    );

    let (records, bytes) = pipeline.retained(&session.id);
    assert!(
        bytes <= logs::DEFAULT_TAIL_BYTES,
        "02 must not keep a second 8 MiB ring: {bytes} bytes / {records} records"
    );
    assert!(records <= logs::DEFAULT_TAIL_RECORDS);
    assert_eq!(
        pipeline.local_capacity(),
        (logs::DEFAULT_TAIL_RECORDS, logs::DEFAULT_TAIL_BYTES)
    );

    let store = fixture.paging_sink.clone().expect("paging sink");
    assert_eq!(store.line_count(), 6_000, "the store received every line");

    // The page is served by the store: a 50-record page starts at sequence 1,
    // while the local tail would start near 5,745.
    let page = completed(
        fixture
            .module
            .handle(&request(
                commands::LAUNCH_LOGS,
                serde_json::json!({ "sessionId": session.id.as_str(), "limit": 50 }),
            ))
            .expect("logs"),
    );
    assert_eq!(page["records"].as_array().map(Vec::len), Some(50));
    assert_eq!(
        page["records"][0]["sequence"],
        serde_json::json!(1),
        "the store's history wins over the tail window"
    );
    assert!(store.page_calls.load(Ordering::Relaxed) >= 1);

    // Without a paging sink 02 is the only store and keeps the full ring.
    let plain = Fixture::new("d02-6-plain", sample_snapshot("1.20.4"));
    assert!(!plain.logs().is_secondary());
    assert_eq!(
        plain.logs().local_capacity(),
        (logs::DEFAULT_MAX_RECORDS, logs::DEFAULT_MAX_BYTES)
    );
}

// ---------------------------------------------------------------------------
// O02-1 (behavioural cross-check through the module surface)
// ---------------------------------------------------------------------------

#[test]
fn o02_1_the_plan_reports_a_single_classpath_pair() {
    let mut snapshot = sample_snapshot("1.20.4");
    snapshot.jvm_args = vec![
        "-cp".to_string(),
        "${classpath}".to_string(),
        "-Xss1M".to_string(),
    ];
    let fixture = Fixture::new("o02-1-cp", snapshot);
    fixture.process.set_java(&java_banner(21));
    fixture.process.exit_game_now(0);

    let task_id = prepare(&fixture);
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Succeeded, "error: {:?}", task.error);
    let result = task.result.expect("result");
    let args: Vec<String> = result["plan"]["args"]
        .as_array()
        .expect("args")
        .iter()
        .map(|value| value.as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        args.iter().filter(|arg| *arg == "-cp").count(),
        1,
        "{args:?}"
    );
    assert!(task
        .evidence
        .iter()
        .any(|marker| marker == evidence::PLAN_FROZEN));
}

#[test]
fn o02_1b_only_a_classpath_flag_claims_the_classpath() {
    // A template that merely reads `${classpath}` (here a `-D` property) must not
    // suppress the `-cp` argument 02 adds (O02-1b).
    let cases: [(&str, Vec<String>); 3] = [
        (
            "o02-1b-property-only",
            vec!["-Dmodpath=${classpath}".to_string(), "-Xss1M".to_string()],
        ),
        (
            "o02-1b-flag",
            vec!["-cp".to_string(), "${classpath}".to_string()],
        ),
        (
            "o02-1b-both",
            vec![
                "-Dmodpath=${classpath}".to_string(),
                "-cp".to_string(),
                "${classpath}".to_string(),
            ],
        ),
    ];

    for (tag, jvm_args) in cases {
        let mut snapshot = sample_snapshot("1.20.4");
        snapshot.jvm_args = jvm_args;
        let fixture = Fixture::new(tag, snapshot);
        fixture.process.set_java(&java_banner(21));
        fixture.process.exit_game_now(0);

        let task_id = prepare(&fixture);
        fixture.module.join_jobs();

        let task = fixture.task(&task_id);
        assert_eq!(task.state, TaskState::Succeeded, "{tag}: {:?}", task.error);
        let result = task.result.expect("result");
        let args: Vec<String> = result["plan"]["args"]
            .as_array()
            .expect("args")
            .iter()
            .map(|value| value.as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            args.iter().filter(|arg| *arg == "-cp").count(),
            1,
            "{tag}: exactly one -cp: {args:?}"
        );
        let cp_index = args.iter().position(|arg| arg == "-cp").expect("-cp");
        assert!(
            args[cp_index + 1].contains("client.jar"),
            "{tag}: the classpath value follows the flag: {args:?}"
        );
    }
}

fn bulk_lines(count: usize, width: usize) -> String {
    let mut out = String::with_capacity(count * (width + 16));
    for index in 0..count {
        out.push_str(&format!("[12:00:00] [main/INFO] line {index:05} "));
        out.push_str(&"x".repeat(width.saturating_sub(32)));
        out.push('\n');
    }
    out
}

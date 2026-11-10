//! 02 · launch plan, argument arrays and the "no files, no process" rule.
//!
//! The instance snapshot is a controlled double (module 01 is not delivered);
//! the plan builder, template expansion, file validation and the refusal
//! ordering are the real implementation.

#[path = "02_support/mod.rs"]
mod support;

use shtmcl_core::launch::plan::RequiredFile;
use shtmcl_core::launch::session::LaunchSessionState;
use shtmcl_core::launch::{commands, evidence};
use shtmcl_core::tasks::EventSink;
use shtmcl_core::CoreModule;
use shtmcl_domain::TaskState;

use support::*;

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

#[test]
fn a_missing_required_file_refuses_the_launch_before_any_process_exists() {
    let fixture = Fixture::new("plan-missing", sample_snapshot("1.20.4"));
    fixture.process.set_java(&java_banner(21));
    fixture
        .files
        .hide("/shtmcl-fixture/instances/one/libraries/gson.jar");

    let task_id = prepare(&fixture);
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Failed);
    let error = task.error.expect("failure envelope");
    assert_eq!(error.code.as_str(), "launch.files.missing");
    assert_eq!(
        error.category,
        shtmcl_domain::ErrorCategory::Storage,
        "a missing file is a local storage problem, not a network one"
    );
    assert!(
        error.detail_ref.unwrap_or_default().contains("gson.jar"),
        "the detail names the missing file"
    );
    assert!(
        task.evidence
            .iter()
            .any(|marker| marker == evidence::REQUIRED_FILES_MISSING),
        "evidence records the refusal: {:?}",
        task.evidence
    );
    assert!(
        fixture.process.game_spawns().is_empty(),
        "no game process may be created when required files are missing"
    );

    let session = fixture.session_by_task(&task_id).expect("session");
    assert_eq!(session.state, LaunchSessionState::Failed);
    assert_eq!(
        session
            .error
            .as_ref()
            .map(|error| error.code.as_str().to_string()),
        Some("launch.files.missing".to_string())
    );
    assert!(session.identity.is_none(), "no process identity exists");
}

#[test]
fn the_plan_uses_argument_arrays_order_and_never_publishes_the_token() {
    let fixture = Fixture::new("plan-order", sample_snapshot("1.20.4"));
    fixture.process.set_java(&java_banner(21));
    fixture.process.exit_game_now(0);
    fixture.process.set_game_output(
        "[12:00:00] [Render thread/INFO]: Backend library: LWJGL\n",
        "",
    );

    let task_id = prepare(&fixture);
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Succeeded, "error: {:?}", task.error);
    let result = task.result.expect("result");
    assert_eq!(result["javaMajor"], serde_json::json!(21));
    assert_eq!(result["sensitiveArgs"], serde_json::json!(1));
    assert!(result["argsDigest"]
        .as_str()
        .unwrap_or_default()
        .starts_with("fnv1a64:"));

    let args: Vec<String> = result["plan"]["args"]
        .as_array()
        .expect("args array")
        .iter()
        .map(|value| value.as_str().unwrap_or_default().to_string())
        .collect();

    // JVM layout: memory, natives, launcher identity, version args, then the
    // classpath pair and the main class.
    assert_eq!(args[0], "-Xmx2048M");
    assert!(args[1].starts_with("-Djava.library.path="));
    assert!(args.contains(&"-cp".to_string()));
    let cp_index = args
        .iter()
        .position(|arg| arg == "-cp")
        .expect("classpath flag");
    assert_eq!(
        args[cp_index + 2],
        "net.minecraft.client.main.Main",
        "the main class follows the classpath"
    );
    assert!(args[cp_index + 1].contains("client.jar"));
    assert!(args[cp_index + 1].contains("gson.jar"));

    // Game arguments keep the metadata order and expand templates.
    let game_start = args
        .iter()
        .position(|arg| arg == "--username")
        .expect("username arg");
    assert_eq!(args[game_start + 1], "Steve");
    assert!(args.contains(&"--gameDir".to_string()));
    assert!(args.contains(&"/shtmcl-fixture/instances/one".to_string()));

    // The credential never appears in the published plan, the task result or
    // the events; only the process argument list carries it.
    let published = serde_json::to_string(&result).expect("json");
    assert!(
        !published.contains("token-value-that-must-not-leak"),
        "the task result must not contain the access token"
    );
    assert!(args.contains(&"<redacted>".to_string()));

    let spawns = fixture.process.game_spawns();
    assert_eq!(spawns.len(), 1, "exactly one game process");
    let spawn = &spawns[0];
    assert_eq!(
        spawn
            .args
            .iter()
            .filter(|arg| *arg == "token-value-that-must-not-leak")
            .count(),
        1,
        "the token is passed as exactly one argument to the game only"
    );
    assert!(
        spawn.environment.is_empty(),
        "no credential is written into the environment"
    );
    assert_eq!(
        spawn.working_dir,
        std::path::PathBuf::from("/shtmcl-fixture/instances/one")
    );
    assert_eq!(
        spawn.program,
        fixture.runtimes().first().expect("runtime").path
    );

    // Every published event also stays credential-free.
    let events = fixture.events.since(
        &shtmcl_domain::OperationId::new("op-launch.prepare").expect("op"),
        None,
    );
    assert!(!events.is_empty(), "the session published events");
    for event in &events {
        let payload = serde_json::to_string(&event.payload).expect("json");
        assert!(!payload.contains("token-value-that-must-not-leak"));
    }
    assert!(events
        .iter()
        .any(|event| event.kind.as_str() == "launch.process.started"));
}

#[test]
fn an_undefined_template_variable_is_a_structured_plan_error() {
    let mut snapshot = sample_snapshot("1.20.4");
    snapshot.game_args.push("--width".to_string());
    snapshot.game_args.push("${window_width}".to_string());
    let fixture = Fixture::new("plan-template", snapshot);
    fixture.process.set_java(&java_banner(21));

    let task_id = prepare(&fixture);
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Failed);
    let error = task.error.expect("failure envelope");
    assert_eq!(error.code.as_str(), "launch.plan.invalid");
    assert!(
        error
            .detail_ref
            .unwrap_or_default()
            .contains("window_width"),
        "the refusal names the undefined variable"
    );
    assert!(fixture.process.game_spawns().is_empty());
}

#[test]
fn an_unparsable_game_version_is_flagged_instead_of_guessed() {
    let fixture = Fixture::new("plan-unknown-version", sample_snapshot("24w14a"));
    fixture.process.set_java(&java_banner(21));
    fixture.process.exit_game_now(0);

    let task_id = prepare(&fixture);
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Succeeded, "error: {:?}", task.error);
    assert!(
        task.evidence
            .iter()
            .any(|marker| marker == evidence::JAVA_REQUIREMENT_UNVERIFIED),
        "an unknown version must be flagged, not silently guessed: {:?}",
        task.evidence
    );
}

#[test]
fn a_java_version_mismatch_refuses_instead_of_spawning() {
    let mut snapshot = sample_snapshot("1.20.4");
    snapshot.required_files = vec![RequiredFile::new(
        "/shtmcl-fixture/instances/one/client.jar",
        shtmcl_core::launch::plan::ArtifactRole::Client,
    )];
    let fixture = Fixture::new("plan-java-mismatch", snapshot);
    // Only Java 8 is available for a version that needs 21.
    fixture.process.set_java(&java_banner(8));

    let task_id = prepare(&fixture);
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Failed);
    let error = task.error.expect("failure envelope");
    assert_eq!(error.code.as_str(), "launch.java.version_mismatch");
    assert_eq!(
        error
            .user_action
            .as_ref()
            .map(|action| action.as_str().to_string()),
        Some("install_java".to_string())
    );
    assert!(
        !error.detail_ref.unwrap_or_default().contains("OpenJDK"),
        "no raw runtime banner reaches the error envelope"
    );
    assert!(fixture.process.game_spawns().is_empty());
}

#[test]
fn a_launch_time_profile_choice_is_not_silently_ignored() {
    let fixture = Fixture::new("plan-profile", sample_snapshot("1.20.4"));
    fixture.process.set_java(&java_banner(21));
    fixture.process.exit_game_now(0);

    let (_, task_id) = accepted(
        fixture
            .module
            .handle(&mutation(
                commands::LAUNCH_PREPARE,
                serde_json::json!({ "instanceId": "instance-1", "profileRef": "perf" }),
            ))
            .expect("prepare"),
    );
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Succeeded, "error: {:?}", task.error);
    let result = task.result.expect("result");
    assert_eq!(
        result["plan"]["profileRef"],
        serde_json::json!("perf"),
        "the launch-time profile choice overrides the snapshot default"
    );
}

#[test]
fn a_pinned_runtime_outside_the_conventional_locations_is_used() {
    let fixture = Fixture::new("plan-pin", sample_snapshot("1.20.4"));
    fixture.process.set_java(&java_banner(21));
    fixture.process.exit_game_now(0);

    // A JDK that no conventional root covers: only the pin points at it.
    let pinned_root = fixture.dirs.root.join("pinned-jdk");
    std::fs::create_dir_all(pinned_root.join("bin")).expect("pin dir");
    let pinned_java = pinned_root.join("bin").join(java_executable());
    std::fs::write(&pinned_java, b"fake").expect("pin java");

    let mut snapshot = sample_snapshot("1.20.4");
    snapshot.java_path = Some(pinned_java.clone());
    *fixture.instances.snapshot.lock().expect("instances") = Some(Ok(snapshot));

    let task_id = prepare(&fixture);
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Succeeded, "error: {:?}", task.error);
    let result = task.result.expect("result");
    assert_eq!(
        result["plan"]["program"].as_str(),
        Some(pinned_java.to_string_lossy().as_ref()),
        "the explicitly pinned runtime wins and is probed even outside the usual roots"
    );
}

#[test]
fn a_token_in_a_jvm_template_is_redacted_everywhere_but_the_process_argv() {
    // D02-2: a credential substituted into a *JVM* property must be treated like
    // one substituted into a game argument.
    let mut snapshot = sample_snapshot("1.20.4");
    snapshot
        .jvm_args
        .push("-Dminecraft.api.accessToken=${auth_access_token}".to_string());
    let fixture = Fixture::new("plan-token-jvm", snapshot);
    fixture.process.set_java(&java_banner(21));
    fixture.process.exit_game_now(0);

    let task_id = prepare(&fixture);
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Succeeded, "error: {:?}", task.error);
    let result = task.result.clone().expect("result");
    assert_eq!(
        result["sensitiveArgs"],
        serde_json::json!(2),
        "both the JVM property and the game argument carry the credential"
    );
    assert!(
        result["plan"]["args"]
            .as_array()
            .expect("args")
            .iter()
            .any(|value| value.as_str() == Some("<redacted>")),
        "the JVM property is redacted in the published argument list"
    );

    // Whole-projection assertion: no serialized surface may contain the token.
    let published = serde_json::to_string(&result).expect("json");
    assert!(
        !published.contains("token-value-that-must-not-leak"),
        "Task.result must not contain the access token: {published}"
    );

    let events = fixture.events.since(
        &shtmcl_domain::OperationId::new("op-launch.prepare").expect("op"),
        None,
    );
    assert!(!events.is_empty());
    for event in &events {
        let payload = serde_json::to_string(&event.payload).expect("json");
        assert!(
            !payload.contains("token-value-that-must-not-leak"),
            "event {} leaked the token: {payload}",
            event.kind
        );
    }

    let session_id = fixture
        .session_by_task(&task_id)
        .expect("session")
        .id
        .as_str()
        .to_string();
    let sessions = serde_json::to_string(
        &fixture
            .module
            .sessions()
            .iter()
            .map(|session| session.to_json())
            .collect::<Vec<_>>(),
    )
    .expect("json");
    assert!(!sessions.contains("token-value-that-must-not-leak"));
    let log_page = completed(
        fixture
            .module
            .handle(&request(
                commands::LAUNCH_LOGS,
                serde_json::json!({ "sessionId": session_id }),
            ))
            .expect("logs"),
    );
    assert!(!serde_json::to_string(&log_page)
        .expect("json")
        .contains("token-value-that-must-not-leak"));

    // The credential still reaches the game exactly once per argument, in argv.
    let spawns = fixture.process.game_spawns();
    assert_eq!(spawns.len(), 1);
    assert_eq!(
        spawns[0]
            .args
            .iter()
            .filter(|arg| arg.contains("token-value-that-must-not-leak"))
            .count(),
        2,
        "argv: {:?}",
        spawns[0].args
    );
    assert!(spawns[0].environment.is_empty());
}

#[test]
fn classpath_is_owned_by_one_side_only() {
    // O02-1: vanilla metadata already ships `-cp ${classpath}`; 02 must not append
    // a second pair.
    let mut snapshot = sample_snapshot("1.20.4");
    snapshot.jvm_args = vec![
        "-cp".to_string(),
        "${classpath}".to_string(),
        "-Xss1M".to_string(),
    ];
    let fixture = Fixture::new("plan-classpath", snapshot);
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
        "exactly one -cp: {args:?}"
    );
    let cp_index = args.iter().position(|arg| arg == "-cp").expect("-cp");
    assert!(args[cp_index + 1].contains("client.jar"));
    assert_eq!(args[cp_index + 2], "-Xss1M", "metadata order is preserved");
}

#[test]
fn a_failing_account_port_blocks_the_launch_with_an_actionable_code() {
    let fixture = Fixture::with_gate("plan-auth", sample_snapshot("1.20.4"), None);
    fixture.process.set_java(&java_banner(21));
    *fixture.accounts.session.lock().expect("accounts") = Err(
        shtmcl_core::launch::errors::account_required("account:missing"),
    );

    let task_id = prepare(&fixture);
    fixture.module.join_jobs();

    let task = fixture.task(&task_id);
    assert_eq!(task.state, TaskState::Failed);
    let error = task.error.expect("failure envelope");
    assert_eq!(error.code.as_str(), "launch.account.required");
    assert_eq!(
        error
            .user_action
            .as_ref()
            .map(|action| action.as_str().to_string()),
        Some("reauthenticate".to_string())
    );
    assert!(fixture.process.game_spawns().is_empty());
}

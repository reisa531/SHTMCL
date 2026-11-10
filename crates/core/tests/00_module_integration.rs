//! Cross-module wiring gate for the application layer (owned by 00).
//!
//! Every delivered module is constructed the way the desktop bridge constructs
//! it — fail-closed ports, `ModuleContext` from one `CoreApp` — and one command
//! per module is dispatched through the single entry point. This is the test
//! that catches a module whose command namespace, constructor or default ports
//! drifted away from the frozen contract; a module's own tests call `handle()`
//! directly and cannot see it.
//!
//! It deliberately does not assert business results that need 01/03/05: the
//! point is the wiring, not the outcome.

use std::sync::Arc;

use shtmcl_core::{CoreApp, CoreModule};
use shtmcl_domain::{
    CommandRequest, ErrorCode, ModuleId, OperationId, RequestId, CONTRACT_VERSION,
};
use shtmcl_platform::PlatformDirs;

/// Build the app exactly the way `apps/desktop/src-tauri` builds it: diagnostics
/// first, its log store handed to the launch module as the single `LogSink`, then
/// all modules registered on one context. If this helper drifts from the bridge,
/// this test stops proving anything about the shipped wiring.
fn app() -> CoreApp {
    let mut app = CoreApp::new(PlatformDirs::under("C:\\shtmcl-integration-root"));
    let context = app.context();

    let diagnostics_module = shtmcl_core::diagnostics::module_with_ports(
        context.clone(),
        shtmcl_core::diagnostics::DiagnosticsPorts::unavailable(),
    );
    let log_sink = diagnostics_module.log_sink();

    let launch = shtmcl_core::launch::module_with(
        context.clone(),
        shtmcl_core::launch::LaunchPorts::unavailable().with_logs(log_sink),
    );
    let launch: Arc<dyn CoreModule> = launch;
    let diagnostics: Arc<dyn CoreModule> = diagnostics_module;

    let modules: Vec<(&str, Arc<dyn CoreModule>)> = vec![
        ("launch", launch),
        ("content", shtmcl_core::content::module(context.clone())),
        ("rooms", shtmcl_core::rooms::module(context.clone())),
        ("diagnostics", diagnostics),
    ];
    for (name, module) in modules {
        app.register(module)
            .unwrap_or_else(|error| panic!("{name} failed to register: {error}"));
    }
    app
}

fn read(
    app: &CoreApp,
    module: ModuleId,
    command: &str,
    payload: serde_json::Value,
) -> Result<shtmcl_domain::CommandResponse, shtmcl_domain::ErrorEnvelope> {
    let mut body = serde_json::Map::new();
    body.insert("command".into(), serde_json::Value::String(command.into()));
    if let serde_json::Value::Object(extra) = payload {
        body.extend(extra);
    }
    let request = CommandRequest {
        request_id: RequestId::new("req-integration").expect("id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload: serde_json::Value::Object(body),
    };
    app.dispatch_read(module, &request)
}

fn mutate(
    app: &CoreApp,
    module: ModuleId,
    command: &str,
    payload: serde_json::Value,
) -> Result<shtmcl_domain::CommandResponse, shtmcl_domain::ErrorEnvelope> {
    let mut body = serde_json::Map::new();
    body.insert("command".into(), serde_json::Value::String(command.into()));
    if let serde_json::Value::Object(extra) = payload {
        body.extend(extra);
    }
    let request = CommandRequest {
        request_id: RequestId::new("req-integration").expect("id"),
        operation_id: Some(OperationId::new("op-integration").expect("id")),
        entity_id: None,
        expected_revision: None,
        payload: serde_json::Value::Object(body),
    };
    app.dispatch_mutation(module, &request)
}

#[test]
fn every_delivered_module_registers_once() {
    let app = app();
    let ids: Vec<ModuleId> = app.module_ids();
    assert_eq!(
        ids,
        vec![
            ModuleId::Launch,
            ModuleId::Content,
            ModuleId::Rooms,
            ModuleId::Diagnostics
        ],
        "the bridge registers exactly these modules today"
    );
    assert!(
        app.snapshot().is_empty(),
        "registration alone creates no task"
    );
}

#[test]
fn the_launch_log_path_runs_through_the_shared_store() {
    let app = app();
    // The bridge hands the diagnostics LogStore to the launch module as its
    // LogSink. No session has been registered here, so a read must answer with a
    // stable module error — not a panic and not an empty page that looks fine.
    let response = read(
        &app,
        ModuleId::Launch,
        "launch.logs",
        serde_json::json!({ "sessionId": "session-missing" }),
    );
    let error = response.expect_err("an unknown session must not resolve to a page");
    assert!(
        error.code.as_str().starts_with("launch."),
        "the log read must be answered by the launch module, got {}",
        error.code
    );
    assert!(
        !error.code.as_str().starts_with("launch.plan"),
        "a log read must not be confused with a plan command: {}",
        error.code
    );
}

#[test]
fn each_module_answers_on_its_own_namespace() {
    let app = app();

    // 02: discovering runtimes is a read that must not need a repository.
    let launch = read(
        &app,
        ModuleId::Launch,
        "launch.runtimes.list",
        serde_json::json!({}),
    );
    assert!(
        launch.is_ok(),
        "launch.runtimes.list must be dispatchable, got {:?}",
        launch.err()
    );

    // 04: the vanilla loader plan works without any injected port.
    let content = read(
        &app,
        ModuleId::Content,
        "content.loader.plan",
        serde_json::json!({ "loader": "vanilla", "gameVersion": "1.20.1" }),
    );
    assert!(
        content.is_ok(),
        "content.loader.plan must be dispatchable, got {:?}",
        content.err()
    );

    // 06: an unknown command inside the rooms namespace must reach the module
    // and come back as a rooms error — that proves routing, without pinning the
    // shape of a specific read.
    let rooms = read(
        &app,
        ModuleId::Rooms,
        "rooms.no_such_command",
        serde_json::json!({}),
    );
    let error = rooms.expect_err("an unknown rooms command must not succeed");
    assert!(
        error.code.as_str().starts_with("rooms."),
        "unknown command must be answered by the rooms module, got {}",
        error.code
    );
    assert!(
        error.code.as_str() != ErrorCode::COMMAND_NAME_MISMATCH
            && error.code.as_str() != ErrorCode::COMMAND_NAME_REQUIRED,
        "the command must be routed, not rejected by the envelope: {}",
        error.code
    );
}

#[test]
fn a_mutation_lands_on_the_callers_operation_stream() {
    let app = app();
    // 06 creates a room as a task; the contract says the task and its events use
    // the caller's operation id, never an invented one.
    let response = mutate(
        &app,
        ModuleId::Rooms,
        "rooms.create",
        serde_json::json!({
            "name": "integration room",
            "visibility": "private",
            "joinPolicy": "approval",
            "requirements": {
                "gameVersion": "1.20.1",
                "loader": "vanilla",
                "authProvider": "offline"
            }
        }),
    );

    match response {
        Ok(shtmcl_domain::CommandResponse::Accepted {
            operation_id,
            task_id,
        }) => {
            assert_eq!(operation_id.as_str(), "op-integration");
            let task = app
                .snapshot()
                .into_iter()
                .find(|task| task.id == task_id)
                .expect("the accepted task must be in the snapshot");
            assert_eq!(task.operation_id.as_str(), "op-integration");
            let cursor = app.event_sink().cursor_after(&operation_id);
            assert!(
                cursor.is_some_and(|cursor| cursor.sequence.get() > 0),
                "accepted work must publish on the caller's operation stream"
            );
        }
        Ok(other) => panic!("rooms.create must be accepted, got {other:?}"),
        Err(error) => {
            // Fail-closed is acceptable here only if it is a rooms code that
            // names a missing dependency, not a namespace or parsing failure.
            let code = error.code.as_str();
            assert!(
                code.starts_with("rooms."),
                "unexpected error from rooms.create: {code}"
            );
            assert!(
                !matches!(
                    code,
                    ErrorCode::COMMAND_NAME_MISMATCH | ErrorCode::COMMAND_NAME_REQUIRED
                ),
                "the command name/namespace must be valid, got {code}"
            );
        }
    }
}

#[test]
fn a_module_cannot_dispatch_a_foreign_command() {
    let app = app();
    let error = read(
        &app,
        ModuleId::Content,
        "launch.runtimes.list",
        serde_json::json!({}),
    )
    .expect_err("content must not answer a launch command");
    assert_eq!(error.code.as_str(), ErrorCode::COMMAND_NAME_MISMATCH);
}

#[test]
fn envelope_rules_come_before_module_lookup() {
    assert_eq!(CONTRACT_VERSION, 1);

    // A request without a command name is rejected by the envelope, even when
    // no module is registered: the name is a transport requirement.
    let unnamed = CommandRequest {
        request_id: RequestId::new("req-1").expect("id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload: serde_json::json!({}),
    };
    let error = CoreApp::new(PlatformDirs::under("C:\\shtmcl-x"))
        .dispatch_read(ModuleId::Launch, &unnamed)
        .expect_err("empty payload has no command name");
    assert_eq!(error.code.as_str(), ErrorCode::COMMAND_NAME_REQUIRED);

    // A named command on an unregistered module reports unavailable.
    let named = CommandRequest {
        payload: serde_json::json!({ "command": "launch.runtimes.list" }),
        ..unnamed
    };
    let error = CoreApp::new(PlatformDirs::under("C:\\shtmcl-x"))
        .dispatch_read(ModuleId::Launch, &named)
        .expect_err("no module is registered in this app");
    assert_eq!(error.code.as_str(), ErrorCode::MODULE_UNAVAILABLE);
}

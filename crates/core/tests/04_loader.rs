//! 04 · loader install descriptions: plan generation, dedupe, ordering, and the
//! `content.loader.plan` command.

use std::sync::Arc;

use shtmcl_core::content::loader::{plan_loader_install, LoaderKind, LoaderRequest};
use shtmcl_core::content::ports::{ContentPorts, FixtureLoaderCatalog};
use shtmcl_core::content::service::module_with_ports;
use shtmcl_core::content::version::VersionRange;
use shtmcl_core::tasks::{InMemoryEvents, InMemoryTasks};
use shtmcl_core::ModuleContext;
use shtmcl_domain::{CommandRequest, CommandResponse, RequestId};
use shtmcl_platform::PlatformDirs;

fn catalog() -> FixtureLoaderCatalog {
    FixtureLoaderCatalog::new()
}

#[test]
fn fabric_plan_merges_duplicate_libraries_and_keeps_a_stable_order() {
    let request = LoaderRequest::new(LoaderKind::Fabric, "1.20.1");
    let first = plan_loader_install(&request, &catalog()).expect("plan");
    let second = plan_loader_install(&request, &catalog()).expect("plan");

    assert_eq!(first, second, "plan generation is deterministic");
    assert_eq!(first.loader_version.as_deref(), Some("0.15.11"));
    assert_eq!(
        first.main_class,
        "net.fabricmc.loader.impl.launch.knot.KnotClient"
    );
    assert!(
        first.merged_libraries >= 1,
        "asm 9.6/9.5 collapse to one coordinate"
    );

    let coordinates: Vec<String> = first
        .libraries
        .iter()
        .map(|library| library.coordinate())
        .collect();
    let mut sorted = coordinates.clone();
    sorted.sort();
    assert_eq!(
        coordinates, sorted,
        "libraries are emitted in coordinate order"
    );
    let asm = first
        .libraries
        .iter()
        .find(|library| library.artifact == "asm")
        .expect("asm library");
    assert_eq!(asm.version, "9.6", "the higher version wins");
    assert_eq!(first.required_java_major, Some(17));
}

#[test]
fn every_loader_kind_produces_a_plan() {
    for kind in LoaderKind::ALL {
        let plan = plan_loader_install(&LoaderRequest::new(kind, "1.20.1"), &catalog())
            .unwrap_or_else(|error| panic!("{kind:?}: {error}"));
        assert_eq!(plan.loader, kind);
        assert!(!plan.main_class.is_empty());
        if kind == LoaderKind::Vanilla {
            assert!(plan.libraries.is_empty());
            assert_eq!(plan.loader_version, None);
        } else {
            assert!(!plan.libraries.is_empty(), "{kind:?} must list libraries");
            assert!(plan.loader_version.is_some());
        }
    }
}

#[test]
fn a_pinned_selector_picks_that_loader_version() {
    let request = LoaderRequest::new(LoaderKind::Fabric, "1.20.1")
        .with_selector(VersionRange::parse("=0.15.0").expect("range"));
    let plan = plan_loader_install(&request, &catalog()).expect("plan");
    assert_eq!(plan.loader_version.as_deref(), Some("0.15.0"));
}

#[test]
fn an_impossible_selector_reports_loader_incompatibility() {
    let request = LoaderRequest::new(LoaderKind::Fabric, "1.20.1")
        .with_selector(VersionRange::parse(">=9.0").expect("range"));
    let error = plan_loader_install(&request, &catalog()).expect_err("no such loader version");
    assert_eq!(error.code(), "content.loader.incompatible");
    assert_eq!(
        error.to_envelope().category,
        shtmcl_domain::ErrorCategory::Unsupported
    );
}

#[test]
fn an_unknown_loader_name_is_rejected_before_any_plan() {
    let error = LoaderKind::parse("liteloader").expect_err("unsupported");
    assert_eq!(error.code(), "content.loader.unsupported");
}

#[test]
fn the_loader_plan_command_answers_immediately() {
    let events = Arc::new(InMemoryEvents::new());
    let tasks: Arc<InMemoryTasks> = Arc::new(InMemoryTasks::new(events.clone()));
    let dirs = PlatformDirs::under(
        std::env::temp_dir().join(format!("shtmcl-04-loader-{}", std::process::id())),
    );
    let context = ModuleContext::new(tasks, events, dirs);
    let module = module_with_ports(context, ContentPorts::fixtures());

    let request = CommandRequest {
        request_id: RequestId::new("req-1").expect("request id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload: serde_json::json!({
            "command": "content.loader.plan",
            "loader": "neoforge",
            "gameVersion": "1.20.1",
        }),
    };
    let response = module.handle(&request).expect("completed response");
    match response {
        CommandResponse::Completed { result } => {
            assert_eq!(result["loader"], "neoforge");
            assert_eq!(result["gameVersion"], "1.20.1");
            assert!(result["libraries"].as_array().expect("libraries").len() >= 2);
            // Builder is deterministic and offline: it names its source.
            assert_eq!(result["metadataSource"], "loader-catalog-port");
            assert!(result["mainClass"]
                .as_str()
                .expect("main class")
                .contains('.'));
        }
        other => panic!("expected a completed response, got {other:?}"),
    }

    // An unsupported loader is a stable envelope, not a panic.
    let bad = CommandRequest {
        payload: serde_json::json!({
            "command": "content.loader.plan",
            "loader": "liteloader",
            "gameVersion": "1.20.1",
        }),
        ..request
    };
    let error = module.handle(&bad).expect_err("unsupported loader");
    assert_eq!(error.code.as_str(), "content.loader.unsupported");

    // An unknown command is reported as an unavailable module action.
    let unknown = CommandRequest {
        request_id: RequestId::new("req-2").expect("request id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload: serde_json::json!({ "command": "content.nope" }),
    };
    let error = module.handle(&unknown).expect_err("unknown command");
    assert_eq!(
        error.code.as_str(),
        shtmcl_domain::ErrorCode::MODULE_UNAVAILABLE
    );
}

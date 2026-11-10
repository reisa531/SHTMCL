use std::sync::Arc;

use shtmcl_core::{CoreModule, ModuleRegistry, RegistryError};
use shtmcl_domain::{
    CommandRequest, CommandResponse, ErrorCode, ErrorEnvelope, ModuleId, RequestId,
};

struct Dummy;

impl CoreModule for Dummy {
    fn id(&self) -> ModuleId {
        ModuleId::Instances
    }

    fn handle(&self, _request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        Ok(CommandResponse::Completed {
            result: serde_json::Value::Null,
        })
    }
}

fn request() -> CommandRequest {
    CommandRequest {
        request_id: RequestId::new("req-1").expect("valid request id"),
        operation_id: None,
        entity_id: None,
        expected_revision: None,
        payload: serde_json::json!({}),
    }
}

#[test]
fn registers_and_dispatches_a_module() {
    let mut registry = ModuleRegistry::new();
    let module: Arc<dyn CoreModule> = Arc::new(Dummy);
    registry.register(module).expect("register");
    assert_eq!(registry.len(), 1);
    assert!(registry.get(ModuleId::Instances).is_some());
    assert!(registry.get(ModuleId::Servers).is_none());

    let response = registry
        .dispatch(ModuleId::Instances, &request())
        .expect("dispatch to registered module");
    assert!(matches!(response, CommandResponse::Completed { .. }));
}

#[test]
fn rejects_duplicate_module_ids() {
    let mut registry = ModuleRegistry::new();
    registry
        .register(Arc::new(Dummy))
        .expect("first registration");
    let duplicate: Arc<dyn CoreModule> = Arc::new(Dummy);
    assert_eq!(
        registry.register(duplicate),
        Err(RegistryError::Duplicate(ModuleId::Instances))
    );
}

#[test]
fn missing_modules_return_a_stable_error() {
    let registry = ModuleRegistry::new();
    let error = registry
        .dispatch(ModuleId::Servers, &request())
        .expect_err("missing module must fail");
    assert_eq!(error.code.as_str(), ErrorCode::MODULE_UNAVAILABLE);
}

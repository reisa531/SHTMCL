//! Recovery routes a new operation through the existing installer adapters.
use serde_json::json;
use shtmcl_core::{CoreModule, ModuleContext};
use shtmcl_domain::{
    CommandKind, CommandRequest, CommandResponse, ErrorCategory, ErrorCode, ErrorEnvelope, ModuleId,
};
use std::sync::Arc;

pub(crate) struct InstallRecovery {
    ctx: ModuleContext,
    delegate: Arc<dyn CoreModule>,
    instances: Option<Arc<dyn CoreModule>>,
}

impl InstallRecovery {
    pub(crate) fn new(
        ctx: ModuleContext,
        delegate: Arc<dyn CoreModule>,
        instances: Option<Arc<dyn CoreModule>>,
    ) -> Self {
        Self {
            ctx,
            delegate,
            instances,
        }
    }
}

pub(crate) fn recovery_error(reason: &str) -> ErrorEnvelope {
    let code = match reason {
        "installation_record_active" | "installation_records_busy" | "recovery_not_stopped" => {
            "launch.install.busy"
        }
        "recovery_inputs_missing" => "launch.install.recovery_unavailable",
        "target_exists" => "launch.install.target_exists",
        _ => "launch.install.recovery_unavailable",
    };
    ErrorEnvelope::new(ErrorCode::from_static(code), ErrorCategory::Conflict, false)
}

impl CoreModule for InstallRecovery {
    fn id(&self) -> ModuleId {
        ModuleId::Launch
    }
    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        if request.command()?.as_str() != "launch.installations.retry" {
            return self.delegate.handle(request);
        }
        request.validate(CommandKind::Mutate)?;
        let (next, _source_lease) =
            crate::install_operations::InstallOperations::new(self.ctx.dirs())
                .retry_request(request, &self.ctx.tasks().snapshots())
                .map_err(|e| recovery_error(&e))?;
        let instances = self
            .instances
            .as_ref()
            .ok_or_else(ErrorEnvelope::module_unavailable)?;
        let mut read = next.clone();
        read.operation_id = None;
        read.entity_id = None;
        read.expected_revision = None;
        read.payload =
            json!({"command":"instances.snapshot","instanceId":next.payload["instanceId"]});
        match instances.handle(&read) {
            Ok(_) => return Err(recovery_error("target_exists")),
            Err(e) if e.code.as_str() == "instances.instance.not_found" => {}
            Err(e) => return Err(e),
        }
        self.delegate.handle(&next)
    }
}

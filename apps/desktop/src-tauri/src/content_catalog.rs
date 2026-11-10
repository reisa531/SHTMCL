//! Desktop wiring for public catalog reads. Installation remains owned by 01/03/04.
use crate::modrinth::{invalid, Modrinth};
use serde_json::json;
use shtmcl_core::{
    content::{loader::GameTarget, ports::ContentPorts},
    CoreModule, ModuleContext,
};
use shtmcl_domain::{CommandKind, CommandRequest, CommandResponse, ErrorEnvelope, ModuleId};
use std::sync::Arc;

pub(crate) struct ContentCatalog {
    inner: Arc<dyn CoreModule>,
    source: Arc<Modrinth>,
    instances: Option<Arc<dyn CoreModule>>,
}
impl ContentCatalog {
    pub(crate) fn new(ctx: ModuleContext, instances: Option<Arc<dyn CoreModule>>) -> Self {
        Self::with_source(ctx, instances, Arc::new(Modrinth::new()))
    }
    pub(crate) fn with_source(
        ctx: ModuleContext,
        instances: Option<Arc<dyn CoreModule>>,
        source: Arc<Modrinth>,
    ) -> Self {
        let mut ports = ContentPorts::unavailable();
        ports.sources = source.clone();
        Self {
            inner: shtmcl_core::content::module_with_ports(ctx, ports),
            source,
            instances,
        }
    }
}
impl CoreModule for ContentCatalog {
    fn id(&self) -> ModuleId {
        ModuleId::Content
    }
    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let command = request.command()?;
        match command.as_str() {
            "content.modrinth.project" | "content.modrinth.versions" => {
                request.validate(CommandKind::Read)?;
                let project = crate::modrinth_catalog::text(&request.payload, "projectId", 128)?;
                let result = if command.as_str() == "content.modrinth.project" {
                    self.source.project(project)?
                } else {
                    // The instance owner supplies the real game/loader selection.
                    // Any caller-supplied target is deliberately ignored.
                    let instance_id =
                        crate::modrinth_catalog::text(&request.payload, "instanceId", 128)?;
                    let instances = self.instances.as_ref().ok_or_else(|| {
                        shtmcl_core::content::error::ContentError::OwnerUnavailable {
                            owner: "instances",
                        }
                        .to_envelope()
                    })?;
                    let mut read = request.clone();
                    read.operation_id = None;
                    read.payload = json!({"command":"instances.snapshot","instanceId":instance_id});
                    let CommandResponse::Completed { result } = instances.handle(&read)? else {
                        return Err(invalid());
                    };
                    if result["instance"]["id"] != instance_id {
                        return Err(invalid());
                    }
                    let target = GameTarget::from_value(&result["instance"]["version"])
                        .map_err(|e| e.to_envelope())?;
                    let offset = match request.payload.get("offset") {
                        None => 0,
                        Some(v) => v.as_u64().filter(|v| *v <= 4096).ok_or_else(invalid)? as usize,
                    };
                    let mut page = self.source.versions(project, &target, offset)?;
                    page["instanceId"] = json!(instance_id);
                    page
                };
                Ok(CommandResponse::Completed { result })
            }
            _ => self.inner.handle(request),
        }
    }
}

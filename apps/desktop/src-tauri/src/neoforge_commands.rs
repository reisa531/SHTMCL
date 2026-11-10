//! Internal preparation command while publication and launch acceptance are pending.
use crate::{
    neoforge_install, runtime_install::RuntimeInstaller,
    version_metadata_source::CachedVersionMetadataSource,
};
use serde_json::json;
use shtmcl_core::{launch::catalog::VersionCatalog, tasks::TaskSpec, CoreModule, ModuleContext};
use shtmcl_domain::{
    CommandKind, CommandRequest, CommandResponse, ErrorCategory, ErrorCode, ErrorEnvelope,
    ModuleId, TaskPhase, TaskState,
};
use shtmcl_platform::StdProcesses;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub(crate) struct NeoForgeCommands {
    ctx: ModuleContext,
    delegate: Arc<dyn CoreModule>,
    runtime: Arc<RuntimeInstaller>,
    metadata: Arc<CachedVersionMetadataSource>,
    catalog: Arc<VersionCatalog>,
    downloads: Arc<dyn CoreModule>,
    processes: Arc<StdProcesses>,
    busy: Arc<AtomicBool>,
    instances: Option<Arc<dyn CoreModule>>,
}

impl NeoForgeCommands {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ctx: ModuleContext,
        delegate: Arc<dyn CoreModule>,
        runtime: Arc<RuntimeInstaller>,
        metadata: Arc<CachedVersionMetadataSource>,
        catalog: Arc<VersionCatalog>,
        downloads: Arc<dyn CoreModule>,
        processes: Arc<StdProcesses>,
        instances: Option<Arc<dyn CoreModule>>,
    ) -> Self {
        Self {
            ctx,
            delegate,
            runtime,
            metadata,
            catalog,
            downloads,
            processes,
            busy: Arc::new(AtomicBool::new(false)),
            instances,
        }
    }
}

impl CoreModule for NeoForgeCommands {
    fn id(&self) -> ModuleId {
        ModuleId::Launch
    }
    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let published = request.command()?.as_str() == "launch.install"
            && request.payload["loader"] == "neoforge";
        if request.command()?.as_str() != "launch.neoforge.prepare" && !published {
            return self.delegate.handle(request);
        }
        request.validate(CommandKind::Mutate)?;
        let selection = if published {
            let id = request.payload["instanceId"]
                .as_str()
                .filter(|id| {
                    !id.is_empty()
                        && id.len() <= 64
                        && id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                })
                .ok_or_else(|| shtmcl_core::launch::errors::request_invalid("instanceId"))?
                .to_string();
            let name = crate::game_install::install_name(&request.payload, &id)?;
            if self.instances.is_none() || request.payload.get("environmentId").is_some() {
                return Err(shtmcl_core::launch::errors::request_invalid(
                    "environmentId",
                ));
            }
            Some((id, name))
        } else {
            None
        };
        if !cfg!(all(windows, target_arch = "x86_64")) {
            return Err(ErrorEnvelope::new(
                ErrorCode::from_static("launch.runtime.unsupported"),
                ErrorCategory::Unsupported,
                false,
            ));
        }
        if request.payload["versionId"] != "1.20.4"
            || request.payload["loaderVersion"] != "20.4.253"
        {
            return Err(shtmcl_core::launch::errors::request_invalid(
                "loaderVersion",
            ));
        }
        let operation = request
            .operation_id
            .clone()
            .ok_or_else(ErrorEnvelope::command_operation_required)?;
        let pinned = crate::install_operations::InstallOperations::new(self.ctx.dirs())
            .source_metadata(&request.payload, "1.20.4")
            .map_err(|_| crate::install_recovery::recovery_error("metadata"))?;
        let source = crate::install_operations::recovery_source(&request.payload)?;
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(ErrorEnvelope::new(
                ErrorCode::from_static("launch.install.busy"),
                ErrorCategory::Conflict,
                false,
            ));
        }
        let task = match self.ctx.tasks().accept(TaskSpec::new(
            operation.clone(),
            TaskPhase::new("neoforge_prepare").unwrap(),
        )) {
            Ok(task) => task,
            Err(error) => {
                self.busy.store(false, Ordering::Release);
                return Err(error);
            }
        };
        let task_id = task.id.clone();
        let record_lease = if let Some((id, name)) = &selection {
            let record = crate::install_operations::InstallOperation::instance(
                operation.clone(),
                task.id.clone(),
                "1.20.4",
                id,
                name,
                self.ctx.dirs(),
            ).with_recovery(crate::install_operations::InstallSelection {
                loader: "neoforge".into(),
                loader_version: Some("20.4.253".into()),
                environment_id: None,
            }).with_source(source);
            match crate::install_operations::InstallOperations::new(self.ctx.dirs()).insert(&record)
            {
                Ok(lease) => Some(lease),
                Err(_) => {
                    self.busy.store(false, Ordering::Release);
                    let error = ErrorEnvelope::internal_unavailable();
                    let _ = self
                        .ctx
                        .tasks()
                        .fail(&task.id, task.attempt_generation, error.clone());
                    return Err(error);
                }
            }
        } else {
            None
        };
        if let Some(raw) = &pinned {
            if crate::install_operations::InstallOperations::new(self.ctx.dirs()).pin_metadata(&task, raw).is_err() {
                self.busy.store(false, Ordering::Release);
                let error = crate::install_recovery::recovery_error("metadata");
                let _ = self.ctx.tasks().fail(&task.id, task.attempt_generation, error.clone());
                return Err(error);
            }
        }
        let instances = self.instances.clone();
        let (ctx, metadata, catalog, runtime, downloads, processes, busy) = (
            self.ctx.clone(),
            self.metadata.clone(),
            self.catalog.clone(),
            self.runtime.clone(),
            self.downloads.clone(),
            self.processes.clone(),
            self.busy.clone(),
        );
        if std::thread::Builder::new()
            .name("neoforge-prepare".into())
            .spawn(move || {
                let _busy = crate::game_install::BusyGuard(busy);
                let _record_lease = record_lease;
                let generation = task.attempt_generation;
                let _ = ctx
                    .tasks()
                    .transition(&task.id, generation, TaskState::Running);
                let result = (|| -> Result<_, String> {
                    let cancelled = || ctx.tasks().is_cancel_requested(&task.id, generation);
                    let _lock = selection.as_ref().map(|(id,_)| crate::neoforge_publication::lock(&ctx,id)).transpose()?;
                    let prepared_result = (|| -> Result<neoforge_install::PreparedInstall, String> {
                        if let Some((id,_)) = &selection {
                            if let Some((root,digest)) = crate::neoforge_publication::recover(&ctx, instances.as_ref().unwrap(),id)? {
                                let verified = crate::neoforge_integrity::verify(&root,&digest,&cancelled)?;
                                return Ok(neoforge_install::PreparedInstall {root,prepared_sha256:digest,verified_files:verified.files.len(),main_class:"cpw.mods.bootstraplauncher.BootstrapLauncher".into()});
                            }
                        }
                        if ctx.tasks().is_cancel_requested(&task.id, generation) {
                            return Err("cancelled".into());
                        }
                        if catalog.list(None, None, 0, 1).is_err() {
                            catalog.refresh().map_err(|_| "catalog_refresh")?;
                        }
                        let detail = metadata
                            .load_verified("1.20.4")?
                            .ok_or("version_metadata_missing")?;
                        if pinned.as_ref().is_some_and(|raw| raw != detail.metadata()) {
                            return Err("recovery_metadata_changed".into());
                        }
                        crate::install_operations::InstallOperations::new(ctx.dirs()).pin_metadata(
                            &ctx.tasks().snapshot(&task.id).ok_or("install_task_missing")?, detail.metadata())?;
                        if ctx.tasks().is_cancel_requested(&task.id, generation) {
                            return Err("cancelled".into());
                        }
                        ctx.tasks()
                            .set_phase(&task.id, generation, TaskPhase::new("runtime").unwrap())
                            .map_err(|_| "phase_update")?;
                        let java =
                            runtime
                                .ensure_runtime(17, &task.id, generation)
                                .map_err(|error| {
                                    if ctx.tasks().is_cancel_requested(&task.id, generation) {
                                        "cancelled".into()
                                    } else {
                                        format!("runtime:{}", error.code)
                                    }
                                })?;
                        let attempt = ctx.dirs().instances.join(format!(
                            ".neoforge-{}",
                            shtmcl_platform::random_hex(16).map_err(|_| "attempt_id")?
                        ));
                        neoforge_install::prepare(
                            &ctx,
                            &task.id,
                            generation,
                            &detail,
                            &downloads,
                            &java,
                            &attempt,
                            processes.as_ref(),
                            processes.as_ref(),
                        )
                    })();
                    if matches!(&prepared_result, Err(error) if error == "cancelled") {
                        if let Some((id, _)) = &selection {
                            let stage = ctx.dirs().instances.join(format!(".install-neo-{id}"));
                            crate::game_install::cleanup_cancelled_stage(&ctx.dirs().instances, &stage)?;
                        }
                    }
                    prepared_result.map(|prepared| (prepared, _lock))
                })();
                match result {
                    Ok((prepared,_lock)) => {
                        let publication = match &selection {
                            Some((id,name)) => crate::neoforge_publication::publish(&ctx, instances.as_ref().unwrap(), id,name,&prepared.root,&prepared.prepared_sha256,&|| ctx.tasks().is_cancel_requested(&task.id,generation))
                                .map(|path| json!({"instanceId":id,"versionId":"1.20.4","loader":"neoforge","loaderVersion":"20.4.253","path":path,"installed":true,"launchReady":false})),
                            None => Ok(json!({"prepared":true,"path":prepared.root,"versionId":"1.20.4",
                                "preparedSha256":prepared.prepared_sha256,"verifiedFiles":prepared.verified_files,
                                "mainClass":prepared.main_class,"loaderVersion":"20.4.253","installed":false,"launchReady":false}))
                        };
                        match publication {
                            Ok(result) => { let _ = ctx.tasks().succeed(
                            &task.id,
                            generation,
                            Some(result),
                        ); },
                            Err(error) if error == "cancelled" => { let _ = ctx.tasks().cancel_now(&task.id,generation); },
                            Err(error) => { let _ = ctx.tasks().fail(&task.id,generation,ErrorEnvelope::new(ErrorCode::from_static("launch.install.failed"),ErrorCategory::Internal,false).with_detail_ref(format!("neoforge:{error}"))); }
                        }
                    }
                    Err(error) if error == "cancelled" => {
                        let _ = ctx.tasks().cancel_now(&task.id, generation);
                    }
                    Err(error) => {
                        let _ = ctx.tasks().fail(
                            &task.id,
                            generation,
                            ErrorEnvelope::new(
                                ErrorCode::from_static("launch.install.failed"),
                                ErrorCategory::Internal,
                                false,
                            )
                            .with_detail_ref(format!("neoforge:{error}")),
                        );
                    }
                }
                if selection.is_some() { let _ = crate::game_install::record_execution(&ctx,&task.id); }
            })
            .is_err()
        {
            self.busy.store(false, Ordering::Release);
            let _ = self.ctx.tasks().fail(
                &task_id,
                task.attempt_generation,
                ErrorEnvelope::internal_unavailable(),
            );
            return Err(ErrorEnvelope::internal_unavailable());
        }
        Ok(CommandResponse::Accepted {
            operation_id: operation,
            task_id,
        })
    }
}

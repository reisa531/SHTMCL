//! One user action prepares the managed runtime, then delegates process ownership
//! to module 02. All work uses the existing task table and cancellation service.
use crate::runtime_install::RuntimeInstaller;
use serde_json::json;
use shtmcl_core::{
    launch::{java::JavaRequirement, ports::InstanceSnapshotSource},
    tasks::TaskSpec,
    CoreModule, ModuleContext,
};
use shtmcl_domain::{
    CommandKind, CommandRequest, CommandResponse, ErrorEnvelope, InstanceId, ModuleId, OperationId,
    TaskPhase, TaskState,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub(crate) struct GameLauncher {
    ctx: ModuleContext,
    runtime: Arc<RuntimeInstaller>,
    instances: Option<Arc<dyn InstanceSnapshotSource>>,
    busy: Arc<AtomicBool>,
}

impl GameLauncher {
    pub(crate) fn new(
        ctx: ModuleContext,
        runtime: Arc<RuntimeInstaller>,
        instances: Option<Arc<dyn InstanceSnapshotSource>>,
    ) -> Self {
        Self {
            ctx,
            runtime,
            instances,
            busy: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct Busy(Arc<AtomicBool>);
impl Drop for Busy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl CoreModule for GameLauncher {
    fn id(&self) -> ModuleId {
        ModuleId::Launch
    }
    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        if request.command()?.as_str() != "launch.game.start" {
            return self.runtime.handle(request);
        }
        request.validate(CommandKind::Mutate)?;
        let instance_id =
            InstanceId::new(request.payload["instanceId"].as_str().unwrap_or_default())
                .map_err(|_| shtmcl_core::launch::errors::request_invalid("instanceId"))?;
        let source = self
            .instances
            .clone()
            .ok_or_else(ErrorEnvelope::internal_unavailable)?;
        let operation_id = request
            .operation_id
            .clone()
            .ok_or_else(ErrorEnvelope::command_operation_required)?;
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(shtmcl_core::launch::errors::request_invalid(
                "launch_preparation_busy",
            ));
        }
        let guard = Busy(self.busy.clone());
        let task = self.ctx.tasks().accept(
            TaskSpec::new(
                operation_id.clone(),
                TaskPhase::new("runtime_metadata").unwrap(),
            )
            .with_instance(Some(instance_id.clone())),
        )?;
        let (task_id, generation) = (task.id, task.attempt_generation);
        let (ctx, runtime, job_id) = (self.ctx.clone(), self.runtime.clone(), task_id.clone());
        let mut prepare = request.clone();
        prepare.payload["command"] = json!("launch.prepare");
        prepare.operation_id = Some(
            OperationId::new(format!("game-start-{task_id}"))
                .map_err(|_| ErrorEnvelope::internal_unavailable())?,
        );
        let spawn = std::thread::Builder::new()
            .name("game-runtime-prepare".into())
            .spawn(move || {
                let _guard = guard;
                let _ = ctx
                    .tasks()
                    .transition(&job_id, generation, TaskState::Running);
                let result = (|| {
                    let snapshot = source.preparation(&instance_id)?;
                    if let Some(expected) = prepare.expected_revision {
                        if expected != snapshot.revision {
                            return Err(shtmcl_core::launch::errors::instance_stale_revision(
                                expected.get(),
                                snapshot.revision.get(),
                            ));
                        }
                    }
                    prepare.expected_revision = Some(snapshot.revision);
                    prepare.payload["expectedOfflineUsername"] = json!(snapshot.offline_username);
                    let major = match snapshot
                        .java_requirement
                        .unwrap_or(JavaRequirement::Exact(8))
                    {
                        JavaRequirement::Exact(major) | JavaRequirement::AtLeast(major) => major,
                        JavaRequirement::Range { min, .. } => min,
                        JavaRequirement::Any => 8,
                    };
                    if snapshot.java_path.is_none() {
                        runtime.ensure_runtime(major, &job_id, generation)?;
                    }
                    if ctx.tasks().is_cancel_requested(&job_id, generation) {
                        return Ok(None);
                    }
                    let _ = ctx.tasks().set_phase(
                        &job_id,
                        generation,
                        TaskPhase::new("launch_prepare").unwrap(),
                    );
                    let child = match runtime.handle(&prepare)? {
                        CommandResponse::Accepted { task_id, .. } => task_id,
                        _ => return Err(ErrorEnvelope::internal_unavailable()),
                    };
                    // Preparation now includes verified resource repair. Use the
                    // installation budget so a real download can finish on a
                    // slower connection; cancellation still propagates below.
                    let deadline = Instant::now() + Duration::from_secs(20 * 60);
                    loop {
                        let child_task = ctx
                            .tasks()
                            .snapshot(&child)
                            .ok_or_else(ErrorEnvelope::internal_unavailable)?;
                        match child_task.state {
                            TaskState::Succeeded => {
                                return child_task
                                    .result
                                    .map(Some)
                                    .ok_or_else(ErrorEnvelope::internal_unavailable)
                            }
                            TaskState::Cancelled
                                if ctx.tasks().is_cancel_requested(&job_id, generation) =>
                            {
                                return Ok(None)
                            }
                            TaskState::Cancelled => {
                                return Err(ErrorEnvelope::internal_unavailable())
                            }
                            TaskState::Failed => {
                                return Err(child_task
                                    .error
                                    .unwrap_or_else(ErrorEnvelope::internal_unavailable))
                            }
                            _ => {}
                        }
                        if ctx.tasks().is_cancel_requested(&job_id, generation)
                            || Instant::now() > deadline
                        {
                            ctx.tasks()
                                .request_cancel(&shtmcl_domain::CancelTaskRequest {
                                    task_id: child.clone(),
                                    attempt_generation: child_task.attempt_generation,
                                });
                        }
                        if Instant::now() > deadline + Duration::from_secs(5) {
                            return Err(ErrorEnvelope::internal_unavailable());
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                })();
                match result {
                    Ok(Some(result)) => {
                        let _ = ctx.tasks().succeed(&job_id, generation, Some(result));
                    }
                    Ok(None) => {
                        let _ = ctx.tasks().cancel_now(&job_id, generation);
                    }
                    Err(error) if ctx.tasks().is_cancel_requested(&job_id, generation) => {
                        let _ = ctx.tasks().cancel_now(&job_id, generation);
                        drop(error);
                    }
                    Err(error) => {
                        let _ = ctx.tasks().fail(&job_id, generation, error);
                    }
                }
            });
        if spawn.is_err() {
            let _ =
                self.ctx
                    .tasks()
                    .fail(&task_id, generation, ErrorEnvelope::internal_unavailable());
            return Err(ErrorEnvelope::internal_unavailable());
        }
        Ok(CommandResponse::Accepted {
            operation_id,
            task_id,
        })
    }
}

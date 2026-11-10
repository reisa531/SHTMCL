//! `CoreModule` implementation: command routing for module 04.
//!
//! Routing: the command name travels in `payload.command` (frozen
//! `CommandName::PAYLOAD_KEY`); the dispatch layer also checks that its first
//! segment is this module's id. `TaskSpec.operation_id` is always the caller's
//! `operationId`, so task events land on the caller's stream.
//!
//! Reads answer immediately; mutations validate, `accept` a task through the
//! shared `TaskService`, run the work on the core thread pool (a plain thread here)
//! and report progress/result only through the task port.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use shtmcl_domain::{
    AttemptGeneration, CommandKind, CommandRequest, CommandResponse, EntityId, ErrorEnvelope,
    InstanceId, ModuleId, OperationId, Revision, TaskId, TaskPhase,
};

use crate::content::MODULE;
use crate::tasks::TaskSpec;
use crate::{CoreModule, ModuleContext};

use super::error::ContentError;
use super::install::{InstallOutcome, InstallProgress, InstallRunner, TaskProgressAdapter, phases};
use super::loader::{GameTarget, LoaderKind, LoaderRequest, plan_loader_install};
use super::modpack::{
    ExportCandidate, Hashes, PackFormat, SideSupport, build_modrinth_index, parse_modrinth_index,
    parse_modrinth_index_value, plan_export,
};
use super::plan::{ChangePlan, ContentLock, plan_changes};
use super::ports::{ContentPorts, SourceKind, SourceQuery};
use super::resolve::{SolveBudget, SolveRequest, candidate_from_value, solve};
use super::version::VersionRange;

/// Actions understood by this module. `payload.command` values are stable and
/// registered in the handoff.
pub mod actions {
    pub const LOADER_PLAN: &str = "content.loader.plan";
    pub const SOLVE: &str = "content.solve";
    pub const SOLVE_TASK: &str = "content.solve.task";
    pub const SEARCH: &str = "content.search";
    pub const INSPECT: &str = "content.packs.inspect";
    pub const PLAN: &str = "content.plan";
    pub const APPLY: &str = "content.apply";
    pub const ROLLBACK: &str = "content.rollback";
    pub const RESUME: &str = "content.recovery.resume";
    pub const EXPORT: &str = "content.packs.export";
}

/// Module-defined event kinds published through `EventSink`.
pub mod events {
    /// A change plan was built and is ready for confirmation.
    pub const PLAN_READY: &str = "content.plan_ready";
    /// The instance content version changed (carries the new revision).
    pub const CHANGED: &str = "content.changed";
}

pub struct ContentModule {
    ctx: ModuleContext,
    ports: ContentPorts,
    /// Plans produced by `content.plan` and consumed by `content.apply`. Bounded:
    /// only the most recent `MAX_RETAINED_PLANS` are kept, oldest evicted first.
    plans: Arc<Mutex<PlanRegistry>>,
    counter: Arc<AtomicU64>,
}

/// Bounded plan registry. A plan is only valid for the instance revision it was
/// computed from, so keeping an unbounded history is a memory leak with no user
/// value; an evicted plan is reported as expired instead of being applied stale.
pub const MAX_RETAINED_PLANS: usize = 32;

#[derive(Default)]
pub struct PlanRegistry {
    order: std::collections::VecDeque<String>,
    plans: BTreeMap<String, ChangePlan>,
}

impl PlanRegistry {
    pub fn insert(&mut self, plan: ChangePlan) {
        let plan_id = plan.plan_id.clone();
        if self.plans.insert(plan_id.clone(), plan).is_none() {
            self.order.push_back(plan_id);
        }
        while self.order.len() > MAX_RETAINED_PLANS {
            if let Some(oldest) = self.order.pop_front() {
                self.plans.remove(&oldest);
            }
        }
    }

    pub fn get(&self, plan_id: &str) -> Option<&ChangePlan> {
        self.plans.get(plan_id)
    }

    pub fn len(&self) -> usize {
        self.plans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plans.is_empty()
    }
}

impl ContentModule {
    pub fn new(ctx: ModuleContext, ports: ContentPorts) -> Self {
        Self {
            ctx,
            ports,
            plans: Arc::new(Mutex::new(PlanRegistry::default())),
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn ports(&self) -> &ContentPorts {
        &self.ports
    }

    fn next_id(&self, prefix: &str) -> String {
        format!(
            "{prefix}-{}",
            self.counter.fetch_add(1, Ordering::Relaxed) + 1
        )
    }

    fn instance_id(&self, request: &CommandRequest) -> Result<InstanceId, ErrorEnvelope> {
        let raw = request
            .payload
            .get("instanceId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| request.entity_id.as_ref().map(|id| id.as_str().to_string()));
        let raw = raw.ok_or_else(|| {
            ContentError::InvalidPlan {
                detail: "instanceId is required".to_string(),
            }
            .to_envelope()
        })?;
        InstanceId::new(raw).map_err(|error| {
            ContentError::InvalidPlan {
                detail: error.to_string(),
            }
            .to_envelope()
        })
    }

    /// Accept a task on the caller's operation stream.
    ///
    /// `TaskSpec.operation_id` **must** be the request's `operationId` (frozen
    /// contract): a module that invents its own id puts its events in a stream the
    /// caller never subscribed to, so `get_events(operationId)` returns nothing.
    fn start_task(
        &self,
        request: &CommandRequest,
        phase: &str,
        instance_id: Option<InstanceId>,
        input_revision: Option<Revision>,
    ) -> Result<(shtmcl_domain::Task, OperationId), ErrorEnvelope> {
        let operation_id = match &request.operation_id {
            Some(operation_id) => operation_id.clone(),
            // Mutations always carry one (validated above); be explicit rather than
            // fabricating an id.
            None => return Err(ErrorEnvelope::command_operation_required()),
        };
        let phase = TaskPhase::new(phase).map_err(|error| {
            ContentError::Internal {
                detail: error.to_string(),
            }
            .to_envelope()
        })?;
        let task = self
            .ctx
            .tasks()
            .accept(
                TaskSpec::new(operation_id.clone(), phase)
                    .with_instance(instance_id)
                    .with_input_revision(input_revision),
            )
            .map_err(|error| error)?;
        Ok((task, operation_id))
    }

    // -- reads -------------------------------------------------------------

    fn loader_plan(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Read)?;
        let payload = &request.payload;
        let kind = LoaderKind::parse(
            payload
                .get("loader")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("loader is required"))?,
        )
        .map_err(|error| error.to_envelope())?;
        let game_version = payload
            .get("gameVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("gameVersion is required"))?;
        let mut loader_request = LoaderRequest::new(kind, game_version);
        if let Some(selector) = payload.get("selector").and_then(Value::as_str) {
            loader_request = loader_request
                .with_selector(VersionRange::parse(selector).map_err(|error| error.to_envelope())?);
        }
        let plan = plan_loader_install(&loader_request, self.ports.catalog.as_ref())
            .map_err(|error| error.to_envelope())?;
        Ok(CommandResponse::Completed {
            result: plan.to_value(),
        })
    }

    /// Synchronous solve. The budget is clamped for a read: a `content.solve` call
    /// must answer quickly and never hold the dispatch thread. Larger problems go
    /// through `content.solve.task`.
    fn solve_once(&self, payload: &Value) -> Result<Value, ContentError> {
        let solve_request = solve_request_from_value(payload, SolveBudget::clamped_for_read)?;
        Ok(solve(&solve_request, &|| false).to_value())
    }

    fn search(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Read)?;
        let payload = &request.payload;
        let query_id = payload
            .get("queryId")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("queryId is required"))?
            .to_string();
        let provider = match payload.get("provider").and_then(Value::as_str) {
            Some(value) => Some(SourceKind::parse(value).map_err(|error| error.to_envelope())?),
            None => None,
        };
        let target = match payload.get("target") {
            Some(value) => {
                Some(GameTarget::from_value(value).map_err(|error| error.to_envelope())?)
            }
            None => None,
        };
        let query = SourceQuery {
            query_id: query_id.clone(),
            provider,
            text: payload
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            target,
            limit: payload.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize,
        };
        let page = self
            .ports
            .sources
            .search(&query)
            .map_err(|error| error.to_envelope())?;
        let identities: Vec<Value> = SourceKind::ALL
            .iter()
            .map(|kind| self.ports.sources.identity(*kind).to_value())
            .collect();
        Ok(CommandResponse::Completed {
            result: json!({
                "queryId": page.query_id,
                "connected": page.connected,
                "projects": page
                    .projects
                    .iter()
                    .map(|project| json!({
                        "provider": project.provider.as_str(),
                        "projectId": project.project_id,
                        "slug": project.slug,
                        "title": project.title,
                    }))
                    .collect::<Vec<_>>(),
                "identities": identities,
            }),
        })
    }

    /// `content.packs.inspect` accepts either an inline manifest (`manifest`) or a
    /// pack file on disk (`path`). The path form is the product route: it opens the
    /// archive through the same hardened reader the installer uses, including entry
    /// verification.
    fn inspect(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Read)?;
        let payload = &request.payload;

        if let Some(path) = payload.get("path").and_then(Value::as_str) {
            let manifest = read_pack_file(path).map_err(|error| error.to_envelope())?;
            return Ok(CommandResponse::Completed {
                result: manifest.summary_value(),
            });
        }

        let manifest = match payload.get("manifest") {
            Some(Value::String(text)) => {
                parse_modrinth_index(text).map_err(|error| error.to_envelope())?
            }
            Some(value @ Value::Object(_)) => {
                parse_modrinth_index_value(value).map_err(|error| error.to_envelope())?
            }
            _ => return Err(invalid("manifest or path is required")),
        };
        Ok(CommandResponse::Completed {
            result: manifest.summary_value(),
        })
    }

    // -- mutations ---------------------------------------------------------

    fn start_plan(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let lock = ContentLock::from_value(
            request
                .payload
                .get("lock")
                .ok_or_else(|| invalid("lock is required"))?,
        )
        .map_err(|error| error.to_envelope())?;
        let instance_id = self.instance_id(request)?;
        let expected = request.expected_revision;
        let (task, operation_id) =
            self.start_task(request, phases::PLAN, Some(instance_id.clone()), expected)?;

        let tasks = self.ctx.tasks().clone();
        let events = self.ctx.events().clone();
        let ports = self.ports.clone();
        let plans = self.plans.clone();
        let counter = self.counter.clone();
        let instance = instance_id.clone();
        let task_id = task.id.clone();
        let generation = task.attempt_generation;
        let operation = operation_id.clone();
        std::thread::spawn(move || {
            let progress = TaskProgressAdapter::new(tasks.clone(), task_id.clone(), generation);
            let outcome = (|| -> Result<Value, ContentError> {
                progress.enter_phase(phases::PLAN)?;
                let view = ports.instance.view(&instance)?;
                if let Some(expected) = expected {
                    if expected != view.revision {
                        return Err(ContentError::PlanConflict {
                            expected: expected.get(),
                            actual: view.revision.get(),
                        });
                    }
                }
                let plan_id = format!("plan-{}", counter.fetch_add(1, Ordering::Relaxed) + 1);
                let plan = plan_changes(plan_id, &view, &lock)?;
                progress.advance(1, 1)?;
                publish(
                    &events,
                    &operation,
                    generation,
                    events_kind(events::PLAN_READY),
                    Some(&instance),
                    view.revision,
                    json!({
                        "planId": plan.plan_id,
                        "planDigest": plan.plan_digest,
                        "plan": plan.to_value(),
                    }),
                );
                if let Ok(mut registry) = plans.lock() {
                    registry.insert(plan.clone());
                }
                Ok(plan.to_value())
            })();
            finish_task(&tasks, &task_id, generation, outcome);
        });

        Ok(CommandResponse::Accepted {
            operation_id,
            task_id: task.id,
        })
    }

    fn start_apply(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let plan_id = request
            .payload
            .get("planId")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("planId is required"))?
            .to_string();
        let plan = {
            let registry = self
                .plans
                .lock()
                .map_err(|_| ErrorEnvelope::internal_unavailable())?;
            registry.get(&plan_id).cloned()
        }
        .ok_or_else(|| {
            ContentError::InvalidPlan {
                detail: format!("unknown plan {plan_id}"),
            }
            .to_envelope()
        })?;
        if let Some(digest) = request.payload.get("planDigest").and_then(Value::as_str) {
            if digest != plan.plan_digest {
                return Err(ContentError::InvalidPlan {
                    detail: "plan digest changed; recompute the plan".to_string(),
                }
                .to_envelope());
            }
        }
        let (task, operation_id) = self.start_task(
            request,
            phases::PLAN,
            Some(plan.instance_id.clone()),
            Some(plan.base_revision),
        )?;

        let tasks = self.ctx.tasks().clone();
        let events = self.ctx.events().clone();
        let ports = self.ports.clone();
        let task_id = task.id.clone();
        let generation = task.attempt_generation;
        let operation = operation_id.clone();
        let instance = plan.instance_id.clone();
        std::thread::spawn(move || {
            let progress = TaskProgressAdapter::new(tasks.clone(), task_id.clone(), generation);
            let runner = InstallRunner::new(&ports, &progress);
            let outcome = ports
                .instance
                .view(&instance)
                .and_then(|view| runner.apply(&plan, &view));
            match outcome {
                Ok(report) => {
                    if report.is_cancelled() {
                        let _ = tasks.cancel_now(&task_id, generation);
                    } else {
                        if let InstallOutcome::Installed { revision, .. } = &report {
                            publish(
                                &events,
                                &operation,
                                generation,
                                events_kind(events::CHANGED),
                                Some(&instance),
                                *revision,
                                json!({ "outcome": report.to_value() }),
                            );
                        }
                        let _ = tasks.succeed(&task_id, generation, Some(report.to_value()));
                    }
                }
                Err(error) => {
                    let _ = tasks.fail(&task_id, generation, error.to_envelope());
                }
            }
        });

        Ok(CommandResponse::Accepted {
            operation_id,
            task_id: task.id,
        })
    }

    fn start_rollback(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let instance_id = self.instance_id(request)?;
        let plan_id = request
            .payload
            .get("planId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| self.next_id("rollback"));
        let (task, operation_id) =
            self.start_task(request, phases::ROLLBACK, Some(instance_id.clone()), None)?;

        let tasks = self.ctx.tasks().clone();
        let events = self.ctx.events().clone();
        let ports = self.ports.clone();
        let task_id = task.id.clone();
        let generation = task.attempt_generation;
        let operation = operation_id.clone();
        let instance = instance_id.clone();
        std::thread::spawn(move || {
            let progress = TaskProgressAdapter::new(tasks.clone(), task_id.clone(), generation);
            let runner = InstallRunner::new(&ports, &progress);
            let outcome = ports
                .instance
                .view(&instance)
                .and_then(|view| runner.rollback(&plan_id, &view));
            match outcome {
                Ok(report) => {
                    if report.is_cancelled() {
                        let _ = tasks.cancel_now(&task_id, generation);
                    } else {
                        if let InstallOutcome::RestoredPrevious { revision, .. } = &report {
                            publish(
                                &events,
                                &operation,
                                generation,
                                events_kind(events::CHANGED),
                                Some(&instance),
                                *revision,
                                json!({ "outcome": report.to_value() }),
                            );
                        }
                        let _ = tasks.succeed(&task_id, generation, Some(report.to_value()));
                    }
                }
                Err(error) => {
                    let _ = tasks.fail(&task_id, generation, error.to_envelope());
                }
            }
        });

        Ok(CommandResponse::Accepted {
            operation_id,
            task_id: task.id,
        })
    }

    fn start_resume(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let instance_id = self.instance_id(request)?;
        let (task, operation_id) =
            self.start_task(request, phases::RESUME, Some(instance_id.clone()), None)?;

        let tasks = self.ctx.tasks().clone();
        let events = self.ctx.events().clone();
        let ports = self.ports.clone();
        let task_id = task.id.clone();
        let generation = task.attempt_generation;
        let operation = operation_id.clone();
        let instance = instance_id.clone();
        std::thread::spawn(move || {
            let progress = TaskProgressAdapter::new(tasks.clone(), task_id.clone(), generation);
            let runner = InstallRunner::new(&ports, &progress);
            let outcome = ports
                .instance
                .view(&instance)
                .and_then(|view| runner.resume(&view));
            match outcome {
                Ok(report) => {
                    if report.is_cancelled() {
                        let _ = tasks.cancel_now(&task_id, generation);
                    } else {
                        // A resume that finalized a landed commit is an entity change too.
                        if let InstallOutcome::Resumed {
                            status: crate::content::install::ResumeStatus::FinalizedCommit,
                        } = &report
                        {
                            if let Ok(view) = ports.instance.view(&instance) {
                                publish(
                                    &events,
                                    &operation,
                                    generation,
                                    events_kind(events::CHANGED),
                                    Some(&instance),
                                    view.revision,
                                    json!({ "outcome": report.to_value() }),
                                );
                            }
                        }
                        let _ = tasks.succeed(&task_id, generation, Some(report.to_value()));
                    }
                }
                Err(error) => {
                    let _ = tasks.fail(&task_id, generation, error.to_envelope());
                }
            }
        });

        Ok(CommandResponse::Accepted {
            operation_id,
            task_id: task.id,
        })
    }

    fn start_export(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let payload = &request.payload;
        let format = PackFormat::parse(
            payload
                .get("format")
                .and_then(Value::as_str)
                .unwrap_or("mrpack"),
        )
        .map_err(|error| error.to_envelope())?;
        let name = payload
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("name is required"))?;
        let version_id = payload
            .get("versionId")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("versionId is required"))?;
        let target = GameTarget::from_value(
            payload
                .get("target")
                .ok_or_else(|| invalid("target is required"))?,
        )
        .map_err(|error| error.to_envelope())?;
        let files = payload
            .get("files")
            .and_then(Value::as_array)
            .map(|files| {
                files
                    .iter()
                    .map(export_candidate)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .map_err(|error| error.to_envelope())?
            .unwrap_or_default();
        let include_saves = payload
            .get("includeSaves")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let spec = plan_export(format, name, version_id, target, files, include_saves)
            .map_err(|error| error.to_envelope())?;
        let document = build_modrinth_index(&spec).map_err(|error| error.to_envelope())?;

        let (task, operation_id) = self.start_task(request, phases::FINALIZE, None, None)?;
        let tasks = self.ctx.tasks().clone();
        let task_id = task.id.clone();
        let generation = task.attempt_generation;
        std::thread::spawn(move || {
            let progress = TaskProgressAdapter::new(tasks.clone(), task_id.clone(), generation);
            let outcome = (|| -> Result<Value, ContentError> {
                progress.enter_phase(phases::FINALIZE)?;
                progress.advance(1, 1)?;
                Ok(json!({ "spec": spec.to_value(), "index": document }))
            })();
            finish_task(&tasks, &task_id, generation, outcome);
        });

        Ok(CommandResponse::Accepted {
            operation_id,
            task_id: task.id,
        })
    }
}

impl CoreModule for ContentModule {
    fn id(&self) -> ModuleId {
        MODULE
    }

    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        // `payload.command` is the frozen routing key (`CommandName::PAYLOAD_KEY`).
        // There is deliberately no alias: one key, one truth.
        let command = request.command()?;
        match command.as_str() {
            actions::LOADER_PLAN => self.loader_plan(request),
            actions::SOLVE => {
                request.validate(CommandKind::Read)?;
                let result = self
                    .solve_once(&request.payload)
                    .map_err(|e| e.to_envelope())?;
                Ok(CommandResponse::Completed { result })
            }
            actions::SOLVE_TASK => self.start_solve_task(request),
            actions::SEARCH => self.search(request),
            actions::INSPECT => self.inspect(request),
            actions::PLAN => self.start_plan(request),
            actions::APPLY => self.start_apply(request),
            actions::ROLLBACK => self.start_rollback(request),
            actions::RESUME => self.start_resume(request),
            actions::EXPORT => self.start_export(request),
            _ => Err(ErrorEnvelope::module_unavailable()),
        }
    }
}

impl ContentModule {
    /// Cancellable solver task: the budget stops the search, and a cancellation
    /// request is observed on every state.
    fn start_solve_task(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let payload = request.payload.clone();
        let (task, operation_id) = self.start_task(request, phases::PLAN, None, None)?;
        let tasks = self.ctx.tasks().clone();
        let task_id = task.id.clone();
        let generation = task.attempt_generation;
        std::thread::spawn(move || {
            let progress = TaskProgressAdapter::new(tasks.clone(), task_id.clone(), generation);
            let outcome = (|| -> Result<Value, ContentError> {
                progress.enter_phase(phases::PLAN)?;
                // A task may search harder than a read, but never beyond the
                // documented envelope.
                let solve_request = solve_request_from_value(&payload, SolveBudget::clamped)?;
                let cancel = || progress.cancel_requested();
                let result = solve(&solve_request, &cancel);
                let value = result.to_value();
                progress.advance(1, 1)?;
                // A non-solution is a failure with a stable code, not a success: the
                // UI must not treat "no candidate" as "installed". The structured
                // detail stays available through the read command.
                match &result.outcome {
                    super::resolve::SolveOutcome::Solved { .. } => Ok(value),
                    super::resolve::SolveOutcome::Unsatisfiable { .. } => {
                        Err(ContentError::Unsatisfiable {
                            detail: format!(
                                "{} conflicting project(s) after {} states",
                                result.conflicts_len(),
                                result.states_visited
                            ),
                        })
                    }
                    super::resolve::SolveOutcome::BudgetExhausted { limit, used_states } => {
                        Err(ContentError::SolveBudgetExhausted {
                            states: *used_states,
                            limit: limit.as_str(),
                        })
                    }
                    super::resolve::SolveOutcome::Cancelled { .. } => {
                        // Cancellation confirmation happens through the task port.
                        Ok(value)
                    }
                }
            })();
            finish_task(&tasks, &task_id, generation, outcome);
        });
        Ok(CommandResponse::Accepted {
            operation_id,
            task_id: task.id,
        })
    }
}

/// Parse the shared solve payload (`target`, `roots`, `candidates`, `budget`).
fn solve_request_from_value(
    payload: &Value,
    clamp: fn(SolveBudget) -> SolveBudget,
) -> Result<SolveRequest, ContentError> {
    let target = GameTarget::from_value(payload.get("target").ok_or_else(|| {
        ContentError::InvalidPlan {
            detail: "target is required".to_string(),
        }
    })?)?;
    let roots: Vec<String> = payload
        .get("roots")
        .and_then(Value::as_array)
        .map(|roots| {
            roots
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let candidates = payload
        .get("candidates")
        .and_then(Value::as_array)
        .map(|candidates| {
            candidates
                .iter()
                .map(candidate_from_value)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let budget = match payload.get("budget") {
        Some(value) => clamp(SolveBudget::from_value(value)?),
        None => clamp(SolveBudget::default()),
    };
    Ok(SolveRequest {
        target,
        roots,
        candidates,
        budget,
    })
}

/// Open a pack file from disk and read it through the hardened archive reader.
///
/// The size cap keeps a hostile or accidental multi-gigabyte file from being
/// slurped into memory before the format budget even applies.
fn read_pack_file(path: &str) -> Result<super::modpack::PackManifest, ContentError> {
    const MAX_PACK_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > MAX_PACK_FILE_BYTES {
        return Err(ContentError::BudgetExceeded {
            budget: "pack_file_bytes",
            limit: MAX_PACK_FILE_BYTES,
            observed: metadata.len(),
        });
    }
    let bytes = std::fs::read(path)?;
    super::modpack::read_mrpack(&bytes)
}

fn finish_task(
    tasks: &Arc<dyn crate::tasks::TaskService>,
    task_id: &TaskId,
    generation: AttemptGeneration,
    outcome: Result<Value, ContentError>,
) {
    match outcome {
        Ok(value) => {
            if tasks.is_cancel_requested(task_id, generation) {
                let _ = tasks.cancel_now(task_id, generation);
            } else {
                let _ = tasks.succeed(task_id, generation, Some(value));
            }
        }
        Err(error) => {
            if tasks.is_cancel_requested(task_id, generation) {
                let _ = tasks.cancel_now(task_id, generation);
            } else {
                let _ = tasks.fail(task_id, generation, error.to_envelope());
            }
        }
    }
}

fn invalid(detail: impl Into<String>) -> ErrorEnvelope {
    ContentError::InvalidPlan {
        detail: detail.into(),
    }
    .to_envelope()
}

fn events_kind(value: &'static str) -> shtmcl_domain::EventKind {
    shtmcl_domain::EventKind::from_static(value)
}

/// Publish one module event. `event_id` / `sequence` are owned by the sink, so the
/// module only supplies kind, revision and payload.
fn publish(
    events: &Arc<dyn crate::tasks::EventSink>,
    operation: &OperationId,
    generation: AttemptGeneration,
    kind: shtmcl_domain::EventKind,
    instance: Option<&InstanceId>,
    revision: Revision,
    payload: Value,
) {
    let entity = instance.and_then(|id| EntityId::new(id.as_str().to_string()).ok());
    let request = crate::tasks::EventRequest::new(operation.clone(), generation, kind)
        .with_entity(entity)
        .with_revision(revision)
        .with_payload(payload);
    let _ = events.publish(request);
}

fn export_candidate(value: &Value) -> Result<ExportCandidate, ContentError> {
    let object = value.as_object().ok_or_else(|| ContentError::InvalidPlan {
        detail: "export file must be an object".to_string(),
    })?;
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ContentError::InvalidPlan {
            detail: "export file.path is required".to_string(),
        })?
        .to_string();
    let side = |key: &str| {
        object
            .get("env")
            .and_then(|env| env.get(key))
            .and_then(Value::as_str)
            .map(|value| match value {
                "optional" => SideSupport::Optional,
                "unsupported" => SideSupport::Unsupported,
                _ => SideSupport::Required,
            })
            .unwrap_or(SideSupport::Required)
    };
    Ok(ExportCandidate {
        path,
        hashes: Hashes {
            sha1: object
                .get("sha1")
                .and_then(Value::as_str)
                .map(str::to_string),
            sha512: object
                .get("sha512")
                .and_then(Value::as_str)
                .map(str::to_string),
            sha256: object
                .get("sha256")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        downloads: object
            .get("downloads")
            .and_then(Value::as_array)
            .map(|urls| {
                urls.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        file_size: object.get("fileSize").and_then(Value::as_u64),
        client: side("client"),
        server: side("server"),
    })
}

/// Module entry point used by the application layer. The default ports are
/// **fail-closed** ([`ContentPorts::unavailable`]): while 01/03 are unintegrated,
/// every command that needs them returns `content.owner.unavailable` instead of a
/// fabricated success. Replace with [`module_with_ports`] once the owners land.
pub fn module(ctx: ModuleContext) -> Arc<dyn CoreModule> {
    Arc::new(ContentModule::new(ctx, ContentPorts::unavailable()))
}

/// Explicit wiring: the application layer injects the real ports (01 instance
/// transaction, 03 artifact staging, source adapters, loader metadata catalog).
pub fn module_with_ports(ctx: ModuleContext, ports: ContentPorts) -> Arc<dyn CoreModule> {
    Arc::new(ContentModule::new(ctx, ports))
}

//! `CoreModule` implementation: command routing for module 08.
//!
//! Routing convention (frozen by 00): the command name travels in
//! `payload.command`, the first segment is the module id (`diagnostics.`), and
//! `CoreApp::dispatch_*` rejects a foreign namespace before it reaches this
//! module.
//!
//! Three shapes exist on purpose:
//!
//! - **long operations** (`diagnostics.analyze`, `diagnostics.export` in write
//!   mode, `diagnostics.update.prepare`) `accept` a task through the shared
//!   [`crate::tasks::TaskService`] and answer `accepted`; progress, results and
//!   cancellation all travel through that service;
//! - **reads** (`diagnostics.snapshot`, `diagnostics.logs`,
//!   `diagnostics.update.check`, `diagnostics.help`, `diagnostics.repair.plan`
//!   without `commit`) answer `completed` immediately;
//! - **transactions** (`diagnostics.update.apply`, `diagnostics.update.recover`)
//!   answer `completed` because the store commits them in one step, and they may
//!   answer `waiting_active_work` instead of restarting.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use shtmcl_domain::{
    AttemptGeneration, CommandKind, CommandRequest, CommandResponse, ErrorEnvelope, EventKind,
    InstanceId, LaunchSessionId, ModuleId, OperationId, Revision, TaskId, TaskPhase, TaskState,
};

use super::error::DiagnosticsError;
use super::export::{ExportResult, ExportSelection, Exporter};
use super::help;
use super::logstore::{LogLimits, LogStore};
use super::repair::{RepairPlanner, RepairRequest};
use super::rules::{
    AnalysisRequest, DiagnosticEngine, Finding, ReportStatus, DEFAULT_BUDGET_MS, MAX_BUDGET_MS,
};
use super::update::{
    AppVersion, Channel, CheckRequest, HostPlatform, PrepareRequest, ReleaseRecord, UpdatePorts,
    UpdateStore, Updater,
};
use super::MODULE;
use crate::content::ports::InstanceContentPort;
use crate::launch::logs::LogSink;
use crate::tasks::{EventRequest, EventSink, TaskService, TaskSpec};
use crate::{CoreModule, ModuleContext};

/// Commands this module owns. Stable; registered in the 08 handoff.
pub mod actions {
    pub const SNAPSHOT: &str = "diagnostics.snapshot";
    pub const LOGS: &str = "diagnostics.logs";
    pub const ANALYZE: &str = "diagnostics.analyze";
    pub const REPAIR_PLAN: &str = "diagnostics.repair.plan";
    pub const EXPORT: &str = "diagnostics.export";
    pub const UPDATE_CHECK: &str = "diagnostics.update.check";
    pub const UPDATE_PREPARE: &str = "diagnostics.update.prepare";
    pub const UPDATE_APPLY: &str = "diagnostics.update.apply";
    pub const UPDATE_RECOVER: &str = "diagnostics.update.recover";
    pub const HELP: &str = "diagnostics.help";
}

/// Event kinds published by this module (registered in the 08 handoff).
pub mod events {
    pub const REPORT_READY: &str = "diagnostics.report.ready";
    pub const EXPORT_READY: &str = "diagnostics.export.ready";
    pub const UPDATE_CHANGED: &str = "diagnostics.update.changed";
}

/// Task phases (deliberately distinct from command names).
pub mod phases {
    pub const ANALYZE: &str = "diagnostics.phase.analyze";
    pub const EXPORT: &str = "diagnostics.phase.export";
    pub const UPDATE_PREPARE: &str = "diagnostics.phase.update_prepare";
}

/// Evidence markers written into `Task.evidence`.
pub mod evidence {
    pub const ANALYSIS_COMPLETED: &str = "analysis_completed";
    /// At least one rule did not run: the report is partial on purpose.
    pub const ANALYSIS_PARTIAL: &str = "analysis_partial";
    pub const EXPORT_WRITTEN: &str = "export_written";
    /// The destination did not exist before this run.
    pub const EXPORT_NEW_FILE: &str = "export_new_file";
    pub const ARTIFACT_VERIFIED: &str = "artifact_verified";
    pub const SIGNATURE_VERIFIED: &str = "signature_verified";
    pub const RESTART_DEFERRED: &str = "restart_deferred";
    pub const SWITCHED: &str = "switched_to_new_version";
    pub const STARTUP_CONFIRMED: &str = "startup_confirmed";
    pub const ROLLED_BACK: &str = "rolled_back_to_old_version";
    pub const OLD_VERSION_AVAILABLE: &str = "old_version_available";
}

/// Neighbour ports this module needs, injected at construction.
#[derive(Clone)]
pub struct DiagnosticsPorts {
    /// 01, through 04's port shape: read the instance, apply a plan.
    pub instances: Arc<dyn InstanceContentPort>,
    /// Release feed, download, signature, startup probe, active work.
    pub update: UpdatePorts,
    /// Where the shared log store writes its rolling files.
    pub log_limits: LogLimits,
}

impl DiagnosticsPorts {
    /// Default wiring for `diagnostics::module(ctx)`: **every** neighbour is
    /// fail-closed, so registering the module can neither read an instance, check
    /// a release, verify a signature, nor restart anything.
    pub fn unavailable() -> Self {
        Self {
            instances: Arc::new(crate::content::ports::UnavailableInstancePort),
            update: UpdatePorts::unavailable(),
            log_limits: LogLimits::default(),
        }
    }

    /// Offline doubles. **Never** the default in `module(ctx)`.
    pub fn fixtures(instances: Arc<dyn InstanceContentPort>, record: ReleaseRecord) -> Self {
        Self {
            instances,
            update: UpdatePorts::fixtures(record),
            log_limits: LogLimits::default(),
        }
    }

    pub fn is_provisional(&self) -> bool {
        true
    }
}

pub struct DiagnosticsModule {
    ctx: ModuleContext,
    store: LogStore,
    engine: Arc<DiagnosticEngine>,
    exporter: Exporter,
    planner: RepairPlanner,
    updater: Arc<Updater>,
    jobs: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
    counter: Arc<AtomicU64>,
}

impl DiagnosticsModule {
    pub fn new(ctx: ModuleContext, ports: DiagnosticsPorts) -> Self {
        let store = LogStore::with_limits(&ctx.dirs().logs, ports.log_limits);
        let engine = Arc::new(DiagnosticEngine::new(store.clone()));
        let exporter = Exporter::new(store.clone());
        let planner = RepairPlanner::new(ports.instances.clone());
        let updates_root: PathBuf = ctx.dirs().data.join("updates");
        let updater = Arc::new(Updater::new(ports.update, UpdateStore::new(updates_root)));
        Self {
            ctx,
            store,
            engine,
            exporter,
            planner,
            updater,
            jobs: Arc::new(Mutex::new(Vec::new())),
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The shared log store. The application hands this to 02's `LogPipeline` so
    /// there is exactly one durable copy of the game output.
    pub fn log_store(&self) -> &LogStore {
        &self.store
    }

    pub fn log_sink(&self) -> Arc<dyn LogSink> {
        Arc::new(self.store.clone())
    }

    pub fn engine(&self) -> &Arc<DiagnosticEngine> {
        &self.engine
    }

    pub fn updater(&self) -> &Arc<Updater> {
        &self.updater
    }

    /// Wait for every background worker this module started (tests and shutdown).
    pub fn join_jobs(&self) {
        let handles: Vec<std::thread::JoinHandle<()>> = self
            .jobs
            .lock()
            .map(|mut jobs| jobs.drain(..).collect())
            .unwrap_or_default();
        for handle in handles {
            let _ = handle.join();
        }
    }

    fn next_suffix(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        let handle = std::thread::Builder::new()
            .name(format!("shtmcl-diagnostics-{}", self.next_suffix()))
            .spawn(job);
        if let Ok(handle) = handle {
            if let Ok(mut jobs) = self.jobs.lock() {
                jobs.push(handle);
            }
        }
    }

    fn accept_task(
        &self,
        request: &CommandRequest,
        phase_name: &'static str,
        instance_id: Option<InstanceId>,
        input_revision: Option<Revision>,
    ) -> Result<(shtmcl_domain::Task, OperationId), ErrorEnvelope> {
        let operation_id = request
            .operation_id
            .clone()
            .ok_or_else(ErrorEnvelope::command_operation_required)?;
        let task = self.ctx.tasks().accept(
            TaskSpec::new(operation_id.clone(), phase(phase_name))
                .with_instance(instance_id)
                .with_input_revision(input_revision),
        )?;
        Ok((task, operation_id))
    }

    fn publish(&self, operation: &OperationId, kind: &'static str, payload: Value) {
        let request = EventRequest::new(
            operation.clone(),
            AttemptGeneration::INITIAL,
            EventKind::from_static(kind),
        )
        .with_payload(payload);
        let _ = self.ctx.events().publish(request);
    }

    // -- reads -------------------------------------------------------------

    fn snapshot(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Read)?;
        let sessions: Vec<Value> = self
            .store
            .session_ids()
            .into_iter()
            .filter_map(|session| {
                self.store.status(&session).map(|status| {
                    let mut value = status.to_value();
                    if let Some(object) = value.as_object_mut() {
                        object.insert(
                            "sessionId".to_string(),
                            Value::String(session.as_str().to_string()),
                        );
                    }
                    value
                })
            })
            .collect();
        let reports: Vec<Value> = self
            .store
            .session_ids()
            .into_iter()
            .filter_map(|session| self.engine.cached_report(&session))
            .map(|report| {
                json!({
                    "sessionId": report.session_id.as_str(),
                    "status": report.status.as_str(),
                    "verdict": report.verdict(),
                    "findings": report.findings.len(),
                    "logRange": { "from": report.from_sequence, "to": report.to_sequence },
                    "timedOut": report.budget.timed_out,
                })
            })
            .collect();
        let transactions = self
            .updater
            .transactions()
            .map(|list| {
                list.iter()
                    .map(|transaction| transaction.to_value())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(CommandResponse::Completed {
            result: json!({
                "logStore": {
                    "sessions": sessions,
                    "queuedBytes": self.store.queued_bytes(),
                    "persistedBytes": self.store.total_persisted_bytes(),
                    "evictedSessions": self.store.evicted_sessions(),
                    "writeFailures": self.store.write_failures(),
                },
                "reports": reports,
                "updates": {
                    "transactions": transactions,
                    "installed": self.updater.installed().map(|pointer| json!({
                        "version": pointer.version,
                        "artifactId": pointer.artifact_id,
                        "sha256": pointer.sha256,
                    })),
                },
                "helpCategories": help::categories(),
            }),
        })
    }

    fn logs(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Read)?;
        let session = as_session(&request.payload)?;
        let cursor = request.payload.get("cursor").and_then(Value::as_u64);
        let limit = request
            .payload
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(200) as usize;
        let page = self
            .store
            .read_log(&session, cursor, limit)
            .map_err(|error| error.to_envelope())?;
        let mut result = page.to_json();
        if let Some(object) = result.as_object_mut() {
            object.insert(
                "sessionId".to_string(),
                Value::String(session.as_str().to_string()),
            );
            if let Some(status) = self.store.status(&session) {
                object.insert("session".to_string(), status.to_value());
            }
        }
        Ok(CommandResponse::Completed { result })
    }

    fn help(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Read)?;
        let category = optional_str(&request.payload, "category");
        let entry = optional_str(&request.payload, "entry");
        let code = optional_str(&request.payload, "code");
        Ok(CommandResponse::Completed {
            result: help::resolve(category.as_deref(), entry.as_deref(), code.as_deref()),
        })
    }

    fn repair_plan(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let commit = request
            .payload
            .get("commit")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        request.validate(if commit {
            CommandKind::Mutate
        } else {
            CommandKind::Read
        })?;
        let session = as_session(&request.payload)?;
        let finding_id = payload_str(&request.payload, "findingId")?;
        let finding: Finding = self.engine.find(&session, &finding_id).ok_or_else(|| {
            DiagnosticsError::AnalysisNotFound {
                report: format!("{}:{}", session.as_str(), finding_id),
            }
            .to_envelope()
        })?;
        let revision = request
            .payload
            .get("instanceRevision")
            .and_then(Value::as_u64)
            .map(Revision::new)
            .unwrap_or(Revision::new(0));
        let target = optional_str(&request.payload, "target");
        let mut repair = RepairRequest::new(finding).with_target(target);
        repair.expected_revision = revision;
        repair.commit = commit;
        let outcome = self.planner.plan(&repair).map_err(|e| e.to_envelope())?;
        if let Some(operation) = &request.operation_id {
            if commit {
                self.publish(
                    operation,
                    events::REPORT_READY,
                    json!({
                        "scope": "repair",
                        "planId": outcome.plan.plan_id,
                        "entries": outcome.plan.entries.len(),
                        "planDigest": outcome.plan.plan_digest,
                    }),
                );
            }
        }
        Ok(CommandResponse::Completed {
            result: outcome.to_value(),
        })
    }

    fn update_check(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Read)?;
        let current = AppVersion::parse(&payload_str(&request.payload, "currentVersion")?)
            .map_err(|error| error.to_envelope())?;
        let channel = match optional_str(&request.payload, "channel") {
            Some(value) => Channel::parse(&value).map_err(|error| error.to_envelope())?,
            None => Channel::Stable,
        };
        let host = host_from(&request.payload);
        let check = self
            .updater
            .check(&CheckRequest {
                current,
                channel,
                host,
            })
            .map_err(|error| error.to_envelope())?;
        Ok(CommandResponse::Completed {
            result: check.to_value(),
        })
    }

    // -- long operations ---------------------------------------------------

    fn start_analyze(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let session = as_session(&request.payload)?;
        let instance_id = optional_str(&request.payload, "instanceId")
            .map(InstanceId::new)
            .transpose()
            .map_err(|error| DiagnosticsError::invalid(error.to_string()).to_envelope())?;
        let revision = request
            .payload
            .get("instanceRevision")
            .and_then(Value::as_u64)
            .map(Revision::new)
            .unwrap_or(Revision::ZERO);
        let budget = request
            .payload
            .get("budgetMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_BUDGET_MS)
            .min(MAX_BUDGET_MS);
        let (task, operation_id) = self.accept_task(
            request,
            phases::ANALYZE,
            instance_id.clone(),
            Some(revision),
        )?;

        let job = AnalyzeJob {
            tasks: self.ctx.tasks().clone(),
            events: self.ctx.events().clone(),
            engine: self.engine.clone(),
            task_id: task.id.clone(),
            generation: task.attempt_generation,
            operation: operation_id.clone(),
            request: AnalysisRequest::new(session)
                .with_instance(instance_id)
                .with_revision(revision)
                .with_budget_ms(budget),
        };
        self.spawn(move || job.run());
        Ok(CommandResponse::Accepted {
            operation_id,
            task_id: task.id,
        })
    }

    fn start_export(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let selection = selection_from(&request.payload)?;
        let destination = PathBuf::from(payload_str(&request.payload, "destination")?);
        if selection.sessions.is_empty() {
            return Err(
                DiagnosticsError::invalid("at least one session is required").to_envelope(),
            );
        }
        let (task, operation_id) = self.accept_task(request, phases::EXPORT, None, None)?;
        let job = ExportJob {
            tasks: self.ctx.tasks().clone(),
            events: self.ctx.events().clone(),
            exporter: Exporter::new(self.store.clone()),
            report_source: self.engine.clone(),
            task_id: task.id.clone(),
            generation: task.attempt_generation,
            operation: operation_id.clone(),
            selection,
            destination,
        };
        self.spawn(move || job.run());
        Ok(CommandResponse::Accepted {
            operation_id,
            task_id: task.id,
        })
    }

    fn export(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let mode = optional_str(&request.payload, "mode").unwrap_or_else(|| "preview".to_string());
        match mode.as_str() {
            "preview" => {
                request.validate(CommandKind::Read)?;
                let selection = selection_from(&request.payload)?;
                let preview = self
                    .exporter
                    .preview(&selection)
                    .map_err(|error| error.to_envelope())?;
                Ok(CommandResponse::Completed {
                    result: preview.to_value(),
                })
            }
            "write" => self.start_export(request),
            other => Err(DiagnosticsError::invalid(format!("mode: {other}")).to_envelope()),
        }
    }

    fn start_update_prepare(
        &self,
        request: &CommandRequest,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let current = AppVersion::parse(&payload_str(&request.payload, "currentVersion")?)
            .map_err(|error| error.to_envelope())?;
        let candidate_value = request
            .payload
            .get("candidate")
            .cloned()
            .ok_or_else(|| DiagnosticsError::invalid("candidate is required").to_envelope())?;
        let candidate: ReleaseRecord =
            serde_json::from_value(candidate_value).map_err(|error| {
                DiagnosticsError::invalid(format!("candidate: {error}")).to_envelope()
            })?;
        // `ReleaseRecord` deserialises its version as a plain string; validate it
        // here so an invalid release never reaches the download stage.
        AppVersion::parse(candidate.version.as_str()).map_err(|error| error.to_envelope())?;
        let host = host_from(&request.payload);
        let (task, operation_id) = self.accept_task(request, phases::UPDATE_PREPARE, None, None)?;
        let job = PrepareJob {
            tasks: self.ctx.tasks().clone(),
            events: self.ctx.events().clone(),
            updater: self.updater.clone(),
            task_id: task.id.clone(),
            generation: task.attempt_generation,
            operation: operation_id.clone(),
            request: PrepareRequest {
                current,
                host,
                candidate,
            },
        };
        self.spawn(move || job.run());
        Ok(CommandResponse::Accepted {
            operation_id,
            task_id: task.id,
        })
    }

    // -- transactions ------------------------------------------------------

    fn update_apply(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let transaction_id = payload_str(&request.payload, "transactionId")?;
        let confirm = request
            .payload
            .get("confirmStart")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let outcome = self
            .updater
            .apply(&transaction_id, confirm)
            .map_err(|error| error.to_envelope())?;
        let value = outcome.to_value();
        self.publish_update(request, &value);
        Ok(CommandResponse::Completed { result: value })
    }

    fn update_recover(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let transaction_id = payload_str(&request.payload, "transactionId")?;
        let outcome = self
            .updater
            .recover(&transaction_id)
            .map_err(|error| error.to_envelope())?;
        let value = outcome.to_value();
        self.publish_update(request, &value);
        Ok(CommandResponse::Completed { result: value })
    }

    fn publish_update(&self, request: &CommandRequest, value: &Value) {
        if let Some(operation) = &request.operation_id {
            let transaction = value.get("transaction").cloned().unwrap_or(Value::Null);
            let status = value
                .get("status")
                .or_else(|| value.get("action"))
                .cloned()
                .unwrap_or(Value::Null);
            self.publish(
                operation,
                events::UPDATE_CHANGED,
                json!({
                    "status": status,
                    "stage": transaction.get("stage").cloned().unwrap_or(Value::Null),
                    "transactionId": transaction.get("transactionId").cloned().unwrap_or(Value::Null),
                    "targetVersion": transaction.get("targetVersion").cloned().unwrap_or(Value::Null),
                    "evidence": transaction.get("evidence").cloned().unwrap_or(json!([])),
                }),
            );
        }
    }
}

fn phase(name: &'static str) -> TaskPhase {
    TaskPhase::new(name).unwrap_or_else(|_| {
        TaskPhase::new("diagnostics.phase.unknown").expect("static phase is valid")
    })
}

fn payload_str(payload: &Value, key: &str) -> Result<String, ErrorEnvelope> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| DiagnosticsError::invalid(format!("{key} is required")).to_envelope())
}

fn optional_str(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

fn as_session(payload: &Value) -> Result<LaunchSessionId, ErrorEnvelope> {
    let raw = payload_str(payload, "sessionId")?;
    LaunchSessionId::new(raw)
        .map_err(|error| DiagnosticsError::invalid(format!("sessionId: {error}")).to_envelope())
}

fn host_from(payload: &Value) -> HostPlatform {
    let current = HostPlatform::current();
    HostPlatform {
        os: optional_str(payload, "platform").unwrap_or(current.os),
        arch: optional_str(payload, "arch").unwrap_or(current.arch),
    }
}

fn selection_from(payload: &Value) -> Result<ExportSelection, ErrorEnvelope> {
    let mut selection = ExportSelection::default();
    let sessions = payload
        .get("sessions")
        .and_then(Value::as_array)
        .ok_or_else(|| DiagnosticsError::invalid("sessions is required").to_envelope())?;
    for value in sessions {
        let raw = value
            .as_str()
            .ok_or_else(|| DiagnosticsError::invalid("sessions[] must be ids").to_envelope())?;
        selection
            .sessions
            .push(LaunchSessionId::new(raw).map_err(|error| {
                DiagnosticsError::invalid(format!("session: {error}")).to_envelope()
            })?);
    }
    if let Some(value) = payload.get("stdout").and_then(Value::as_bool) {
        selection.include_stdout = value;
    }
    if let Some(value) = payload.get("stderr").and_then(Value::as_bool) {
        selection.include_stderr = value;
    }
    if let Some(value) = payload.get("findings").and_then(Value::as_bool) {
        selection.include_findings = value;
    }
    if let Some(value) = payload.get("stripPaths").and_then(Value::as_bool) {
        selection.strip_paths = value;
    }
    if let Some(value) = payload.get("redactDigests").and_then(Value::as_bool) {
        selection.redact_digests = value;
    }
    if let Some(value) = payload.get("maxBytes").and_then(Value::as_u64) {
        selection.max_bytes = value;
    }
    Ok(selection)
}

// ---------------------------------------------------------------------------
// workers
// ---------------------------------------------------------------------------

struct AnalyzeJob {
    tasks: Arc<dyn TaskService>,
    events: Arc<dyn EventSink>,
    engine: Arc<DiagnosticEngine>,
    task_id: TaskId,
    generation: AttemptGeneration,
    operation: OperationId,
    request: AnalysisRequest,
}

impl AnalyzeJob {
    fn run(self) {
        let tasks = self.tasks.clone();
        let task_id = self.task_id.clone();
        let generation = self.generation;
        let cancelled = move || tasks.is_cancel_requested(&task_id, generation);

        let _ = self
            .tasks
            .transition(&self.task_id, self.generation, TaskState::Running);
        let _ = self
            .tasks
            .set_phase(&self.task_id, self.generation, phase(phases::ANALYZE));

        match self.engine.analyze(&self.request, &cancelled) {
            Ok(outcome) => {
                let report = outcome.report;
                if report.status == ReportStatus::Cancelled {
                    let _ = self.tasks.cancel_now(&self.task_id, self.generation);
                    return;
                }
                let marker = if report.budget.timed_out || report.status == ReportStatus::TimedOut {
                    evidence::ANALYSIS_PARTIAL
                } else {
                    evidence::ANALYSIS_COMPLETED
                };
                let _ = self.tasks.add_evidence(&self.task_id, marker);
                let mut report_value = report.to_value();
                if let Some(object) = report_value.as_object_mut() {
                    object.insert("cached".to_string(), Value::Bool(outcome.cached));
                }
                let payload = json!({
                    "sessionId": report.session_id.as_str(),
                    "status": report.status.as_str(),
                    "verdict": report.verdict(),
                    "findings": report.findings.len(),
                    "timedOut": report.budget.timed_out,
                    "budgetMs": report.budget.budget_ms,
                    "elapsedMs": report.budget.elapsed_ms,
                    "logIncomplete": report.log_incomplete,
                    "cached": outcome.cached,
                    "from": report.from_sequence,
                    "to": report.to_sequence,
                });
                let _ = self.events.publish(
                    EventRequest::new(
                        self.operation.clone(),
                        self.generation,
                        EventKind::from_static(events::REPORT_READY),
                    )
                    .with_payload(payload),
                );
                let _ = self
                    .tasks
                    .succeed(&self.task_id, self.generation, Some(report_value));
            }
            Err(error) => {
                if cancelled() {
                    let _ = self.tasks.cancel_now(&self.task_id, self.generation);
                } else {
                    let _ = self
                        .tasks
                        .fail(&self.task_id, self.generation, error.to_envelope());
                }
            }
        }
    }
}

struct ExportJob {
    tasks: Arc<dyn TaskService>,
    events: Arc<dyn EventSink>,
    exporter: Exporter,
    report_source: Arc<DiagnosticEngine>,
    task_id: TaskId,
    generation: AttemptGeneration,
    operation: OperationId,
    selection: ExportSelection,
    destination: PathBuf,
}

impl ExportJob {
    fn run(self) {
        let tasks = self.tasks.clone();
        let task_id = self.task_id.clone();
        let generation = self.generation;
        let cancelled = move || tasks.is_cancel_requested(&task_id, generation);

        let _ = self
            .tasks
            .transition(&self.task_id, self.generation, TaskState::Running);
        let _ = self
            .tasks
            .set_phase(&self.task_id, self.generation, phase(phases::EXPORT));

        let report = self
            .selection
            .sessions
            .first()
            .and_then(|session| self.report_source.cached_report(session));
        let outcome = self.exporter.export(
            &self.selection,
            report.as_ref(),
            &self.destination,
            &cancelled,
        );
        let absolute = absolutise(&self.destination);
        match outcome {
            Ok(result) if result.cancelled => {
                let _ = self.tasks.cancel_now(&self.task_id, self.generation);
            }
            Ok(result) => {
                let _ = self
                    .tasks
                    .add_evidence(&self.task_id, evidence::EXPORT_WRITTEN);
                if !result.replaced_existing {
                    let _ = self
                        .tasks
                        .add_evidence(&self.task_id, evidence::EXPORT_NEW_FILE);
                }
                let payload = json!({
                    "bytes": result.bytes,
                    "records": result.records,
                    "redactions": result.redactions,
                    "stripped": result.stripped,
                    "sessions": result.sessions,
                    "destinationName": result
                        .destination
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string()),
                    "replacedExisting": result.replaced_existing,
                });
                let _ = self.events.publish(
                    EventRequest::new(
                        self.operation.clone(),
                        self.generation,
                        EventKind::from_static(events::EXPORT_READY),
                    )
                    .with_payload(payload),
                );
                let _ = self.tasks.succeed(
                    &self.task_id,
                    self.generation,
                    Some(export_result_value(&result, &absolute)),
                );
            }
            Err(error) => {
                if cancelled() {
                    let _ = self.tasks.cancel_now(&self.task_id, self.generation);
                } else {
                    let _ = self
                        .tasks
                        .fail(&self.task_id, self.generation, error.to_envelope());
                }
            }
        }
    }
}

/// Resolve a caller-supplied destination to an absolute path for the task result.
/// `std::path::absolute` is newer than this workspace's MSRV, so it is open-coded.
fn absolutise(path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|dir| dir.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn export_result_value(result: &ExportResult, destination: &std::path::Path) -> Value {
    let mut value = result.to_value();
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "destination".to_string(),
            Value::String(destination.to_string_lossy().to_string()),
        );
    }
    value
}

struct PrepareJob {
    tasks: Arc<dyn TaskService>,
    events: Arc<dyn EventSink>,
    updater: Arc<Updater>,
    task_id: TaskId,
    generation: AttemptGeneration,
    operation: OperationId,
    request: PrepareRequest,
}

impl PrepareJob {
    fn run(self) {
        let tasks = self.tasks.clone();
        let task_id = self.task_id.clone();
        let generation = self.generation;
        let cancelled = move || tasks.is_cancel_requested(&task_id, generation);

        let _ = self
            .tasks
            .transition(&self.task_id, self.generation, TaskState::Running);
        let _ = self.tasks.set_phase(
            &self.task_id,
            self.generation,
            phase(phases::UPDATE_PREPARE),
        );

        match self.updater.prepare(&self.request, &cancelled) {
            Ok(transaction) => {
                if cancelled() {
                    let _ = self.tasks.cancel_now(&self.task_id, self.generation);
                    return;
                }
                if transaction.artifact_verified {
                    let _ = self
                        .tasks
                        .add_evidence(&self.task_id, evidence::ARTIFACT_VERIFIED);
                }
                if transaction.signature_verified {
                    let _ = self
                        .tasks
                        .add_evidence(&self.task_id, evidence::SIGNATURE_VERIFIED);
                }
                let _ = self.events.publish(
                    EventRequest::new(
                        self.operation.clone(),
                        self.generation,
                        EventKind::from_static(events::UPDATE_CHANGED),
                    )
                    .with_payload(json!({
                        "status": "prepared",
                        "stage": transaction.stage.as_str(),
                        "transactionId": transaction.transaction_id,
                        "targetVersion": transaction.target.version.as_str(),
                        "evidence": transaction.evidence,
                    })),
                );
                let _ = self.tasks.succeed(
                    &self.task_id,
                    self.generation,
                    Some(transaction.to_value()),
                );
            }
            Err(error) => {
                if cancelled() {
                    let _ = self.tasks.cancel_now(&self.task_id, self.generation);
                } else {
                    let _ = self
                        .tasks
                        .fail(&self.task_id, self.generation, error.to_envelope());
                }
            }
        }
    }
}

impl CoreModule for DiagnosticsModule {
    fn id(&self) -> ModuleId {
        MODULE
    }

    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        // `payload.command` is the frozen routing key; there is no alias.
        let command = request.command()?;
        match command.as_str() {
            actions::SNAPSHOT => self.snapshot(request),
            actions::LOGS => self.logs(request),
            actions::ANALYZE => self.start_analyze(request),
            actions::REPAIR_PLAN => self.repair_plan(request),
            actions::EXPORT => self.export(request),
            actions::UPDATE_CHECK => self.update_check(request),
            actions::UPDATE_PREPARE => self.start_update_prepare(request),
            actions::UPDATE_APPLY => self.update_apply(request),
            actions::UPDATE_RECOVER => self.update_recover(request),
            actions::HELP => self.help(request),
            _ => Err(DiagnosticsError::UnknownCommand {
                command: command.as_str().to_string(),
            }
            .to_envelope()),
        }
    }
}

/// Build the module with **fail-closed** ports (the application may replace them
/// with `module_with_ports` once 01 and a release feed exist).
pub fn module(ctx: ModuleContext) -> Arc<dyn CoreModule> {
    Arc::new(DiagnosticsModule::new(ctx, DiagnosticsPorts::unavailable()))
}

pub fn module_with_ports(ctx: ModuleContext, ports: DiagnosticsPorts) -> Arc<DiagnosticsModule> {
    Arc::new(DiagnosticsModule::new(ctx, ports))
}

/// Convenience for the shutdown path: drain the log queue before the process ends.
pub fn flush_logs(store: &LogStore, timeout: Duration) -> bool {
    store.flush_now(timeout).is_ok()
}

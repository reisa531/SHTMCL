//! 02 · 启动与运行时（Owner: 02-launch-and-runtime）
//!
//! Java 探测与约束匹配、冻结快照上的启动计划、游戏会话状态机、游戏日志唯一
//! 读取者、Quick Play 能力表。进程创建一律经 `shtmcl_platform::ProcessLauncher`。
//!
//! 本轮交付的边界（详见 `docs/development/handoffs/02/HANDOFF.md`）：
//!
//! - 真实实现：版本解析与匹配、受限路径扫描、探测缓存、文件校验、计划与参数
//!   数组、摘要、日志有界环、能力表、会话幂等、取消安全点、错误码与证据标记。
//! - 端口替身：01 实例快照、05 游戏会话句柄、08 LogStore、进程管道/退出监督。
//!   它们经 [`ports`] 注入；默认实现 fail-closed，绝不伪造真实实例。
//!
//! 命令入口在 `payload.command`（`runtimes.list` 等）。长任务立即返回
//! `Accepted { operation_id, task_id }`，进度与结果只经 `TaskService`。
#![allow(clippy::type_complexity)]

pub mod catalog;
pub mod errors;
pub mod install;
pub mod java;
mod lease;
pub mod logs;
pub mod plan;
pub mod ports;
pub mod quickplay;
#[cfg(test)]
mod resource_prepare_tests;
pub mod runtime;
pub mod session;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;
use shtmcl_domain::{
    AttemptGeneration, CommandKind, CommandRequest, CommandResponse, EntityId, ErrorEnvelope,
    EventKind, InstanceId, LaunchSessionId, ModuleId, Revision, TaskId, TaskPhase, TaskState,
};

use crate::context::ModuleContext;
use crate::tasks::{EventRequest, TaskService, TaskSpec};
use crate::CoreModule;

use self::java::{
    CacheStats, DiscoveryLimits, JavaCache, JavaProber, JavaRuntime, RuntimeConstraint, Selection,
};
use self::logs::LogPipeline;
use self::plan::{LaunchPlan, PlanInput};
use self::ports::{AuthRequirement, WaitOutcome};
use self::quickplay::{CapabilityKey, QuickPlayTable};
use self::session::{LaunchSession, LaunchSessionRegistry, LaunchSessionState, ProcessIdentity};

pub const MODULE: ModuleId = ModuleId::Launch;
pub const OWNER: &str = "02";

/// Re-exported so the application layer (and the behaviour tests) can construct
/// the module with explicit ports without reaching into a private `use`.
pub use self::ports::LaunchPorts;

/// Command names carried in `payload.command`.
///
/// The first segment must be this module's id (`launch`), which the application
/// layer enforces with `CommandName::owner()`.
pub mod commands {
    pub const VERSIONS_LIST: &str = "launch.versions.list";
    pub const VERSIONS_REFRESH: &str = "launch.versions.refresh";
    pub const RUNTIMES_LIST: &str = "launch.runtimes.list";
    pub const RUNTIMES_DISCOVER: &str = "launch.runtimes.discover";
    pub const LAUNCH_PREPARE: &str = "launch.prepare";
    pub const LAUNCH_START_AGAIN: &str = "launch.start_again";
    pub const LAUNCH_SESSION: &str = "launch.session";
    pub const LAUNCH_SESSIONS: &str = "launch.sessions";
    pub const LAUNCH_STOP: &str = "launch.stop";
    pub const LAUNCH_LOGS: &str = "launch.logs";
    pub const QUICKPLAY_CAPABILITIES: &str = "launch.quickplay.capabilities";
}

/// Event kinds this module publishes (registered in the handoff).
pub mod events {
    /// `{ sessionId, instanceId, state, ... }` — session state changed.
    pub const SESSION_CHANGED: &str = "launch.session.changed";
    /// `{ sessionId, pid }` — a process was created for the session.
    pub const PROCESS_STARTED: &str = "launch.process.started";
    /// `{ sessionId, pid, exitCode }` — the process ended.
    pub const PROCESS_EXITED: &str = "launch.process.exited";
}

/// Task phases (stable IDs, not UI sentences).
pub mod phases {
    pub const CATALOG: &str = "catalog";
    pub const SNAPSHOT: &str = "snapshot";
    pub const RESOURCE_PREPARE: &str = "resource_prepare";
    pub const AUTH: &str = "auth";
    pub const RUNTIME: &str = "runtime";
    pub const FILES: &str = "files";
    pub const SPAWN: &str = "spawn";
    pub const STOP: &str = "stop";
}

/// Evidence markers produced by this module.
pub mod evidence {
    /// A plan was built; safe to show plan details.
    pub const PLAN_FROZEN: &str = "plan_frozen";
    /// A process was actually created (not proof the game is playable).
    pub const PROCESS_CREATED: &str = "process_created";
    /// Cancellation reached a safe stop before any process existed.
    pub const SAFE_STOP_REACHED: &str = "safe_stop_reached";
    /// Cancellation arrived after the process already existed.
    pub const CANCEL_DURING_SPAWN: &str = "cancel_during_spawn";
    /// Required files were missing, so no process was created.
    pub const REQUIRED_FILES_MISSING: &str = "required_files_missing";
    /// The Java requirement came from an unparsable game version.
    pub const JAVA_REQUIREMENT_UNVERIFIED: &str = "java_requirement_unverified";
    /// The runtime architecture could not be read from its banner.
    pub const JAVA_ARCH_UNVERIFIED: &str = "java_arch_unverified";
    /// A stop request was delivered to the process.
    pub const STOP_REQUESTED: &str = "stop_requested";
    /// The process had already ended before the stop request.
    pub const ALREADY_EXITED: &str = "already_exited";
    /// The game itself confirmed it started (never inferred from the pid).
    pub const GAME_REACHED: &str = "game_reached";
}

/// Construct the module 02 instance registered by the application layer.
///
/// Every undelivered dependency fails closed until the real adapter is
/// registered with [`module_with`].
pub fn module(ctx: ModuleContext) -> Arc<dyn CoreModule> {
    Arc::new(LaunchModule::new(ctx, LaunchPorts::unavailable()))
}

/// Construct with explicit ports (application layer / behaviour tests).
pub fn module_with(ctx: ModuleContext, ports: LaunchPorts) -> Arc<LaunchModule> {
    Arc::new(LaunchModule::new(ctx, ports))
}

/// Thin `CoreModule` wrapper so the dispatch entry point can hand an `Arc` to
/// background jobs.
pub struct LaunchModule {
    inner: Arc<LaunchInner>,
}

impl LaunchModule {
    pub fn new(ctx: ModuleContext, ports: LaunchPorts) -> Self {
        Self {
            inner: Arc::new(LaunchInner::new(ctx, ports)),
        }
    }

    pub fn with_catalog(
        ctx: ModuleContext,
        ports: LaunchPorts,
        catalog: Arc<catalog::VersionCatalog>,
    ) -> Self {
        Self {
            inner: Arc::new(LaunchInner::new_with_catalog(ctx, ports, Some(catalog))),
        }
    }

    pub fn inner(&self) -> Arc<LaunchInner> {
        self.inner.clone()
    }

    pub fn sessions(&self) -> Vec<LaunchSession> {
        self.inner.session_snapshot()
    }

    pub fn session(&self, id: &LaunchSessionId) -> Option<LaunchSession> {
        self.inner
            .sessions
            .lock()
            .ok()
            .and_then(|registry| registry.get(id).cloned())
    }

    pub fn logs(&self) -> &Arc<LogPipeline> {
        &self.inner.logs
    }

    pub fn runtimes(&self) -> Vec<JavaRuntime> {
        self.inner
            .runtimes
            .lock()
            .map(|runtimes| runtimes.clone())
            .unwrap_or_default()
    }

    /// Join every outstanding background job (prepare, discovery, stop, exit
    /// watchers). Used by behaviour tests so they never poll with sleeps.
    pub fn join_jobs(&self) -> usize {
        self.inner.join_jobs()
    }
}

impl CoreModule for LaunchModule {
    fn id(&self) -> ModuleId {
        MODULE
    }

    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        self.inner.handle(request)
    }
}

/// One preparation job.
#[derive(Debug, Clone)]
struct PrepareJob {
    session_id: LaunchSessionId,
    task_id: TaskId,
    task_generation: AttemptGeneration,
    instance_id: InstanceId,
    expected_revision: Option<Revision>,
    start_again: bool,
    quick_play_address: Option<String>,
    /// The user's launch-time profile choice. It overrides the snapshot default
    /// instead of being silently ignored.
    profile_ref: Option<String>,
    expected_offline_username: Option<String>,
}

/// Shared module state. Kept behind an `Arc` so background jobs can own it.
pub struct LaunchInner {
    ctx: ModuleContext,
    ports: LaunchPorts,
    catalog: Option<Arc<catalog::VersionCatalog>>,
    sessions: Mutex<LaunchSessionRegistry>,
    logs: Arc<LogPipeline>,
    runtimes: Mutex<Vec<JavaRuntime>>,
    java_cache: Arc<JavaCache>,
    limits: DiscoveryLimits,
    jobs: Mutex<Vec<(String, JoinHandle<()>)>>,
    plan_counter: AtomicU64,
}

impl LaunchInner {
    pub fn new(ctx: ModuleContext, ports: LaunchPorts) -> Self {
        Self::new_with_catalog(ctx, ports, None)
    }

    pub fn new_with_catalog(
        ctx: ModuleContext,
        ports: LaunchPorts,
        catalog: Option<Arc<catalog::VersionCatalog>>,
    ) -> Self {
        let logs = Arc::new(LogPipeline::new(ports.logs.clone()));
        Self {
            ctx,
            ports,
            catalog,
            sessions: Mutex::new(LaunchSessionRegistry::new()),
            logs,
            runtimes: Mutex::new(Vec::new()),
            java_cache: Arc::new(JavaCache::new()),
            limits: DiscoveryLimits::default(),
            jobs: Mutex::new(Vec::new()),
            plan_counter: AtomicU64::new(0),
        }
    }

    fn tasks(&self) -> &Arc<dyn TaskService> {
        self.ctx.tasks()
    }

    fn session_snapshot(&self) -> Vec<LaunchSession> {
        self.sessions
            .lock()
            .map(|registry| registry.all())
            .unwrap_or_default()
    }

    fn join_jobs(&self) -> usize {
        let mut joined = 0usize;
        loop {
            let handles: Vec<(String, JoinHandle<()>)> = {
                let Ok(mut jobs) = self.jobs.lock() else {
                    return joined;
                };
                if jobs.is_empty() {
                    return joined;
                }
                std::mem::take(&mut *jobs)
            };
            for (_, handle) in handles {
                let _ = handle.join();
                joined += 1;
            }
        }
    }

    fn track(&self, label: String, handle: JoinHandle<()>) {
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.retain(|(_, existing)| !existing.is_finished());
            jobs.push((label, handle));
        }
    }

    // -- dispatch ----------------------------------------------------------

    fn handle(
        self: &Arc<Self>,
        request: &CommandRequest,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        // Canonical command key owned by `shtmcl_domain::CommandName`
        // (`payload.command`); missing/invalid names get the shared codes.
        let command = request.command()?;

        match command.as_str() {
            commands::VERSIONS_LIST => self.versions_list(request),
            commands::VERSIONS_REFRESH => self.versions_refresh(request),
            commands::RUNTIMES_LIST => self.runtimes_list(request),
            commands::RUNTIMES_DISCOVER => self.runtimes_discover(request),
            commands::LAUNCH_PREPARE => self.launch_prepare(request, false),
            commands::LAUNCH_START_AGAIN => self.launch_prepare(request, true),
            commands::LAUNCH_SESSION => self.session_get(request),
            commands::LAUNCH_SESSIONS => self.sessions_list(),
            commands::LAUNCH_LOGS => self.logs_page(request),
            commands::LAUNCH_STOP => self.launch_stop(request),
            commands::QUICKPLAY_CAPABILITIES => self.quickplay_request(request),
            other => Err(errors::request_invalid(format!("command:{other}"))),
        }
    }

    fn versions_list(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let catalog = self.catalog.as_ref().ok_or_else(catalog_unavailable)?;
        let payload = &request.payload;
        let kind = optional_bounded_string(payload, "kind", 32)?;
        let query = optional_bounded_string(payload, "query", 64)?;
        let offset = optional_usize(payload, "offset", 0)?;
        let limit = optional_usize(payload, "limit", 100)?;
        let result = catalog
            .list(kind.as_deref(), query.as_deref(), offset, limit)
            .map_err(catalog_error)?;
        Ok(CommandResponse::Completed { result })
    }

    fn versions_refresh(
        self: &Arc<Self>,
        request: &CommandRequest,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let operation_id = request
            .operation_id
            .clone()
            .ok_or_else(ErrorEnvelope::command_operation_required)?;
        if self.catalog.is_none() {
            return Err(catalog_unavailable());
        }
        let task = self.tasks().accept(TaskSpec::new(
            operation_id.clone(),
            TaskPhase::new(phases::CATALOG).expect("static phase"),
        ))?;
        let task_id = task.id.clone();
        let generation = task.attempt_generation;
        let inner = self.clone();
        let job_task = task_id.clone();
        let handle = std::thread::Builder::new()
            .name(format!("02-catalog-{}", task_id.as_str()))
            .spawn(move || inner.run_catalog_refresh(job_task, generation));
        let handle = match handle {
            Ok(handle) => handle,
            Err(_) => {
                let error = ErrorEnvelope::internal_unavailable();
                let _ = self.tasks().fail(&task_id, generation, error.clone());
                return Err(error);
            }
        };
        self.track(format!("catalog:{}", task_id.as_str()), handle);
        Ok(CommandResponse::Accepted {
            operation_id,
            task_id,
        })
    }

    fn run_catalog_refresh(self: Arc<Self>, task_id: TaskId, generation: AttemptGeneration) {
        let tasks = self.ctx.tasks().clone();
        let _ = tasks.transition(&task_id, generation, TaskState::Running);
        if tasks.is_cancel_requested(&task_id, generation) {
            let _ = tasks.cancel_now(&task_id, generation);
            return;
        }
        let result = self
            .catalog
            .as_ref()
            .ok_or_else(catalog_unavailable)
            .and_then(|catalog| catalog.refresh().map_err(catalog_error));
        if tasks.is_cancel_requested(&task_id, generation) {
            let _ = tasks.cancel_now(&task_id, generation);
            return;
        }
        match result {
            Ok(result) => {
                let _ = tasks.succeed(&task_id, generation, Some(result));
            }
            Err(error) => {
                let _ = tasks.fail(&task_id, generation, error);
            }
        }
    }

    // -- runtimes.list -----------------------------------------------------

    fn runtimes_list(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let constraint = self.constraint_from_payload(&request.payload)?;
        let requirement_verified = request
            .payload
            .get("requirement")
            .and_then(Value::as_str)
            .is_some();
        let runtimes = self
            .runtimes
            .lock()
            .map(|runtimes| runtimes.clone())
            .unwrap_or_default();
        let selection = java::select(&runtimes, &constraint, requirement_verified);
        Ok(CommandResponse::Completed {
            result: json_selection(&selection, &constraint, self.java_cache.stats()),
        })
    }

    fn constraint_from_payload(&self, payload: &Value) -> Result<RuntimeConstraint, ErrorEnvelope> {
        let requirement = match payload.get("requirement").and_then(Value::as_str) {
            Some(spec) => java::JavaRequirement::parse(spec)
                .ok_or_else(|| errors::request_invalid(format!("requirement:{spec}")))?,
            None => java::JavaRequirement::Any,
        };
        let mut constraint = RuntimeConstraint::new(
            requirement,
            if payload.get("requirement").is_some() {
                java::ConstraintSource::Instance
            } else {
                java::ConstraintSource::Fallback
            },
        );
        if let Some(platform) = payload.get("platform").and_then(Value::as_str) {
            constraint.platform = java::HostPlatform::from_name(platform);
            if constraint.platform.is_none() {
                return Err(errors::request_invalid(format!("platform:{platform}")));
            }
        }
        if let Some(arch) = payload.get("arch").and_then(Value::as_str) {
            constraint.arch = java::Arch::from_name(arch);
            if constraint.arch.is_none() {
                return Err(errors::request_invalid(format!("arch:{arch}")));
            }
        }
        if let Some(path) = payload.get("javaPath").and_then(Value::as_str) {
            constraint = constraint.with_explicit_path(PathBuf::from(path));
        }
        Ok(constraint)
    }

    // -- runtimes.discover -------------------------------------------------

    fn runtimes_discover(
        self: &Arc<Self>,
        request: &CommandRequest,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let operation_id = request
            .operation_id
            .clone()
            .ok_or_else(|| ErrorEnvelope::command_operation_required())?;
        let extra: Vec<PathBuf> = request
            .payload
            .get("candidateRoots")
            .and_then(Value::as_array)
            .map(|roots| {
                roots
                    .iter()
                    .filter_map(Value::as_str)
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();

        let task = self.tasks().accept(TaskSpec::new(
            operation_id.clone(),
            TaskPhase::new(phases::RUNTIME).expect("static phase"),
        ))?;
        let task_id = task.id.clone();
        let generation = task.attempt_generation;

        let inner = self.clone();
        let job_task = task_id.clone();
        let job_operation = operation_id.clone();
        let handle = std::thread::Builder::new()
            .name(format!("02-discover-{}", task_id.as_str()))
            .spawn(move || inner.run_discovery(job_task, generation, job_operation, extra))
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        self.track(format!("discover:{}", task_id.as_str()), handle);

        Ok(CommandResponse::Accepted {
            operation_id,
            task_id,
        })
    }

    fn run_discovery(
        self: Arc<Self>,
        task_id: TaskId,
        generation: AttemptGeneration,
        _operation_id: shtmcl_domain::OperationId,
        extra: Vec<PathBuf>,
    ) {
        let tasks = self.ctx.tasks().clone();
        let _ = tasks.transition(&task_id, generation, TaskState::Running);

        let prober = JavaProber::new(self.ports.launcher.clone(), self.ports.supervisor.clone());
        let discovery =
            java::JavaDiscovery::new(prober, self.limits).with_cache(self.java_cache.clone());
        let roots = java::candidate_roots(self.ctx.dirs(), &extra);
        let report = discovery.discover(&roots);

        if tasks.is_cancel_requested(&task_id, generation) {
            let _ = tasks.add_evidence(&task_id, evidence::SAFE_STOP_REACHED);
            let _ = tasks.cancel_now(&task_id, generation);
            return;
        }

        if let Ok(mut runtimes) = self.runtimes.lock() {
            *runtimes = report.runtimes.clone();
        }
        let _ = tasks.succeed(&task_id, generation, Some(report.to_json()));
    }

    // -- launch.prepare / launch.start_again -------------------------------

    fn launch_prepare(
        self: &Arc<Self>,
        request: &CommandRequest,
        start_again: bool,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let operation_id = request
            .operation_id
            .clone()
            .ok_or_else(|| ErrorEnvelope::command_operation_required())?;
        let instance_id = resolve_instance_id(request)?;
        let key = request
            .payload
            .get("idempotencyKey")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("instance:{}", instance_id.as_str()));
        let key = if start_again {
            format!("again:{key}:{}", now_unix_ms())
        } else {
            key
        };
        let quick_play_address = request
            .payload
            .get("quickPlayAddress")
            .and_then(Value::as_str)
            .map(str::to_string);
        let profile_ref = request
            .payload
            .get("profileRef")
            .and_then(Value::as_str)
            .map(str::to_string);
        let expected_offline_username =
            optional_bounded_string(&request.payload, "expectedOfflineUsername", 16)?;

        let mut registry = self
            .sessions
            .lock()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;

        // Idempotent repeat: the same click returns the existing session/task.
        if let Some(existing) = registry.find_live(&instance_id, &key) {
            return Ok(CommandResponse::Accepted {
                operation_id: existing.operation_id.clone(),
                task_id: existing.task_id.clone(),
            });
        }

        let task_spec = TaskSpec::new(
            operation_id.clone(),
            TaskPhase::new(phases::SNAPSHOT).expect("static phase"),
        )
        .with_instance(Some(instance_id.clone()));
        let task = self.tasks().accept(task_spec)?;
        let task_id = task.id.clone();
        let generation = task.attempt_generation;

        let session_id = registry.allocate_id();
        let session = LaunchSession::new(
            session_id.clone(),
            instance_id.clone(),
            operation_id.clone(),
            task_id.clone(),
            generation,
            key,
            false,
        );
        registry.insert(session.clone());
        drop(registry);

        self.publish_session(&session, events::SESSION_CHANGED, Value::Null);

        let job = PrepareJob {
            session_id: session_id.clone(),
            task_id: task_id.clone(),
            task_generation: generation,
            instance_id,
            expected_revision: request.expected_revision,
            start_again,
            quick_play_address,
            profile_ref,
            expected_offline_username,
        };
        let inner = self.clone();
        let job_task = task_id.clone();
        let handle = std::thread::Builder::new()
            .name(format!("02-prepare-{}", task_id.as_str()))
            .spawn(move || inner.run_prepare(job))
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        self.track(format!("prepare:{}", job_task.as_str()), handle);

        Ok(CommandResponse::Accepted {
            operation_id,
            task_id,
        })
    }

    fn run_prepare(self: Arc<Self>, job: PrepareJob) {
        let tasks = self.ctx.tasks().clone();
        let generation = job.task_generation;
        let _ = tasks.transition(&job.task_id, generation, TaskState::Running);

        let operation_id = self.sessions.lock().ok().and_then(|registry| {
            registry
                .get(&job.session_id)
                .map(|session| session.operation_id.clone())
        });
        let Some(operation_id) = operation_id else {
            self.fail_job(&job, ErrorEnvelope::internal_unavailable(), "lease-session");
            return;
        };
        let lease = match self
            .ports
            .leases
            .acquire(&job.instance_id, job.session_id.as_str(), &operation_id)
            .and_then(|lease| lease::LaunchLeaseGuard::new(lease, self.ports.supervisor.clone()))
        {
            Ok(lease) => Arc::new(lease),
            Err(error) => {
                self.fail_job(&job, error, "instance-lease");
                return;
            }
        };
        if let Some(preparer) = &self.ports.resource_preparer {
            if self.cancel_point(&job, phases::RESOURCE_PREPARE) {
                return;
            }
            self.begin_phase(&job, phases::RESOURCE_PREPARE);
            // The phase observer may request cancellation, just like a real UI.
            if self.cancel_point(&job, phases::RESOURCE_PREPARE) {
                return;
            }
            let prepared = preparer.prepare(
                &job.instance_id,
                job.session_id.as_str(),
                job.expected_revision,
                &job.task_id,
                generation,
            );
            if self.cancel_point(&job, phases::RESOURCE_PREPARE) {
                return;
            }
            if let Err(error) = prepared {
                self.fail_job(&job, error, phases::RESOURCE_PREPARE);
                return;
            }
        }
        let snapshot = match self
            .ports
            .instances
            .snapshot_under_lease(&job.instance_id, job.session_id.as_str())
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let code = error.code.as_str().to_string();
                self.fail_job(&job, error, &code);
                return;
            }
        };

        if let Some(expected) = job.expected_revision {
            if expected != snapshot.revision {
                let error =
                    errors::instance_stale_revision(expected.get(), snapshot.revision.get());
                self.fail_job(&job, error, "stale-revision");
                return;
            }
        }
        if job
            .expected_offline_username
            .as_ref()
            .is_some_and(|expected| snapshot.offline_username.as_ref() != Some(expected))
        {
            let error = ErrorEnvelope::new(
                shtmcl_domain::ErrorCode::from_static("launch.identity.changed"),
                shtmcl_domain::ErrorCategory::Conflict,
                false,
            );
            self.fail_job(&job, error, "identity-changed");
            return;
        }

        self.apply_session(&job, |session| {
            session.instance_revision = snapshot.revision;
            session.multi_open = snapshot.multi_open;
        });

        if job.start_again && !snapshot.multi_open {
            let error = errors::multi_open_disabled(job.instance_id.as_str());
            self.fail_job(&job, error, "multi-open");
            return;
        }
        if !job.start_again && snapshot.multi_open {
            let live = self
                .sessions
                .lock()
                .map(|registry| registry.live_for_instance(&job.instance_id).len())
                .unwrap_or(1);
            if live > 1 {
                let error = errors::with_detail(
                    errors::codes::SESSION_CONFLICT,
                    shtmcl_domain::ErrorCategory::Conflict,
                    false,
                    format!("session:{}:already-running", job.session_id.as_str()),
                );
                self.fail_job(&job, error, "session-conflict");
                return;
            }
        }

        // ---- phase: auth (independent checks run in parallel) ------------
        if self.cancel_point(&job, phases::AUTH) {
            return;
        }
        self.begin_phase(&job, phases::AUTH);

        let auth_requirement = snapshot.auth;
        let username = snapshot
            .offline_username
            .clone()
            .unwrap_or_else(|| "Player".to_string());
        let required = snapshot.collect_required();
        let accounts = self.ports.accounts.clone();
        let files = self.ports.files.clone();

        let (auth_result, validation) = std::thread::scope(|scope| {
            let auth_handle = {
                let accounts = accounts.clone();
                let username = username.clone();
                scope.spawn(move || accounts.ensure_game_session(auth_requirement, &username))
            };
            let file_handle = {
                let files = files.clone();
                let required = required.clone();
                scope.spawn(move || plan::validate_files(&required, files.as_ref()))
            };
            (
                auth_handle
                    .join()
                    .unwrap_or_else(|_| Err(ErrorEnvelope::internal_unavailable())),
                file_handle
                    .join()
                    .unwrap_or_else(|_| plan::FileValidation::default()),
            )
        });

        let game_session = match auth_result {
            Ok(session) => session,
            Err(error) => {
                self.fail_job(&job, error, "auth");
                return;
            }
        };

        // ---- phase: runtime ---------------------------------------------
        self.begin_phase(&job, phases::RUNTIME);
        let (requirement, requirement_verified) = match snapshot.java_requirement {
            Some(requirement) => (requirement, true),
            None => java::requirement_for_game_version(&snapshot.game_version),
        };
        if !requirement_verified {
            let _ = tasks.add_evidence(&job.task_id, evidence::JAVA_REQUIREMENT_UNVERIFIED);
        }
        // A runtime the instance pinned may live outside the conventional
        // locations, so its own directory is always part of this scan.
        let mut discovery_roots: Vec<PathBuf> = Vec::new();
        if let Some(pin) = snapshot.java_path.as_ref() {
            if let Some(root) = pin.parent().and_then(Path::parent) {
                discovery_roots.push(root.to_path_buf());
            }
        }
        if self.ensure_runtimes(&discovery_roots) == 0 {
            let error = errors::java_not_found(&requirement.describe());
            self.fail_job(&job, error, "runtime");
            return;
        }
        let mut constraint = RuntimeConstraint::new(
            requirement,
            if snapshot.java_requirement.is_some() {
                java::ConstraintSource::Instance
            } else {
                java::ConstraintSource::GameMetadata
            },
        );
        if let Some(path) = snapshot.java_path.clone() {
            constraint = constraint.with_explicit_path(path);
        }
        let runtimes = self
            .runtimes
            .lock()
            .map(|runtimes| runtimes.clone())
            .unwrap_or_default();
        let selection = java::select(&runtimes, &constraint, requirement_verified);
        let Some(runtime) = selection.selected.clone() else {
            let error = selection.failure_envelope(&constraint);
            self.fail_job(&job, error, "runtime");
            return;
        };
        if selection.arch_unverified {
            let _ = tasks.add_evidence(&job.task_id, evidence::JAVA_ARCH_UNVERIFIED);
        }

        // ---- phase: files (refusal point) -------------------------------
        if self.cancel_point(&job, phases::FILES) {
            return;
        }
        self.begin_phase(&job, phases::FILES);
        if !validation.is_complete() {
            let _ = tasks.add_evidence(&job.task_id, evidence::REQUIRED_FILES_MISSING);
            let error = errors::files_missing(validation.missing.len());
            let detail: Vec<String> = validation
                .missing
                .iter()
                .take(8)
                .map(|path| path.display().to_string())
                .collect();
            self.fail_job_with(&job, error, "files", detail.join(","));
            return;
        }

        // ---- phase: spawn ----------------------------------------------
        if self.cancel_point(&job, phases::SPAWN) {
            return;
        }
        self.begin_phase(&job, phases::SPAWN);
        match self.session_state_of(&job.session_id) {
            Some(LaunchSessionState::Preparing) => {}
            Some(other) => {
                let error = errors::with_detail(
                    errors::codes::SESSION_CONFLICT,
                    shtmcl_domain::ErrorCategory::Conflict,
                    false,
                    format!("session:{}:{}", job.session_id.as_str(), other.as_str()),
                );
                self.fail_job(&job, error, "spawn");
                return;
            }
            None => {
                let error = errors::session_not_found(job.session_id.as_str());
                self.fail_job(&job, error, "spawn");
                return;
            }
        }
        let _ = self.apply_session(&job, |session| {
            let _ = session.transition(LaunchSessionState::Spawning);
        });

        let quick_play = match self.quick_play_for(&snapshot, job.quick_play_address.as_deref()) {
            Ok(address) => address,
            Err(error) => {
                self.fail_job(&job, error, "quickplay");
                return;
            }
        };

        let plan_id = format!(
            "plan-{}-{}",
            snapshot.instance_id.as_str(),
            self.plan_counter.fetch_add(1, Ordering::Relaxed) + 1
        );
        let mut plan = match plan::build_plan(
            PlanInput {
                snapshot: &snapshot,
                runtime: &runtime,
                session: &game_session,
                quick_play_address: quick_play.as_deref(),
            },
            plan_id,
        ) {
            Ok(plan) => plan,
            Err(refusal) => {
                self.fail_job_with(&job, refusal.envelope, "plan", String::new());
                return;
            }
        };
        // A launch-time profile choice wins over the snapshot default.
        if job.profile_ref.is_some() {
            plan.profile_ref = job.profile_ref.clone();
        }

        let _ = self.apply_session(&job, |session| {
            session.plan_id = Some(plan.id.clone());
            session.runtime_id = Some(plan.runtime_id.clone());
            session.java_major = Some(plan.java_major);
            session.add_evidence(evidence::PLAN_FROZEN);
        });
        let _ = tasks.add_evidence(&job.task_id, evidence::PLAN_FROZEN);

        if let Err(error) = lease.check_before_spawn() {
            self.fail_job(&job, error, "instance-lease");
            return;
        }
        let handle = match self.ports.launcher.spawn(&plan.to_process_request()) {
            Ok(handle) => handle,
            Err(error) => {
                let failure = errors::spawn_failed(format!(
                    "spawn:{}:{}",
                    job.session_id.as_str(),
                    error.kind() as i32
                ));
                self.fail_job(&job, failure, "spawn");
                return;
            }
        };

        let binding = lease.attach_process(handle.pid);

        let identity = ProcessIdentity::new(
            handle.pid,
            now_unix_ms(),
            format!(
                "{}:{}:{}",
                job.session_id.as_str(),
                handle.pid,
                job.task_generation.get()
            ),
        );
        let updated = self.apply_session(&job, |session| {
            session.attach_process(identity.clone());
            let _ = session.transition(LaunchSessionState::ProcessRunning);
            session.add_evidence(evidence::PROCESS_CREATED);
        });
        let _ = tasks.add_evidence(&job.task_id, evidence::PROCESS_CREATED);

        if let Some(session) = updated.as_ref() {
            self.publish_session(
                session,
                events::PROCESS_STARTED,
                serde_json::json!({ "pid": handle.pid }),
            );
        }

        self.start_log_pump(&job.session_id, handle.pid);
        self.start_exit_watcher(&job.session_id, handle.pid, lease);

        if let Err(error) = binding {
            // A failed journal write is not proof that the spawned game exited.
            // Keep the session live and protected until the watcher confirms exit.
            if let Some(session) = self.apply_session(&job, |session| {
                session.error = Some(error.clone());
                session.add_evidence("failed:instance-process-binding");
            }) {
                self.publish_session(&session, events::SESSION_CHANGED, Value::Null);
            }
            let _ = tasks.fail(&job.task_id, generation, error);
            return;
        }

        // The spawn critical section decides what "cancelled" may mean now.
        if tasks.is_cancel_requested(&job.task_id, generation) {
            let session = self.apply_session(&job, |session| {
                session.add_evidence(evidence::CANCEL_DURING_SPAWN);
            });
            let _ = tasks.add_evidence(&job.task_id, evidence::CANCEL_DURING_SPAWN);
            if let Some(session) = session.as_ref() {
                self.publish_session(session, events::SESSION_CHANGED, Value::Null);
            }
            let result = serde_json::json!({
                "sessionId": job.session_id.as_str(),
                "planId": plan.id,
                "runtimeId": plan.runtime_id,
                "javaMajor": plan.java_major,
                "pid": handle.pid,
                "cancelDuringSpawn": true,
                "nextAction": "launch.stop",
            });
            let _ = tasks.succeed(&job.task_id, generation, Some(result));
            return;
        }

        let result = json_launch_result(&job.session_id, &plan, handle.pid);
        let _ = tasks.succeed(&job.task_id, generation, Some(result));
    }

    fn quick_play_for(
        &self,
        snapshot: &plan::InstanceLaunchSnapshot,
        requested: Option<&str>,
    ) -> Result<Option<String>, ErrorEnvelope> {
        let Some(address) = requested else {
            return Ok(None);
        };
        if address.trim().is_empty() {
            return Err(errors::request_invalid("quickPlayAddress:empty"));
        }
        let key = CapabilityKey::new(
            snapshot.game_version.clone(),
            snapshot.loader.clone(),
            snapshot.auth,
        );
        let entry = QuickPlayTable::lookup(&key);
        match QuickPlayTable::direct_join_argument(&entry) {
            Some(_) => Ok(Some(address.to_string())),
            None => Err(errors::with_detail(
                entry.reason_code,
                shtmcl_domain::ErrorCategory::Unsupported,
                false,
                format!("quickplay:{}", entry.capability.as_str()),
            )),
        }
    }

    /// Ensure runtimes are known. `extra` roots (for example the directory of a
    /// pinned runtime that lives outside the conventional locations) are always
    /// scanned; results are merged by path instead of replacing the cache.
    fn ensure_runtimes(&self, extra: &[PathBuf]) -> usize {
        let existing = self
            .runtimes
            .lock()
            .map(|runtimes| runtimes.len())
            .unwrap_or(0);
        if existing > 0 && extra.is_empty() {
            return existing;
        }
        let prober = JavaProber::new(self.ports.launcher.clone(), self.ports.supervisor.clone());
        let discovery =
            java::JavaDiscovery::new(prober, self.limits).with_cache(self.java_cache.clone());
        let roots = java::candidate_roots(self.ctx.dirs(), extra);
        let report = discovery.discover(&roots);

        let Ok(mut runtimes) = self.runtimes.lock() else {
            return report.runtimes.len();
        };
        for runtime in report.runtimes {
            if !runtimes
                .iter()
                .any(|known| java::same_file_path(&known.path, &runtime.path))
            {
                runtimes.push(runtime);
            }
        }
        runtimes.len()
    }

    // -- session reads -----------------------------------------------------

    fn session_get(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let id = LaunchSessionId::new(
            request
                .payload
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| errors::request_invalid("sessionId:missing"))?,
        )
        .map_err(|_| errors::request_invalid("sessionId:invalid"))?;
        let session = self
            .sessions
            .lock()
            .ok()
            .and_then(|registry| registry.get(&id).cloned())
            .ok_or_else(|| errors::session_not_found(id.as_str()))?;
        Ok(CommandResponse::Completed {
            result: session.to_json(),
        })
    }

    fn sessions_list(&self) -> Result<CommandResponse, ErrorEnvelope> {
        let sessions: Vec<Value> = self
            .session_snapshot()
            .iter()
            .map(LaunchSession::to_json)
            .collect();
        Ok(CommandResponse::Completed {
            result: serde_json::json!({ "sessions": sessions }),
        })
    }

    fn logs_page(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let id = LaunchSessionId::new(
            request
                .payload
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| errors::request_invalid("sessionId:missing"))?,
        )
        .map_err(|_| errors::request_invalid("sessionId:invalid"))?;
        let cursor = request.payload.get("cursor").and_then(Value::as_u64);
        let limit = request
            .payload
            .get("limit")
            .and_then(Value::as_u64)
            .map(|limit| limit as usize)
            .unwrap_or(logs::DEFAULT_PAGE_LIMIT);
        let page = self.logs.page(&id, cursor, limit).ok_or_else(|| {
            errors::with_detail(
                errors::codes::LOGS_UNAVAILABLE,
                shtmcl_domain::ErrorCategory::NotFound,
                false,
                format!("session:{}", id.as_str()),
            )
        })?;
        Ok(CommandResponse::Completed {
            result: page.to_json(),
        })
    }

    // -- launch.stop -------------------------------------------------------

    fn launch_stop(
        self: &Arc<Self>,
        request: &CommandRequest,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        request.validate(CommandKind::Mutate)?;
        let operation_id = request
            .operation_id
            .clone()
            .ok_or_else(|| ErrorEnvelope::command_operation_required())?;
        let id = LaunchSessionId::new(
            request
                .payload
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| errors::request_invalid("sessionId:missing"))?,
        )
        .map_err(|_| errors::request_invalid("sessionId:invalid"))?;
        let expected_pid = request.payload.get("expectedPid").and_then(Value::as_u64);

        let session = self
            .sessions
            .lock()
            .ok()
            .and_then(|registry| registry.get(&id).cloned())
            .ok_or_else(|| errors::session_not_found(id.as_str()))?;

        let task = self.tasks().accept(
            TaskSpec::new(
                operation_id.clone(),
                TaskPhase::new(phases::STOP).expect("static phase"),
            )
            .with_instance(Some(session.instance_id.clone()))
            .with_input_revision(Some(session.instance_revision)),
        )?;
        let task_id = task.id.clone();
        let generation = task.attempt_generation;

        let inner = self.clone();
        let job_session = id.clone();
        let job_task = task_id.clone();
        let handle = std::thread::Builder::new()
            .name(format!("02-stop-{}", task_id.as_str()))
            .spawn(move || inner.run_stop(job_task, generation, job_session, expected_pid))
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        self.track(format!("stop:{}", task_id.as_str()), handle);

        Ok(CommandResponse::Accepted {
            operation_id,
            task_id,
        })
    }

    fn run_stop(
        self: Arc<Self>,
        task_id: TaskId,
        generation: AttemptGeneration,
        session_id: LaunchSessionId,
        expected_pid: Option<u64>,
    ) {
        let tasks = self.ctx.tasks().clone();
        let _ = tasks.transition(&task_id, generation, TaskState::Running);

        let session = self
            .sessions
            .lock()
            .ok()
            .and_then(|registry| registry.get(&session_id).cloned());
        let Some(session) = session else {
            let _ = tasks.fail(
                &task_id,
                generation,
                errors::session_not_found(session_id.as_str()),
            );
            return;
        };
        let Some(identity) = session.identity.clone() else {
            let _ = tasks.fail(
                &task_id,
                generation,
                errors::process_mismatch(session_id.as_str()),
            );
            return;
        };
        if let Some(expected) = expected_pid {
            if expected as u32 != identity.pid {
                let _ = tasks.fail(
                    &task_id,
                    generation,
                    errors::process_mismatch(session_id.as_str()),
                );
                return;
            }
        }

        if !self.ports.supervisor.is_alive(identity.pid) {
            let updated = self.apply_session_id(&session_id, |session| {
                session.add_evidence(evidence::ALREADY_EXITED);
                if session.state.is_live() {
                    let _ = session.transition(LaunchSessionState::Exited);
                }
            });
            let _ = tasks.add_evidence(&task_id, evidence::ALREADY_EXITED);
            if let Some(session) = updated {
                self.publish_session(&session, events::SESSION_CHANGED, Value::Null);
            }
            let _ = tasks.succeed(
                &task_id,
                generation,
                Some(
                    serde_json::json!({ "sessionId": session_id.as_str(), "alreadyExited": true }),
                ),
            );
            return;
        }

        // Record intent before signalling: the exit watcher can run as soon as
        // the platform terminates the process.
        self.apply_session_id(&session_id, |session| {
            session.add_evidence(evidence::STOP_REQUESTED);
        });
        match self.ports.supervisor.request_stop(identity.pid) {
            Ok(()) => {
                let updated = self.apply_session_id(&session_id, |session| {
                    session.add_evidence(evidence::STOP_REQUESTED);
                });
                let _ = tasks.add_evidence(&task_id, evidence::STOP_REQUESTED);
                if let Some(session) = updated {
                    self.publish_session(&session, events::SESSION_CHANGED, Value::Null);
                }
                let _ = tasks.succeed(
                    &task_id,
                    generation,
                    Some(serde_json::json!({
                        "sessionId": session_id.as_str(),
                        "pid": identity.pid,
                        "stopRequested": true,
                    })),
                );
            }
            Err(error) => {
                let failure = errors::with_detail(
                    errors::codes::PROCESS_MISMATCH,
                    shtmcl_domain::ErrorCategory::Unsupported,
                    true,
                    format!("stop:{}:{}", session_id.as_str(), error.kind() as i32),
                );
                let _ = tasks.fail(&task_id, generation, failure);
            }
        }
    }

    // -- quickplay.capabilities -------------------------------------------

    fn quickplay_request(
        &self,
        request: &CommandRequest,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        let payload = &request.payload;
        let game_version = payload
            .get("gameVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| errors::request_invalid("gameVersion:missing"))?;
        let family = payload
            .get("loaderFamily")
            .and_then(Value::as_str)
            .unwrap_or("vanilla");
        let loader_version = payload.get("loaderVersion").and_then(Value::as_str);
        let auth = payload
            .get("auth")
            .and_then(Value::as_str)
            .map(|value| {
                AuthRequirement::parse(value)
                    .ok_or_else(|| errors::request_invalid(format!("auth:{value}")))
            })
            .transpose()?
            .unwrap_or(AuthRequirement::Any);

        let loader = plan::LoaderKind::parse(family, loader_version);
        let key = CapabilityKey::new(game_version, loader.clone(), auth);
        let entry = QuickPlayTable::lookup(&key);
        Ok(CommandResponse::Completed {
            result: serde_json::json!({
                "gameVersion": game_version,
                "loader": loader.to_json(),
                "auth": auth.as_str(),
                "result": entry.to_json(),
                "matrix": QuickPlayTable::matrix(game_version, loader),
            }),
        })
    }

    // -- helpers -----------------------------------------------------------

    fn begin_phase(&self, job: &PrepareJob, phase: &'static str) {
        let _ = self.tasks().set_phase(
            &job.task_id,
            job.task_generation,
            TaskPhase::new(phase).expect("static phase"),
        );
        self.ports.steps.on_phase(phase);
    }

    /// Cooperative cancellation check between phases; reaches a safe stop.
    fn cancel_point(&self, job: &PrepareJob, _phase: &'static str) -> bool {
        if !self
            .tasks()
            .is_cancel_requested(&job.task_id, job.task_generation)
        {
            return false;
        }
        let _ = self
            .tasks()
            .add_evidence(&job.task_id, evidence::SAFE_STOP_REACHED);
        let session = self.apply_session(job, |session| {
            session.add_evidence(evidence::SAFE_STOP_REACHED);
            if session.state.is_live() {
                let _ = session.transition(LaunchSessionState::Cancelled);
            }
        });
        if let Some(session) = session {
            self.publish_session(&session, events::SESSION_CHANGED, Value::Null);
        }
        let _ = self.tasks().cancel_now(&job.task_id, job.task_generation);
        true
    }

    fn apply_session(
        &self,
        job: &PrepareJob,
        mutate: impl FnOnce(&mut LaunchSession),
    ) -> Option<LaunchSession> {
        self.apply_session_id(&job.session_id, mutate)
    }

    fn apply_session_id(
        &self,
        id: &LaunchSessionId,
        mutate: impl FnOnce(&mut LaunchSession),
    ) -> Option<LaunchSession> {
        self.sessions
            .lock()
            .ok()
            .and_then(|mut registry| registry.apply(id, mutate))
    }

    fn session_state_of(&self, id: &LaunchSessionId) -> Option<LaunchSessionState> {
        self.sessions
            .lock()
            .ok()
            .and_then(|registry| registry.get(id).map(|session| session.state))
    }

    fn fail_job(&self, job: &PrepareJob, error: ErrorEnvelope, marker: &str) {
        self.fail_job_with(job, error, marker, String::new());
    }

    fn fail_job_with(
        &self,
        job: &PrepareJob,
        error: ErrorEnvelope,
        marker: &str,
        extra_detail: String,
    ) {
        let tasks = self.ctx.tasks();
        let mut error = error;
        if !extra_detail.is_empty() {
            let detail = match error.detail_ref.take() {
                Some(existing) => format!("{existing}|{extra_detail}"),
                None => extra_detail,
            };
            error.detail_ref = Some(detail);
        }
        let session = self.apply_session(job, |session| {
            session.error = Some(error.clone());
            session.add_evidence(format!("failed:{marker}"));
            if session.state.is_live() {
                let _ = session.transition(LaunchSessionState::Failed);
            }
        });
        if let Some(session) = session {
            self.publish_session(&session, events::SESSION_CHANGED, Value::Null);
        }
        let _ = tasks.fail(&job.task_id, job.task_generation, error);
    }

    fn publish_session(&self, session: &LaunchSession, kind: &'static str, extra: Value) {
        let mut payload = serde_json::json!({
            "sessionId": session.id.as_str(),
            "instanceId": session.instance_id.as_str(),
            "state": session.state.as_str(),
        });
        if let (Some(object), Value::Object(extra)) = (payload.as_object_mut(), extra) {
            for (key, value) in extra {
                object.insert(key, value);
            }
        }
        let entity = EntityId::new(session.instance_id.as_str().to_string()).ok();
        let _ = self.ctx.events().publish(
            EventRequest::new(
                session.operation_id.clone(),
                session.attempt_generation,
                EventKind::from_static(kind),
            )
            .with_entity(entity)
            .with_revision(session.instance_revision)
            .with_payload(payload),
        );
    }

    /// Module 02 is the only reader of the game's stdout/stderr.
    fn start_log_pump(self: &Arc<Self>, session: &LaunchSessionId, pid: u32) {
        let Some(streams) = self.ports.supervisor.take_streams(pid) else {
            return;
        };
        self.logs.ensure(session);
        let max_line = self.logs.max_line_bytes();
        let session_id = session.clone();

        for (stream, reader) in [
            (logs::LogStream::Stdout, streams.stdout),
            (logs::LogStream::Stderr, streams.stderr),
        ] {
            let pipeline = self.logs.clone();
            let session_id = session_id.clone();
            let label = format!("log:{}:{}", session_id.as_str(), stream.as_str());
            let spawned = std::thread::Builder::new()
                .name(format!(
                    "02-log-{}-{}",
                    session_id.as_str(),
                    stream.as_str()
                ))
                .spawn(move || {
                    let mut reader = reader;
                    logs::drain_reader(reader.as_mut(), max_line, |line, omitted| {
                        pipeline.ingest(&session_id, stream, line, omitted);
                    });
                });
            // The pump is tracked so tests can join it, but nothing on a UI path
            // ever waits for it: the pipe keeps draining regardless.
            if let Ok(handle) = spawned {
                self.track(label, handle);
            }
        }
    }

    /// Observe the process until it exits, then close the session.
    ///
    /// A crashed game is *recorded*, never restarted: the module creates exactly
    /// one process per session and leaves the retry decision to the user.
    fn start_exit_watcher(
        self: &Arc<Self>,
        session: &LaunchSessionId,
        pid: u32,
        lease: Arc<lease::LaunchLeaseGuard>,
    ) {
        let inner = self.clone();
        let session_id = session.clone();
        let label = format!("exit:{}", session_id.as_str());
        let watcher_lease = lease.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("02-exit-{}", session_id.as_str()))
            .spawn(move || {
                inner.watch_exit(&session_id, pid, watcher_lease);
            });
        match spawned {
            Ok(handle) => self.track(label, handle),
            // Thread creation failure cannot release protection around a live game.
            Err(_) => self.watch_exit(session, pid, lease),
        }
    }

    fn watch_exit(
        &self,
        session: &LaunchSessionId,
        pid: u32,
        _lease: Arc<lease::LaunchLeaseGuard>,
    ) {
        loop {
            match self
                .ports
                .supervisor
                .wait_for_exit(pid, Some(Duration::from_millis(250)))
            {
                WaitOutcome::Exited(code) => {
                    self.finish_session(session, pid, Some(code));
                    break;
                }
                WaitOutcome::TimedOut => {}
                // Unknown is not evidence of exit. Preserve the lease and retry.
                WaitOutcome::Unknown => std::thread::sleep(Duration::from_millis(250)),
            }
        }
    }

    fn finish_session(&self, session_id: &LaunchSessionId, pid: u32, exit_code: Option<i32>) {
        let game_reached = self.ports.game.reached_game(pid);
        let reached = game_reached == Some(true);
        let updated = self.apply_session_id(session_id, |session| {
            session.exit_code = exit_code;
            session.game_reached = game_reached;
            if reached {
                session.add_evidence(evidence::GAME_REACHED);
            }
            if session.state.is_live() {
                let _ = session.transition(LaunchSessionState::Exited);
            }
        });
        let Some(session) = updated else {
            return;
        };
        self.publish_session(
            &session,
            events::PROCESS_EXITED,
            serde_json::json!({ "pid": pid, "exitCode": exit_code }),
        );

        // Exit without evidence that the game ever reported itself: record the
        // cause on the session so the UI can offer a real next step.
        if !reached
            && !session
                .evidence
                .iter()
                .any(|item| item == evidence::STOP_REQUESTED)
        {
            let error = errors::process_exited_early(exit_code);
            if let Some(session) = self.apply_session_id(session_id, |session| {
                session.error = Some(error.clone());
                session.add_evidence("process_exited_early");
            }) {
                self.publish_session(&session, events::SESSION_CHANGED, Value::Null);
            }
        }
    }
}

fn now_unix_ms() -> u64 {
    session::now_unix_ms()
}

fn optional_bounded_string(
    payload: &Value,
    key: &'static str,
    max_bytes: usize,
) -> Result<Option<String>, ErrorEnvelope> {
    match payload.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.len() <= max_bytes => {
            let value = value.trim();
            if value.is_empty() {
                Ok(None)
            } else {
                Ok(Some(value.to_owned()))
            }
        }
        _ => Err(errors::request_invalid(format!("{key}:invalid"))),
    }
}

fn optional_usize(
    payload: &Value,
    key: &'static str,
    default: usize,
) -> Result<usize, ErrorEnvelope> {
    match payload.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| errors::request_invalid(format!("{key}:invalid"))),
    }
}

fn catalog_unavailable() -> ErrorEnvelope {
    errors::with_detail(
        errors::codes::CATALOG_UNAVAILABLE,
        shtmcl_domain::ErrorCategory::Network,
        true,
        "catalog:not_configured",
    )
}

fn catalog_error(error: catalog::VersionCatalogError) -> ErrorEnvelope {
    if matches!(&error, catalog::VersionCatalogError::InvalidPage) {
        return errors::request_invalid(error.detail_ref());
    }
    errors::with_detail(
        errors::codes::CATALOG_UNAVAILABLE,
        shtmcl_domain::ErrorCategory::Network,
        error.is_retryable(),
        error.detail_ref(),
    )
}

fn resolve_instance_id(request: &CommandRequest) -> Result<InstanceId, ErrorEnvelope> {
    if let Some(value) = request.payload.get("instanceId").and_then(Value::as_str) {
        return InstanceId::new(value).map_err(|_| errors::request_invalid("instanceId:invalid"));
    }
    if let Some(entity) = request.entity_id.as_ref() {
        return InstanceId::new(entity.as_str())
            .map_err(|_| errors::request_invalid("entityId:invalid"));
    }
    Err(errors::request_invalid("instanceId:missing"))
}

fn json_selection(
    selection: &Selection,
    constraint: &RuntimeConstraint,
    cache: CacheStats,
) -> Value {
    serde_json::json!({
        "requirement": constraint.requirement.to_json(),
        "requirementSource": constraint.source.as_str(),
        "requirementVerified": selection.requirement_verified,
        "runtime": selection.selected.as_ref().map(JavaRuntime::to_json),
        "rejected": selection.rejected.iter().map(|rejected| rejected.to_json()).collect::<Vec<_>>(),
        "archUnverified": selection.arch_unverified,
        "cache": { "hits": cache.hits, "misses": cache.misses, "entries": cache.entries },
    })
}

fn json_launch_result(session: &LaunchSessionId, plan: &LaunchPlan, pid: u32) -> Value {
    serde_json::json!({
        "sessionId": session.as_str(),
        "planId": plan.id,
        "runtimeId": plan.runtime_id,
        "javaMajor": plan.java_major,
        "pid": pid,
        "argsDigest": plan.args_digest,
        "sensitiveArgs": plan.sensitive_args,
        "gameVersion": plan.game_version,
        "loader": plan.loader.to_json(),
        "quickPlay": plan.quick_play,
        "plan": plan.summary_json(),
    })
}

/// A `GameSession` is exposed so callers that own 05's adapter can construct one.
pub use ports::GameSession as LaunchGameSession;

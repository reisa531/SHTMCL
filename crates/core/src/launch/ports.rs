//! Ports (traits) 02 needs from modules that are not delivered yet, plus the
//! conservative defaults the application gets until each real implementation is
//! registered.
//!
//! Ownership: 01 owns instance snapshots, 05 owns account/game-session handles,
//! 08 owns the LogStore sink, the application layer owns the OS process
//! implementation behind `shtmcl_platform::ProcessLauncher`. None of them exist
//! yet, so this module defines the *shape* it consumes and never fabricates a
//! real instance, account or game process: a missing port fails closed with a
//! stable code.
//!
//! Every port here is a test seam. The behaviour tests instantiate controlled
//! doubles; they are not a substitute for the real adapter.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use shtmcl_domain::{AttemptGeneration, ErrorEnvelope, InstanceId, OperationId, Revision, TaskId};
use shtmcl_platform::fs;
use shtmcl_platform::process::{ProcessHandle, ProcessLauncher, ProcessRequest};

use crate::launch::errors;
use crate::launch::java::JavaRequirement;
use crate::launch::logs::{LogSink, NoopLogSink};
use crate::launch::plan::InstanceLaunchSnapshot;

/// How much authentication the frozen snapshot says this launch needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRequirement {
    /// A local/offline identity is enough (the instance accepts it).
    Offline,
    /// A provider-backed game session is mandatory (online-mode servers).
    Online,
    /// Either is acceptable; prefer the provider session when one exists.
    Any,
}

impl AuthRequirement {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "offline" => Some(Self::Offline),
            "online" => Some(Self::Online),
            "any" => Some(Self::Any),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Online => "online",
            Self::Any => "any",
        }
    }
}

/// A game-session handle produced by module 05.
///
/// The access token lives only in memory, is never serialized into a plan,
/// event, log line or error and `Debug` redacts it so an accidental
/// `{:?}` cannot leak it.
#[derive(Clone, PartialEq, Eq)]
pub struct GameSession {
    pub account_id: Option<String>,
    pub username: String,
    pub uuid: String,
    pub user_type: String,
    access_token: String,
}

impl GameSession {
    pub fn new(
        account_id: Option<String>,
        username: impl Into<String>,
        uuid: impl Into<String>,
        user_type: impl Into<String>,
        access_token: impl Into<String>,
    ) -> Self {
        Self {
            account_id,
            username: username.into(),
            uuid: uuid.into(),
            user_type: user_type.into(),
            access_token: access_token.into(),
        }
    }

    /// Local identity used when the instance accepts an offline launch.
    pub fn offline(username: impl Into<String>) -> Self {
        Self {
            account_id: None,
            username: username.into(),
            uuid: "00000000000000000000000000000000".to_string(),
            user_type: "legacy".to_string(),
            access_token: "0".to_string(),
        }
    }

    /// The token, for the spawn argument list only.
    pub fn access_token(&self) -> &str {
        &self.access_token
    }
}

impl fmt::Debug for GameSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GameSession")
            .field("account_id", &self.account_id)
            .field("username", &self.username)
            .field("uuid", &self.uuid)
            .field("user_type", &self.user_type)
            .field("access_token", &"<redacted>")
            .finish()
    }
}

/// Settings needed before resource restoration and Java installation.
#[derive(Debug, Clone)]
pub struct InstancePreparation {
    pub revision: Revision,
    pub offline_username: Option<String>,
    pub java_requirement: Option<JavaRequirement>,
    pub java_path: Option<PathBuf>,
}

/// 01's frozen instance snapshot and its lightweight preparation settings.
pub trait InstanceSnapshotSource: Send + Sync {
    fn snapshot(&self, instance_id: &InstanceId) -> Result<InstanceLaunchSnapshot, ErrorEnvelope>;

    /// Read launch settings without requiring installed resources to be complete.
    /// Adapters with separate settings and resource validation should override this.
    fn preparation(&self, instance_id: &InstanceId) -> Result<InstancePreparation, ErrorEnvelope> {
        let snapshot = self.snapshot(instance_id)?;
        Ok(InstancePreparation {
            revision: snapshot.revision,
            offline_username: snapshot.offline_username,
            java_requirement: snapshot.java_requirement,
            java_path: snapshot.java_path,
        })
    }

    /// Production adapters verify that a busy instance belongs to this holder.
    fn snapshot_under_lease(
        &self,
        instance_id: &InstanceId,
        _holder: &str,
    ) -> Result<InstanceLaunchSnapshot, ErrorEnvelope> {
        self.snapshot(instance_id)
    }
}

/// Verify and restore launch resources while the caller owns the launch lease.
/// Implementations must honor the task's cancellation and attempt generation.
pub trait LaunchResourcePreparer: Send + Sync {
    fn prepare(
        &self,
        instance_id: &InstanceId,
        holder: &str,
        expected_revision: Option<Revision>,
        task_id: &TaskId,
        generation: AttemptGeneration,
    ) -> Result<(), ErrorEnvelope>;
}

/// Owned instance protection. The adapter releases its holder on Drop.
/// Renew must reject expired or displaced holders rather than reclaim a lease.
pub trait InstanceLaunchLease: Send {
    fn renew(&mut self) -> Result<(), ErrorEnvelope>;

    /// Persist the created process identity before reporting launch success.
    fn bind_process(&mut self, _pid: u32) -> Result<(), ErrorEnvelope> {
        Ok(())
    }
}

pub trait InstanceLaunchLeaseSource: Send + Sync {
    fn acquire(
        &self,
        instance_id: &InstanceId,
        holder: &str,
        operation_id: &OperationId,
    ) -> Result<Box<dyn InstanceLaunchLease>, ErrorEnvelope>;
}

#[derive(Debug, Default)]
pub struct UnavailableInstanceLeases;

impl InstanceLaunchLeaseSource for UnavailableInstanceLeases {
    fn acquire(
        &self,
        _instance_id: &InstanceId,
        _holder: &str,
        _operation_id: &OperationId,
    ) -> Result<Box<dyn InstanceLaunchLease>, ErrorEnvelope> {
        Err(errors::instance_unavailable())
    }
}

/// 05's restricted game-session handle.
pub trait AccountSessionSource: Send + Sync {
    fn ensure_game_session(
        &self,
        requirement: AuthRequirement,
        fallback_username: &str,
    ) -> Result<GameSession, ErrorEnvelope>;
}

/// File existence probe. Defaults to the real filesystem; tests inject a map.
pub trait FileProbe: Send + Sync {
    fn exists(&self, path: &Path) -> bool;
}

/// Owned pipes and supervision of an already-created process are platform ports
/// (owned by 00 in `crates/platform`), re-exported here so every consumer of
/// `launch::ports` uses the one definition instead of a copy.
pub use shtmcl_platform::process::{ChildStreams, ProcessSupervisor, WaitOutcome};

/// Evidence-based check for "the game actually reported itself".
pub trait GameProbe: Send + Sync {
    fn reached_game(&self, pid: u32) -> Option<bool>;
}

/// Test/telemetry seam invoked between preparation phases.
///
/// The real implementation is a no-op; behaviour tests use it to request a
/// cancellation at a deterministic phase boundary.
pub trait StepObserver: Send + Sync {
    fn on_phase(&self, phase: &'static str);
}

// ---------------------------------------------------------------------------
// Conservative defaults
// ---------------------------------------------------------------------------

/// 01 has not delivered a snapshot source yet.
#[derive(Debug, Default)]
pub struct UnavailableInstances;

impl InstanceSnapshotSource for UnavailableInstances {
    fn snapshot(&self, _instance_id: &InstanceId) -> Result<InstanceLaunchSnapshot, ErrorEnvelope> {
        Err(errors::instance_unavailable())
    }
}

/// 05 has not delivered a session source yet.
#[derive(Debug, Default)]
pub struct UnavailableAccounts;

impl AccountSessionSource for UnavailableAccounts {
    fn ensure_game_session(
        &self,
        _requirement: AuthRequirement,
        _fallback_username: &str,
    ) -> Result<GameSession, ErrorEnvelope> {
        Err(errors::account_required("account-port:unavailable"))
    }
}

/// Real filesystem probe.
#[derive(Debug, Default)]
pub struct FsFileProbe;

impl FileProbe for FsFileProbe {
    fn exists(&self, path: &Path) -> bool {
        fs::probe_file(path).is_ok()
    }
}

/// No process supervision until the application layer registers one.
#[derive(Debug, Default)]
pub struct UnavailableSupervisor;
impl ProcessSupervisor for UnavailableSupervisor {
    fn take_streams(&self, _pid: u32) -> Option<ChildStreams> {
        None
    }

    fn wait_for_exit(&self, _pid: u32, _timeout: Option<Duration>) -> WaitOutcome {
        WaitOutcome::Unknown
    }

    fn is_alive(&self, _pid: u32) -> bool {
        false
    }

    fn request_stop(&self, _pid: u32) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no process supervisor registered",
        ))
    }
}

/// A launcher that refuses to create anything.
#[derive(Debug, Default)]
pub struct UnavailableLauncher;

impl ProcessLauncher for UnavailableLauncher {
    fn spawn(&self, _request: &ProcessRequest) -> std::io::Result<ProcessHandle> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no process launcher registered",
        ))
    }
}

/// "Did the game start?" stays unknown.
#[derive(Debug, Default)]
pub struct NoopGameProbe;

impl GameProbe for NoopGameProbe {
    fn reached_game(&self, _pid: u32) -> Option<bool> {
        None
    }
}

/// Real step observer: nothing to do.
#[derive(Debug, Default)]
pub struct NoopStepObserver;

impl StepObserver for NoopStepObserver {
    fn on_phase(&self, _phase: &'static str) {}
}

/// Everything module 02 consumes from the outside world.
#[derive(Clone)]
pub struct LaunchPorts {
    pub instances: Arc<dyn InstanceSnapshotSource>,
    pub leases: Arc<dyn InstanceLaunchLeaseSource>,
    pub resource_preparer: Option<Arc<dyn LaunchResourcePreparer>>,
    pub accounts: Arc<dyn AccountSessionSource>,
    pub launcher: Arc<dyn ProcessLauncher>,
    pub supervisor: Arc<dyn ProcessSupervisor>,
    pub files: Arc<dyn FileProbe>,
    pub logs: Arc<dyn LogSink>,
    pub game: Arc<dyn GameProbe>,
    pub steps: Arc<dyn StepObserver>,
}

impl LaunchPorts {
    /// Defaults used by [`crate::launch::module`]: every undelivered dependency
    /// fails closed, the filesystem probe is real and the log sink is a no-op.
    pub fn unavailable() -> Self {
        Self {
            instances: Arc::new(UnavailableInstances),
            leases: Arc::new(UnavailableInstanceLeases),
            resource_preparer: None,
            accounts: Arc::new(UnavailableAccounts),
            launcher: Arc::new(UnavailableLauncher),
            supervisor: Arc::new(UnavailableSupervisor),
            files: Arc::new(FsFileProbe),
            logs: Arc::new(NoopLogSink),
            game: Arc::new(NoopGameProbe),
            steps: Arc::new(NoopStepObserver),
        }
    }

    pub fn with_instances(mut self, instances: Arc<dyn InstanceSnapshotSource>) -> Self {
        self.instances = instances;
        self
    }

    pub fn with_resource_preparer(mut self, preparer: Arc<dyn LaunchResourcePreparer>) -> Self {
        self.resource_preparer = Some(preparer);
        self
    }

    pub fn with_accounts(mut self, accounts: Arc<dyn AccountSessionSource>) -> Self {
        self.accounts = accounts;
        self
    }

    pub fn with_leases(mut self, leases: Arc<dyn InstanceLaunchLeaseSource>) -> Self {
        self.leases = leases;
        self
    }

    pub fn with_process(
        mut self,
        launcher: Arc<dyn ProcessLauncher>,
        supervisor: Arc<dyn ProcessSupervisor>,
    ) -> Self {
        self.launcher = launcher;
        self.supervisor = supervisor;
        self
    }

    pub fn with_files(mut self, files: Arc<dyn FileProbe>) -> Self {
        self.files = files;
        self
    }

    pub fn with_logs(mut self, logs: Arc<dyn LogSink>) -> Self {
        self.logs = logs;
        self
    }

    pub fn with_game_probe(mut self, game: Arc<dyn GameProbe>) -> Self {
        self.game = game;
        self
    }

    pub fn with_steps(mut self, steps: Arc<dyn StepObserver>) -> Self {
        self.steps = steps;
        self
    }
}

impl fmt::Debug for LaunchPorts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchPorts").finish_non_exhaustive()
    }
}

/// Convenience for tests and for a launcher that wants an empty environment.
pub fn process_request(
    program: PathBuf,
    working_dir: PathBuf,
    args: Vec<String>,
) -> ProcessRequest {
    ProcessRequest {
        program,
        args,
        working_dir,
        environment: BTreeMap::new(),
    }
}

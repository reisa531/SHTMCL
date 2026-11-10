//! Install / update / rollback runner.
//!
//! The runner is the only place 04 turns a `ChangePlan` into filesystem effects,
//! and it never writes files itself: staging goes to the artifact port (03) and
//! the commit goes to the instance port (01). Cancellation is cooperative and only
//! confirmed at a safe point:
//!
//! - before the commit request, a cancellation discards staging and returns
//!   [`InstallOutcome::RolledBack`]; the caller then calls `cancel_now`,
//! - after the commit succeeded, a cancellation is too late and is reported as
//!   `cancelled_too_late` instead of pretending the write did not happen.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use shtmcl_domain::{
    AttemptGeneration, ProgressUnit, Revision, TaskId, TaskPhase, TaskProgress, TaskState,
};

use crate::tasks::TaskService;

use super::error::ContentError;
use super::plan::{
    ChangePlan, InstanceContentView, InstanceFile, PlanAction, digest_of, rollback_plan,
};
use super::ports::ContentPorts;
use super::recovery::{PlannedFile, RecoveryKind, RecoveryRecord, Stage};

/// Stable phase IDs published through `TaskService::set_phase`.
///
/// They live in the `content.phase.*` sub-namespace so a phase id can never be
/// confused with a command name (`content.plan` is a command; `content.phase.plan`
/// is the stage that task is in).
pub mod phases {
    pub const PLAN: &str = "content.phase.plan";
    pub const STAGE: &str = "content.phase.stage";
    pub const VERIFY: &str = "content.phase.verify";
    pub const COMMIT: &str = "content.phase.commit";
    pub const FINALIZE: &str = "content.phase.finalize";
    pub const ROLLBACK: &str = "content.phase.rollback";
    pub const RESUME: &str = "content.phase.resume";
}

/// Evidence markers this module actually produces. Copy that asserts an outcome
/// may only be shown when the matching marker is present in `Task.evidence`.
pub mod evidence {
    pub const SAFE_STOP_REACHED: &str = "content_safe_stop_reached";
    pub const STAGING_DISCARDED: &str = "content_staging_discarded";
    pub const COMMIT_CONFIRMED: &str = "content_commit_confirmed";

    pub const ROLLBACK_APPLIED: &str = "content_rollback_applied";
    pub const CANCEL_TOO_LATE: &str = "content_cancel_too_late";
    pub const RESTRICTED_ARTIFACTS_PENDING: &str = "content_restricted_artifacts_pending";
    pub const COMMIT_OUTCOME_UNKNOWN: &str = "content_commit_outcome_unknown";
    /// An undecidable resume kept its recovery record as a rollback point.
    pub const ROLLBACK_POINT_KEPT: &str = "content_rollback_point_kept";
    pub const RECOVERY_DISCARDED: &str = "content_recovery_discarded";
    /// Staging cleanup failed at a safe stop: the recovery record was kept.
    pub const STAGING_DISCARD_FAILED: &str = "content_staging_discard_failed";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeStatus {
    /// No recovery record existed.
    NothingPending,
    /// Staging was dropped; the instance was never touched.
    DiscardedStaging,
    /// The commit had already landed; the record is now a rollback point.
    FinalizedCommit,
    /// The instance content does not match the planned content; needs a new plan.
    CommitOutcomeUnknown,
    /// A previous attempt completed and left a rollback point.
    AlreadyCommitted,
}

impl ResumeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NothingPending => "nothing_pending",
            Self::DiscardedStaging => "discarded_staging",
            Self::FinalizedCommit => "finalized_commit",
            Self::CommitOutcomeUnknown => "commit_outcome_unknown",
            Self::AlreadyCommitted => "already_committed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed {
        plan_id: String,
        revision: Revision,
        cancelled_too_late: bool,
    },
    /// Cancelled (or stopped) before the commit; nothing was written.
    RolledBack {
        plan_id: String,
        at_phase: String,
    },
    /// A rollback plan was committed; the instance is back to the recorded set.
    RestoredPrevious {
        plan_id: String,
        revision: Revision,
        cancelled_too_late: bool,
    },
    Resumed {
        status: ResumeStatus,
    },
}

impl InstallOutcome {
    pub fn to_value(&self) -> Value {
        match self {
            Self::Installed {
                plan_id,
                revision,
                cancelled_too_late,
            } => json!({
                "kind": "installed",
                "planId": plan_id,
                "revision": revision.get(),
                "cancelledTooLate": cancelled_too_late,
            }),
            Self::RolledBack { plan_id, at_phase } => json!({
                "kind": "rolled_back",
                "planId": plan_id,
                "atPhase": at_phase,
            }),
            Self::RestoredPrevious {
                plan_id,
                revision,
                cancelled_too_late,
            } => json!({
                "kind": "restored_previous",
                "planId": plan_id,
                "revision": revision.get(),
                "cancelledTooLate": cancelled_too_late,
            }),
            Self::Resumed { status } => json!({
                "kind": "resumed",
                "status": status.as_str(),
            }),
        }
    }

    /// `true` when the run should end as `cancelled` rather than `succeeded`.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::RolledBack { .. })
    }
}

/// What the runner reports to the task owner between steps.
pub trait InstallProgress: Send + Sync {
    fn enter_phase(&self, phase: &str) -> Result<(), ContentError>;
    fn evidence(&self, marker: &str);
    fn cancel_requested(&self) -> bool;
    fn advance(&self, done: u64, total: u64) -> Result<(), ContentError>;
}

/// Real adapter: everything goes through the shared [`TaskService`], so the app
/// layer keeps a single scheduler and a single cancellation entry point.
pub struct TaskProgressAdapter {
    tasks: Arc<dyn TaskService>,
    task_id: TaskId,
    generation: AttemptGeneration,
    started: Mutex<bool>,
}

impl TaskProgressAdapter {
    pub fn new(
        tasks: Arc<dyn TaskService>,
        task_id: TaskId,
        generation: AttemptGeneration,
    ) -> Self {
        Self {
            tasks,
            task_id,
            generation,
            started: Mutex::new(false),
        }
    }

    fn task_error(error: shtmcl_domain::ErrorEnvelope) -> ContentError {
        ContentError::Internal {
            detail: error.to_string(),
        }
    }
}

impl InstallProgress for TaskProgressAdapter {
    fn enter_phase(&self, phase: &str) -> Result<(), ContentError> {
        let mut started = self.started.lock().map_err(|_| ContentError::Internal {
            detail: "progress lock poisoned".to_string(),
        })?;
        if !*started {
            let _ = self
                .tasks
                .transition(&self.task_id, self.generation, TaskState::Running);
            *started = true;
        }
        let phase = TaskPhase::new(phase).map_err(|error| ContentError::Internal {
            detail: error.to_string(),
        })?;
        self.tasks
            .set_phase(&self.task_id, self.generation, phase)
            .map_err(Self::task_error)?;
        Ok(())
    }

    fn evidence(&self, marker: &str) {
        let _ = self.tasks.add_evidence(&self.task_id, marker);
    }

    fn cancel_requested(&self) -> bool {
        self.tasks
            .is_cancel_requested(&self.task_id, self.generation)
    }

    fn advance(&self, done: u64, total: u64) -> Result<(), ContentError> {
        let total = total.max(done);
        let progress =
            TaskProgress::new(done, Some(total), ProgressUnit::Steps).map_err(|error| {
                ContentError::Internal {
                    detail: error.to_string(),
                }
            })?;
        let _ = self
            .tasks
            .set_progress(&self.task_id, self.generation, progress);
        Ok(())
    }
}

/// Recording double used by tests; `with_cancel_from` makes cancellation
/// deterministic at a chosen safe point.
#[derive(Debug, Default)]
pub struct FixtureProgress {
    state: Mutex<FixtureProgressState>,
}

#[derive(Debug, Default)]
struct FixtureProgressState {
    phases: Vec<String>,
    evidence: Vec<String>,
    advances: Vec<(u64, u64)>,
    current_phase: Option<String>,
    cancel_from: Option<String>,
}

impl FixtureProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation as soon as this phase is entered.
    pub fn with_cancel_from(self, phase: &str) -> Self {
        if let Ok(mut state) = self.state.lock() {
            state.cancel_from = Some(phase.to_string());
        }
        self
    }

    pub fn phases(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|state| state.phases.clone())
            .unwrap_or_default()
    }

    pub fn evidence(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|state| state.evidence.clone())
            .unwrap_or_default()
    }

    pub fn has_evidence(&self, marker: &str) -> bool {
        self.evidence().iter().any(|value| value == marker)
    }

    pub fn advances(&self) -> Vec<(u64, u64)> {
        self.state
            .lock()
            .map(|state| state.advances.clone())
            .unwrap_or_default()
    }
}

impl InstallProgress for FixtureProgress {
    fn enter_phase(&self, phase: &str) -> Result<(), ContentError> {
        let mut state = self.state.lock().map_err(|_| ContentError::Internal {
            detail: "progress lock poisoned".to_string(),
        })?;
        state.phases.push(phase.to_string());
        state.current_phase = Some(phase.to_string());
        Ok(())
    }

    fn evidence(&self, marker: &str) {
        if let Ok(mut state) = self.state.lock() {
            if !state.evidence.iter().any(|value| value == marker) {
                state.evidence.push(marker.to_string());
            }
        }
    }

    fn cancel_requested(&self) -> bool {
        self.state
            .lock()
            .map(|state| match (&state.cancel_from, &state.current_phase) {
                (Some(target), Some(current)) => target == current,
                _ => false,
            })
            .unwrap_or(false)
    }

    fn advance(&self, done: u64, total: u64) -> Result<(), ContentError> {
        if let Ok(mut state) = self.state.lock() {
            state.advances.push((done, total));
        }
        Ok(())
    }
}

pub struct InstallRunner<'a> {
    ports: &'a ContentPorts,
    progress: &'a dyn InstallProgress,
}

impl<'a> InstallRunner<'a> {
    pub fn new(ports: &'a ContentPorts, progress: &'a dyn InstallProgress) -> Self {
        Self { ports, progress }
    }

    /// Apply a prepared plan. The instance is only touched at the commit step.
    pub fn apply(
        &self,
        plan: &ChangePlan,
        view: &InstanceContentView,
    ) -> Result<InstallOutcome, ContentError> {
        if plan.base_revision != view.revision {
            return Err(ContentError::PlanConflict {
                expected: plan.base_revision.get(),
                actual: view.revision.get(),
            });
        }
        if view.game_running {
            return Err(ContentError::InstanceBusy {
                instance: view.instance_id.as_str().to_string(),
            });
        }

        self.progress.enter_phase(phases::PLAN)?;
        // A cancellation request wins over "nothing to do": the user asked to stop.
        if self.progress.cancel_requested() {
            return self.safe_stop(plan, phases::PLAN);
        }
        // A user-owned file blocks the plan; nothing may be written until it is chosen.
        if let Some(conflict) = plan.conflicts.first() {
            return Err(ContentError::UnmanagedConflict {
                path: conflict.path.clone(),
            });
        }
        if plan.is_empty() {
            // Nothing is written, so there is no commit to confirm.
            self.progress.enter_phase(phases::FINALIZE)?;
            return Ok(InstallOutcome::Installed {
                plan_id: plan.plan_id.clone(),
                revision: view.revision,
                cancelled_too_late: false,
            });
        }

        let record = RecoveryRecord::new(
            plan.instance_id.as_str(),
            &plan.plan_id,
            &plan.plan_digest,
            RecoveryKind::Apply,
            view.revision,
            view.files.clone(),
            planned_content_digest(view, plan),
            planned_files(view, plan),
        );
        self.ports.journal.write(&record)?;

        self.progress.enter_phase(phases::STAGE)?;
        let total = plan.required_artifacts.len() as u64;
        for (index, requirement) in plan.required_artifacts.iter().enumerate() {
            if self.progress.cancel_requested() {
                return self.safe_stop(plan, phases::STAGE);
            }
            match self.ports.artifacts.stage(requirement) {
                Ok(_staged) => {}
                Err(error @ ContentError::RestrictedFile { .. }) => {
                    self.progress
                        .evidence(evidence::RESTRICTED_ARTIFACTS_PENDING);
                    let _ = self.ports.artifacts.discard(&plan.plan_id);
                    let _ = self.ports.journal.clear(plan.instance_id.as_str());
                    return Err(error);
                }
                Err(error) => {
                    let _ = self.ports.artifacts.discard(&plan.plan_id);
                    let _ = self.ports.journal.clear(plan.instance_id.as_str());
                    return Err(error);
                }
            }
            self.progress.advance(index as u64 + 1, total)?;
        }

        self.progress.enter_phase(phases::VERIFY)?;
        if self.progress.cancel_requested() {
            return self.safe_stop(plan, phases::VERIFY);
        }

        let mut committing = record.clone();
        committing.stage = Stage::CommitStarted;
        self.ports.journal.write(&committing)?;

        self.progress.enter_phase(phases::COMMIT)?;
        // Last interlock: the commit must not start after a cancellation request.
        if self.progress.cancel_requested() {
            return self.safe_stop(plan, phases::COMMIT);
        }
        let receipt = self.ports.instance.apply(plan)?;

        let mut committed = record;
        committed.stage = Stage::Committed;
        self.ports.journal.write(&committed)?;
        // Only `content_commit_confirmed` is asserted: the instance receipt proves the
        // commit, while "previous version preserved" needs a backup fact 01 does not
        // expose yet, so no evidence marker is written for it.
        self.progress.evidence(evidence::COMMIT_CONFIRMED);

        self.progress.enter_phase(phases::FINALIZE)?;
        let cancelled_too_late = self.progress.cancel_requested();
        if cancelled_too_late {
            self.progress.evidence(evidence::CANCEL_TOO_LATE);
        }
        Ok(InstallOutcome::Installed {
            plan_id: plan.plan_id.clone(),
            revision: receipt.revision,
            cancelled_too_late,
        })
    }

    /// Restore the file set recorded before the last committed operation.
    pub fn rollback(
        &self,
        plan_id: &str,
        view: &InstanceContentView,
    ) -> Result<InstallOutcome, ContentError> {
        let record = self
            .ports
            .journal
            .read(view.instance_id.as_str())?
            .ok_or_else(|| ContentError::RecoveryMissing {
                instance: view.instance_id.as_str().to_string(),
            })?;
        if record.stage != Stage::Committed {
            return Err(ContentError::RecoveryCorrupt {
                detail: format!(
                    "record is at stage {} and cannot be rolled back",
                    record.stage.as_str()
                ),
            });
        }

        self.progress.enter_phase(phases::ROLLBACK)?;
        if self.progress.cancel_requested() {
            return Ok(InstallOutcome::RolledBack {
                plan_id: plan_id.to_string(),
                at_phase: phases::ROLLBACK.to_string(),
            });
        }

        let inverse = rollback_plan(plan_id, view, &record.previous)?;
        self.progress.advance(1, 1)?;
        let receipt = self.ports.instance.apply(&inverse)?;
        self.progress.evidence(evidence::ROLLBACK_APPLIED);
        self.ports.journal.clear(view.instance_id.as_str())?;

        Ok(InstallOutcome::RestoredPrevious {
            plan_id: inverse.plan_id,
            revision: receipt.revision,
            cancelled_too_late: false,
        })
    }

    /// Resume an interrupted operation without re-resolving any version.
    pub fn resume(&self, view: &InstanceContentView) -> Result<InstallOutcome, ContentError> {
        self.progress.enter_phase(phases::RESUME)?;
        let record = match self.ports.journal.read(view.instance_id.as_str())? {
            Some(record) => record,
            None => {
                return Ok(InstallOutcome::Resumed {
                    status: ResumeStatus::NothingPending,
                });
            }
        };

        match record.stage {
            Stage::Staged => {
                let _ = self.ports.artifacts.discard(&record.plan_id);
                self.ports.journal.clear(view.instance_id.as_str())?;
                self.progress.evidence(evidence::SAFE_STOP_REACHED);
                self.progress.evidence(evidence::RECOVERY_DISCARDED);
                Ok(InstallOutcome::Resumed {
                    status: ResumeStatus::DiscardedStaging,
                })
            }
            Stage::CommitStarted => match verify_planned_content(view, &record) {
                Verdict::Landed => {
                    let mut committed = record;
                    committed.stage = Stage::Committed;
                    self.ports.journal.write(&committed)?;
                    self.progress.evidence(evidence::COMMIT_CONFIRMED);
                    Ok(InstallOutcome::Resumed {
                        status: ResumeStatus::FinalizedCommit,
                    })
                }
                Verdict::NotLanded => {
                    // The commit provably did not land, so there is nothing to roll
                    // back; the record can go.
                    let _ = self.ports.artifacts.discard(&record.plan_id);
                    self.ports.journal.clear(view.instance_id.as_str())?;
                    self.progress.evidence(evidence::RECOVERY_DISCARDED);
                    Ok(InstallOutcome::Resumed {
                        status: ResumeStatus::CommitOutcomeUnknown,
                    })
                }
                Verdict::Undecidable => {
                    // Cannot prove either way: keep the record as a rollback point.
                    // Rolling back is computed from the *current* content, so it is a
                    // no-op when the commit did not land and a restore when it did.
                    let mut kept = record;
                    kept.stage = Stage::Committed;
                    self.ports.journal.write(&kept)?;
                    self.progress.evidence(evidence::COMMIT_OUTCOME_UNKNOWN);
                    self.progress.evidence(evidence::ROLLBACK_POINT_KEPT);
                    Ok(InstallOutcome::Resumed {
                        status: ResumeStatus::CommitOutcomeUnknown,
                    })
                }
            },
            Stage::Committed => Ok(InstallOutcome::Resumed {
                status: ResumeStatus::AlreadyCommitted,
            }),
        }
    }

    /// Stop at a safe point: drop staging, then drop the record.
    ///
    /// If discarding fails the record is **kept**, so the leftovers stay
    /// recoverable instead of becoming an untracked half-written staging area.
    fn safe_stop(&self, plan: &ChangePlan, at_phase: &str) -> Result<InstallOutcome, ContentError> {
        if let Err(error) = self.ports.artifacts.discard(&plan.plan_id) {
            self.progress.evidence(evidence::STAGING_DISCARD_FAILED);
            return Err(error);
        }
        self.ports.journal.clear(plan.instance_id.as_str())?;
        self.progress.evidence(evidence::SAFE_STOP_REACHED);
        self.progress.evidence(evidence::STAGING_DISCARDED);
        Ok(InstallOutcome::RolledBack {
            plan_id: plan.plan_id.clone(),
            at_phase: at_phase.to_string(),
        })
    }
}

/// The content the plan wants to reach, expressed as the same digest form the
/// instance view produces. Used to decide whether an interrupted commit landed.
pub fn planned_content_digest(view: &InstanceContentView, plan: &ChangePlan) -> String {
    digest_of(
        planned_entries(view, plan)
            .into_iter()
            .map(|file| format!("{}|{}", file.path, file.digest.clone().unwrap_or_default()))
            .collect(),
    )
}

/// The file list the instance should hold after the plan. This is what a resume
/// compares against, because a single aggregate digest cannot say *which* entry is
/// missing and cannot be computed when one entry has no digest.
pub fn planned_files(view: &InstanceContentView, plan: &ChangePlan) -> Vec<PlannedFile> {
    planned_entries(view, plan)
}

fn planned_entries(view: &InstanceContentView, plan: &ChangePlan) -> Vec<PlannedFile> {
    let mut files: BTreeMap<String, PlannedFile> = view
        .files
        .iter()
        .map(|file| {
            (
                file.path.clone(),
                PlannedFile {
                    path: file.path.clone(),
                    digest: Some(file.digest.clone()),
                    size: Some(file.size),
                },
            )
        })
        .collect();
    for entry in &plan.entries {
        match entry.action {
            PlanAction::Add | PlanAction::Replace => {
                files.insert(
                    entry.path.clone(),
                    PlannedFile {
                        path: entry.path.clone(),
                        digest: entry.primary_digest(),
                        size: entry.size,
                    },
                );
            }
            PlanAction::Delete => {
                files.remove(&entry.path);
            }
        }
    }
    files.into_values().collect()
}

/// Outcome of comparing the instance content against a recovery record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Every planned file is present with the recorded digest.
    Landed,
    /// At least one planned file is missing or different: the commit did not land.
    NotLanded,
    /// The record does not carry enough information to decide either way.
    Undecidable,
}

/// Decide whether an interrupted commit landed.
///
/// Evidence order: per-entry digest (strong) → per-entry size (weak) → the aggregate
/// planned digest when the record has no per-entry data (v1 records).
///
/// A missing planned file or a digest/size mismatch means the commit did not land.
/// When every planned entry carries a digest and they all match, the commit landed.
/// When some entry has no digest, "undecidable" is the strongest honest answer — and
/// such a record is *kept*, because discarding it would delete the user's only
/// rollback point.
pub fn verify_planned_content(view: &InstanceContentView, record: &RecoveryRecord) -> Verdict {
    if record.planned.is_empty() {
        // v1 record: only an aggregate digest exists.
        return if view.digest_of_paths() == record.planned_digest {
            Verdict::Landed
        } else {
            Verdict::Undecidable
        };
    }

    let current: BTreeMap<&str, &InstanceFile> = view
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();

    let mut mismatch = false;
    let mut all_have_digest = true;

    for planned in &record.planned {
        let Some(file) = current.get(planned.path.as_str()) else {
            // A planned file is absent: the commit cannot have landed.
            return Verdict::NotLanded;
        };
        match &planned.digest {
            Some(digest) => {
                if !file.digest.eq_ignore_ascii_case(digest) {
                    mismatch = true;
                }
            }
            None => {
                all_have_digest = false;
                match planned.size {
                    Some(size) if size == file.size => {}
                    _ => mismatch = true,
                }
            }
        }
    }

    if mismatch {
        return Verdict::NotLanded;
    }
    if all_have_digest {
        return Verdict::Landed;
    }
    Verdict::Undecidable
}

#[cfg(test)]
mod tests {
    use super::super::loader::{GameTarget, LoaderKind};
    use super::super::modpack::{Hashes, PackSide, SideSupport};
    use super::super::plan::{ContentLock, LocalIdentity, LockFile, LockSource, plan_changes};
    use super::*;
    use shtmcl_domain::InstanceId;

    fn lock(paths: &[(&str, &str)]) -> ContentLock {
        let files = paths
            .iter()
            .map(|(path, sha1)| LockFile {
                path: (*path).to_string(),
                identity: LocalIdentity::Digest {
                    sha1: Some((*sha1).to_string()),
                    sha512: None,
                },
                hashes: Hashes {
                    sha1: Some((*sha1).to_string()),
                    sha512: None,
                    sha256: None,
                },
                size: Some(4),
                urls: vec!["https://example.invalid/a.jar".to_string()],
                source: LockSource::Download,
                client: SideSupport::Required,
                server: SideSupport::Required,
                origin: "pack:files".to_string(),
            })
            .collect();
        ContentLock::new(
            GameTarget::new("1.20.1", LoaderKind::Fabric),
            PackSide::Client,
            files,
        )
        .expect("lock")
    }

    fn instance_id() -> InstanceId {
        InstanceId::new("instance-1").expect("id")
    }

    #[test]
    fn cancellation_before_commit_leaves_the_instance_untouched() {
        use super::super::ports::{FixtureArtifactStager, FixtureInstanceWriter};
        use std::sync::Arc;

        let writer = Arc::new(FixtureInstanceWriter::new(Revision::new(1)));
        let stager = Arc::new(FixtureArtifactStager::new());
        let ports = ContentPorts {
            instance: writer.clone(),
            artifacts: stager.clone(),
            ..ContentPorts::fixtures()
        };
        let progress = FixtureProgress::new().with_cancel_from(phases::VERIFY);
        let runner = InstallRunner::new(&ports, &progress);

        let view = writer.view_of(&instance_id()).expect("view");
        let plan = plan_changes("plan-1", &view, &lock(&[("mods/a.jar", "aaaa")])).expect("plan");
        let outcome = runner.apply(&plan, &view).expect("apply");

        assert!(outcome.is_cancelled());
        assert!(progress.has_evidence(evidence::SAFE_STOP_REACHED));
        assert!(progress.has_evidence(evidence::STAGING_DISCARDED));
        assert!(
            writer.commits().is_empty(),
            "no commit may happen after cancel"
        );
        assert_eq!(writer.revision(), Revision::new(1));
        assert_eq!(stager.discarded_plans(), vec!["plan-1".to_string()]);
    }

    #[test]
    fn successful_apply_then_rollback_restores_the_previous_content() {
        use super::super::plan::InstanceFile;
        use super::super::ports::{FixtureArtifactStager, FixtureInstanceWriter};
        use super::super::recovery::MemoryJournalStore;
        use std::sync::Arc;

        let writer = Arc::new(FixtureInstanceWriter::new(Revision::new(1)));
        writer.seed(vec![InstanceFile::new("mods/old.jar", "dddd", 4)]);
        let ports = ContentPorts {
            instance: writer.clone(),
            artifacts: Arc::new(FixtureArtifactStager::new()),
            journal: Arc::new(MemoryJournalStore::new()),
            ..ContentPorts::fixtures()
        };
        let progress = FixtureProgress::new();
        let runner = InstallRunner::new(&ports, &progress);

        let before = writer.files();
        let view = writer.view_of(&instance_id()).expect("view");
        let plan = plan_changes("plan-1", &view, &lock(&[("mods/a.jar", "aaaa")])).expect("plan");
        let outcome = runner.apply(&plan, &view).expect("apply");
        assert!(matches!(outcome, InstallOutcome::Installed { .. }));

        let after_install = writer.files();
        assert!(after_install.iter().any(|file| file.path == "mods/a.jar"));
        assert!(!after_install.iter().any(|file| file.path == "mods/old.jar"));

        let view_after = writer.view_of(&instance_id()).expect("view");
        let restored = runner.rollback("plan-1-rb", &view_after).expect("rollback");
        assert!(matches!(restored, InstallOutcome::RestoredPrevious { .. }));
        assert_eq!(writer.files(), before, "rollback restores the recorded set");
    }

    #[test]
    fn resume_discards_staging_when_the_commit_never_started() {
        use super::super::ports::{FixtureArtifactStager, FixtureInstanceWriter};
        use super::super::recovery::{JournalStore, MemoryJournalStore};
        use std::sync::Arc;

        let writer = Arc::new(FixtureInstanceWriter::new(Revision::new(1)));
        let journal = Arc::new(MemoryJournalStore::new());
        let ports = ContentPorts {
            instance: writer.clone(),
            artifacts: Arc::new(FixtureArtifactStager::new()),
            journal: journal.clone(),
            ..ContentPorts::fixtures()
        };
        let progress = FixtureProgress::new();
        let runner = InstallRunner::new(&ports, &progress);

        let view = writer.view_of(&instance_id()).expect("view");
        let plan = plan_changes("plan-1", &view, &lock(&[("mods/a.jar", "aaaa")])).expect("plan");
        journal
            .write(&RecoveryRecord::new(
                "instance-1",
                "plan-1",
                plan.plan_digest.clone(),
                RecoveryKind::Apply,
                Revision::new(1),
                view.files.clone(),
                planned_content_digest(&view, &plan),
                planned_files(&view, &plan),
            ))
            .expect("write");

        let outcome = runner.resume(&view).expect("resume");
        assert!(matches!(
            outcome,
            InstallOutcome::Resumed {
                status: ResumeStatus::DiscardedStaging
            }
        ));
        assert!(journal.read("instance-1").expect("read").is_none());
        assert!(writer.commits().is_empty());
    }
}

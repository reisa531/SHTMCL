//! `RepairPlanner`: a diagnostic finding to a real `ChangePlan`.
//!
//! Two rules keep this honest:
//!
//! 1. the plan is 04's [`ChangePlan`], the same shape the content module submits,
//!    so the instance owner (01) has exactly one plan language. Only `add` /
//!    `replace` / `delete` entries exist, they are de-duplicated and sorted, and
//!    the digest is stable;
//! 2. this module never touches the game directory. Reads and writes go through
//!    04's [`InstanceContentPort`], whose default wiring is fail-closed
//!    (`diagnostics.owner.unavailable`) because 01 is not integrated.
//!
//! A finding only keeps log *offsets*, never the text, so the file to add or
//! delete cannot be guessed from the log. The caller (the UI, after showing the
//! evidence) supplies the instance-relative `target`; the planner still verifies
//! it: inside the instance, normalised, managed, present or absent as the action
//! requires.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};
use shtmcl_domain::{InstanceId, Revision};

use super::error::DiagnosticsError;
use super::rules::{Confidence, Finding, RepairKind};

pub use crate::content::plan::{
    ChangeCounts, ChangePlan, InstanceContentView, LockSource, PlanAction, PlanEntry,
};
pub use crate::content::ports::{CommitReceipt, InstanceContentPort};

use crate::content::error::ContentError;
use crate::content::paths::normalise_content_path;
use crate::content::plan::{digest_of, PLAN_SCHEMA};

/// Schema marker of a repair outcome (the plan itself stays `content-plan.v1`).
pub const REPAIR_SCHEMA: &str = "diagnostics-repair.v1";

#[derive(Debug, Clone)]
pub struct RepairRequest {
    pub finding: Finding,
    /// Instance revision the analysis was produced from. A mismatch is a conflict,
    /// never a silent re-base. `0` means "accept the current revision".
    pub expected_revision: Revision,
    /// Instance-relative path chosen by the caller (required for file repairs).
    pub target: Option<String>,
    /// Commit through the instance port after planning (needs a mutation).
    pub commit: bool,
}

impl RepairRequest {
    pub fn new(finding: Finding) -> Self {
        Self {
            finding,
            expected_revision: Revision::new(0),
            target: None,
            commit: false,
        }
    }

    pub fn with_revision(mut self, revision: Revision) -> Self {
        self.expected_revision = revision;
        self
    }

    pub fn with_target(mut self, target: Option<String>) -> Self {
        self.target = target;
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepairOutcome {
    pub plan: ChangePlan,
    pub receipt: Option<CommitReceipt>,
    /// What to run after the change lands. A repair is not "done" until the
    /// condition has been re-checked.
    pub recheck: Value,
}

impl RepairOutcome {
    pub fn to_value(&self) -> Value {
        json!({
            "schema": REPAIR_SCHEMA,
            "plan": self.plan.to_value(),
            "receipt": self.receipt.as_ref().map(|receipt| json!({
                "instanceId": receipt.instance_id.as_str(),
                "revision": receipt.revision.get(),
                "paths": receipt.paths,
                "planDigest": receipt.plan_digest,
            })),
            "recheck": self.recheck,
        })
    }
}

pub struct RepairPlanner {
    instances: Arc<dyn InstanceContentPort>,
}

impl RepairPlanner {
    pub fn new(instances: Arc<dyn InstanceContentPort>) -> Self {
        Self { instances }
    }

    /// Default wiring: no instance owner is connected, so every read fails closed.
    pub fn unavailable() -> Self {
        Self::new(Arc::new(crate::content::ports::UnavailableInstancePort))
    }

    /// Always `true` in this build: the instance owner is a controlled double.
    pub fn is_provisional(&self) -> bool {
        true
    }

    fn map(error: ContentError) -> DiagnosticsError {
        match error {
            ContentError::PlanConflict { expected, actual } => {
                DiagnosticsError::RepairConflict { expected, actual }
            }
            ContentError::InstanceBusy { instance } => DiagnosticsError::RepairBusy { instance },
            ContentError::OwnerUnavailable { owner } => DiagnosticsError::owner(owner),
            ContentError::PathEscape { path } => DiagnosticsError::RepairUnsupported {
                rule: "diagnostics.repair.path".to_string(),
                detail: format!("path is not inside the instance: {path}"),
            },
            ContentError::UnmanagedConflict { path } => DiagnosticsError::RepairUnsupported {
                rule: "diagnostics.repair.unmanaged".to_string(),
                detail: format!("not a managed file: {path}"),
            },
            other => DiagnosticsError::Internal {
                detail: format!("instance port: {other}"),
            },
        }
    }

    pub fn plan(&self, request: &RepairRequest) -> Result<RepairOutcome, DiagnosticsError> {
        let finding = &request.finding;
        let instance_id =
            finding
                .instance_id
                .clone()
                .ok_or_else(|| DiagnosticsError::RepairUnsupported {
                    rule: finding.rule_id.to_string(),
                    detail: "the finding is not tied to an instance".to_string(),
                })?;

        // Only a confirmed condition may change instance files.
        if !finding.action.kind.is_file_change() {
            return Err(DiagnosticsError::RepairUnsupported {
                rule: finding.rule_id.to_string(),
                detail: format!(
                    "{} is a {} repair, not a file change",
                    finding.rule_id,
                    finding.action.kind.as_str()
                ),
            });
        }
        if finding.confidence != Confidence::Confirmed {
            return Err(DiagnosticsError::RepairUnsupported {
                rule: finding.rule_id.to_string(),
                detail: "only a confirmed condition may change instance files".to_string(),
            });
        }

        let target = request
            .target
            .as_deref()
            .or(finding.action.target.as_deref())
            .ok_or_else(|| DiagnosticsError::RepairUnsupported {
                rule: finding.rule_id.to_string(),
                detail: "no instance-relative target was supplied".to_string(),
            })?;
        let path = normalise_content_path(target).map_err(Self::map)?;

        let view = self.instances.view(&instance_id).map_err(Self::map)?;
        if request.expected_revision != Revision::new(0)
            && view.revision != request.expected_revision
        {
            return Err(DiagnosticsError::RepairConflict {
                expected: request.expected_revision.get(),
                actual: view.revision.get(),
            });
        }

        let existing = view
            .files
            .iter()
            .find(|file| file.path.eq_ignore_ascii_case(&path));
        let entry = match finding.action.kind {
            RepairKind::AddFile => {
                if existing.is_some() {
                    return Err(DiagnosticsError::RepairUnsupported {
                        rule: finding.rule_id.to_string(),
                        detail: format!("{path} is already present"),
                    });
                }
                PlanEntry {
                    path: path.clone(),
                    action: PlanAction::Add,
                    identity: None,
                    sha1: None,
                    sha512: None,
                    sha256: None,
                    size: None,
                    source: LockSource::Download,
                    restore_from_backup: false,
                }
            }
            RepairKind::ReplaceFile => {
                let file = existing.ok_or_else(|| DiagnosticsError::RepairUnsupported {
                    rule: finding.rule_id.to_string(),
                    detail: format!("{path} does not exist"),
                })?;
                PlanEntry {
                    path: path.clone(),
                    action: PlanAction::Replace,
                    identity: None,
                    sha1: Some(file.digest.clone()),
                    sha512: None,
                    sha256: None,
                    size: Some(file.size),
                    source: LockSource::Download,
                    restore_from_backup: false,
                }
            }
            RepairKind::DeleteFile => {
                let file = existing.ok_or_else(|| DiagnosticsError::RepairUnsupported {
                    rule: finding.rule_id.to_string(),
                    detail: format!("{path} does not exist"),
                })?;
                if !file.managed {
                    return Err(DiagnosticsError::RepairUnsupported {
                        rule: finding.rule_id.to_string(),
                        detail: format!("{path} is not managed by the launcher"),
                    });
                }
                PlanEntry {
                    path: path.clone(),
                    action: PlanAction::Delete,
                    identity: None,
                    sha1: None,
                    sha512: None,
                    sha256: None,
                    size: Some(file.size),
                    source: LockSource::Download,
                    restore_from_backup: false,
                }
            }
            _ => unreachable!("non-file repairs return above"),
        };

        // De-duplicate by path and keep the order stable, exactly like 04.
        let mut by_path: BTreeMap<String, PlanEntry> = BTreeMap::new();
        by_path.insert(path, entry);
        let entries: Vec<PlanEntry> = by_path.into_values().collect();
        let plan_digest = digest_of(
            std::iter::once(format!(
                "{}|{}|{}|{}",
                PLAN_SCHEMA,
                REPAIR_SCHEMA,
                instance_id.as_str(),
                view.revision.get()
            ))
            .chain(entries.iter().map(|entry| {
                format!(
                    "{}|{}|{}",
                    entry.action.as_str(),
                    entry.path,
                    entry
                        .sha1
                        .clone()
                        .or_else(|| entry.sha512.clone())
                        .unwrap_or_else(|| "none".to_string())
                )
            }))
            .collect(),
        );
        let mut counts = ChangeCounts::default();
        for entry in &entries {
            match entry.action {
                PlanAction::Add => counts.add += 1,
                PlanAction::Replace => counts.replace += 1,
                PlanAction::Delete => counts.delete += 1,
            }
            if entry.action != PlanAction::Delete {
                counts.bytes += entry.size.unwrap_or(0);
            }
        }
        counts.total = counts.add + counts.replace + counts.delete;

        let plan = ChangePlan {
            plan_id: format!("repair-{}", finding.finding_id),
            instance_id: instance_id.clone(),
            base_revision: view.revision,
            entries,
            conflicts: Vec::new(),
            required_artifacts: Vec::new(),
            counts,
            plan_digest,
        };

        let recheck = recheck_for(finding, request.commit);
        let receipt = if request.commit {
            if view.game_running {
                return Err(DiagnosticsError::RepairBusy {
                    instance: instance_id.as_str().to_string(),
                });
            }
            Some(self.instances.apply(&plan).map_err(Self::map)?)
        } else {
            None
        };

        Ok(RepairOutcome {
            plan,
            receipt,
            recheck,
        })
    }
}

/// Recheck descriptor: a repair is only verified by running the analysis again.
pub fn recheck_for(finding: &Finding, committed: bool) -> Value {
    json!({
        "command": "diagnostics.analyze",
        "sessionId": finding.session_id.as_str(),
        "ruleId": finding.rule_id,
        "findingId": finding.finding_id,
        "afterCommit": committed,
        "condition": format!("{} must no longer match", finding.rule_id),
    })
}

/// Instance id of a finding, when it is tied to one.
pub fn instance_or_none(finding: &Finding) -> Option<InstanceId> {
    finding.instance_id.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::plan::InstanceFile;
    use crate::content::ports::FixtureInstanceWriter;
    use crate::diagnostics::error::codes;
    use crate::diagnostics::rules::{EvidenceRef, FindingAction};
    use shtmcl_domain::LaunchSessionId;

    fn finding(kind: RepairKind, confidence: Confidence, target: Option<&str>) -> Finding {
        let session = LaunchSessionId::new("session-1").expect("id");
        Finding {
            finding_id: "finding-test".to_string(),
            rule_id: "diagnostics.rule.duplicate_mod",
            rule_version: 1,
            session_id: session.clone(),
            instance_id: Some(InstanceId::new("instance-1").expect("id")),
            confidence,
            evidence_refs: vec![EvidenceRef {
                session_id: session,
                from_sequence: 10,
                to_sequence: 11,
            }],
            action: FindingAction {
                kind,
                target: target.map(str::to_string),
                summary: "test".to_string(),
                help_entry: "duplicate_mod".to_string(),
            },
        }
    }

    fn writer() -> Arc<FixtureInstanceWriter> {
        let writer = Arc::new(FixtureInstanceWriter::new(Revision::new(3)));
        writer.seed(vec![
            InstanceFile::new("mods/sodium-0.5.8.jar", "sha1:aaa", 100),
            InstanceFile::new("mods/sodium-0.5.3.jar", "sha1:aaa", 100),
        ]);
        writer
    }

    #[test]
    fn a_confirmed_duplicate_produces_a_single_delete_entry() {
        let planner = RepairPlanner::new(writer());
        let request = RepairRequest::new(finding(
            RepairKind::DeleteFile,
            Confidence::Confirmed,
            Some("mods/sodium-0.5.3.jar"),
        ))
        .with_revision(Revision::new(3));
        let outcome = planner.plan(&request).expect("plan");
        assert_eq!(outcome.plan.entries.len(), 1);
        assert_eq!(outcome.plan.entries[0].action, PlanAction::Delete);
        assert_eq!(outcome.plan.entries[0].path, "mods/sodium-0.5.3.jar");
        assert_eq!(outcome.plan.counts.delete, 1);
        assert_eq!(outcome.plan.counts.total, 1);
        assert_eq!(outcome.plan.base_revision.get(), 3);
        assert!(outcome.receipt.is_none(), "planning never writes");
    }

    #[test]
    fn a_candidate_hint_never_plans_a_file_change() {
        let planner = RepairPlanner::new(writer());
        let request = RepairRequest::new(finding(
            RepairKind::DeleteFile,
            Confidence::Candidate,
            Some("mods/sodium-0.5.3.jar"),
        ));
        let error = planner.plan(&request).expect_err("candidate is refused");
        assert_eq!(error.to_envelope().code.as_str(), codes::REPAIR_UNSUPPORTED);
    }

    #[test]
    fn a_configuration_repair_is_not_a_file_change() {
        let planner = RepairPlanner::new(writer());
        let request = RepairRequest::new(finding(
            RepairKind::CloseProcess,
            Confidence::Confirmed,
            None,
        ));
        let error = planner.plan(&request).expect_err("file lock is not a plan");
        assert_eq!(error.to_envelope().code.as_str(), codes::REPAIR_UNSUPPORTED);
        assert!(error
            .to_envelope()
            .detail_ref
            .unwrap_or_default()
            .contains("close_process"));
    }

    #[test]
    fn a_committed_repair_goes_through_the_instance_port() {
        let fixture = writer();
        let planner = RepairPlanner::new(fixture.clone());
        let mut request = RepairRequest::new(finding(
            RepairKind::DeleteFile,
            Confidence::Confirmed,
            Some("mods/sodium-0.5.3.jar"),
        ))
        .with_revision(Revision::new(3));
        request.commit = true;
        let outcome = planner.plan(&request).expect("commit");
        assert_eq!(outcome.receipt.as_ref().map(|r| r.revision.get()), Some(4));
        assert_eq!(fixture.commits().len(), 1);
        assert_eq!(fixture.files().len(), 1, "only the duplicate was removed");
        assert_eq!(
            outcome.recheck["command"].as_str(),
            Some("diagnostics.analyze")
        );
        assert_eq!(outcome.recheck["afterCommit"].as_bool(), Some(true));
    }

    #[test]
    fn a_stale_instance_revision_is_a_conflict() {
        let planner = RepairPlanner::new(writer());
        let request = RepairRequest::new(finding(
            RepairKind::DeleteFile,
            Confidence::Confirmed,
            Some("mods/sodium-0.5.3.jar"),
        ))
        .with_revision(Revision::new(1));
        let error = planner.plan(&request).expect_err("stale revision");
        assert_eq!(error.to_envelope().code.as_str(), codes::REPAIR_CONFLICT);
    }

    #[test]
    fn the_default_wiring_cannot_read_an_instance() {
        let planner = RepairPlanner::unavailable();
        let request = RepairRequest::new(finding(
            RepairKind::DeleteFile,
            Confidence::Confirmed,
            Some("mods/sodium-0.5.3.jar"),
        ));
        let error = planner.plan(&request).expect_err("fail closed");
        assert_eq!(error.to_envelope().code.as_str(), codes::OWNER_UNAVAILABLE);
    }

    #[test]
    fn a_path_outside_the_instance_is_refused() {
        let planner = RepairPlanner::new(writer());
        let request = RepairRequest::new(finding(
            RepairKind::AddFile,
            Confidence::Confirmed,
            Some("../escape.jar"),
        ));
        let error = planner.plan(&request).expect_err("escape");
        assert_eq!(error.to_envelope().code.as_str(), codes::REPAIR_UNSUPPORTED);
    }

    #[test]
    fn an_unmanaged_file_is_never_deleted() {
        let fixture = Arc::new(FixtureInstanceWriter::new(Revision::new(3)));
        let mut file = InstanceFile::new("mods/manual.jar", "sha1:bbb", 10);
        file.managed = false;
        fixture.seed(vec![file]);
        let planner = RepairPlanner::new(fixture);
        let request = RepairRequest::new(finding(
            RepairKind::DeleteFile,
            Confidence::Confirmed,
            Some("mods/manual.jar"),
        ));
        let error = planner.plan(&request).expect_err("unmanaged");
        assert_eq!(error.to_envelope().code.as_str(), codes::REPAIR_UNSUPPORTED);
    }
}

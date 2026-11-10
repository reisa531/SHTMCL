//! Diagnostic rules and the report they produce.
//!
//! A rule reads only the *bounded* in-memory window of one session (never the
//! raw pipe) and returns findings that reference log offsets instead of copying
//! the text. Two confidence levels are kept apart on purpose:
//!
//! - [`Confidence::Confirmed`] — the log states the condition itself
//!   (`UnsupportedClassVersionError`, `Duplicate mods found`, `OutOfMemoryError`).
//! - [`Confidence::Candidate`] — a hint that must be confirmed before the UI may
//!   word it as the cause (`ClassNotFoundException` alone does not prove a missing
//!   dependency).
//!
//! A single analysis run has a wall-clock budget (2 s by default). A timed-out
//! run returns the rules that did finish **plus** the budget numbers, and its
//! [`ReportStatus`] is `timed_out`; it never claims the log is clean.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use shtmcl_domain::{InstanceId, LaunchSessionId, Revision};

use super::error::DiagnosticsError;
use super::logstore::LogStore;

/// Default per-analysis time budget (2 s).
pub const DEFAULT_BUDGET_MS: u64 = 2_000;
/// Hard ceiling a caller may request.
pub const MAX_BUDGET_MS: u64 = 10_000;
/// Lines of context re-read before the increment cursor.
pub const CONTEXT_LINES: u64 = 200;
/// Upper bound of findings in one report (keeps the payload bounded).
pub const MAX_FINDINGS: usize = 64;
/// Upper bound of evidence references per finding.
pub const MAX_EVIDENCE: usize = 5;
/// Two hits closer than this are folded into one evidence reference.
const MERGE_GAP: u64 = 2;

pub mod rules {
    pub const JAVA_MISMATCH: &str = "diagnostics.rule.java_mismatch";
    pub const MISSING_DEPENDENCY: &str = "diagnostics.rule.missing_dependency";
    pub const DUPLICATE_MOD: &str = "diagnostics.rule.duplicate_mod";
    pub const FILE_LOCK: &str = "diagnostics.rule.file_lock";
    pub const OUT_OF_MEMORY: &str = "diagnostics.rule.out_of_memory";
}

/// Version of the rule set. Part of the analysis cache key: changing a pattern
/// must invalidate a cached report.
pub const RULES_VERSION: &str = "diagnostics-rules.v1";
pub const REPORT_SCHEMA: &str = "diagnostics-report.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Confirmed,
    Candidate,
}

impl Confidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Candidate => "candidate",
        }
    }
}

/// What kind of concrete change (if any) would remove the finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairKind {
    AddFile,
    DeleteFile,
    ReplaceFile,
    /// Needs the game process to exit first; not a file change.
    CloseProcess,
    /// Needs a memory setting change; not a file change.
    AdjustMemory,
    /// Needs a different Java runtime; not a file change.
    ChooseRuntime,
    /// Nothing safe can be proposed automatically.
    Manual,
}

impl RepairKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AddFile => "add_file",
            Self::DeleteFile => "delete_file",
            Self::ReplaceFile => "replace_file",
            Self::CloseProcess => "close_process",
            Self::AdjustMemory => "adjust_memory",
            Self::ChooseRuntime => "choose_runtime",
            Self::Manual => "manual",
        }
    }

    /// Only these three can be expressed as a `ChangePlan`.
    pub const fn is_file_change(self) -> bool {
        matches!(self, Self::AddFile | Self::DeleteFile | Self::ReplaceFile)
    }
}

/// Offset reference into one session's log. Never a copy of the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRef {
    pub session_id: LaunchSessionId,
    pub from_sequence: u64,
    pub to_sequence: u64,
}

impl EvidenceRef {
    pub fn to_value(&self) -> Value {
        json!({
            "sessionId": self.session_id.as_str(),
            "fromSequence": self.from_sequence,
            "toSequence": self.to_sequence,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingAction {
    pub kind: RepairKind,
    /// Instance-relative target for a file-level repair, when one is known.
    pub target: Option<String>,
    /// Short, concrete next step (not a disclaimer).
    pub summary: String,
    /// Local help entry id resolved by `diagnostics.help`.
    pub help_entry: String,
}

impl FindingAction {
    pub fn to_value(&self) -> Value {
        json!({
            "kind": self.kind.as_str(),
            "target": self.target,
            "summary": self.summary,
            "helpEntry": self.help_entry,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub finding_id: String,
    pub rule_id: &'static str,
    pub rule_version: u32,
    pub session_id: LaunchSessionId,
    pub instance_id: Option<InstanceId>,
    pub confidence: Confidence,
    pub evidence_refs: Vec<EvidenceRef>,
    pub action: FindingAction,
}

impl Finding {
    pub fn to_value(&self) -> Value {
        json!({
            "findingId": self.finding_id,
            "ruleId": self.rule_id,
            "ruleVersion": self.rule_version,
            "sessionId": self.session_id.as_str(),
            "instanceId": self.instance_id.as_ref().map(|id| id.as_str()),
            "confidence": self.confidence.as_str(),
            "evidenceRefs": self.evidence_refs.iter().map(EvidenceRef::to_value).collect::<Vec<_>>(),
            "action": self.action.to_value(),
        })
    }

    fn sort_key(&self) -> (u8, u32, u64) {
        (
            match self.confidence {
                Confidence::Confirmed => 0,
                Confidence::Candidate => 1,
            },
            rule_index(self.rule_id),
            self.evidence_refs
                .first()
                .map_or(0, |reference| reference.from_sequence),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportStatus {
    Completed,
    NoMatches,
    TimedOut,
    Cancelled,
}

impl ReportStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::NoMatches => "no_matches",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether the run may be described as "the log shows nothing".
    pub const fn is_conclusive(self) -> bool {
        matches!(self, Self::NoMatches | Self::Completed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalysisBudget {
    pub budget_ms: u64,
    pub elapsed_ms: u64,
    pub timed_out: bool,
}

impl AnalysisBudget {
    pub fn to_value(&self) -> Value {
        json!({
            "budgetMs": self.budget_ms,
            "elapsedMs": self.elapsed_ms,
            "timedOut": self.timed_out,
        })
    }
}

/// Identity of a cached analysis. Every part of the key is a real input: a stale
/// report cannot be served after the log, the instance or the rules changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    pub session: LaunchSessionId,
    pub from_sequence: u64,
    pub to_sequence: u64,
    pub instance_revision: Revision,
    pub rules_version: &'static str,
}

impl CacheKey {
    pub fn as_string(&self) -> String {
        format!(
            "{}|{}|{}|r{}|{}-{}",
            REPORT_SCHEMA,
            self.rules_version,
            self.session.as_str(),
            self.instance_revision.get(),
            self.from_sequence,
            self.to_sequence
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiagnosticReport {
    pub session_id: LaunchSessionId,
    pub instance_id: Option<InstanceId>,
    pub instance_revision: Revision,
    pub status: ReportStatus,
    pub conclusions_possible: bool,
    pub findings: Vec<Finding>,
    pub budget: AnalysisBudget,
    pub from_sequence: u64,
    pub to_sequence: u64,
    pub scanned_rules: Vec<&'static str>,
    pub skipped_rules: Vec<&'static str>,
    pub cache_key: CacheKey,
    /// Retention loss inside the analysed window (ring or disk or pump).
    pub log_incomplete: bool,
}

impl DiagnosticReport {
    /// The one-line verdict. `inconclusive` is deliberately not "no problem".
    pub fn verdict(&self) -> &'static str {
        match self.status {
            ReportStatus::TimedOut => "inconclusive",
            ReportStatus::Cancelled => "inconclusive",
            ReportStatus::NoMatches => "no_matches",
            ReportStatus::Completed => "findings",
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "schema": REPORT_SCHEMA,
            "sessionId": self.session_id.as_str(),
            "instanceId": self.instance_id.as_ref().map(|id| id.as_str()),
            "instanceRevision": self.instance_revision.get(),
            "status": self.status.as_str(),
            "verdict": self.verdict(),
            "conclusionsPossible": self.conclusions_possible,
            "findings": self.findings.iter().map(Finding::to_value).collect::<Vec<_>>(),
            "findingCount": self.findings.len(),
            "budget": self.budget.to_value(),
            "logRange": { "from": self.from_sequence, "to": self.to_sequence },
            "scannedRules": self.scanned_rules,
            "skippedRules": self.skipped_rules,
            "cacheKey": self.cache_key.as_string(),
            "logIncomplete": self.log_incomplete,
        })
    }
}

struct RuleSpec {
    id: &'static str,
    version: u32,
    /// Patterns that only appear when the log **reports** the condition itself: an
    /// exception class, a loader report, the OS error sentence. These produce
    /// [`Confidence::Confirmed`].
    confirmed: &'static [&'static str],
    candidate: &'static [&'static str],
    kind: RepairKind,
    summary: &'static str,
    help_entry: &'static str,
}

/// The five rules of this round. Patterns are lower-case substrings.
///
/// Anchoring rule (the boundary an independent reviewer probed): `Confirmed` is
/// reserved for the machine form a program writes when it fails —
/// `UnsupportedClassVersionError`, `OutOfMemoryError: Java heap space`,
/// `Duplicate mods found:` with the colon the loader emits, the Windows
/// sharing-violation sentence. Every looser wording of the same idea lives in
/// `candidate`, because one sentence of prose must never become a diagnosis.
const RULE_SPECS: &[RuleSpec] = &[
    RuleSpec {
        id: rules::JAVA_MISMATCH,
        version: 2,
        confirmed: &[
            "unsupportedclassversionerror",
            "unsupported class file major version",
            "has been compiled by a more recent version of the java runtime",
        ],
        candidate: &[
            "class file version",
            "requires java ",
            "java 21 is required",
            "java 17 is required",
            "incompatible java",
            "please use java",
        ],
        kind: RepairKind::ChooseRuntime,
        summary: "Pick a Java runtime that this game version accepts, then start again",
        help_entry: "java_mismatch",
    },
    RuleSpec {
        id: rules::MISSING_DEPENDENCY,
        version: 2,
        confirmed: &[
            "missing or unsupported mandatory dependencies",
            "dependency resolution failed",
            "which is missing",
            "requires any version of",
        ],
        candidate: &[
            "requires exactly 1 ",
            "noclassdeffounderror",
            "classnotfoundexception",
            "mod resolution encountered an error",
            "failed to load mod",
        ],
        kind: RepairKind::AddFile,
        summary: "Install the missing dependency, then start the instance again",
        help_entry: "missing_dependency",
    },
    RuleSpec {
        id: rules::DUPLICATE_MOD,
        version: 2,
        // The loader prints `Duplicate mods found:` before the list; the colon is
        // the anchor that separates a report from a sentence about duplicates.
        confirmed: &[
            "duplicate mods found:",
            "duplicate mod id detected:",
            "duplicate mod:",
            "duplicate mod id:",
            "found duplicate mod:",
        ],
        candidate: &[
            "duplicate mods found",
            "two mods with the same id",
            "duplicate entry for",
            "already present in the mods folder",
        ],
        kind: RepairKind::DeleteFile,
        summary: "Remove one of the duplicate mod files, then start the instance again",
        help_entry: "duplicate_mod",
    },
    RuleSpec {
        id: rules::FILE_LOCK,
        version: 2,
        confirmed: &[
            "the process cannot access the file because it is being used by another process",
            "being used by another process",
            "the file is locked by another process",
            "file is in use by another process",
        ],
        candidate: &[
            "access is denied",
            "permission denied",
            "failed to write file",
        ],
        kind: RepairKind::CloseProcess,
        summary: "Close the program that still holds the file, then retry",
        help_entry: "file_lock",
    },
    RuleSpec {
        id: rules::OUT_OF_MEMORY,
        version: 2,
        confirmed: &[
            "java.lang.outofmemoryerror",
            "outofmemoryerror: java heap space",
            "gc overhead limit exceeded",
            "there is insufficient memory for the java runtime environment",
        ],
        candidate: &[
            "outofmemoryerror",
            "could not reserve enough space for object heap",
            "the paging file is too small",
        ],
        kind: RepairKind::AdjustMemory,
        summary: "Raise the instance memory limit or close other applications, then start again",
        help_entry: "out_of_memory",
    },
];

fn rule_index(id: &str) -> u32 {
    RULE_SPECS
        .iter()
        .position(|spec| spec.id == id)
        .map_or(u32::MAX, |index| index as u32)
}

pub fn rule_ids() -> Vec<&'static str> {
    RULE_SPECS.iter().map(|spec| spec.id).collect()
}

pub fn rule_version(id: &str) -> Option<u32> {
    RULE_SPECS
        .iter()
        .find(|spec| spec.id == id)
        .map(|spec| spec.version)
}

#[derive(Debug, Clone)]
pub struct AnalysisRequest {
    pub session: LaunchSessionId,
    pub instance_id: Option<InstanceId>,
    pub instance_revision: Revision,
    pub budget_ms: u64,
}

impl AnalysisRequest {
    pub fn new(session: LaunchSessionId) -> Self {
        Self {
            session,
            instance_id: None,
            instance_revision: Revision::ZERO,
            budget_ms: DEFAULT_BUDGET_MS,
        }
    }

    pub fn with_instance(mut self, instance_id: Option<InstanceId>) -> Self {
        self.instance_id = instance_id;
        self
    }

    pub fn with_revision(mut self, revision: Revision) -> Self {
        self.instance_revision = revision;
        self
    }

    pub fn with_budget_ms(mut self, budget_ms: u64) -> Self {
        self.budget_ms = budget_ms.min(MAX_BUDGET_MS);
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisOutcome {
    pub report: DiagnosticReport,
    /// `true` when the answer came from the cache without scanning new lines.
    pub cached: bool,
}

/// Wall-clock budget meter. In tests a stepped clock makes "timed out" a
/// deterministic outcome instead of a flaky race.
struct BudgetMeter {
    budget: Duration,
    start: Instant,
    step: Option<Duration>,
    probes: u32,
}

impl BudgetMeter {
    fn probe(&mut self) -> Duration {
        self.probes += 1;
        match self.step {
            Some(step) => step * self.probes,
            None => self.start.elapsed(),
        }
    }

    fn elapsed_ms(&mut self) -> u64 {
        self.probe().as_millis() as u64
    }

    fn exhausted(&mut self) -> bool {
        self.probe() >= self.budget
    }
}

#[derive(Clone)]
struct CachedAnalysis {
    scanned_until: u64,
    instance_revision: Revision,
    report: DiagnosticReport,
}

/// Rule engine plus its bounded report cache.
pub struct DiagnosticEngine {
    store: LogStore,
    cache: Mutex<BTreeMap<LaunchSessionId, CachedAnalysis>>,
    budget_step: Mutex<Option<Duration>>,
}

impl DiagnosticEngine {
    pub fn new(store: LogStore) -> Self {
        Self {
            store,
            cache: Mutex::new(BTreeMap::new()),
            budget_step: Mutex::new(None),
        }
    }

    /// Deterministic budget clock: every `elapsed` probe advances by `step`
    /// instead of reading the monotonic clock. Used by the timeout test so the
    /// assertion does not depend on machine speed.
    pub fn set_budget_step(&self, step: Option<Duration>) {
        *self
            .budget_step
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = step;
    }

    pub fn cached_report(&self, session: &LaunchSessionId) -> Option<DiagnosticReport> {
        self.cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(session)
            .map(|entry| entry.report.clone())
    }

    pub fn find(&self, session: &LaunchSessionId, finding_id: &str) -> Option<Finding> {
        self.cached_report(session).and_then(|report| {
            report
                .findings
                .into_iter()
                .find(|finding| finding.finding_id == finding_id)
        })
    }

    /// Analyse the new lines of a session, plus a fixed context window before the
    /// cursor. `cancelled` is polled between rules; a cancelled run keeps what it
    /// already produced and reports `cancelled`.
    pub fn analyze(
        &self,
        request: &AnalysisRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<AnalysisOutcome, DiagnosticsError> {
        let status =
            self.store
                .status(&request.session)
                .ok_or_else(|| DiagnosticsError::LogsNotFound {
                    session: request.session.as_str().to_string(),
                })?;
        let last_sequence = status.last_sequence;
        let log_incomplete = status.lossy();

        let mut meter = BudgetMeter {
            budget: Duration::from_millis(request.budget_ms.min(MAX_BUDGET_MS)),
            start: Instant::now(),
            step: *self
                .budget_step
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            probes: 0,
        };

        // Cache lookup. `Reusable` means the cached report already covers every
        // retained line for this instance revision; `Increment` means only new
        // lines (plus the context window) have to be scanned.
        enum CacheLookup {
            Reusable(DiagnosticReport),
            Increment(Box<CachedAnalysis>),
            Miss,
        }
        let lookup = {
            let cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
            match cache.get(&request.session) {
                Some(entry) if entry.instance_revision == request.instance_revision => {
                    if entry.scanned_until >= last_sequence {
                        CacheLookup::Reusable(entry.report.clone())
                    } else {
                        CacheLookup::Increment(Box::new(entry.clone()))
                    }
                }
                _ => CacheLookup::Miss,
            }
        };
        let previous = match lookup {
            CacheLookup::Reusable(mut report) => {
                report.status = if report.findings.is_empty() {
                    ReportStatus::NoMatches
                } else {
                    ReportStatus::Completed
                };
                report.budget = AnalysisBudget {
                    budget_ms: request.budget_ms.min(MAX_BUDGET_MS),
                    elapsed_ms: meter.elapsed_ms(),
                    timed_out: false,
                };
                report.log_incomplete = log_incomplete;
                report.conclusions_possible = report.status.is_conclusive() && !log_incomplete;
                return Ok(AnalysisOutcome {
                    report,
                    cached: true,
                });
            }
            CacheLookup::Increment(entry) => Some(*entry),
            CacheLookup::Miss => None,
        };

        let scan_from = match &previous {
            Some(entry) => entry.scanned_until.saturating_sub(CONTEXT_LINES) + 1,
            None => 1,
        };
        let scan_to = last_sequence;
        let lines = self.collect_lines(&request.session, scan_from, scan_to)?;

        let mut findings: Vec<Finding> = previous
            .as_ref()
            .map(|entry| entry.report.findings.clone())
            .unwrap_or_default();
        let mut scanned_rules: Vec<&'static str> = Vec::new();
        let mut skipped_rules: Vec<&'static str> = Vec::new();
        let mut timed_out = false;
        let mut was_cancelled = false;

        for spec in RULE_SPECS {
            if cancelled() {
                was_cancelled = true;
                skipped_rules.push(spec.id);
                continue;
            }
            if meter.exhausted() {
                timed_out = true;
                skipped_rules.push(spec.id);
                continue;
            }
            let hit = scan_rule(spec, &request.session, &request.instance_id, &lines);
            merge_findings(&mut findings, hit);
            scanned_rules.push(spec.id);
        }

        findings.sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
        findings.dedup_by(|left, right| left.finding_id == right.finding_id);
        findings.truncate(MAX_FINDINGS);

        let status_of_run = if was_cancelled {
            ReportStatus::Cancelled
        } else if timed_out {
            ReportStatus::TimedOut
        } else if findings.is_empty() {
            ReportStatus::NoMatches
        } else {
            ReportStatus::Completed
        };

        let report_from = previous
            .as_ref()
            .map_or(scan_from, |entry| entry.report.from_sequence.min(scan_from));
        let cache_key = CacheKey {
            session: request.session.clone(),
            from_sequence: report_from,
            to_sequence: scan_to,
            instance_revision: request.instance_revision,
            rules_version: RULES_VERSION,
        };
        let report = DiagnosticReport {
            session_id: request.session.clone(),
            instance_id: request
                .instance_id
                .clone()
                .or_else(|| self.store.instance_of(&request.session)),
            instance_revision: request.instance_revision,
            status: status_of_run,
            conclusions_possible: status_of_run.is_conclusive() && !log_incomplete && scan_to > 0,
            findings,
            budget: AnalysisBudget {
                budget_ms: request.budget_ms.min(MAX_BUDGET_MS),
                elapsed_ms: meter.elapsed_ms(),
                timed_out,
            },
            from_sequence: report_from,
            to_sequence: scan_to,
            scanned_rules,
            skipped_rules,
            cache_key,
            log_incomplete,
        };

        {
            let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
            // A cancelled or timed-out run must not advance the cursor past the
            // rules it did not scan, otherwise the skipped lines are lost forever.
            let scanned_until = if was_cancelled || timed_out {
                previous.as_ref().map_or(0, |entry| entry.scanned_until)
            } else {
                scan_to
            };
            cache.insert(
                request.session.clone(),
                CachedAnalysis {
                    scanned_until,
                    instance_revision: request.instance_revision,
                    report: report.clone(),
                },
            );
            while cache.len() > 32 {
                let first = cache.keys().next().cloned().expect("cache is not empty");
                cache.remove(&first);
            }
        }

        Ok(AnalysisOutcome {
            report,
            cached: false,
        })
    }

    fn collect_lines(
        &self,
        session: &LaunchSessionId,
        from: u64,
        to: u64,
    ) -> Result<Vec<(u64, String)>, DiagnosticsError> {
        let mut collection = Vec::new();
        let mut cursor = from.saturating_sub(1);
        loop {
            let page = self.store.read_log(session, Some(cursor), 1_000)?;
            let empty = page.records.is_empty();
            for record in page.records {
                if record.sequence > to {
                    continue;
                }
                collection.push((record.sequence, record.line.to_lowercase()));
            }
            cursor = page.next_cursor;
            if empty || !page.has_more || cursor >= to {
                break;
            }
        }
        Ok(collection)
    }
}

fn scan_rule(
    spec: &RuleSpec,
    session: &LaunchSessionId,
    instance: &Option<InstanceId>,
    lines: &[(u64, String)],
) -> Vec<Finding> {
    let mut confirmed_runs: Vec<(u64, u64)> = Vec::new();
    let mut candidate_runs: Vec<(u64, u64)> = Vec::new();

    for (sequence, lower) in lines {
        if spec.confirmed.iter().any(|pattern| lower.contains(pattern)) {
            push_run(&mut confirmed_runs, *sequence);
            continue;
        }
        if spec.candidate.iter().any(|pattern| lower.contains(pattern)) {
            push_run(&mut candidate_runs, *sequence);
        }
    }

    let mut findings = Vec::new();
    for (confidence, runs) in [
        (Confidence::Confirmed, confirmed_runs),
        (Confidence::Candidate, candidate_runs),
    ] {
        for run in runs {
            let reference = EvidenceRef {
                session_id: session.clone(),
                from_sequence: run.0,
                to_sequence: run.1,
            };
            findings.push(Finding {
                finding_id: finding_id(spec.id, session, run.0, run.1),
                rule_id: spec.id,
                rule_version: spec.version,
                session_id: session.clone(),
                instance_id: instance.clone(),
                confidence,
                evidence_refs: vec![reference],
                action: FindingAction {
                    kind: spec.kind,
                    target: None,
                    summary: spec.summary.to_string(),
                    help_entry: spec.help_entry.to_string(),
                },
            });
        }
    }
    findings
}

fn push_run(runs: &mut Vec<(u64, u64)>, sequence: u64) {
    if runs.len() >= MAX_EVIDENCE {
        return;
    }
    match runs.last_mut() {
        Some(last) if sequence.saturating_sub(last.1) <= MERGE_GAP => last.1 = sequence,
        _ => runs.push((sequence, sequence)),
    }
}

fn merge_findings(findings: &mut Vec<Finding>, mut incoming: Vec<Finding>) {
    for finding in incoming.drain(..) {
        match findings
            .iter_mut()
            .find(|existing| existing.finding_id == finding.finding_id)
        {
            Some(existing) => {
                for reference in finding.evidence_refs {
                    if existing.evidence_refs.len() < MAX_EVIDENCE
                        && !existing
                            .evidence_refs
                            .iter()
                            .any(|known| known.from_sequence == reference.from_sequence)
                    {
                        existing.evidence_refs.push(reference);
                    }
                }
                existing
                    .evidence_refs
                    .sort_by_key(|reference| reference.from_sequence);
            }
            None => findings.push(finding),
        }
    }
}

fn finding_id(rule: &str, session: &LaunchSessionId, from: u64, to: u64) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(rule.as_bytes());
    hasher.update([0x1f]);
    hasher.update(session.as_str().as_bytes());
    hasher.update([0x1f]);
    hasher.update(from.to_string().as_bytes());
    hasher.update(b"-");
    hasher.update(to.to_string().as_bytes());
    let digest = hasher.finalize();
    let mut rendered = String::with_capacity(24);
    rendered.push_str("finding-");
    for byte in digest.iter().take(8) {
        rendered.push_str(&format!("{byte:02x}"));
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::logs::{LogRecord, LogSink, LogStream};

    fn engine_and_session(name: &str) -> (DiagnosticEngine, LogStore, LaunchSessionId) {
        let dir = std::env::temp_dir().join(format!(
            "shtmcl-08-rules-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let store = LogStore::new(&dir);
        let session = LaunchSessionId::new("session-rules").expect("id");
        store.register_session(&session, None);
        (DiagnosticEngine::new(store.clone()), store, session)
    }

    fn feed(store: &LogStore, session: &LaunchSessionId, lines: &[&str]) {
        let mut records = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            records.push(LogRecord {
                sequence: index as u64 + 1,
                stream: LogStream::Stdout,
                line: (*line).to_string(),
                omitted_bytes: 0,
            });
        }
        store.append(session, &records);
    }

    #[test]
    fn prose_hits_are_candidates_and_only_the_machine_form_is_confirmed() {
        let (engine, store, session) = engine_and_session("prose");
        feed(
            &store,
            &session,
            &[
                "[12:00:00] [main/INFO] Duplicate mods found in the folder listing (this is prose, not a duplicate-mod report)",
                "[12:00:00] [main/INFO] see the wiki: class file version mismatches are common",
            ],
        );
        let outcome = engine
            .analyze(&AnalysisRequest::new(session.clone()), &|| false)
            .expect("analysis");
        let duplicates: Vec<&Finding> = outcome
            .report
            .findings
            .iter()
            .filter(|finding| finding.rule_id == rules::DUPLICATE_MOD)
            .collect();
        assert!(
            !duplicates.is_empty(),
            "the prose mention is still a hint: {:?}",
            outcome.report.findings
        );
        for finding in &duplicates {
            assert_eq!(
                finding.confidence,
                Confidence::Candidate,
                "prose must not be reported as a confirmed condition"
            );
        }
        let java: Vec<&Finding> = outcome
            .report
            .findings
            .iter()
            .filter(|finding| finding.rule_id == rules::JAVA_MISMATCH)
            .collect();
        assert!(java
            .iter()
            .all(|finding| finding.confidence == Confidence::Candidate));

        // The machine form of the same condition stays confirmed.
        let (engine, store, session) = engine_and_session("machine");
        feed(
            &store,
            &session,
            &["[12:00:00] [main/WARN] Duplicate mods found: sodium 0.5.8 and sodium 0.5.3"],
        );
        let outcome = engine
            .analyze(&AnalysisRequest::new(session.clone()), &|| false)
            .expect("analysis");
        assert_eq!(outcome.report.findings.len(), 1);
        assert_eq!(outcome.report.findings[0].confidence, Confidence::Confirmed);
    }

    #[test]
    fn each_rule_matches_its_positive_sample_and_ignores_the_negative_one() {
        let (engine, store, session) = engine_and_session("samples");
        feed(
            &store,
            &session,
            &[
                "[12:00:00] [main/INFO] Using Java 17 (17.0.9) from C:\\Java\\jdk-17",
                "[12:00:00] [main/ERROR] java.lang.UnsupportedClassVersionError: net/fabricmc/loader/impl/launch/server/FabricServerLauncher has been compiled by a more recent version of the Java Runtime (class file version 61.0)",
                "[12:00:00] [Worker-Main-1/INFO] Loading 42 mods",
                "[12:00:00] [main/ERROR] Mod resolution encountered an error: mod fabri c requires any version of fabric-api, which is missing",
                "[12:00:00] [main/WARN] Duplicate mods found: sodium 0.5.8 and sodium 0.5.3",
                "[12:00:00] [main/ERROR] java.nio.file.FileSystemException: mods/sodium.jar: The process cannot access the file because it is being used by another process",
                "[12:00:00] [main/INFO] Memory usage: 512 MB / 2048 MB",
                "[12:00:00] [main/ERROR] java.lang.OutOfMemoryError: Java heap space",
            ],
        );
        let outcome = engine
            .analyze(&AnalysisRequest::new(session.clone()), &|| false)
            .expect("analysis");
        let rules: Vec<&str> = outcome
            .report
            .findings
            .iter()
            .map(|finding| finding.rule_id)
            .collect();
        for expected in rule_ids() {
            assert!(
                rules.contains(&expected),
                "rule {expected} should match a positive sample, got {rules:?}"
            );
        }
        assert_eq!(outcome.report.status, ReportStatus::Completed);
        assert!(outcome.report.conclusions_possible);
        // Confidence levels are separated: the Java and memory findings are
        // confirmed conditions, the info lines produced nothing of their own.
        assert!(outcome
            .report
            .findings
            .iter()
            .any(|finding| finding.confidence == Confidence::Confirmed));
    }

    #[test]
    fn a_clean_log_yields_no_findings_and_no_false_positive() {
        let (engine, store, session) = engine_and_session("clean");
        feed(
            &store,
            &session,
            &[
                "[12:00:00] [main/INFO] Using Java 17 (17.0.9) from C:\\Java\\jdk-17",
                "[12:00:00] [Worker-Main-1/INFO] Loading 42 mods",
                "[12:00:00] [main/INFO] Memory usage: 512 MB / 2048 MB",
                "[12:00:00] [main/INFO] Wrote options.txt",
                "[12:00:00] [Render thread/INFO] Sound engine started",
            ],
        );
        let outcome = engine
            .analyze(&AnalysisRequest::new(session.clone()), &|| false)
            .expect("analysis");
        assert!(
            outcome.report.findings.is_empty(),
            "no finding on a clean log: {:?}",
            outcome.report.findings
        );
        assert_eq!(outcome.report.status, ReportStatus::NoMatches);
        assert_eq!(outcome.report.verdict(), "no_matches");
    }

    #[test]
    fn a_timeout_keeps_finished_rules_and_never_claims_the_log_is_clean() {
        let (engine, store, session) = engine_and_session("timeout");
        feed(
            &store,
            &session,
            &["[12:00:00] [main/ERROR] java.lang.OutOfMemoryError: Java heap space"],
        );
        engine.set_budget_step(Some(Duration::from_millis(900)));
        let outcome = engine
            .analyze(&AnalysisRequest::new(session.clone()), &|| false)
            .expect("analysis");
        let report = outcome.report;
        assert!(report.budget.timed_out);
        assert_eq!(report.status, ReportStatus::TimedOut);
        assert!(!report.conclusions_possible);
        assert!(!report.status.is_conclusive());
        assert_eq!(report.verdict(), "inconclusive");
        assert!(
            !report.scanned_rules.is_empty(),
            "rules that finished are kept"
        );
        assert!(
            !report.skipped_rules.is_empty(),
            "rules that did not run are named"
        );
        assert_eq!(report.budget.budget_ms, DEFAULT_BUDGET_MS);
    }

    #[test]
    fn a_second_run_only_reads_the_increment_and_reuses_the_cache() {
        let (engine, store, session) = engine_and_session("increment");
        feed(
            &store,
            &session,
            &["[12:00:00] [main/INFO] Loading 42 mods"],
        );
        let first = engine
            .analyze(&AnalysisRequest::new(session.clone()), &|| false)
            .expect("analysis");
        assert!(!first.cached);

        let second = engine
            .analyze(&AnalysisRequest::new(session.clone()), &|| false)
            .expect("analysis");
        assert!(second.cached, "nothing new: the cache answers");
        assert_eq!(second.report.to_sequence, first.report.to_sequence);

        feed(
            &store,
            &session,
            &["[12:00:00] [main/ERROR] Duplicate mods found: sodium"],
        );
        let third = engine
            .analyze(&AnalysisRequest::new(session.clone()), &|| false)
            .expect("analysis");
        assert!(!third.cached);
        assert!(third.report.to_sequence > second.report.to_sequence);
        assert_eq!(third.report.findings.len(), 1);
        assert!(third
            .report
            .cache_key
            .as_string()
            .contains("diagnostics-rules"));
    }

    #[test]
    fn a_new_instance_revision_invalidates_the_cache() {
        let (engine, store, session) = engine_and_session("revision");
        feed(
            &store,
            &session,
            &["[12:00:00] [main/INFO] Loading 42 mods"],
        );
        engine
            .analyze(&AnalysisRequest::new(session.clone()), &|| false)
            .expect("analysis");
        let rerun = engine
            .analyze(
                &AnalysisRequest::new(session.clone()).with_revision(Revision::new(7)),
                &|| false,
            )
            .expect("analysis");
        assert!(
            !rerun.cached,
            "a different instance revision must rescan, not reuse the report"
        );
    }

    /// Budget measurement (run with `--nocapture`): the real rule pass over a full
    /// retained window. The numbers go into the 08 handoff.
    #[test]
    fn budget_measurement_for_a_full_window() {
        let (engine, store, session) = engine_and_session("measure");
        let mut lines: Vec<String> = Vec::new();
        for index in 0..4_900 {
            lines.push(format!(
                "[12:00:00] [main/INFO] warmup {index:05} {}",
                "x".repeat(48)
            ));
        }
        lines.push(
            "[12:00:00] [main/ERROR] java.lang.OutOfMemoryError: Java heap space".to_string(),
        );
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        feed(&store, &session, &refs);
        let started = std::time::Instant::now();
        let outcome = engine
            .analyze(&AnalysisRequest::new(session.clone()), &|| false)
            .expect("analysis");
        let elapsed = started.elapsed();
        println!(
            "analysis budget: lines_read={} rules={} findings={} elapsed_ms={} budget_ms={} timed_out={}",
            outcome.report.to_sequence,
            outcome.report.scanned_rules.len(),
            outcome.report.findings.len(),
            elapsed.as_millis(),
            outcome.report.budget.budget_ms,
            outcome.report.budget.timed_out
        );
        assert!(!outcome.report.budget.timed_out);
        assert_eq!(outcome.report.findings.len(), 1);
    }
}

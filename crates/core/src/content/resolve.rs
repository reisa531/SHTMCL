//! Bounded dependency solver with explainable failure modes.
//!
//! Design constraints taken from `docs/development/04-content-and-modpacks.md` §5:
//! - candidates are filtered by game version and loader *before* search,
//! - same-project constraints are merged and conflicts are detected early,
//! - search is backtracking with "fewest candidates first" (MRV) ordering and
//!   constraint propagation; there is no unbounded search,
//! - the budget is a wall-clock deadline **or** a count of distinct assignment
//!   states, whichever is reached first,
//! - `solved`, `unsatisfiable` (proven, with a conflict chain) and
//!   `budget_exhausted` (search incomplete) are different outcomes, so a partial
//!   search can never be presented as "no compatible version",
//! - cooperative cancellation is checked on every state.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::error::ContentError;
use super::loader::{GameTarget, LoaderKind};
use super::ports::SourceKind;
use super::version::{Version, VersionRange};

/// Default budget from the module design: 2 s wall clock or 10,000 states.
pub const DEFAULT_MAX_STATES: u64 = 10_000;
pub const DEFAULT_MAX_WALL_MS: u64 = 2_000;
/// Hard ceiling on search depth. Real dependency chains are far shallower; the cap
/// exists so a pathological chain is reported instead of exhausting the budget.
pub const DEFAULT_MAX_DEPTH: u32 = 256;
/// A caller may not raise the budget above the documented envelope.
pub const MAX_STATES_CEILING: u64 = 10_000;
pub const MAX_WALL_CEILING_MS: u64 = 2_000;
pub const MAX_DEPTH_CEILING: u32 = 4_096;

/// A synchronous `content.solve` read must answer quickly and must never block the
/// dispatch thread: anything larger belongs on `content.solve.task`.
pub const READ_MAX_STATES: u64 = 400;
pub const READ_MAX_WALL_MS: u64 = 200;
pub const READ_MAX_DEPTH: u32 = 128;
/// Upper bound on how many dead ends are reported back to the user.
const MAX_CONFLICTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub project_id: String,
    pub range: VersionRange,
}

impl Dependency {
    pub fn new(project_id: impl Into<String>, range: VersionRange) -> Self {
        Self {
            project_id: project_id.into(),
            range,
        }
    }
}

/// One installable file of a project, already constrained to a game version and
/// loader combination by the caller/provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub provider: SourceKind,
    pub project_id: String,
    pub file_id: String,
    pub version: Version,
    pub game_versions: Vec<String>,
    pub loaders: Vec<LoaderKind>,
    pub dependencies: Vec<Dependency>,
    pub incompatible: Vec<Dependency>,
}

impl Candidate {
    pub fn new(
        provider: SourceKind,
        project_id: impl Into<String>,
        file_id: impl Into<String>,
        version: Version,
    ) -> Self {
        Self {
            provider,
            project_id: project_id.into(),
            file_id: file_id.into(),
            version,
            game_versions: Vec::new(),
            loaders: Vec::new(),
            dependencies: Vec::new(),
            incompatible: Vec::new(),
        }
    }

    pub fn for_target(mut self, game_version: impl Into<String>, loader: LoaderKind) -> Self {
        self.game_versions = vec![game_version.into()];
        self.loaders = vec![loader];
        self
    }

    pub fn depends_on(mut self, dependency: Dependency) -> Self {
        self.dependencies.push(dependency);
        self
    }

    pub fn incompatible_with(mut self, dependency: Dependency) -> Self {
        self.incompatible.push(dependency);
        self
    }

    /// Empty metadata is *not* compatible: a missing game version or loader list
    /// must not be treated as "supports everything".
    pub fn matches_target(&self, target: &GameTarget) -> bool {
        let game_ok = self
            .game_versions
            .iter()
            .any(|value| value == "*" || value == &target.game_version);
        let loader_ok = self.loaders.iter().any(|value| *value == target.loader);
        game_ok && loader_ok
    }

    fn conflicts_with(&self, other: &Candidate) -> bool {
        self.incompatible
            .iter()
            .any(|rule| rule.project_id == other.project_id && rule.range.satisfies(&other.version))
            || other.incompatible.iter().any(|rule| {
                rule.project_id == self.project_id && rule.range.satisfies(&self.version)
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolveBudget {
    pub max_states: u64,
    pub max_wall: Duration,
    /// Maximum search depth (number of simultaneously assigned projects).
    pub max_depth: u32,
}

impl Default for SolveBudget {
    fn default() -> Self {
        Self {
            max_states: DEFAULT_MAX_STATES,
            max_wall: Duration::from_millis(DEFAULT_MAX_WALL_MS),
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }
}

impl SolveBudget {
    pub fn new(max_states: u64, max_wall: Duration) -> Self {
        Self {
            max_states,
            max_wall,
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }

    pub fn with_depth(mut self, max_depth: u32) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Clamp a caller-supplied budget into the documented envelope. A request may
    /// ask for *less* work, never more.
    pub fn clamped(self) -> Self {
        Self {
            max_states: self.max_states.min(MAX_STATES_CEILING),
            max_wall: self
                .max_wall
                .min(Duration::from_millis(MAX_WALL_CEILING_MS)),
            max_depth: self.max_depth.min(MAX_DEPTH_CEILING),
        }
    }

    /// Clamp for the synchronous read path: a read must not hold the dispatch
    /// thread. Larger problems go through `content.solve.task`.
    pub fn clamped_for_read(self) -> Self {
        let clamped = self.clamped();
        Self {
            max_states: clamped.max_states.min(READ_MAX_STATES),
            max_wall: clamped
                .max_wall
                .min(Duration::from_millis(READ_MAX_WALL_MS)),
            max_depth: clamped.max_depth.min(READ_MAX_DEPTH),
        }
    }

    pub fn from_value(value: &Value) -> Result<Self, ContentError> {
        let object = value
            .as_object()
            .ok_or_else(|| ContentError::InvalidManifest {
                detail: "budget must be an object".to_string(),
            })?;
        let mut budget = Self::default();
        if let Some(states) = object.get("maxStates").and_then(Value::as_u64) {
            budget.max_states = states;
        }
        if let Some(wall) = object.get("maxWallMs").and_then(Value::as_u64) {
            budget.max_wall = Duration::from_millis(wall);
        }
        if let Some(depth) = object.get("maxDepth").and_then(Value::as_u64) {
            budget.max_depth = depth.min(u32::MAX as u64) as u32;
        }
        Ok(budget)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolveRequest {
    pub target: GameTarget,
    pub roots: Vec<String>,
    pub candidates: Vec<Candidate>,
    pub budget: SolveBudget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetLimit {
    States,
    WallClock,
    /// The dependency chain is deeper than `SolveBudget::max_depth`.
    Depth,
}

impl BudgetLimit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::States => "states",
            Self::WallClock => "wall_clock",
            Self::Depth => "depth",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// A required project has no candidate at all for this target.
    MissingProject,
    /// Candidates exist but none satisfies the merged version constraints.
    RangeUnmet,
    /// Two assigned candidates declare each other incompatible.
    IncompatiblePair,
}

impl ConflictReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingProject => "missing_project",
            Self::RangeUnmet => "range_unmet",
            Self::IncompatiblePair => "incompatible_pair",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintOrigin {
    pub project_id: String,
    pub file_id: String,
    pub range: String,
}

impl ConstraintOrigin {
    fn to_value(&self) -> Value {
        json!({
            "projectId": self.project_id,
            "fileId": self.file_id,
            "range": self.range,
        })
    }
}

/// One explainable dead end. Every entry names who asked, for what, and what was
/// actually available, so the UI can offer a concrete next action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictEntry {
    pub project_id: String,
    pub reason: ConflictReason,
    pub requested_by: Vec<ConstraintOrigin>,
    pub available: Vec<String>,
}

impl ConflictEntry {
    fn to_value(&self) -> Value {
        json!({
            "projectId": self.project_id,
            "reason": self.reason.as_str(),
            "requestedBy": self.requested_by.iter().map(ConstraintOrigin::to_value).collect::<Vec<_>>(),
            "available": self.available,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub provider: SourceKind,
    pub project_id: String,
    pub file_id: String,
    pub version: String,
}

impl Assignment {
    fn to_value(&self) -> Value {
        json!({
            "provider": self.provider.as_str(),
            "projectId": self.project_id,
            "fileId": self.file_id,
            "version": self.version,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolveOutcome {
    Solved {
        assignments: Vec<Assignment>,
    },
    Unsatisfiable {
        conflicts: Vec<ConflictEntry>,
    },
    BudgetExhausted {
        limit: BudgetLimit,
        used_states: u64,
    },
    Cancelled {
        used_states: u64,
    },
}

impl SolveOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Solved { .. } => "solved",
            Self::Unsatisfiable { .. } => "unsatisfiable",
            Self::BudgetExhausted { .. } => "budget_exhausted",
            Self::Cancelled { .. } => "cancelled",
        }
    }

    /// Only a complete solution may produce an installable lock.
    pub fn is_installable(&self) -> bool {
        matches!(self, Self::Solved { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolveResult {
    pub target: GameTarget,
    pub outcome: SolveOutcome,
    pub states_visited: u64,
    pub elapsed_ms: u64,
    pub candidates_considered: usize,
    pub filtered_out: usize,
}

impl SolveResult {
    /// Number of reported conflict entries (dead ends found by the search).
    pub fn conflicts_len(&self) -> usize {
        match &self.outcome {
            SolveOutcome::Unsatisfiable { conflicts } => conflicts.len(),
            _ => 0,
        }
    }

    pub fn to_value(&self) -> Value {
        let (assignments, conflicts, limit) = match &self.outcome {
            SolveOutcome::Solved { assignments } => (
                assignments
                    .iter()
                    .map(Assignment::to_value)
                    .collect::<Vec<_>>(),
                Vec::new(),
                Value::Null,
            ),
            SolveOutcome::Unsatisfiable { conflicts } => (
                Vec::new(),
                conflicts
                    .iter()
                    .map(ConflictEntry::to_value)
                    .collect::<Vec<_>>(),
                Value::Null,
            ),
            SolveOutcome::BudgetExhausted { limit, .. } => {
                (Vec::new(), Vec::new(), json!(limit.as_str()))
            }
            SolveOutcome::Cancelled { .. } => (Vec::new(), Vec::new(), Value::Null),
        };
        json!({
            "target": self.target.to_value(),
            "outcome": self.outcome.label(),
            "installable": self.outcome.is_installable(),
            "assignments": assignments,
            "conflicts": conflicts,
            "budgetLimit": limit,
            "statesVisited": self.states_visited,
            "elapsedMs": self.elapsed_ms,
            "candidatesConsidered": self.candidates_considered,
            "filteredOut": self.filtered_out,
        })
    }
}

/// One decision point of the depth-first search.
///
/// The search is **iterative**: it keeps an explicit stack of frames instead of
/// recursing, so a 5,000-deep dependency chain cannot overflow the thread stack
/// (the previous recursive version aborted the process at ~400 levels on a 1 MiB
/// main stack). Depth is also bounded by the budget, and hitting that bound is
/// reported as `budget_exhausted` rather than a crash.
struct Frame<'a> {
    project_id: String,
    viable: Vec<&'a Candidate>,
    next: usize,
    /// Origins this frame pushed, with the length of each target list *before* the
    /// push, so backtracking can undo exactly what it added.
    pushed: Vec<(String, usize)>,
}

enum Decision<'a> {
    /// Every requirement is assigned.
    Complete,
    /// The current partial assignment is a dead end, with the reason.
    Dead(ConflictEntry),
    /// Branch on this project's viable candidates.
    Branch(String, Vec<&'a Candidate>),
}

struct Search<'a> {
    index: &'a BTreeMap<String, Vec<&'a Candidate>>,
    max_states: u64,
    max_depth: usize,
    deadline: Instant,
    states: u64,
    limit_hit: Option<BudgetLimit>,
    cancelled: bool,
    assignment: BTreeMap<String, &'a Candidate>,
    /// Merged constraints per project, with their origin chain.
    required: BTreeMap<String, Vec<ConstraintOrigin>>,
    conflicts: Vec<ConflictEntry>,
}

impl<'a> Search<'a> {
    fn record(&mut self, entry: ConflictEntry) {
        if self.conflicts.len() >= MAX_CONFLICTS {
            return;
        }
        if self.conflicts.iter().any(|existing| {
            existing.project_id == entry.project_id && existing.reason == entry.reason
        }) {
            return;
        }
        self.conflicts.push(entry);
    }

    /// Consume one unit of budget. Returns `false` when the search must stop.
    fn charge(&mut self) -> bool {
        if self.states >= self.max_states {
            self.limit_hit = Some(BudgetLimit::States);
            return false;
        }
        if Instant::now() >= self.deadline {
            self.limit_hit = Some(BudgetLimit::WallClock);
            return false;
        }
        self.states += 1;
        true
    }

    fn step(&mut self, cancel: &dyn Fn() -> bool) -> bool {
        let mut stack: Vec<Frame<'a>> = Vec::new();

        loop {
            if cancel() {
                self.cancelled = true;
                return false;
            }
            if self.limit_hit.is_some() {
                return false;
            }

            // 1. Constraint propagation: validate every pending requirement and pick
            //    the most constrained project (fewest viable candidates).
            match self.decide() {
                Decision::Complete => return true,
                Decision::Dead(entry) => {
                    self.record(entry);
                    if !self.rewind(&mut stack) {
                        return false;
                    }
                }
                Decision::Branch(project_id, viable) => {
                    if stack.len() >= self.max_depth {
                        // Bounded search: a chain deeper than the budget is reported,
                        // never executed until the stack dies.
                        self.limit_hit = Some(BudgetLimit::Depth);
                        return false;
                    }
                    stack.push(Frame {
                        project_id,
                        viable,
                        next: 0,
                        pushed: Vec::new(),
                    });
                }
            }

            // 2. Advance the top frame until one candidate sticks or it runs out.
            loop {
                let Some(frame) = stack.last_mut() else {
                    return false;
                };
                if frame.next >= frame.viable.len() {
                    let frame = stack.pop().expect("frame is present");
                    self.undo(&frame);
                    if !self.rewind(&mut stack) {
                        return false;
                    }
                    break;
                }
                let candidate = frame.viable[frame.next];
                frame.next += 1;
                if !self.charge() {
                    return false;
                }
                match self.apply_candidate(candidate) {
                    Ok(pushed) => {
                        let frame = stack.last_mut().expect("frame is present");
                        frame.pushed = pushed;
                        break;
                    }
                    Err(conflict) => self.record(conflict),
                }
            }
        }
    }

    /// Pick the next decision point without mutating the assignment.
    fn decide(&self) -> Decision<'a> {
        let mut best: Option<(usize, String, Vec<&'a Candidate>)> = None;
        for (project_id, origins) in &self.required {
            if self.assignment.contains_key(project_id) {
                continue;
            }
            let candidates: &'a Vec<&'a Candidate> = match self.index.get(project_id) {
                Some(list) if !list.is_empty() => list,
                _ => {
                    return Decision::Dead(ConflictEntry {
                        project_id: project_id.clone(),
                        reason: ConflictReason::MissingProject,
                        requested_by: origins.clone(),
                        available: Vec::new(),
                    });
                }
            };
            let viable: Vec<&'a Candidate> = candidates
                .iter()
                .copied()
                .filter(|candidate| {
                    origins
                        .iter()
                        .all(|origin| range_of(origin).satisfies(&candidate.version))
                })
                .collect();
            if viable.is_empty() {
                return Decision::Dead(ConflictEntry {
                    project_id: project_id.clone(),
                    reason: ConflictReason::RangeUnmet,
                    requested_by: origins.clone(),
                    available: candidates
                        .iter()
                        .map(|candidate| candidate.version.as_str().to_string())
                        .collect(),
                });
            }
            let better = match &best {
                None => true,
                Some((count, id, _)) => {
                    viable.len() < *count || (viable.len() == *count && project_id < id)
                }
            };
            if better {
                best = Some((viable.len(), project_id.clone(), viable));
            }
        }

        match best {
            None => Decision::Complete,
            Some((_, project_id, viable)) => Decision::Branch(project_id, viable),
        }
    }

    /// Assign one candidate and push its dependency constraints.
    ///
    /// Returns the undo log on success, or the reason the candidate cannot be used.
    fn apply_candidate(
        &mut self,
        candidate: &'a Candidate,
    ) -> Result<Vec<(String, usize)>, ConflictEntry> {
        if let Some((other_id, other)) = self.assignment.iter().find(|(_, assigned)| {
            candidate.conflicts_with(assigned) && candidate.project_id != assigned.project_id
        }) {
            return Err(ConflictEntry {
                project_id: candidate.project_id.clone(),
                reason: ConflictReason::IncompatiblePair,
                requested_by: vec![ConstraintOrigin {
                    project_id: other_id.clone(),
                    file_id: other.file_id.clone(),
                    range: other.version.as_str().to_string(),
                }],
                available: vec![candidate.version.as_str().to_string()],
            });
        }

        let project_id = candidate.project_id.clone();
        self.assignment.insert(project_id.clone(), candidate);

        let mut pushed: Vec<(String, usize)> = Vec::new();
        for dependency in &candidate.dependencies {
            let origin = ConstraintOrigin {
                project_id: candidate.project_id.clone(),
                file_id: candidate.file_id.clone(),
                range: dependency.range.as_str().to_string(),
            };
            if let Some(assigned) = self.assignment.get(&dependency.project_id) {
                if !dependency.range.satisfies(&assigned.version) {
                    let conflict = ConflictEntry {
                        project_id: dependency.project_id.clone(),
                        reason: ConflictReason::RangeUnmet,
                        requested_by: vec![origin],
                        available: vec![assigned.version.as_str().to_string()],
                    };
                    self.undo_pushes(&pushed);
                    self.assignment.remove(&project_id);
                    return Err(conflict);
                }
            }
            let entry = self
                .required
                .entry(dependency.project_id.clone())
                .or_default();
            pushed.push((dependency.project_id.clone(), entry.len()));
            entry.push(origin);
        }
        Ok(pushed)
    }

    fn undo_pushes(&mut self, pushed: &[(String, usize)]) {
        for (project_id, length) in pushed.iter().rev() {
            if let Some(origins) = self.required.get_mut(project_id) {
                origins.truncate(*length);
                if origins.is_empty() {
                    self.required.remove(project_id);
                }
            }
        }
    }

    fn undo(&mut self, frame: &Frame<'a>) {
        self.undo_pushes(&frame.pushed);
        self.assignment.remove(&frame.project_id);
    }

    /// Drop exhausted frames until one still has candidates to try.
    fn rewind(&mut self, stack: &mut Vec<Frame<'a>>) -> bool {
        while let Some(frame) = stack.last() {
            if frame.next < frame.viable.len() {
                return true;
            }
            let frame = stack.pop().expect("frame is present");
            self.undo(&frame);
        }
        false
    }
}

fn range_of(origin: &ConstraintOrigin) -> VersionRange {
    // Origins are built from already-validated ranges; a parse failure here can
    // only come from an internal bug, so fall back to the widest range.
    VersionRange::parse(&origin.range).unwrap_or_else(|_| VersionRange::any())
}

/// Run the bounded search. Deterministic: candidate order is
/// (version descending, file id ascending), project order is lexicographic.
pub fn solve(request: &SolveRequest, cancel: &dyn Fn() -> bool) -> SolveResult {
    let started = Instant::now();

    let mut index: BTreeMap<String, Vec<&Candidate>> = BTreeMap::new();
    let mut filtered_out = 0usize;
    for candidate in &request.candidates {
        if candidate.matches_target(&request.target) {
            index
                .entry(candidate.project_id.clone())
                .or_default()
                .push(candidate);
        } else {
            filtered_out += 1;
        }
    }
    for list in index.values_mut() {
        list.sort_by(|left, right| {
            right
                .version
                .cmp(&left.version)
                .then_with(|| left.file_id.cmp(&right.file_id))
                .then_with(|| left.provider.as_str().cmp(right.provider.as_str()))
        });
    }

    let mut required: BTreeMap<String, Vec<ConstraintOrigin>> = BTreeMap::new();
    for root in &request.roots {
        required.entry(root.clone()).or_default();
    }

    let budget = request.budget.clone().clamped();
    let mut search = Search {
        index: &index,
        max_states: budget.max_states,
        max_depth: budget.max_depth as usize,
        deadline: started + budget.max_wall,
        states: 0,
        limit_hit: None,
        cancelled: false,
        assignment: BTreeMap::new(),
        required,
        conflicts: Vec::new(),
    };

    let solved = if request.roots.is_empty() {
        true
    } else {
        search.step(cancel)
    };

    let outcome = if search.cancelled {
        SolveOutcome::Cancelled {
            used_states: search.states,
        }
    } else if solved {
        SolveOutcome::Solved {
            assignments: search
                .assignment
                .iter()
                .map(|(project_id, candidate)| Assignment {
                    provider: candidate.provider,
                    project_id: project_id.clone(),
                    file_id: candidate.file_id.clone(),
                    version: candidate.version.as_str().to_string(),
                })
                .collect(),
        }
    } else if let Some(limit) = search.limit_hit {
        SolveOutcome::BudgetExhausted {
            limit,
            used_states: search.states,
        }
    } else {
        if search.conflicts.is_empty() {
            search.record(ConflictEntry {
                project_id: request.roots.first().cloned().unwrap_or_default(),
                reason: ConflictReason::MissingProject,
                requested_by: Vec::new(),
                available: Vec::new(),
            });
        }
        SolveOutcome::Unsatisfiable {
            conflicts: search.conflicts,
        }
    };

    let elapsed = started.elapsed();
    SolveResult {
        target: request.target.clone(),
        outcome,
        states_visited: search.states,
        elapsed_ms: elapsed.as_millis().min(u64::MAX as u128) as u64,
        candidates_considered: index.values().map(Vec::len).sum(),
        filtered_out,
    }
}

/// Parse one candidate from a JSON object (fixture/local-file input, or the shape
/// a source adapter returns).
pub fn candidate_from_value(value: &Value) -> Result<Candidate, ContentError> {
    let object = value
        .as_object()
        .ok_or_else(|| ContentError::InvalidManifest {
            detail: "candidate must be an object".to_string(),
        })?;
    let text = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_string);
    let project_id = text("projectId").ok_or_else(|| ContentError::InvalidManifest {
        detail: "candidate.projectId is required".to_string(),
    })?;
    let file_id = text("fileId").unwrap_or_else(|| project_id.clone());
    let version_raw = text("version").ok_or_else(|| ContentError::InvalidManifest {
        detail: format!("candidate {project_id} has no version"),
    })?;
    let provider = match text("provider") {
        Some(value) => SourceKind::parse(&value)?,
        None => SourceKind::LocalFile,
    };
    let mut candidate =
        Candidate::new(provider, project_id, file_id, Version::parse(&version_raw)?);
    if let Some(versions) = object.get("gameVersions").and_then(Value::as_array) {
        candidate.game_versions = versions
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
    if let Some(loaders) = object.get("loaders").and_then(Value::as_array) {
        candidate.loaders = loaders
            .iter()
            .filter_map(Value::as_str)
            .map(LoaderKind::parse)
            .collect::<Result<Vec<_>, _>>()?;
    }
    for (key, target) in [
        ("dependencies", &mut candidate.dependencies),
        ("incompatible", &mut candidate.incompatible),
    ] {
        if let Some(entries) = object.get(key).and_then(Value::as_array) {
            for entry in entries {
                let entry = entry
                    .as_object()
                    .ok_or_else(|| ContentError::InvalidManifest {
                        detail: format!("{key} entries must be objects"),
                    })?;
                let project = entry
                    .get("projectId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ContentError::InvalidManifest {
                        detail: format!("{key}.projectId is required"),
                    })?;
                let range = entry.get("range").and_then(Value::as_str).unwrap_or("*");
                target.push(Dependency::new(project, VersionRange::parse(range)?));
            }
        }
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: &str) -> Version {
        Version::parse(value).expect("version")
    }

    fn range(value: &str) -> VersionRange {
        VersionRange::parse(value).expect("range")
    }

    fn candidate(project: &str, file: &str, version_value: &str) -> Candidate {
        Candidate::new(SourceKind::Modrinth, project, file, version(version_value))
            .for_target("1.20.1", LoaderKind::Fabric)
    }

    fn target() -> GameTarget {
        GameTarget::new("1.20.1", LoaderKind::Fabric)
    }

    #[test]
    fn resolves_transitive_dependencies_deterministically() {
        let request = SolveRequest {
            target: target(),
            roots: vec!["sodium".to_string()],
            candidates: vec![
                candidate("sodium", "f-1", "0.5.3")
                    .depends_on(Dependency::new("fabric-api", range(">=0.90"))),
                candidate("fabric-api", "f-2", "0.92.0"),
                candidate("fabric-api", "f-3", "0.90.0"),
            ],
            budget: SolveBudget::default(),
        };
        let result = solve(&request, &|| false);
        match &result.outcome {
            SolveOutcome::Solved { assignments } => {
                let pairs: Vec<(&str, &str)> = assignments
                    .iter()
                    .map(|item| (item.project_id.as_str(), item.version.as_str()))
                    .collect();
                assert_eq!(pairs, vec![("fabric-api", "0.92.0"), ("sodium", "0.5.3")]);
            }
            other => panic!("expected solved, got {other:?}"),
        }
        assert!(result.outcome.is_installable());
    }

    #[test]
    fn proves_unsatisfiable_with_a_conflict_chain() {
        let request = SolveRequest {
            target: target(),
            roots: vec!["a".to_string()],
            candidates: vec![
                candidate("a", "a1", "1.0")
                    .depends_on(Dependency::new("b", range(">=2.0")))
                    .depends_on(Dependency::new("c", range("*"))),
                candidate("b", "b1", "1.0"),
                candidate("c", "c1", "1.0"),
            ],
            budget: SolveBudget::default(),
        };
        let result = solve(&request, &|| false);
        match &result.outcome {
            SolveOutcome::Unsatisfiable { conflicts } => {
                assert!(!conflicts.is_empty());
                let entry = conflicts
                    .iter()
                    .find(|conflict| conflict.project_id == "b")
                    .expect("conflict for b");
                assert_eq!(entry.reason, ConflictReason::RangeUnmet);
                assert_eq!(entry.requested_by[0].project_id, "a");
                assert_eq!(entry.requested_by[0].range, ">=2.0");
                assert_eq!(entry.available, vec!["1.0".to_string()]);
            }
            other => panic!("expected unsatisfiable, got {other:?}"),
        }
        assert!(!result.outcome.is_installable());
    }

    #[test]
    fn stops_at_the_state_budget_and_does_not_claim_unsatisfiable() {
        let mut candidates = Vec::new();
        for index in 0..6 {
            candidates.push(
                candidate("a", &format!("a{index}"), &format!("1.{index}"))
                    .depends_on(Dependency::new("missing", range(">=1.0"))),
            );
        }
        let request = SolveRequest {
            target: target(),
            roots: vec!["a".to_string()],
            candidates,
            budget: SolveBudget::new(2, Duration::from_secs(30)),
        };
        let result = solve(&request, &|| false);
        match result.outcome {
            SolveOutcome::BudgetExhausted { limit, used_states } => {
                assert_eq!(limit, BudgetLimit::States);
                assert_eq!(used_states, 2);
                assert!(result.states_visited <= 2);
            }
            other => panic!("expected budget exhaustion, got {other:?}"),
        }
    }

    #[test]
    fn reports_cancellation_distinctly() {
        let request = SolveRequest {
            target: target(),
            roots: vec!["a".to_string()],
            candidates: vec![candidate("a", "a1", "1.0")],
            budget: SolveBudget::default(),
        };
        let result = solve(&request, &|| true);
        assert!(matches!(result.outcome, SolveOutcome::Cancelled { .. }));
    }
}

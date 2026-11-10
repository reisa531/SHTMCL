//! Ports 04 depends on, plus the explicitly named controlled doubles.
//!
//! Ownership (see `docs/development/00-architecture-and-contracts.md` §2):
//! - instance writes belong to 01 → [`InstanceContentPort`]
//! - downloads and artifact caching belong to 03 → [`ArtifactPort`]
//! - platform search and project metadata → [`SourcePort`]
//! - the loader version matrix → [`LoaderCatalog`](super::loader::LoaderCatalog)
//!
//! Nothing in this build performs a network call or writes an instance directory.
//! Every double in this file is named `Fixture*` on purpose so no reader mistakes
//! it for a delivered integration.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use shtmcl_domain::{InstanceId, Revision};

use super::error::ContentError;
use super::loader::{LoaderCatalog, LoaderKind, LoaderLibrary, LoaderVersionInfo};
use super::plan::{ChangePlan, InstanceContentView, InstanceFile, PlanAction};
use super::resolve::Candidate;
use super::version::Version;

/// Where a project or file lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceKind {
    Modrinth,
    CurseForge,
    LocalFile,
}

impl SourceKind {
    pub const ALL: [Self; 3] = [Self::Modrinth, Self::CurseForge, Self::LocalFile];

    pub fn parse(value: &str) -> Result<Self, ContentError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "modrinth" => Ok(Self::Modrinth),
            "curseforge" | "curse-forge" => Ok(Self::CurseForge),
            "local" | "localfile" | "local-file" => Ok(Self::LocalFile),
            other => Err(ContentError::SourceUnavailable {
                provider: other.to_string(),
            }),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Modrinth => "modrinth",
            Self::CurseForge => "curseforge",
            Self::LocalFile => "local",
        }
    }
}

/// Receipt returned by the instance owner after a commit attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReceipt {
    pub instance_id: InstanceId,
    pub revision: Revision,
    pub paths: Vec<String>,
    pub plan_digest: String,
}

impl CommitReceipt {
    pub fn to_value(&self) -> Value {
        json!({
            "instanceId": self.instance_id.as_str(),
            "revision": self.revision.get(),
            "paths": self.paths,
            "planDigest": self.plan_digest,
        })
    }
}

/// 01: read the instance content projection and apply a change plan.
pub trait InstanceContentPort: Send + Sync {
    fn view(&self, instance_id: &InstanceId) -> Result<InstanceContentView, ContentError>;

    /// Apply a plan atomically for the caller: either the whole plan lands with a
    /// new revision, or nothing does. Revision and run-lease checks happen here.
    fn apply(&self, plan: &ChangePlan) -> Result<CommitReceipt, ContentError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedArtifact {
    pub key: String,
    pub targets: Vec<String>,
    pub bytes: u64,
    /// Where the bytes came from: a download or the pack archive itself.
    pub source: super::plan::LockSource,
    /// `true` only when the owner verified the digest itself.
    pub verified: bool,
}

/// 03: make one artifact available locally. No network access happens in 04.
pub trait ArtifactPort: Send + Sync {
    fn stage(
        &self,
        requirement: &super::plan::ArtifactRequirement,
    ) -> Result<StagedArtifact, ContentError>;

    /// Drop staged output for a plan (used on cancellation).
    fn discard(&self, plan_id: &str) -> Result<(), ContentError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    pub provider: SourceKind,
    pub project_id: String,
    pub slug: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceQuery {
    pub query_id: String,
    pub provider: Option<SourceKind>,
    pub text: String,
    pub target: Option<super::loader::GameTarget>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSearchPage {
    pub query_id: String,
    pub projects: Vec<ProjectRef>,
    /// `false` while the source is not integrated; the UI must not promise live
    /// results for it.
    pub connected: bool,
}

/// Restricted identity reference for a platform. It carries no credential and is
/// never serialised with a secret: the account owner (05) holds those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderIdentityRef {
    pub provider: SourceKind,
    pub linked: bool,
    pub account_ref: Option<String>,
    pub scopes: Vec<String>,
}

impl ProviderIdentityRef {
    pub fn unlinked(provider: SourceKind) -> Self {
        Self {
            provider,
            linked: false,
            account_ref: None,
            scopes: Vec::new(),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "provider": self.provider.as_str(),
            "linked": self.linked,
            "accountRef": self.account_ref,
            "scopes": self.scopes,
        })
    }
}

/// Search and project metadata across `Modrinth`, `CurseForge` and local files.
pub trait SourcePort: Send + Sync {
    fn search(&self, query: &SourceQuery) -> Result<SourceSearchPage, ContentError>;
    fn candidates(
        &self,
        project: &ProjectRef,
        target: &super::loader::GameTarget,
    ) -> Result<Vec<Candidate>, ContentError>;
    fn identity(&self, provider: SourceKind) -> ProviderIdentityRef;
}

/// Everything the module needs from its neighbours, injected at construction.
#[derive(Clone)]
pub struct ContentPorts {
    pub instance: Arc<dyn InstanceContentPort>,
    pub artifacts: Arc<dyn ArtifactPort>,
    pub sources: Arc<dyn SourcePort>,
    pub catalog: Arc<dyn LoaderCatalog>,
    pub journal: Arc<dyn super::recovery::JournalStore>,
}

impl ContentPorts {
    /// Default wiring for `content::module(ctx)`: every neighbour is
    /// **fail-closed**. Reads that need an owner (instance view, artifact staging,
    /// platform search, loader metadata) return `content.owner.unavailable`
    /// instead of fabricating success, so registering the module cannot make
    /// `content.apply` look like it wrote an instance.
    pub fn unavailable() -> Self {
        Self {
            instance: Arc::new(UnavailableInstancePort),
            artifacts: Arc::new(UnavailableArtifactPort),
            sources: Arc::new(UnavailableSourcePort),
            catalog: Arc::new(UnavailableLoaderCatalog),
            // The recovery journal is owned by 04, so it stays functional. With no
            // instance owner nothing can reach a commit, so it stays empty.
            journal: Arc::new(super::recovery::MemoryJournalStore::new()),
        }
    }

    /// Test/offline wiring: in-memory doubles. **Never** use this as the default in
    /// `module(ctx)` — a fixture writer would report a fake successful commit.
    pub fn fixtures() -> Self {
        Self {
            instance: Arc::new(FixtureInstanceWriter::new(Revision::new(1))),
            artifacts: Arc::new(FixtureArtifactStager::new()),
            sources: Arc::new(FixtureSourceCatalog::new()),
            catalog: Arc::new(FixtureLoaderCatalog::new()),
            journal: Arc::new(super::recovery::MemoryJournalStore::new()),
        }
    }

    /// `true` while at least one port is a controlled double rather than a real
    /// owner. The application layer uses it to avoid promising real behaviour.
    pub fn is_provisional(&self) -> bool {
        true
    }
}

/// 01 is not integrated: fail closed instead of pretending to write.
pub struct UnavailableInstancePort;

impl InstanceContentPort for UnavailableInstancePort {
    fn view(&self, _instance_id: &InstanceId) -> Result<InstanceContentView, ContentError> {
        Err(ContentError::OwnerUnavailable { owner: "instances" })
    }

    fn apply(&self, _plan: &ChangePlan) -> Result<CommitReceipt, ContentError> {
        Err(ContentError::OwnerUnavailable { owner: "instances" })
    }
}

/// 03 is not integrated: no artifact can be staged.
pub struct UnavailableArtifactPort;

impl ArtifactPort for UnavailableArtifactPort {
    fn stage(
        &self,
        _requirement: &super::plan::ArtifactRequirement,
    ) -> Result<StagedArtifact, ContentError> {
        Err(ContentError::OwnerUnavailable { owner: "downloads" })
    }

    fn discard(&self, _plan_id: &str) -> Result<(), ContentError> {
        Err(ContentError::OwnerUnavailable { owner: "downloads" })
    }
}

/// No platform adapter is connected yet.
pub struct UnavailableSourcePort;

impl SourcePort for UnavailableSourcePort {
    fn search(&self, _query: &SourceQuery) -> Result<SourceSearchPage, ContentError> {
        Err(ContentError::OwnerUnavailable { owner: "content" })
    }

    fn candidates(
        &self,
        _project: &ProjectRef,
        _target: &super::loader::GameTarget,
    ) -> Result<Vec<Candidate>, ContentError> {
        Err(ContentError::OwnerUnavailable { owner: "content" })
    }

    fn identity(&self, provider: SourceKind) -> ProviderIdentityRef {
        ProviderIdentityRef::unlinked(provider)
    }
}

/// No loader metadata source is wired yet: a plan must not invent a library list.
pub struct UnavailableLoaderCatalog;

impl LoaderCatalog for UnavailableLoaderCatalog {
    fn loader_versions(
        &self,
        kind: LoaderKind,
        _game_version: &str,
    ) -> Result<Vec<LoaderVersionInfo>, ContentError> {
        Err(ContentError::OwnerUnavailable {
            owner: loader_owner(kind),
        })
    }
}

fn loader_owner(kind: LoaderKind) -> &'static str {
    // Vanilla needs no loader metadata; every other kind depends on the platform
    // adapter that is not integrated yet.
    match kind {
        LoaderKind::Vanilla => "content",
        _ => "loader-catalog",
    }
}

impl std::fmt::Debug for ContentPorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentPorts")
            .field("provisional", &true)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Controlled doubles
// ---------------------------------------------------------------------------

/// In-memory stand-in for the 01 instance transaction.
///
/// It implements the same contract as the real owner would: revision fence,
/// refusal while a game session runs, all-or-nothing application, and a record of
/// applied plans so a test can assert "nothing was written".
#[derive(Debug)]
pub struct FixtureInstanceWriter {
    state: Mutex<FixtureInstanceState>,
}

#[derive(Debug)]
struct FixtureInstanceState {
    revision: Revision,
    files: BTreeMap<String, InstanceFile>,
    game_running: bool,
    /// Set once to simulate the instance owner rejecting the next commit.
    reject_next: Option<String>,
    commits: Vec<ChangePlan>,
    rejected: u32,
}

impl FixtureInstanceWriter {
    pub fn new(revision: Revision) -> Self {
        Self {
            state: Mutex::new(FixtureInstanceState {
                revision,
                files: BTreeMap::new(),
                game_running: false,
                reject_next: None,
                commits: Vec::new(),
                rejected: 0,
            }),
        }
    }

    pub fn seed(&self, files: Vec<InstanceFile>) -> &Self {
        if let Ok(mut state) = self.state.lock() {
            for file in files {
                state.files.insert(file.path.clone(), file);
            }
        }
        self
    }

    pub fn set_game_running(&self, running: bool) -> &Self {
        if let Ok(mut state) = self.state.lock() {
            state.game_running = running;
        }
        self
    }

    pub fn reject_next(&self, detail: impl Into<String>) -> &Self {
        if let Ok(mut state) = self.state.lock() {
            state.reject_next = Some(detail.into());
        }
        self
    }

    pub fn commits(&self) -> Vec<ChangePlan> {
        self.state
            .lock()
            .map(|state| state.commits.clone())
            .unwrap_or_default()
    }

    pub fn rejected_count(&self) -> u32 {
        self.state.lock().map(|state| state.rejected).unwrap_or(0)
    }

    pub fn files(&self) -> Vec<InstanceFile> {
        self.state
            .lock()
            .map(|state| state.files.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn revision(&self) -> Revision {
        self.state
            .lock()
            .map(|state| state.revision)
            .unwrap_or(Revision::ZERO)
    }

    /// Same projection as [`InstanceContentPort::view`], as an inherent method so
    /// tests do not have to import the port trait.
    pub fn view_of(&self, instance_id: &InstanceId) -> Result<InstanceContentView, ContentError> {
        let state = self.state.lock().map_err(|_| ContentError::Internal {
            detail: "instance lock poisoned".to_string(),
        })?;
        Ok(InstanceContentView {
            instance_id: instance_id.clone(),
            revision: state.revision,
            game_running: state.game_running,
            files: state.files.values().cloned().collect(),
        })
    }
}

impl InstanceContentPort for FixtureInstanceWriter {
    fn view(&self, instance_id: &InstanceId) -> Result<InstanceContentView, ContentError> {
        self.view_of(instance_id)
    }

    fn apply(&self, plan: &ChangePlan) -> Result<CommitReceipt, ContentError> {
        let mut state = self.state.lock().map_err(|_| ContentError::Internal {
            detail: "instance lock poisoned".to_string(),
        })?;
        if let Some(detail) = state.reject_next.take() {
            state.rejected += 1;
            return Err(ContentError::InstanceRejected { detail });
        }
        if state.game_running {
            return Err(ContentError::InstanceBusy {
                instance: plan.instance_id.as_str().to_string(),
            });
        }
        if plan.base_revision != state.revision {
            return Err(ContentError::PlanConflict {
                expected: plan.base_revision.get(),
                actual: state.revision.get(),
            });
        }

        // All-or-nothing: work on a copy, swap only on success.
        let mut next = state.files.clone();
        let mut paths = Vec::new();
        for entry in &plan.entries {
            paths.push(entry.path.clone());
            match entry.action {
                PlanAction::Add | PlanAction::Replace => {
                    // The strongest available digest: manifest files carry sha1/sha512,
                    // archive entries carry the sha256 04 computed from the pack.
                    let digest = entry.primary_digest().unwrap_or_default();
                    next.insert(
                        entry.path.clone(),
                        InstanceFile {
                            path: entry.path.clone(),
                            digest,
                            size: entry.size.unwrap_or(0),
                            managed: true,
                        },
                    );
                }
                PlanAction::Delete => {
                    next.remove(&entry.path);
                }
            }
        }
        state.files = next;
        state.revision = state.revision.checked_next().unwrap_or(state.revision);
        state.commits.push(plan.clone());
        Ok(CommitReceipt {
            instance_id: plan.instance_id.clone(),
            revision: state.revision,
            paths,
            plan_digest: plan.plan_digest.clone(),
        })
    }
}

/// In-memory stand-in for the 03 download/staging owner.
#[derive(Debug, Default)]
pub struct FixtureArtifactStager {
    inner: Mutex<StagerState>,
}

#[derive(Debug, Default)]
struct StagerState {
    staged: Vec<StagedArtifact>,
    discarded: Vec<String>,
    /// When set, `discard` fails (used to test the "cleanup failed" path).
    fail_discard: Option<String>,
}

impl FixtureArtifactStager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn staged(&self) -> Vec<StagedArtifact> {
        self.inner
            .lock()
            .map(|state| state.staged.clone())
            .unwrap_or_default()
    }

    /// Make the next `discard` calls fail with the given detail.
    pub fn fail_discard(&self, detail: impl Into<String>) -> &Self {
        if let Ok(mut state) = self.inner.lock() {
            state.fail_discard = Some(detail.into());
        }
        self
    }

    pub fn discarded_plans(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|state| state.discarded.clone())
            .unwrap_or_default()
    }
}

impl ArtifactPort for FixtureArtifactStager {
    fn stage(
        &self,
        requirement: &super::plan::ArtifactRequirement,
    ) -> Result<StagedArtifact, ContentError> {
        if requirement.restricted {
            return Err(ContentError::RestrictedFile {
                path: requirement
                    .targets
                    .first()
                    .cloned()
                    .unwrap_or_else(|| requirement.key.clone()),
            });
        }
        let mut state = self.inner.lock().map_err(|_| ContentError::Internal {
            detail: "stager lock poisoned".to_string(),
        })?;
        let staged = StagedArtifact {
            key: requirement.key.clone(),
            targets: requirement.targets.clone(),
            bytes: requirement.size.unwrap_or(0),
            source: requirement.source.clone(),
            verified: true,
        };
        state.staged.push(staged.clone());
        Ok(staged)
    }

    fn discard(&self, plan_id: &str) -> Result<(), ContentError> {
        let mut state = self.inner.lock().map_err(|_| ContentError::Internal {
            detail: "stager lock poisoned".to_string(),
        })?;
        if let Some(detail) = state.fail_discard.clone() {
            return Err(ContentError::Io { detail });
        }
        state.discarded.push(plan_id.to_string());
        Ok(())
    }
}

/// In-memory stand-in for the source adapters. `connected == false` everywhere:
/// Modrinth and CurseForge are **not** integrated in this build.
#[derive(Debug, Default)]
pub struct FixtureSourceCatalog {
    projects: Mutex<Vec<ProjectRef>>,
    candidates: Mutex<Vec<Candidate>>,
}

impl FixtureSourceCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_project(&self, project: ProjectRef) -> &Self {
        if let Ok(mut projects) = self.projects.lock() {
            projects.push(project);
        }
        self
    }

    pub fn add_candidate(&self, candidate: Candidate) -> &Self {
        if let Ok(mut candidates) = self.candidates.lock() {
            candidates.push(candidate);
        }
        self
    }
}

impl SourcePort for FixtureSourceCatalog {
    fn search(&self, query: &SourceQuery) -> Result<SourceSearchPage, ContentError> {
        let projects = self.projects.lock().map_err(|_| ContentError::Internal {
            detail: "source lock poisoned".to_string(),
        })?;
        let needle = query.text.to_lowercase();
        let matched: Vec<ProjectRef> = projects
            .iter()
            .filter(|project| match query.provider {
                Some(provider) => project.provider == provider,
                None => true,
            })
            .filter(|project| {
                needle.is_empty()
                    || project.title.to_lowercase().contains(&needle)
                    || project.slug.contains(&needle)
            })
            .take(query.limit.max(1))
            .cloned()
            .collect();
        Ok(SourceSearchPage {
            query_id: query.query_id.clone(),
            projects: matched,
            connected: false,
        })
    }

    fn candidates(
        &self,
        project: &ProjectRef,
        target: &super::loader::GameTarget,
    ) -> Result<Vec<Candidate>, ContentError> {
        let candidates = self.candidates.lock().map_err(|_| ContentError::Internal {
            detail: "source lock poisoned".to_string(),
        })?;
        Ok(candidates
            .iter()
            .filter(|candidate| {
                candidate.provider == project.provider && candidate.project_id == project.project_id
            })
            .filter(|candidate| candidate.matches_target(target))
            .cloned()
            .collect())
    }

    fn identity(&self, provider: SourceKind) -> ProviderIdentityRef {
        // Deliberately unlinked: no platform account is connected in this build.
        ProviderIdentityRef::unlinked(provider)
    }
}

/// Offline loader metadata. Values are representative fixtures, **not** live
/// platform data; a real catalog must be supplied before release.
#[derive(Debug, Default)]
pub struct FixtureLoaderCatalog;

impl FixtureLoaderCatalog {
    pub fn new() -> Self {
        Self
    }
}

fn library(group: &str, artifact: &str, version: &str) -> LoaderLibrary {
    LoaderLibrary::new(group, artifact, version)
}

impl LoaderCatalog for FixtureLoaderCatalog {
    fn loader_versions(
        &self,
        kind: LoaderKind,
        game_version: &str,
    ) -> Result<Vec<LoaderVersionInfo>, ContentError> {
        let entry = |version: &str,
                     stable: bool,
                     main_class: &str,
                     libraries: Vec<LoaderLibrary>,
                     jvm_args: Vec<String>,
                     java: Option<u32>| LoaderVersionInfo {
            version: Version::parse(version).expect("fixture version"),
            stable,
            game_versions: vec![game_version.to_string()],
            main_class: main_class.to_string(),
            libraries,
            jvm_args,
            required_java_major: java,
        };

        Ok(match kind {
            LoaderKind::Vanilla => Vec::new(),
            LoaderKind::Fabric => vec![
                entry(
                    "0.15.11",
                    true,
                    "net.fabricmc.loader.impl.launch.knot.KnotClient",
                    vec![
                        library("net.fabricmc", "fabric-loader", "0.15.11"),
                        library("net.fabricmc", "intermediary", "1.20.1"),
                        library("org.ow2.asm", "asm", "9.6"),
                        library("org.ow2.asm", "asm", "9.5"),
                    ],
                    vec!["-DFabricMcEmu=net.minecraft.client.main.Main".to_string()],
                    Some(17),
                ),
                entry(
                    "0.15.0",
                    true,
                    "net.fabricmc.loader.impl.launch.knot.KnotClient",
                    vec![library("net.fabricmc", "fabric-loader", "0.15.0")],
                    Vec::new(),
                    Some(17),
                ),
            ],
            LoaderKind::Quilt => vec![entry(
                "0.20.0",
                true,
                "org.quiltmc.loader.impl.launch.knot.KnotClient",
                vec![
                    library("org.quiltmc", "quilt-loader", "0.20.0"),
                    library("net.fabricmc", "intermediary", "1.20.1"),
                ],
                Vec::new(),
                Some(17),
            )],
            LoaderKind::Forge => vec![entry(
                "47.2.0",
                true,
                "cpw.mods.bootstraplauncher.BootstrapLauncher",
                vec![
                    library("cpw.mods", "securejarhandler", "2.1.10"),
                    library("net.minecraftforge", "fmlloader", "1.20.1-47.2.0"),
                    library("cpw.mods", "securejarhandler", "2.1.6"),
                ],
                vec![
                    "-Djava.net.preferIPv4Stack=true".to_string(),
                    "-Djava.net.preferIPv4Stack=true".to_string(),
                ],
                Some(17),
            )],
            LoaderKind::NeoForge => vec![entry(
                "20.4.167",
                false,
                "cpw.mods.bootstraplauncher.BootstrapLauncher",
                vec![
                    library("net.neoforged", "fmlcore", "1.20.4-20.4.167"),
                    library("cpw.mods", "securejarhandler", "2.1.10"),
                ],
                Vec::new(),
                Some(17),
            )],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::loader::{LoaderRequest, plan_loader_install};
    use super::*;

    #[test]
    fn fixture_instance_writer_fences_revision_and_run_state() {
        let writer = FixtureInstanceWriter::new(Revision::new(2));
        let view = writer
            .view(&InstanceId::new("instance-1").expect("id"))
            .expect("view");
        assert_eq!(view.revision, Revision::new(2));

        writer.set_game_running(true);
        let writer_view = writer
            .view(&InstanceId::new("instance-1").expect("id"))
            .expect("view");
        assert!(writer_view.game_running);
    }

    #[test]
    fn every_loader_kind_has_metadata_or_is_vanilla() {
        let catalog = FixtureLoaderCatalog::new();
        for kind in LoaderKind::ALL {
            if kind == LoaderKind::Vanilla {
                continue;
            }
            let plan = plan_loader_install(&LoaderRequest::new(kind, "1.20.1"), &catalog)
                .unwrap_or_else(|error| panic!("{kind:?} plan failed: {error}"));
            assert!(!plan.main_class.is_empty());
            assert!(!plan.libraries.is_empty());
        }
    }

    #[test]
    fn sources_are_explicitly_not_connected() {
        let sources = FixtureSourceCatalog::new();
        for provider in SourceKind::ALL {
            let identity = sources.identity(provider);
            assert!(!identity.linked);
            assert!(identity.account_ref.is_none());
            let encoded = identity.to_value().to_string();
            assert!(!encoded.contains("token") && !encoded.contains("secret"));
        }
    }
}

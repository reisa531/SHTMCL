//! `ContentLock` (the fixed install input) and `ChangePlan` (the only thing 04
//! submits to the instance owner).
//!
//! Invariants:
//! - a lock lists unique, normalised, `/`-separated instance-relative paths,
//! - a plan lists only *changes* (add / replace / delete), never the unchanged
//!   bulk, so the confirmation list stays short,
//! - both are canonically ordered and digests are stable, so re-planning the same
//!   input yields byte-identical output.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use shtmcl_domain::{InstanceId, Revision};

use super::error::ContentError;
use super::loader::GameTarget;
use super::modpack::{Hashes, PackManifest, PackSide, SideSupport};
use super::paths::{collision_key, normalise_content_path};
use super::ports::SourceKind;

pub const LOCK_SCHEMA: &str = "content-lock.v1";
pub const PLAN_SCHEMA: &str = "content-plan.v1";

/// How a locked file is identified. Platform IDs win over digests; an unknown
/// local file stays a local file and is never guessed into a project version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalIdentity {
    Artifact {
        provider: SourceKind,
        project_id: String,
        file_id: String,
    },
    Digest {
        sha1: Option<String>,
        sha512: Option<String>,
    },
    LocalFile {
        name: String,
    },
}

impl LocalIdentity {
    pub fn key(&self) -> String {
        match self {
            Self::Artifact {
                provider,
                project_id,
                file_id,
            } => format!("{}:{project_id}:{file_id}", provider.as_str()),
            Self::Digest { sha1, sha512 } => match (sha512, sha1) {
                (Some(value), _) => format!("sha512:{value}"),
                (None, Some(value)) => format!("sha1:{value}"),
                (None, None) => "digest:unknown".to_string(),
            },
            Self::LocalFile { name } => format!("local:{name}"),
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Self::Artifact {
                provider,
                project_id,
                file_id,
            } => json!({
                "kind": "artifact",
                "provider": provider.as_str(),
                "projectId": project_id,
                "fileId": file_id,
            }),
            Self::Digest { sha1, sha512 } => json!({
                "kind": "digest",
                "sha1": sha1,
                "sha512": sha512,
            }),
            Self::LocalFile { name } => json!({ "kind": "local_file", "name": name }),
        }
    }

    pub fn from_value(value: &Value) -> Result<Self, ContentError> {
        let object = value.as_object().ok_or_else(|| ContentError::InvalidPlan {
            detail: "identity must be an object".to_string(),
        })?;
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("digest");
        match kind {
            "artifact" => Ok(Self::Artifact {
                provider: SourceKind::parse(
                    object
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or("local"),
                )?,
                project_id: object
                    .get("projectId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                file_id: object
                    .get("fileId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            "local_file" => Ok(Self::LocalFile {
                name: object
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            _ => Ok(Self::Digest {
                sha1: object
                    .get("sha1")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                sha512: object
                    .get("sha512")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }),
        }
    }
}

/// Where the bytes of a locked file come from.
///
/// This is what separates a *restricted download* (no URL → the user must supply
/// the file, task waits for the user) from an *override* (the bytes are inside the
/// pack archive the user already opened). Treating both as restricted made every
/// pack with `overrides/` uninstallable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockSource {
    /// Provided by the download owner (03) from the listed URLs.
    Download,
    /// Extracted from the pack archive by the staging owner (03); never downloaded.
    Archive { archive_path: String },
}

impl LockSource {
    pub fn to_value(&self) -> Value {
        match self {
            Self::Download => json!({ "kind": "download" }),
            Self::Archive { archive_path } => {
                json!({ "kind": "archive", "archivePath": archive_path })
            }
        }
    }

    pub fn from_value(value: &Value) -> Result<Self, ContentError> {
        let object = value.as_object().ok_or_else(|| ContentError::InvalidPlan {
            detail: "source must be an object".to_string(),
        })?;
        match object.get("kind").and_then(Value::as_str) {
            Some("archive") => Ok(Self::Archive {
                archive_path: object
                    .get("archivePath")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            Some("download") | None => Ok(Self::Download),
            Some(other) => Err(ContentError::InvalidPlan {
                detail: format!("unknown source kind {other}"),
            }),
        }
    }

    pub fn is_archive(&self) -> bool {
        matches!(self, Self::Archive { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockFile {
    pub path: String,
    pub identity: LocalIdentity,
    pub hashes: Hashes,
    pub size: Option<u64>,
    /// Empty for a restricted *download*: the user must supply the bytes. Archive
    /// entries (overrides) legitimately have no URL — their bytes come from the pack.
    pub urls: Vec<String>,
    pub source: LockSource,
    pub client: SideSupport,
    pub server: SideSupport,
    /// Where the entry came from (`modrinth`, `pack:files`, `pack:overrides`).
    pub origin: String,
}

impl LockFile {
    /// Only a **download** without URLs is restricted (waiting_user). An override
    /// comes from the pack archive, so it is never restricted.
    pub fn is_restricted(&self) -> bool {
        matches!(self.source, LockSource::Download) && self.urls.is_empty()
    }

    fn digest_key(&self) -> String {
        self.hashes
            .identity_key()
            .unwrap_or_else(|| self.identity.key())
    }

    fn to_value(&self) -> Value {
        json!({
            "path": self.path,
            "identity": self.identity.to_value(),
            "hashes": {
                "sha1": self.hashes.sha1,
                "sha512": self.hashes.sha512,
                "sha256": self.hashes.sha256,
            },
            "size": self.size,
            "urls": self.urls,
            "source": self.source.to_value(),
            "env": { "client": self.client.as_str(), "server": self.server.as_str() },
            "origin": self.origin,
        })
    }

    fn from_value(value: &Value) -> Result<Self, ContentError> {
        let object = value.as_object().ok_or_else(|| ContentError::InvalidPlan {
            detail: "lock file must be an object".to_string(),
        })?;
        let raw_path = object.get("path").and_then(Value::as_str).ok_or_else(|| {
            ContentError::InvalidPlan {
                detail: "lock file.path is required".to_string(),
            }
        })?;
        let path = normalise_content_path(raw_path)?;
        let hashes_object = object.get("hashes");
        let hashes = Hashes {
            sha1: hashes_object
                .and_then(|value| value.get("sha1"))
                .and_then(Value::as_str)
                .map(str::to_string),
            sha512: hashes_object
                .and_then(|value| value.get("sha512"))
                .and_then(Value::as_str)
                .map(str::to_string),
            sha256: hashes_object
                .and_then(|value| value.get("sha256"))
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        let identity = match object.get("identity") {
            Some(value) => LocalIdentity::from_value(value)?,
            None => LocalIdentity::Digest {
                sha1: hashes.sha1.clone(),
                sha512: hashes.sha512.clone(),
            },
        };
        let origin = object
            .get("origin")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let source = match object.get("source") {
            Some(value) => LockSource::from_value(value)?,
            // Older locks: an override was identifiable only by its origin string.
            None if origin == "pack:overrides" => LockSource::Archive {
                archive_path: String::new(),
            },
            None => LockSource::Download,
        };
        let side = |key: &str| {
            object
                .get("env")
                .and_then(|env| env.get(key))
                .and_then(Value::as_str)
                .and_then(|value| match value {
                    "required" => Some(SideSupport::Required),
                    "optional" => Some(SideSupport::Optional),
                    "unsupported" => Some(SideSupport::Unsupported),
                    _ => None,
                })
                .unwrap_or(SideSupport::Required)
        };
        Ok(Self {
            path,
            identity,
            hashes,
            size: object.get("size").and_then(Value::as_u64),
            urls: object
                .get("urls")
                .and_then(Value::as_array)
                .map(|urls| {
                    urls.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            client: side("client"),
            server: side("server"),
            origin,
            source,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentLock {
    pub schema: String,
    pub target: GameTarget,
    pub side: PackSide,
    pub files: Vec<LockFile>,
    pub digest: String,
}

impl ContentLock {
    pub fn new(
        target: GameTarget,
        side: PackSide,
        files: Vec<LockFile>,
    ) -> Result<Self, ContentError> {
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        let mut normalised: Vec<LockFile> = Vec::new();
        for mut file in files {
            file.path = normalise_content_path(&file.path)?;
            let key = collision_key(&file.path);
            let identity = file.digest_key();
            match seen.get(&key) {
                Some(existing) if existing == &identity => continue,
                Some(_) => {
                    return Err(ContentError::DuplicatePath { path: file.path });
                }
                None => {
                    seen.insert(key, identity);
                }
            }
            normalised.push(file);
        }
        normalised.sort_by(|left, right| left.path.cmp(&right.path));

        let digest = digest_of(
            std::iter::once(format!(
                "{}|{}|{}",
                LOCK_SCHEMA,
                target.game_version,
                target.loader.as_str()
            ))
            .chain(normalised.iter().map(|file| {
                format!(
                    "{}|{}|{}",
                    file.path,
                    file.digest_key(),
                    file.size.unwrap_or(0)
                )
            }))
            .collect(),
        );

        Ok(Self {
            schema: LOCK_SCHEMA.to_string(),
            target,
            side,
            files: normalised,
            digest,
        })
    }

    /// Build the lock for one side from a parsed pack manifest.
    pub fn from_manifest(manifest: &PackManifest, side: PackSide) -> Result<Self, ContentError> {
        let target = manifest
            .target()
            .ok_or_else(|| ContentError::InvalidManifest {
                detail: "manifest has no minecraft dependency".to_string(),
            })?;
        let mut files = Vec::new();
        for file in &manifest.files {
            files.push(LockFile {
                path: file.path.clone(),
                identity: LocalIdentity::Digest {
                    sha1: file.hashes.sha1.clone(),
                    sha512: file.hashes.sha512.clone(),
                },
                hashes: file.hashes.clone(),
                size: file.file_size,
                urls: file.downloads.clone(),
                source: LockSource::Download,
                client: file.client,
                server: file.server,
                origin: "pack:files".to_string(),
            });
        }
        for entry in &manifest.overrides {
            let wanted = match side {
                PackSide::Client => entry.scope != super::modpack::OverrideScope::Server,
                PackSide::Server => entry.scope != super::modpack::OverrideScope::Client,
            };
            if !wanted {
                continue;
            }
            // An override is *not* a restricted download: its bytes come from the
            // pack archive the user already opened, and 04 verified them at read
            // time (length + CRC + SHA-256).
            files.push(LockFile {
                path: entry.target_path.clone(),
                identity: LocalIdentity::LocalFile {
                    name: entry.archive_path.clone(),
                },
                hashes: entry.hashes(),
                size: entry.size,
                urls: Vec::new(),
                source: LockSource::Archive {
                    archive_path: entry.archive_path.clone(),
                },
                client: SideSupport::Required,
                server: SideSupport::Required,
                origin: "pack:overrides".to_string(),
            });
        }
        Self::new(target, side, files)
    }

    pub fn files_for(&self, side: PackSide) -> Vec<&LockFile> {
        self.files
            .iter()
            .filter(|file| match side {
                PackSide::Client => file.client != SideSupport::Unsupported,
                PackSide::Server => file.server != SideSupport::Unsupported,
            })
            .collect()
    }

    pub fn to_value(&self) -> Value {
        json!({
            "schema": self.schema,
            "target": self.target.to_value(),
            "side": self.side.as_str(),
            "files": self.files.iter().map(LockFile::to_value).collect::<Vec<_>>(),
            "digest": self.digest,
        })
    }

    pub fn from_value(value: &Value) -> Result<Self, ContentError> {
        let object = value.as_object().ok_or_else(|| ContentError::InvalidPlan {
            detail: "lock must be an object".to_string(),
        })?;
        let target = object
            .get("target")
            .ok_or_else(|| ContentError::InvalidPlan {
                detail: "lock.target is required".to_string(),
            })
            .and_then(GameTarget::from_value)?;
        let side = match object.get("side").and_then(Value::as_str) {
            Some("server") => PackSide::Server,
            _ => PackSide::Client,
        };
        let files = object
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| ContentError::InvalidPlan {
                detail: "lock.files is required".to_string(),
            })?
            .iter()
            .map(LockFile::from_value)
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(target, side, files)
    }
}

/// One artifact the download/staging owner has to provide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRequirement {
    pub key: String,
    pub urls: Vec<String>,
    pub sha1: Option<String>,
    pub sha512: Option<String>,
    pub sha256: Option<String>,
    pub size: Option<u64>,
    /// Download (03 fetches it) or archive extraction (03 unpacks it from the pack).
    pub source: LockSource,
    /// Every locked path that this single artifact satisfies.
    pub targets: Vec<String>,
    /// `true` only for a download with no URL: the user must supply the file.
    pub restricted: bool,
}

impl ArtifactRequirement {
    pub fn to_value(&self) -> Value {
        json!({
            "key": self.key,
            "urls": self.urls,
            "sha1": self.sha1,
            "sha512": self.sha512,
            "sha256": self.sha256,
            "size": self.size,
            "source": self.source.to_value(),
            "targets": self.targets,
            "restricted": self.restricted,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAction {
    Add,
    Replace,
    Delete,
}

impl PlanAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Replace => "replace",
            Self::Delete => "delete",
        }
    }
}

/// Why a planned path cannot be committed without a user decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanConflictReason {
    /// The path holds a file the instance owner does not consider content (`managed:false`).
    UnmanagedFile,
}

impl PlanConflictReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnmanagedFile => "unmanaged_file",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanConflict {
    pub path: String,
    pub reason: PlanConflictReason,
}

impl PlanConflict {
    pub fn to_value(&self) -> Value {
        json!({ "path": self.path, "reason": self.reason.as_str() })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub path: String,
    pub action: PlanAction,
    pub identity: Option<LocalIdentity>,
    pub sha1: Option<String>,
    pub sha512: Option<String>,
    pub sha256: Option<String>,
    pub size: Option<u64>,
    pub source: LockSource,
    /// The previous bytes must come from the instance transaction backup (01),
    /// not from a download; used by rollback entries.
    pub restore_from_backup: bool,
}

impl PlanEntry {
    fn digest_key(&self) -> String {
        match (&self.sha512, &self.sha256, &self.sha1) {
            (Some(value), _, _) => format!("sha512:{value}"),
            (None, Some(value), _) => format!("sha256:{value}"),
            (None, None, Some(value)) => format!("sha1:{value}"),
            (None, None, None) => "none".to_string(),
        }
    }

    /// The digest a port should compare against, preferring the strongest family.
    pub fn primary_digest(&self) -> Option<String> {
        self.sha512
            .clone()
            .or_else(|| self.sha256.clone())
            .or_else(|| self.sha1.clone())
    }

    pub fn to_value(&self) -> Value {
        json!({
            "path": self.path,
            "action": self.action.as_str(),
            "identity": self.identity.as_ref().map(LocalIdentity::to_value),
            "sha1": self.sha1,
            "sha512": self.sha512,
            "sha256": self.sha256,
            "size": self.size,
            "source": self.source.to_value(),
            "restoreFromBackup": self.restore_from_backup,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeCounts {
    pub add: u32,
    pub replace: u32,
    pub delete: u32,
    pub total: u32,
    pub bytes: u64,
}

impl ChangeCounts {
    fn of(entries: &[PlanEntry]) -> Self {
        let mut counts = Self::default();
        for entry in entries {
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
        counts
    }

    pub fn to_value(&self) -> Value {
        json!({
            "add": self.add,
            "replace": self.replace,
            "delete": self.delete,
            "total": self.total,
            "bytes": self.bytes,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangePlan {
    pub plan_id: String,
    pub instance_id: InstanceId,
    pub base_revision: Revision,
    pub entries: Vec<PlanEntry>,
    /// Paths that need a user decision before anything may be committed.
    pub conflicts: Vec<PlanConflict>,
    pub required_artifacts: Vec<ArtifactRequirement>,
    pub counts: ChangeCounts,
    pub plan_digest: String,
}

impl ChangePlan {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Nothing to do, but a user decision is pending.
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    pub fn to_value(&self) -> Value {
        json!({
            "schema": PLAN_SCHEMA,
            "planId": self.plan_id,
            "instanceId": self.instance_id.as_str(),
            "baseRevision": self.base_revision.get(),
            "entries": self.entries.iter().map(PlanEntry::to_value).collect::<Vec<_>>(),
            "conflicts": self.conflicts.iter().map(PlanConflict::to_value).collect::<Vec<_>>(),
            "requiredArtifacts": self
                .required_artifacts
                .iter()
                .map(ArtifactRequirement::to_value)
                .collect::<Vec<_>>(),
            "counts": self.counts.to_value(),
            "planDigest": self.plan_digest,
        })
    }
}

/// A file the instance currently holds. `managed` distinguishes content this
/// module may delete from user files that only differ in path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceFile {
    pub path: String,
    pub digest: String,
    pub size: u64,
    pub managed: bool,
}

impl InstanceFile {
    pub fn new(path: impl Into<String>, digest: impl Into<String>, size: u64) -> Self {
        Self {
            path: path.into(),
            digest: digest.into(),
            size,
            managed: true,
        }
    }

    pub fn to_value(&self) -> Value {
        json!({ "path": self.path, "digest": self.digest, "size": self.size, "managed": self.managed })
    }

    pub fn from_value(value: &Value) -> Result<Self, ContentError> {
        let object = value.as_object().ok_or_else(|| ContentError::InvalidPlan {
            detail: "instance file must be an object".to_string(),
        })?;
        Ok(Self {
            path: normalise_content_path(
                object
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )?,
            digest: object
                .get("digest")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            size: object.get("size").and_then(Value::as_u64).unwrap_or(0),
            managed: object
                .get("managed")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        })
    }
}

/// Read-only instance projection provided by the instance owner (01).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceContentView {
    pub instance_id: InstanceId,
    pub revision: Revision,
    pub game_running: bool,
    pub files: Vec<InstanceFile>,
}

impl InstanceContentView {
    pub fn new(instance_id: InstanceId, revision: Revision, files: Vec<InstanceFile>) -> Self {
        Self {
            instance_id,
            revision,
            game_running: false,
            files,
        }
    }

    pub fn digest_of_paths(&self) -> String {
        let mut paths: Vec<String> = self
            .files
            .iter()
            .map(|file| format!("{}|{}", file.path, file.digest))
            .collect();
        paths.sort();
        digest_of(paths)
    }

    pub fn to_value(&self) -> Value {
        json!({
            "instanceId": self.instance_id.as_str(),
            "revision": self.revision.get(),
            "gameRunning": self.game_running,
            "files": self.files.iter().map(InstanceFile::to_value).collect::<Vec<_>>(),
        })
    }

    pub fn from_value(value: &Value) -> Result<Self, ContentError> {
        let object = value.as_object().ok_or_else(|| ContentError::InvalidPlan {
            detail: "instance view must be an object".to_string(),
        })?;
        let instance_id = InstanceId::new(
            object
                .get("instanceId")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .map_err(|error| ContentError::InvalidPlan {
            detail: error.to_string(),
        })?;
        Ok(Self {
            instance_id,
            revision: Revision::new(object.get("revision").and_then(Value::as_u64).unwrap_or(0)),
            game_running: object
                .get("gameRunning")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            files: object
                .get("files")
                .and_then(Value::as_array)
                .map(|files| {
                    files
                        .iter()
                        .map(InstanceFile::from_value)
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default(),
        })
    }
}

/// Build the change plan for a lock against the current instance content.
///
/// A file the user owns (`managed:false`) is never silently replaced: it becomes a
/// plan conflict, which blocks `content.apply` until the user decides.
pub fn plan_changes(
    plan_id: impl Into<String>,
    view: &InstanceContentView,
    lock: &ContentLock,
) -> Result<ChangePlan, ContentError> {
    let mut current: BTreeMap<String, &InstanceFile> = BTreeMap::new();
    for file in &view.files {
        current.insert(collision_key(&normalise_content_path(&file.path)?), file);
    }

    let mut entries: Vec<PlanEntry> = Vec::new();
    let mut conflicts: Vec<PlanConflict> = Vec::new();
    let mut artifacts: BTreeMap<String, ArtifactRequirement> = BTreeMap::new();
    let mut desired: BTreeMap<String, ()> = BTreeMap::new();

    for file in lock.files_for(lock.side) {
        let key = collision_key(&file.path);
        desired.insert(key.clone(), ());
        let existing = current.get(&key);
        let action = match existing {
            None => PlanAction::Add,
            Some(file_now) => {
                if digest_matches(file_now, file) {
                    continue;
                }
                if !file_now.managed {
                    // The instance owner says this file is not ours to overwrite.
                    conflicts.push(PlanConflict {
                        path: file.path.clone(),
                        reason: PlanConflictReason::UnmanagedFile,
                    });
                    continue;
                }
                PlanAction::Replace
            }
        };

        if action != PlanAction::Delete {
            let artifact_key = file.digest_key();
            artifacts
                .entry(artifact_key.clone())
                .and_modify(|requirement| {
                    if !requirement.targets.contains(&file.path) {
                        requirement.targets.push(file.path.clone());
                    }
                })
                .or_insert_with(|| ArtifactRequirement {
                    key: artifact_key,
                    urls: file.urls.clone(),
                    sha1: file.hashes.sha1.clone(),
                    sha512: file.hashes.sha512.clone(),
                    sha256: file.hashes.sha256.clone(),
                    size: file.size,
                    source: file.source.clone(),
                    targets: vec![file.path.clone()],
                    // Only a download without a URL needs the user to supply bytes.
                    restricted: file.is_restricted(),
                });
        }

        entries.push(PlanEntry {
            path: file.path.clone(),
            action,
            identity: Some(file.identity.clone()),
            sha1: file.hashes.sha1.clone(),
            sha512: file.hashes.sha512.clone(),
            sha256: file.hashes.sha256.clone(),
            size: file.size,
            source: file.source.clone(),
            restore_from_backup: false,
        });
    }

    for (key, file) in &current {
        if desired.contains_key(key) || !file.managed {
            continue;
        }
        entries.push(PlanEntry {
            path: file.path.clone(),
            action: PlanAction::Delete,
            identity: None,
            sha1: None,
            sha512: None,
            sha256: None,
            size: Some(file.size),
            source: LockSource::Download,
            restore_from_backup: false,
        });
    }

    finish_plan(
        plan_id.into(),
        view,
        entries,
        conflicts,
        artifacts.into_values().collect(),
    )
}

/// Inverse plan that restores a recorded previous file set. Bytes come from the
/// instance transaction backup, so no artifact is requested.
pub fn rollback_plan(
    plan_id: impl Into<String>,
    view: &InstanceContentView,
    previous: &[InstanceFile],
) -> Result<ChangePlan, ContentError> {
    let mut current: BTreeMap<String, &InstanceFile> = BTreeMap::new();
    for file in &view.files {
        current.insert(collision_key(&normalise_content_path(&file.path)?), file);
    }
    let mut entries = Vec::new();
    let mut restore: BTreeMap<String, ()> = BTreeMap::new();

    for file in previous {
        let path = normalise_content_path(&file.path)?;
        let key = collision_key(&path);
        restore.insert(key.clone(), ());
        match current.get(&key) {
            Some(current_file) if current_file.digest == file.digest => continue,
            Some(_) => entries.push(PlanEntry {
                path,
                action: PlanAction::Replace,
                identity: None,
                sha1: Some(file.digest.clone()),
                sha512: None,
                sha256: None,
                size: Some(file.size),
                source: LockSource::Download,
                restore_from_backup: true,
            }),
            None => entries.push(PlanEntry {
                path,
                action: PlanAction::Add,
                identity: None,
                sha1: Some(file.digest.clone()),
                sha512: None,
                sha256: None,
                size: Some(file.size),
                source: LockSource::Download,
                restore_from_backup: true,
            }),
        }
    }

    for (key, file) in &current {
        if restore.contains_key(key) || !file.managed {
            continue;
        }
        entries.push(PlanEntry {
            path: file.path.clone(),
            action: PlanAction::Delete,
            identity: None,
            sha1: None,
            sha512: None,
            sha256: None,
            size: Some(file.size),
            source: LockSource::Download,
            restore_from_backup: false,
        });
    }

    finish_plan(plan_id.into(), view, entries, Vec::new(), Vec::new())
}

fn finish_plan(
    plan_id: String,
    view: &InstanceContentView,
    mut entries: Vec<PlanEntry>,
    mut conflicts: Vec<PlanConflict>,
    mut required_artifacts: Vec<ArtifactRequirement>,
) -> Result<ChangePlan, ContentError> {
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    conflicts.sort_by(|left, right| left.path.cmp(&right.path));
    required_artifacts.sort_by(|left, right| left.key.cmp(&right.key));
    for requirement in &mut required_artifacts {
        requirement.targets.sort();
        requirement.targets.dedup();
    }
    let counts = ChangeCounts::of(&entries);
    let plan_digest = digest_of(
        std::iter::once(format!(
            "{}|{}|{}",
            PLAN_SCHEMA,
            view.instance_id.as_str(),
            view.revision.get()
        ))
        .chain(entries.iter().map(|entry| {
            format!(
                "{}|{}|{}",
                entry.action.as_str(),
                entry.path,
                entry.digest_key()
            )
        }))
        .collect(),
    );
    Ok(ChangePlan {
        plan_id,
        instance_id: view.instance_id.clone(),
        base_revision: view.revision,
        entries,
        conflicts,
        required_artifacts,
        counts,
        plan_digest,
    })
}

/// Compare a locked file against what the instance currently holds. Any of the
/// three digest families counts: a manifest file carries sha1/sha512, an archive
/// entry is identified by the sha256 04 computed from the pack.
fn digest_matches(current: &InstanceFile, locked: &LockFile) -> bool {
    let matches = |value: &Option<String>| {
        value
            .as_ref()
            .map(|digest| current.digest.eq_ignore_ascii_case(digest))
            .unwrap_or(false)
    };
    matches(&locked.hashes.sha512) || matches(&locked.hashes.sha256) || matches(&locked.hashes.sha1)
}

/// SHA-256 over a canonical line set, rendered as `sha256:<hex>`.
///
/// Used as the stable compare/dedupe key for a `ContentLock` and a `ChangePlan`
/// (two runs over the same input must produce the same digest). It is a real hash
/// but it is **not** the file-integrity check: locked file digests come from the
/// provider manifest and are verified by the download owner (03).
pub fn digest_of(lines: Vec<String>) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    for line in lines {
        hasher.update(line.as_bytes());
        // Unit separator: keeps `["ab","c"]` distinct from `["a","bc"]`.
        hasher.update([0x1f]);
    }
    let digest = hasher.finalize();
    let mut rendered = String::with_capacity(71);
    rendered.push_str("sha256:");
    for byte in digest.iter() {
        rendered.push_str(&format!("{byte:02x}"));
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

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
                size: Some(1),
                urls: vec!["https://example.invalid/x.jar".to_string()],
                source: LockSource::Download,
                client: SideSupport::Required,
                server: SideSupport::Required,
                origin: "pack:files".to_string(),
            })
            .collect();
        ContentLock::new(
            GameTarget::new("1.20.1", super::super::loader::LoaderKind::Fabric),
            PackSide::Client,
            files,
        )
        .expect("lock")
    }

    fn view(files: &[(&str, &str)]) -> InstanceContentView {
        InstanceContentView::new(
            InstanceId::new("instance-1").expect("id"),
            Revision::new(3),
            files
                .iter()
                .map(|(path, digest)| InstanceFile::new(*path, *digest, 1))
                .collect(),
        )
    }

    #[test]
    fn plan_lists_only_changes_in_path_order() {
        let lock = lock(&[
            ("mods/b.jar", "bbbb"),
            ("mods/a.jar", "aaaa"),
            ("mods/c.jar", "cccc"),
        ]);
        let view = view(&[("mods/a.jar", "aaaa"), ("mods/old.jar", "dddd")]);
        let plan = plan_changes("plan-1", &view, &lock).expect("plan");
        let actions: Vec<(&str, &str)> = plan
            .entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry.action.as_str()))
            .collect();
        assert_eq!(
            actions,
            vec![
                ("mods/b.jar", "add"),
                ("mods/c.jar", "add"),
                ("mods/old.jar", "delete")
            ]
        );
        assert_eq!(plan.counts.add, 2);
        assert_eq!(plan.counts.delete, 1);
        assert_eq!(plan.counts.replace, 0);
    }

    #[test]
    fn plan_is_deterministic_and_artifacts_are_deduplicated() {
        let first = plan_changes(
            "plan-1",
            &view(&[]),
            &lock(&[("mods/a.jar", "aaaa"), ("mods/b.jar", "aaaa")]),
        )
        .expect("plan");
        let second = plan_changes(
            "plan-1",
            &view(&[]),
            &lock(&[("mods/b.jar", "aaaa"), ("mods/a.jar", "aaaa")]),
        )
        .expect("plan");
        assert_eq!(first.plan_digest, second.plan_digest);
        assert_eq!(
            first.required_artifacts.len(),
            1,
            "one artifact serves both paths"
        );
        assert_eq!(first.required_artifacts[0].targets.len(), 2);
    }

    #[test]
    fn lock_rejects_duplicate_and_escaping_paths() {
        assert!(matches!(
            ContentLock::new(
                GameTarget::new("1.20.1", super::super::loader::LoaderKind::Vanilla),
                PackSide::Client,
                vec![
                    LockFile {
                        path: "mods/a.jar".to_string(),
                        identity: LocalIdentity::Digest {
                            sha1: Some("a".to_string()),
                            sha512: None
                        },
                        hashes: Hashes::default(),
                        size: None,
                        urls: Vec::new(),
                        source: LockSource::Download,
                        client: SideSupport::Required,
                        server: SideSupport::Required,
                        origin: "x".to_string()
                    },
                    LockFile {
                        path: "MODS/A.jar".to_string(),
                        identity: LocalIdentity::Digest {
                            sha1: Some("b".to_string()),
                            sha512: None
                        },
                        hashes: Hashes::default(),
                        size: None,
                        urls: Vec::new(),
                        source: LockSource::Download,
                        client: SideSupport::Required,
                        server: SideSupport::Required,
                        origin: "x".to_string()
                    }
                ]
            ),
            Err(ContentError::DuplicatePath { .. })
        ));

        assert!(matches!(
            ContentLock::new(
                GameTarget::new("1.20.1", super::super::loader::LoaderKind::Vanilla),
                PackSide::Client,
                vec![LockFile {
                    path: "../evil.jar".to_string(),
                    identity: LocalIdentity::Digest {
                        sha1: Some("a".to_string()),
                        sha512: None
                    },
                    hashes: Hashes::default(),
                    size: None,
                    urls: Vec::new(),
                    source: LockSource::Download,
                    client: SideSupport::Required,
                    server: SideSupport::Required,
                    origin: "x".to_string()
                }]
            ),
            Err(ContentError::PathEscape { .. })
        ));
    }
}

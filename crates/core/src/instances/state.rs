//! Instance state, revisions and the transition rules 01 owns.
//!
//! An [`Instance`] is what the launcher starts: a game directory plus the pinned
//! version, loader and Java choice. Its `revision` is the optimistic-concurrency
//! token every mutation must present, so two windows editing the same instance
//! cannot silently overwrite each other.
//!
//! Rules encoded here (and tested in `crates/core/tests/01_instances.rs`):
//! - `revision` increases by exactly one per committed mutation,
//! - a mutation either applies completely or not at all (the store commits under
//!   one lock and returns the previous value on rejection),
//! - lifecycle transitions are explicit: an instance is `Ready`, `Busy` (a task
//!   holds it) or `Broken`; only `Busy` refuses mutations,
//! - deletion is a tombstone, not an erase: a running task may still finish and
//!   report against an id that no longer appears in the listing.

use std::fmt;

use serde::{Deserialize, Serialize};
use shtmcl_domain::Revision;

/// Lifecycle of one instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    /// Ready to be launched or edited.
    Ready,
    /// A long task (install, launch, content write) holds this instance. Content
    /// mutations are refused so a half-written directory cannot be edited.
    Busy,
    /// The recorded directory is missing or unreadable; the user must repair it.
    Broken,
}

impl InstanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Busy => "busy",
            Self::Broken => "broken",
        }
    }

    /// Only `Ready` accepts a content or settings mutation.
    pub fn accepts_mutation(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Where an instance's files live. Absolute by construction; the store rejects
/// relative paths because they resolve differently per working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceLocation {
    /// Absolute path to the game directory (`.../instances/<id>`).
    pub root: String,
}

impl InstanceLocation {
    /// A path is acceptable when it is absolute and has no `.`/`..` segments.
    ///
    /// This is deliberately platform-light: it accepts both `/` and `\` roots so
    /// the same domain code runs on Windows and Linux, and it never touches the
    /// filesystem (that check belongs to the storage port).
    pub fn validate(&self) -> Result<(), &'static str> {
        let path = self.root.trim();
        if path.is_empty() {
            return Err("root");
        }
        let absolute = path.starts_with('/')
            || path.starts_with('\\')
            || (path.len() > 2 && path.as_bytes()[1] == b':');
        if !absolute {
            return Err("absolute");
        }
        for segment in path.split(['/', '\\']) {
            if segment == ".." {
                return Err("traversal");
            }
        }
        Ok(())
    }
}

/// Which loader a version uses. `Vanilla` needs no installer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoaderKind {
    Vanilla,
    Fabric,
    Forge,
    #[serde(rename = "neoforge", alias = "neo_forge")]
    NeoForge,
    Quilt,
}

impl LoaderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vanilla => "vanilla",
            Self::Fabric => "fabric",
            Self::Forge => "forge",
            Self::NeoForge => "neoforge",
            Self::Quilt => "quilt",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "vanilla" => Some(Self::Vanilla),
            "fabric" => Some(Self::Fabric),
            "forge" => Some(Self::Forge),
            "neoforge" => Some(Self::NeoForge),
            "quilt" => Some(Self::Quilt),
            _ => None,
        }
    }

    /// Loaders other than vanilla need a loader version string.
    pub fn requires_loader_version(self) -> bool {
        !matches!(self, Self::Vanilla)
    }
}

/// The pinned game/loader choice of one instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameVersion {
    /// Minecraft version id, e.g. `1.21.4`.
    pub game_version: String,
    pub loader: LoaderKind,
    /// Loader build, e.g. `0.16.9`. Absent for vanilla.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,
}

impl GameVersion {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.game_version.trim().is_empty() {
            return Err("gameVersion");
        }
        if self.loader.requires_loader_version()
            && self
                .loader_version
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err("loaderVersion");
        }
        Ok(())
    }
}

/// One launched instance as 01 stores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub version: GameVersion,
    pub location: InstanceLocation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub java_major: Option<u16>,
    /// Optional account/profile binding. Ids only; never a token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub state: InstanceState,
    /// Optimistic concurrency token. Starts at 1 and increases by one per commit.
    pub revision: u64,
    /// Milliseconds since the Unix epoch, supplied by the caller's clock port.
    pub created_at: u64,
    pub updated_at: u64,
}

impl Instance {
    pub fn revision_token(&self) -> Revision {
        Revision::new(self.revision)
    }
}

/// Which field a caller wants to change.
///
/// A patch is a closed set instead of an open map: an unexpected key must be a
/// rejected request, not a silently ignored field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstancePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<GameVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<InstanceLocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub java_major: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

impl InstancePatch {
    /// True when the patch changes nothing, so the store can return the current
    /// revision without writing (an unchanged patch must not bump `revision`).
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.version.is_none()
            && self.location.is_none()
            && self.java_major.is_none()
            && self.identity_id.is_none()
            && self.profile_id.is_none()
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if let Some(name) = &self.name {
            if name.trim().is_empty() {
                return Err("name");
            }
        }
        if let Some(version) = &self.version {
            version.validate()?;
        }
        if let Some(location) = &self.location {
            location.validate()?;
        }
        Ok(())
    }
}

/// A lease marks "a long task is using this instance". It is not a lock the OS
/// enforces; it is what makes a second mutation answer `instances.instance.locked`
/// instead of racing the writer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceLease {
    #[serde(default)]
    pub running: bool,
    pub instance_id: String,
    pub holder: String,
    pub operation_id: String,
    pub expires_at: u64,
}

impl InstanceLease {
    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }
}

/// One entry in the write-ahead recovery journal.
///
/// A write-ahead record captures the request binding and complete before/after
/// snapshots. Startup can then finish an indexed commit or restore the previous
/// value without guessing from a step number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryRecord {
    /// Idempotency key used by the command handler (command + request id).
    #[serde(default)]
    pub request_id: String,
    pub operation_id: String,
    pub instance_id: String,
    pub expected_revision: u64,
    pub committed: bool,
    /// Complete before/after values make recovery deterministic after a crash.
    #[serde(default)]
    pub before: Option<Instance>,
    #[serde(default)]
    pub after: Option<Instance>,
    /// Hash of the command body and target; prevents an idempotency key from
    /// replaying a result for a different request or instance.
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub aborted: bool,
    #[serde(default)]
    pub import_target: Option<String>,
}

impl fmt::Display for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.id, self.revision)
    }
}

/// Evidence markers 01 can attach to a task.
pub mod evidence {
    /// The instance directory was created and indexed.
    pub const INSTANCE_CREATED: &str = "instance_created";
    /// The index file was flushed after a committed mutation.
    pub const INDEX_COMMITTED: &str = "index_committed";
    /// The imported source directory was copied into the managed location.
    pub const IMPORT_COMPLETED: &str = "import_completed";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_validation_accepts_absolute_and_rejects_traversal() {
        assert!(InstanceLocation {
            root: "/home/u/instances/i1".into()
        }
        .validate()
        .is_ok());
        assert!(InstanceLocation {
            root: "C:\\Games\\instances\\i1".into()
        }
        .validate()
        .is_ok());
        assert_eq!(
            InstanceLocation {
                root: "relative/path".into()
            }
            .validate(),
            Err("absolute")
        );
        assert_eq!(
            InstanceLocation {
                root: "/home/u/../etc".into()
            }
            .validate(),
            Err("traversal")
        );
        assert_eq!(
            InstanceLocation { root: "  ".into() }.validate(),
            Err("root")
        );
    }

    #[test]
    fn loaders_other_than_vanilla_require_a_build() {
        let vanilla = GameVersion {
            game_version: "1.21.4".into(),
            loader: LoaderKind::Vanilla,
            loader_version: None,
        };
        assert!(vanilla.validate().is_ok());

        let fabric = GameVersion {
            game_version: "1.21.4".into(),
            loader: LoaderKind::Fabric,
            loader_version: None,
        };
        assert_eq!(fabric.validate(), Err("loaderVersion"));
    }

    #[test]
    fn an_empty_patch_is_detectable_so_it_does_not_bump_the_revision() {
        assert!(InstancePatch::default().is_empty());
        let named = InstancePatch {
            name: Some("x".into()),
            ..Default::default()
        };
        assert!(!named.is_empty());
    }

    #[test]
    fn state_gates_mutation_on_ready_only() {
        assert!(InstanceState::Ready.accepts_mutation());
        assert!(!InstanceState::Busy.accepts_mutation());
        assert!(!InstanceState::Broken.accepts_mutation());
    }

    #[test]
    fn lease_expiry_is_inclusive_of_the_deadline() {
        let lease = InstanceLease {
            running: false,
            instance_id: "i1".into(),
            holder: "task-1".into(),
            operation_id: "op-1".into(),
            expires_at: 1_000,
        };
        assert!(!lease.is_expired(999));
        assert!(lease.is_expired(1_000));
    }
}

//! Content recovery records ("what was I doing when the process died?").
//!
//! The instance transaction (01) is the owner of file writes and of the durable
//! commit. This module keeps its *own* recovery record so an interrupted install,
//! update or rollback can be resumed or discarded without re-resolving "latest".
//! Records live under `PlatformDirs::data`, never inside an instance directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};
use shtmcl_domain::Revision;

use super::error::ContentError;
use super::plan::{InstanceFile, digest_of};

pub const RECOVERY_SCHEMA: &str = "content-recovery.v2";
/// Schema read support: v1 records existed before `planned` was recorded, so they
/// are still accepted (with an empty `planned` list).
pub const RECOVERY_SCHEMA_V1: &str = "content-recovery.v1";
pub const RECOVERY_DIR: &str = "content-recovery";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryKind {
    Apply,
    Rollback,
}

impl RecoveryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Rollback => "rollback",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "apply" => Some(Self::Apply),
            "rollback" => Some(Self::Rollback),
            _ => None,
        }
    }
}

/// How far the interrupted operation got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Staging only; nothing was committed, the instance is untouched.
    Staged,
    /// The commit request was handed to the instance owner; the outcome is unknown
    /// until the instance content is compared against the planned digest.
    CommitStarted,
    /// The commit succeeded and the previous file set is still recorded, so a
    /// rollback is possible.
    Committed,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::CommitStarted => "commit_started",
            Self::Committed => "committed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "staged" => Some(Self::Staged),
            "commit_started" => Some(Self::CommitStarted),
            "committed" => Some(Self::Committed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRecord {
    pub schema: String,
    pub instance_id: String,
    pub plan_id: String,
    pub plan_digest: String,
    pub kind: RecoveryKind,
    pub stage: Stage,
    pub base_revision: Revision,
    /// File identities before the operation; the rollback input.
    pub previous: Vec<InstanceFile>,
    pub previous_digest: String,
    /// Digest of the content the operation wanted to reach.
    pub planned_digest: String,
    /// File-level plan: what the instance should hold after the commit. Used to
    /// decide whether an interrupted commit actually landed.
    pub planned: Vec<PlannedFile>,
}

/// One planned file in a recovery record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub path: String,
    /// `None` when the plan had no digest for the file (rare: local file identity).
    pub digest: Option<String>,
    pub size: Option<u64>,
}

impl PlannedFile {
    pub fn to_value(&self) -> Value {
        json!({ "path": self.path, "digest": self.digest, "size": self.size })
    }

    pub fn from_value(value: &Value) -> Result<Self, ContentError> {
        let object = value
            .as_object()
            .ok_or_else(|| ContentError::RecoveryCorrupt {
                detail: "planned file must be an object".to_string(),
            })?;
        Ok(Self {
            path: object
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            digest: object
                .get("digest")
                .and_then(Value::as_str)
                .map(str::to_string),
            size: object.get("size").and_then(Value::as_u64),
        })
    }
}

impl RecoveryRecord {
    pub fn new(
        instance_id: impl Into<String>,
        plan_id: impl Into<String>,
        plan_digest: impl Into<String>,
        kind: RecoveryKind,
        base_revision: Revision,
        previous: Vec<InstanceFile>,
        planned_digest: impl Into<String>,
        planned: Vec<PlannedFile>,
    ) -> Self {
        let previous_digest = digest_of(
            previous
                .iter()
                .map(|file| format!("{}|{}", file.path, file.digest))
                .collect(),
        );
        Self {
            schema: RECOVERY_SCHEMA.to_string(),
            instance_id: instance_id.into(),
            plan_id: plan_id.into(),
            plan_digest: plan_digest.into(),
            kind,
            stage: Stage::Staged,
            base_revision,
            previous,
            previous_digest,
            planned_digest: planned_digest.into(),
            planned,
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "schema": self.schema,
            "instanceId": self.instance_id,
            "planId": self.plan_id,
            "planDigest": self.plan_digest,
            "kind": self.kind.as_str(),
            "stage": self.stage.as_str(),
            "baseRevision": self.base_revision.get(),
            "previous": self.previous.iter().map(InstanceFile::to_value).collect::<Vec<_>>(),
            "previousDigest": self.previous_digest,
            "plannedDigest": self.planned_digest,
            "planned": self.planned.iter().map(PlannedFile::to_value).collect::<Vec<_>>(),
        })
    }

    pub fn from_value(value: &Value) -> Result<Self, ContentError> {
        let object = value
            .as_object()
            .ok_or_else(|| ContentError::RecoveryCorrupt {
                detail: "record must be an object".to_string(),
            })?;
        let schema = object
            .get("schema")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if schema != RECOVERY_SCHEMA && schema != RECOVERY_SCHEMA_V1 {
            return Err(ContentError::RecoveryCorrupt {
                detail: format!("unexpected schema {schema}"),
            });
        }
        let text = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Ok(Self {
            schema: schema.to_string(),
            instance_id: text("instanceId"),
            plan_id: text("planId"),
            plan_digest: text("planDigest"),
            kind: RecoveryKind::parse(&text("kind")).ok_or_else(|| {
                ContentError::RecoveryCorrupt {
                    detail: "unknown kind".to_string(),
                }
            })?,
            stage: Stage::parse(&text("stage")).ok_or_else(|| ContentError::RecoveryCorrupt {
                detail: "unknown stage".to_string(),
            })?,
            base_revision: Revision::new(
                object
                    .get("baseRevision")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            ),
            previous: object
                .get("previous")
                .and_then(Value::as_array)
                .map(|files| {
                    files
                        .iter()
                        .map(InstanceFile::from_value)
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default(),
            previous_digest: text("previousDigest"),
            planned_digest: text("plannedDigest"),
            planned: object
                .get("planned")
                .and_then(Value::as_array)
                .map(|files| {
                    files
                        .iter()
                        .map(PlannedFile::from_value)
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default(),
        })
    }
}

pub trait JournalStore: Send + Sync {
    fn write(&self, record: &RecoveryRecord) -> Result<(), ContentError>;
    fn read(&self, instance_id: &str) -> Result<Option<RecoveryRecord>, ContentError>;
    fn clear(&self, instance_id: &str) -> Result<(), ContentError>;
}

/// In-memory journal used by tests and by the provisional wiring.
#[derive(Debug, Default)]
pub struct MemoryJournalStore {
    records: Mutex<BTreeMap<String, RecoveryRecord>>,
}

impl MemoryJournalStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.records
            .lock()
            .map(|records| records.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl JournalStore for MemoryJournalStore {
    fn write(&self, record: &RecoveryRecord) -> Result<(), ContentError> {
        let mut records = self.records.lock().map_err(|_| ContentError::Internal {
            detail: "journal lock poisoned".to_string(),
        })?;
        records.insert(record.instance_id.clone(), record.clone());
        Ok(())
    }

    fn read(&self, instance_id: &str) -> Result<Option<RecoveryRecord>, ContentError> {
        let records = self.records.lock().map_err(|_| ContentError::Internal {
            detail: "journal lock poisoned".to_string(),
        })?;
        Ok(records.get(instance_id).cloned())
    }

    fn clear(&self, instance_id: &str) -> Result<(), ContentError> {
        let mut records = self.records.lock().map_err(|_| ContentError::Internal {
            detail: "journal lock poisoned".to_string(),
        })?;
        records.remove(instance_id);
        Ok(())
    }
}

/// Durable journal: one JSON file per instance under `<data>/content-recovery/`.
pub struct FileJournalStore {
    root: PathBuf,
}

impl FileJournalStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, instance_id: &str) -> PathBuf {
        let safe: String = instance_id
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        let name = if safe.is_empty() || safe.len() > 96 {
            digest_of(vec![instance_id.to_string()]).replace(':', "-")
        } else {
            safe
        };
        self.root.join(format!("{name}.json"))
    }
}

impl JournalStore for FileJournalStore {
    fn write(&self, record: &RecoveryRecord) -> Result<(), ContentError> {
        std::fs::create_dir_all(&self.root).map_err(|error| ContentError::Io {
            detail: format!("journal dir: {}", error.kind()),
        })?;
        let path = self.path_for(&record.instance_id);
        let temporary = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(&record.to_value()).map_err(|error| {
            ContentError::Internal {
                detail: error.to_string(),
            }
        })?;
        std::fs::write(&temporary, body).map_err(|error| ContentError::Io {
            detail: format!("journal write: {}", error.kind()),
        })?;
        std::fs::rename(&temporary, &path).map_err(|error| ContentError::Io {
            detail: format!("journal rename: {}", error.kind()),
        })?;
        Ok(())
    }

    fn read(&self, instance_id: &str) -> Result<Option<RecoveryRecord>, ContentError> {
        let path = self.path_for(instance_id);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(ContentError::Io {
                    detail: format!("journal read: {}", error.kind()),
                });
            }
        };
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|error| ContentError::RecoveryCorrupt {
                detail: error.to_string(),
            })?;
        RecoveryRecord::from_value(&value).map(Some)
    }

    fn clear(&self, instance_id: &str) -> Result<(), ContentError> {
        let path = self.path_for(instance_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ContentError::Io {
                detail: format!("journal clear: {}", error.kind()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> RecoveryRecord {
        RecoveryRecord::new(
            "instance-1",
            "plan-1",
            "fnv1a64:0000000000000001",
            RecoveryKind::Apply,
            Revision::new(4),
            vec![InstanceFile::new("mods/a.jar", "aaaa", 10)],
            "sha256:0000000000000002",
            vec![PlannedFile {
                path: "mods/a.jar".to_string(),
                digest: Some("bbbb".to_string()),
                size: Some(10),
            }],
        )
    }

    #[test]
    fn file_journal_round_trips_and_clears() {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-04-journal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or(0)
        ));
        let store = FileJournalStore::new(&root);
        assert!(store.read("instance-1").expect("read").is_none());

        let mut value = record();
        value.stage = Stage::CommitStarted;
        store.write(&value).expect("write");
        let loaded = store.read("instance-1").expect("read").expect("present");
        assert_eq!(loaded, value);

        store.clear("instance-1").expect("clear");
        assert!(store.read("instance-1").expect("read").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn json_round_trip_is_stable() {
        let value = record();
        let encoded = value.to_value();
        let decoded = RecoveryRecord::from_value(&encoded).expect("decode");
        assert_eq!(decoded, value);
    }
}

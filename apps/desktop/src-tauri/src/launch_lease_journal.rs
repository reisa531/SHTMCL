//! Write-ahead process records. Missing or inconclusive evidence never unlocks games.
use serde::{Deserialize, Serialize};
use shtmcl_platform::process_identity::{self, ProcessIdentity, ProcessLiveness};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const MAX_RECORD: u64 = 64 * 1024;
const MAX_RECORDS: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub instance: String,
    pub owner: String,
    pub operation: String,
    pub sessions: BTreeMap<String, Option<Identity>>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    pub pid: u32,
    pub creation_time: u64,
}
impl From<ProcessIdentity> for Identity {
    fn from(value: ProcessIdentity) -> Self {
        Self {
            pid: value.pid,
            creation_time: value.creation_time,
        }
    }
}
impl Record {
    pub(crate) fn exited(&self) -> bool {
        self.exited_with(|identity| {
            process_identity::probe(&ProcessIdentity {
                pid: identity.pid,
                creation_time: identity.creation_time,
            })
        })
    }
    fn exited_with(&self, probe: impl Fn(Identity) -> ProcessLiveness) -> bool {
        !self.sessions.is_empty()
            && self.sessions.values().all(|identity| {
                identity.is_some_and(|identity| probe(identity) == ProcessLiveness::Exited)
            })
    }
}

pub(crate) struct Journal {
    root: Option<PathBuf>,
}
impl Journal {
    #[cfg(test)]
    pub(crate) fn memory() -> Self {
        Self { root: None }
    }
    pub(crate) fn open(root: PathBuf) -> io::Result<Self> {
        check_ancestors(&root)?;
        fs::create_dir_all(&root)?;
        check(&root)?;
        Ok(Self { root: Some(root) })
    }
    fn path(&self, id: &str) -> io::Result<Option<PathBuf>> {
        if id.is_empty() || id.len() > 128 {
            return Err(invalid());
        }
        // Hex names avoid path syntax, reserved Windows names, and collisions.
        Ok(self.root.as_ref().map(|root| {
            root.join(format!(
                "{}.json",
                id.as_bytes()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ))
        }))
    }
    pub(crate) fn contains(&self, id: &str) -> io::Result<bool> {
        if let Some(root) = &self.root {
            check_ancestors(root)?;
        }
        match self.path(id)? {
            Some(path) => match fs::symlink_metadata(path) {
                Ok(_) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            },
            None => Ok(false),
        }
    }
    pub(crate) fn write(&self, record: &Record) -> io::Result<()> {
        let Some(path) = self.path(&record.instance)? else {
            return Ok(());
        };
        check(self.root.as_ref().unwrap())?;
        if record.sessions.is_empty() || record.sessions.len() > 256 {
            return Err(invalid());
        }
        let bytes = serde_json::to_vec(record)?;
        if bytes.len() as u64 > MAX_RECORD {
            return Err(invalid());
        }
        let tmp = path.with_extension(format!("{}.tmp", shtmcl_platform::entropy::random_hex(8)?));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&tmp, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result
    }
    pub(crate) fn remove(&self, id: &str) -> io::Result<()> {
        if let Some(root) = &self.root {
            check_ancestors(root)?;
        }
        if let Some(path) = self.path(id)? {
            fs::remove_file(path)?;
        }
        Ok(())
    }
    pub(crate) fn records(&self) -> io::Result<Vec<Record>> {
        let Some(root) = &self.root else {
            return Ok(Vec::new());
        };
        check(root)?;
        let mut records = Vec::new();
        for (index, entry) in fs::read_dir(root)?.enumerate() {
            if index >= MAX_RECORDS {
                return Err(invalid());
            }
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            check(&path)?;
            let mut bytes = Vec::new();
            fs::File::open(&path)?
                .take(MAX_RECORD + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_RECORD {
                return Err(invalid());
            }
            let record: Record = serde_json::from_slice(&bytes)?;
            if self.path(&record.instance)?.as_ref() != Some(&path)
                || record.sessions.is_empty()
                || record.sessions.len() > 256
            {
                return Err(invalid());
            }
            records.push(record);
        }
        Ok(records)
    }
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid launch lease journal")
}
fn check_ancestors(path: &Path) -> io::Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    for ancestor in absolute.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => check_one(ancestor)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
fn check(path: &Path) -> io::Result<()> {
    check_ancestors(path)?;
    check_one(path)
}
fn check_one(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(invalid());
        }
    }
    if metadata.file_type().is_symlink() {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_and_unknown_never_prove_exit() {
        let mut record = Record {
            instance: "a".into(),
            owner: "s".into(),
            operation: "op".into(),
            sessions: BTreeMap::from([("s".into(), None)]),
        };
        assert!(!record.exited_with(|_| ProcessLiveness::Exited));
        record.sessions.insert(
            "s".into(),
            Some(Identity {
                pid: 1,
                creation_time: 2,
            }),
        );
        assert!(!record.exited_with(|_| ProcessLiveness::Unknown));
        assert!(!record.exited_with(|_| ProcessLiveness::Alive));
        assert!(record.exited_with(|_| ProcessLiveness::Exited));
        record.sessions.insert("other".into(), None);
        assert!(!record.exited_with(|_| ProcessLiveness::Exited));
    }
    #[cfg(windows)]
    #[test]
    fn ancestor_junction_rejected_on_open_and_after_open() {
        use std::os::windows::process::CommandExt;
        let base = std::env::temp_dir().join(format!(
            "shtmcl-junction-journal-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        let parent = base.join("parent");
        let moved = base.join("moved");
        let target = base.join("target");
        fs::create_dir_all(&target).unwrap();
        let journal = Journal::open(parent.join("journal")).unwrap();
        fs::rename(&parent, &moved).unwrap();
        let output = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&parent)
            .arg(&target)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction creation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(Journal::open(parent.join("new")).is_err());
        assert!(!target.join("new").exists());
        let record = Record {
            instance: "school".into(),
            owner: "one".into(),
            operation: "op".into(),
            sessions: BTreeMap::from([("one".into(), None)]),
        };
        assert!(journal.write(&record).is_err());
        assert!(journal.records().is_err());
        assert!(journal.contains("school").is_err());
        fs::remove_dir(&parent).unwrap();
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn journal_roundtrip_and_corruption_preserved() {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-journal-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        let journal = Journal::open(root.clone()).unwrap();
        let record = Record {
            instance: "school".into(),
            owner: "one".into(),
            operation: "op".into(),
            sessions: BTreeMap::from([("one".into(), None)]),
        };
        journal.write(&record).unwrap();
        assert_eq!(journal.records().unwrap()[0].owner, "one");
        let path = journal.path("school").unwrap().unwrap();
        fs::write(&path, b"broken").unwrap();
        assert!(journal.records().is_err());
        assert_eq!(fs::read(path).unwrap(), b"broken");
        fs::remove_dir_all(root).unwrap();
    }
}

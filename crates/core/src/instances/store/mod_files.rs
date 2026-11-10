//! Instance-owned single-file Mod toggles. The store lock excludes launches and
//! other mutations. A durable intent bridges file changes and the index commit.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Read;

const JOURNAL: &str = "mod-transaction.json";
const MAX_FILE: u64 = 32 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct ModFileTransaction {
    record: RecoveryRecord,
    managed_root: PathBuf,
    from: String,
    to: String,
    sha256: String,
}

fn checked(path: &Path) -> Result<(), String> {
    for part in path.ancestors() {
        match fs::symlink_metadata(part) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err("linked path".into());
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        return Err("reparse path".into());
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err("path unavailable".into()),
        }
    }
    Ok(())
}

fn names(file: &str, enabled: bool) -> Result<String, InstancesError> {
    let lower = file.to_ascii_lowercase();
    if file.is_empty()
        || file.len() > 255
        || file.chars().any(char::is_control)
        || file.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
    {
        return Err(InstancesError::InvalidRequest("modFileName"));
    }
    let target = if enabled && lower.ends_with(".jar.disabled") && file.len() > 13 {
        file[..file.len() - 9].to_string()
    } else if !enabled && lower.ends_with(".jar") && file.len() > 4 {
        format!("{file}.disabled")
    } else {
        return Err(InstancesError::InvalidRequest("modFileName"));
    };
    if target.len() > 255 {
        return Err(InstancesError::InvalidRequest("modFileName"));
    }
    Ok(target)
}

fn managed(instance: &Instance, base: &Path) -> Result<PathBuf, String> {
    if instance.id.is_empty()
        || instance.id.len() > 64
        || !instance
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("invalid instance id".into());
    }
    let root = base.join(&instance.id);
    if !base.is_absolute() || Path::new(&instance.location.root) != root {
        return Err("unmanaged instance".into());
    }
    checked(&root)?;
    if !root.is_dir() {
        return Err("missing instance".into());
    }
    Ok(root.join("mods"))
}

fn hash(path: &Path) -> Result<(u64, String), String> {
    checked(path)?;
    let mut file = fs::File::open(path).map_err(|_| "mod unavailable")?;
    let meta = file.metadata().map_err(|_| "mod unavailable")?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return Err("mod exceeds file budget or is not regular".into());
    }
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer).map_err(|_| "mod read failed")?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        if bytes > MAX_FILE {
            return Err("mod exceeds file budget".into());
        }
        digest.update(&buffer[..count]);
    }
    if bytes != meta.len() {
        return Err("mod changed during read".into());
    }
    Ok((bytes, format!("{:x}", digest.finalize())))
}

fn exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("cannot inspect target".into()),
    }
}

fn sync_dir(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| "directory sync failed".to_string())?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

impl InstanceStore {
    pub fn mod_file_snapshot(
        &self,
        id: &str,
        base: &Path,
        file: &str,
    ) -> Result<Value, InstancesError> {
        // Validate either suffix without guessing a filename from user input.
        let enabled = file.to_ascii_lowercase().ends_with(".jar");
        names(file, !enabled)?;
        let inner = self.lock()?;
        let instance = inner
            .instances
            .get(id)
            .ok_or_else(|| InstancesError::NotFound(id.into()))?;
        let directory = managed(instance, base).map_err(|_| InstancesError::ModUnavailable)?;
        let (bytes, sha256) =
            hash(&directory.join(file)).map_err(|_| InstancesError::ModUnavailable)?;
        Ok(
            json!({"instanceId":id,"revision":instance.revision,"state":instance.state,"fileName":file,"enabled":enabled,"bytes":bytes,"sha256":sha256}),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn toggle_mod(
        &self,
        request_id: &str,
        operation: &str,
        id: &str,
        base: &Path,
        expected: Revision,
        file: &str,
        enabled: bool,
        sha256: &str,
        now: u64,
    ) -> Result<CommitOutcome, InstancesError> {
        let target = names(file, enabled)?;
        if sha256.len() != 64
            || !sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(InstancesError::InvalidRequest("sha256"));
        }
        let mut inner = self.lock()?;
        let fingerprint = fingerprint("mod_toggle", &(id, expected.get(), file, enabled, sha256))?;
        if let Some(applied) = inner.applied_request(request_id) {
            if applied.instance_id != id || applied.fingerprint != fingerprint {
                return Err(InstancesError::IdempotencyConflict);
            }
            let instance = inner
                .recovery
                .get(request_id)
                .and_then(|r| r.after.clone())
                .ok_or(InstancesError::IdempotencyConflict)?;
            return Ok(CommitOutcome {
                instance,
                replayed: true,
            });
        }
        let current = inner
            .instances
            .get(id)
            .cloned()
            .ok_or_else(|| InstancesError::NotFound(id.into()))?;
        if inner.live_lease(id, now).is_some() {
            return Err(InstancesError::Locked(id.into()));
        }
        if current.state == InstanceState::Busy {
            return Err(InstancesError::Running(id.into()));
        }
        if !current.state.accepts_mutation() {
            return Err(InstancesError::Terminal(id.into()));
        }
        if current.revision != expected.get() {
            return Err(InstancesError::RevisionStale {
                expected: expected.get(),
                actual: current.revision,
            });
        }
        let directory = managed(&current, base).map_err(|_| InstancesError::ModUnavailable)?;
        if exists(&directory.join(&target)).map_err(|_| InstancesError::ModUnavailable)? {
            return Err(InstancesError::ModTargetExists);
        }
        let (_, observed) =
            hash(&directory.join(file)).map_err(|_| InstancesError::ModUnavailable)?;
        if observed != sha256 {
            return Err(InstancesError::ModChanged);
        }
        let revision = current
            .revision
            .checked_add(1)
            .ok_or(InstancesError::Internal)?;
        let updated = Instance {
            revision,
            updated_at: now,
            ..current.clone()
        };
        let record = RecoveryRecord {
            request_id: request_id.into(),
            operation_id: operation.into(),
            instance_id: id.into(),
            expected_revision: revision,
            committed: false,
            before: Some(current),
            after: Some(updated.clone()),
            fingerprint: fingerprint.clone(),
            aborted: false,
            import_target: None,
        };
        let transaction = ModFileTransaction {
            record: record.clone(),
            managed_root: base.into(),
            from: file.into(),
            to: target,
            sha256: sha256.into(),
        };
        let mut snapshot = inner.snapshot();
        *snapshot
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or(InstancesError::Internal)? = updated.clone();
        let leases: Vec<_> = inner
            .leases
            .values()
            .filter(|lease| lease.instance_id != id)
            .cloned()
            .collect();
        if self
            .storage
            .commit_mod_toggle(&transaction, &snapshot, &leases)
            .is_err()
        {
            // An unresolved disk intent must never leave the in-memory instance
            // launchable. Storage recovery must succeed before it is used again.
            match self.storage.load_index() {
                Ok(values) => {
                    if let Some(value) = values.into_iter().find(|item| item.id == id) {
                        inner.instances.insert(id.into(), value);
                    }
                }
                Err(_) => {
                    if let Some(value) = inner.instances.get_mut(id) {
                        value.state = InstanceState::Broken;
                    }
                }
            }
            return Err(InstancesError::ModUnavailable);
        }
        inner.instances.insert(id.into(), updated.clone());
        inner.leases.remove(id);
        let mut committed = record;
        committed.committed = true;
        inner.recovery.insert(request_id.into(), committed);
        inner.remember(
            request_id,
            AppliedRequest {
                instance_id: id.into(),
                revision,
                fingerprint,
            },
        );
        Ok(CommitOutcome {
            instance: updated,
            replayed: false,
        })
    }
}

fn transaction_paths(tx: &ModFileTransaction) -> Result<(PathBuf, PathBuf), String> {
    let before = tx.record.before.as_ref().ok_or("missing before")?;
    let after = tx.record.after.as_ref().ok_or("missing after")?;
    if before.id != after.id
        || tx.record.instance_id != before.id
        || before.revision.checked_add(1) != Some(after.revision)
        || before.location != after.location
        || before.state != InstanceState::Ready
        || after.state != InstanceState::Ready
        || tx.sha256.len() != 64
        || !tx.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("invalid mod intent".into());
    }
    let enabling = tx.from.to_ascii_lowercase().ends_with(".jar.disabled");
    if names(&tx.from, enabling).map_err(|_| "invalid mod filename")? != tx.to {
        return Err("invalid mod target".into());
    }
    let directory = managed(before, &tx.managed_root)?;
    Ok((directory.join(&tx.from), directory.join(&tx.to)))
}

fn matches(path: &Path, digest: &str) -> Result<(), String> {
    if hash(path)?.1 != digest {
        return Err("mod intent content mismatch".into());
    }
    Ok(())
}

fn same_file(left: &Path, right: &Path) -> Result<bool, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let a = fs::metadata(left).map_err(|_| "file identity unavailable")?;
        let b = fs::metadata(right).map_err(|_| "file identity unavailable")?;
        Ok(a.dev() == b.dev() && a.ino() == b.ino())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        #[repr(C)]
        #[derive(Default, PartialEq, Eq)]
        struct FileIdInfo {
            volume: u64,
            id: [u8; 16],
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetFileInformationByHandleEx(
                handle: *mut std::ffi::c_void,
                class: i32,
                info: *mut std::ffi::c_void,
                size: u32,
            ) -> i32;
        }
        let identity = |path: &Path| -> Result<FileIdInfo, String> {
            let file = fs::File::open(path).map_err(|_| "file identity unavailable")?;
            let mut info = FileIdInfo::default();
            // FileIdInfo (18) preserves the full 128-bit identity on ReFS too.
            if unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    18,
                    (&mut info as *mut FileIdInfo).cast(),
                    std::mem::size_of::<FileIdInfo>() as u32,
                )
            } == 0
            {
                return Err("file identity unavailable".into());
            }
            Ok(info)
        };
        Ok(identity(left)? == identity(right)?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (left, right);
        Err("file identity unavailable".into())
    }
}

fn clear(storage: &FileStorage) -> Result<(), String> {
    // Atomic replacement with null, not deletion, makes journal clearing use
    // the same flushed JSON primitive as intent creation.
    storage.atomic_json(JOURNAL, &Option::<ModFileTransaction>::None)?;
    sync_dir(&storage.root)
}

pub(super) fn recover(storage: &FileStorage) -> Result<(), String> {
    for name in [
        JOURNAL,
        ".mod-transaction.json.tmp",
        "state.json",
        ".state.json.tmp",
        "recovery.json",
        ".recovery.json.tmp",
    ] {
        checked(&storage.root.join(name))?;
    }
    let Some(tx) = storage.read_json::<Option<ModFileTransaction>>(JOURNAL)? else {
        return Ok(());
    };
    let (from, to) = transaction_paths(&tx)?;
    let state: PersistedState = storage.read_json("state.json")?;
    let current = state
        .instances
        .iter()
        .find(|i| i.id == tx.record.instance_id)
        .ok_or("mod intent instance missing")?;
    let committed = if Some(current) == tx.record.after.as_ref() {
        true
    } else if Some(current) == tx.record.before.as_ref() {
        false
    } else {
        return Err("mod intent revision mismatch".into());
    };
    let (keep, remove) = if committed {
        (&to, &from)
    } else {
        (&from, &to)
    };
    if !exists(keep)? {
        matches(remove, &tx.sha256)?;
        fs::hard_link(remove, keep).map_err(|_| "cannot restore mod file")?;
        sync_dir(keep.parent().ok_or("missing directory")?)?;
    }
    matches(keep, &tx.sha256)?;
    if exists(remove)? {
        matches(remove, &tx.sha256)?;
        if !same_file(keep, remove)? {
            return Err("mod intent has unrelated file names".into());
        }
        fs::remove_file(remove).map_err(|_| "cannot finish mod recovery")?;
        sync_dir(keep.parent().ok_or("missing directory")?)?;
    }
    let mut record = tx.record;
    record.committed = committed;
    record.aborted = !committed;
    storage.write_recovery(&record)?;
    clear(storage)
}

pub(super) fn commit(
    storage: &FileStorage,
    tx: &ModFileTransaction,
    instances: &[Instance],
    leases: &[InstanceLease],
) -> Result<(), String> {
    recover(storage)?;
    let (from, to) = transaction_paths(tx)?;
    if exists(&to)? {
        return Err("mod target already exists".into());
    }
    matches(&from, &tx.sha256)?;
    checked(&storage.root.join(JOURNAL))?;
    storage.atomic_json(JOURNAL, &Some(tx))?;
    sync_dir(&storage.root)?;
    let result: Result<(), String> = (|| {
        // hard_link is no-replace on both NTFS and Unix filesystems. A crash
        // between link/unlink is recovered from the persisted index commit point.
        if fs::hard_link(&from, &to).is_err() {
            // No link was created; preserve a target that appeared concurrently.
            clear(storage)?;
            return Err("cannot create mod target (hard links required)".into());
        }
        sync_dir(to.parent().ok_or("missing directory")?)?;
        matches(&to, &tx.sha256)?;
        fs::remove_file(&from).map_err(|_| "cannot unlink previous mod name")?;
        sync_dir(to.parent().ok_or("missing directory")?)?;
        storage.atomic_json(
            "state.json",
            &PersistedState {
                instances: instances.to_vec(),
                leases: leases.to_vec(),
            },
        )?;
        sync_dir(&storage.root)
    })();
    // Reconcile ambiguous filesystem/JSON failures using disk readback. If the
    // index commit landed, report the durable committed result; otherwise roll back.
    let recovery = recover(storage);
    if recovery.is_ok() {
        let state: PersistedState = storage.read_json("state.json")?;
        if state
            .instances
            .iter()
            .find(|i| i.id == tx.record.instance_id)
            == tx.record.after.as_ref()
        {
            return Ok(());
        }
    }
    result
        .and(recovery)
        .and(Err("mod toggle did not commit".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::{GameVersion, InstanceLocation, LoaderKind};
    struct Fixture {
        root: PathBuf,
        base: PathBuf,
        game: PathBuf,
    }
    impl Fixture {
        fn new() -> (Self, InstanceStore) {
            let root = std::env::temp_dir().join(format!(
                "shtmcl-mod-toggle-{}",
                shtmcl_platform::entropy::random_hex(8).unwrap()
            ));
            let base = root.join("instances");
            let game = base.join("school");
            fs::create_dir_all(game.join("mods")).unwrap();
            fs::write(game.join("mods/a.jar"), b"synthetic jar bytes").unwrap();
            let store = InstanceStore::new(Box::new(FileStorage::new(root.join("index")).unwrap()))
                .unwrap();
            store
                .create(
                    "create",
                    Instance {
                        id: "school".into(),
                        name: "School".into(),
                        version: GameVersion {
                            game_version: "1.20.4".into(),
                            loader: LoaderKind::Vanilla,
                            loader_version: None,
                        },
                        location: InstanceLocation {
                            root: game.to_str().unwrap().into(),
                        },
                        java_major: Some(17),
                        identity_id: None,
                        profile_id: None,
                        state: InstanceState::Ready,
                        revision: 1,
                        created_at: 1,
                        updated_at: 1,
                    },
                    1,
                )
                .unwrap();
            (Self { root, base, game }, store)
        }
        fn reopen(&self) -> Result<InstanceStore, InstancesError> {
            InstanceStore::new(Box::new(FileStorage::new(self.root.join("index")).unwrap()))
        }
        fn digest(&self) -> String {
            hash(&self.game.join("mods/a.jar")).unwrap().1
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn toggle_roundtrip_reopen_and_request_replay_preserve_bytes() {
        let (f, store) = Fixture::new();
        let digest = f.digest();
        let view = store.mod_file_snapshot("school", &f.base, "a.jar").unwrap();
        assert_eq!(view["sha256"], digest);
        assert_eq!(view["enabled"], true);
        let outcome = store
            .toggle_mod(
                "toggle",
                "op",
                "school",
                &f.base,
                Revision::new(1),
                "a.jar",
                false,
                &digest,
                2,
            )
            .unwrap();
        assert_eq!(outcome.instance.revision, 2);
        assert!(!f.game.join("mods/a.jar").exists());
        assert_eq!(
            fs::read(f.game.join("mods/a.jar.disabled")).unwrap(),
            b"synthetic jar bytes"
        );
        drop(store);
        let store = f.reopen().unwrap();
        assert!(
            store
                .toggle_mod(
                    "toggle",
                    "op",
                    "school",
                    &f.base,
                    Revision::new(1),
                    "a.jar",
                    false,
                    &digest,
                    2
                )
                .unwrap()
                .replayed
        );
        assert_eq!(
            store
                .mod_file_snapshot("school", &f.base, "a.jar.disabled")
                .unwrap()["enabled"],
            false
        );
        store
            .toggle_mod(
                "enable",
                "op2",
                "school",
                &f.base,
                Revision::new(2),
                "a.jar.disabled",
                true,
                &digest,
                3,
            )
            .unwrap();
        assert_eq!(
            fs::read(f.game.join("mods/a.jar")).unwrap(),
            b"synthetic jar bytes"
        );
        assert!(!f.game.join("mods/a.jar.disabled").exists());
        assert_eq!(store.get("school").unwrap().revision, 3);
    }

    #[test]
    fn refuses_stale_bytes_revision_collision_and_running_leases() {
        let (f, store) = Fixture::new();
        let digest = f.digest();
        fs::write(f.game.join("mods/a.jar.disabled"), b"do not overwrite").unwrap();
        assert!(matches!(
            store.toggle_mod(
                "1",
                "op",
                "school",
                &f.base,
                Revision::new(1),
                "a.jar",
                false,
                &digest,
                2
            ),
            Err(InstancesError::ModTargetExists)
        ));
        assert_eq!(
            fs::read(f.game.join("mods/a.jar.disabled")).unwrap(),
            b"do not overwrite"
        );
        fs::remove_file(f.game.join("mods/a.jar.disabled")).unwrap();
        assert!(matches!(
            store.toggle_mod(
                "2",
                "op",
                "school",
                &f.base,
                Revision::new(7),
                "a.jar",
                false,
                &digest,
                2
            ),
            Err(InstancesError::RevisionStale { .. })
        ));
        fs::write(f.game.join("mods/a.jar"), b"changed file").unwrap();
        assert!(matches!(
            store.toggle_mod(
                "3",
                "op",
                "school",
                &f.base,
                Revision::new(1),
                "a.jar",
                false,
                &digest,
                2
            ),
            Err(InstancesError::ModChanged)
        ));
        let digest = f.digest();
        store
            .acquire_running_lease("school", "game", "run", 2, 100)
            .unwrap();
        assert!(store
            .toggle_mod(
                "4",
                "op",
                "school",
                &f.base,
                Revision::new(1),
                "a.jar",
                false,
                &digest,
                3
            )
            .is_err());
        assert!(!f.game.join("mods/a.jar.disabled").exists());
    }

    #[test]
    fn rejects_unmanaged_paths_invalid_names_links_and_large_files() {
        let (f, store) = Fixture::new();
        for file in ["../a.jar", "a.jar/child", "a:stream.jar", ".jar", "a.txt"] {
            assert!(store.mod_file_snapshot("school", &f.base, file).is_err());
        }
        assert!(store.mod_file_snapshot("school", &f.root, "a.jar").is_err());
        let large = fs::File::create(f.game.join("mods/large.jar")).unwrap();
        large.set_len(MAX_FILE + 1).unwrap();
        assert!(store
            .mod_file_snapshot("school", &f.base, "large.jar")
            .is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(f.game.join("mods/a.jar"), f.game.join("mods/linked.jar"))
                .unwrap();
            assert!(store
                .mod_file_snapshot("school", &f.base, "linked.jar")
                .is_err());
        }
    }

    fn interrupted(f: &Fixture, phase: u8) {
        let storage = FileStorage::new(f.root.join("index")).unwrap();
        let mut state: PersistedState = storage.read_json("state.json").unwrap();
        let before = state.instances[0].clone();
        let after = Instance {
            revision: 2,
            updated_at: 2,
            ..before.clone()
        };
        let digest = f.digest();
        let tx = ModFileTransaction {
            record: RecoveryRecord {
                request_id: "toggle".into(),
                operation_id: "op".into(),
                instance_id: "school".into(),
                expected_revision: 2,
                committed: false,
                before: Some(before),
                after: Some(after.clone()),
                fingerprint: fingerprint("mod_toggle", &("school", 1u64, "a.jar", false, &digest))
                    .unwrap(),
                aborted: false,
                import_target: None,
            },
            managed_root: f.base.clone(),
            from: "a.jar".into(),
            to: "a.jar.disabled".into(),
            sha256: digest,
        };
        storage.atomic_json(JOURNAL, &Some(tx)).unwrap();
        if phase >= 1 {
            fs::hard_link(
                f.game.join("mods/a.jar"),
                f.game.join("mods/a.jar.disabled"),
            )
            .unwrap();
        }
        if phase >= 2 {
            fs::remove_file(f.game.join("mods/a.jar")).unwrap();
        }
        if phase >= 3 {
            state.instances[0] = after;
            storage.atomic_json("state.json", &state).unwrap();
        }
    }

    #[test]
    fn recovers_each_interruption_boundary_using_the_durable_index() {
        for phase in 0..4 {
            let (f, store) = Fixture::new();
            drop(store);
            interrupted(&f, phase);
            let reopened = f.reopen().unwrap();
            let committed = phase == 3;
            assert_eq!(f.game.join("mods/a.jar.disabled").exists(), committed);
            assert_eq!(f.game.join("mods/a.jar").exists(), !committed);
            assert_eq!(
                reopened.get("school").unwrap().revision,
                if committed { 2 } else { 1 }
            );
            let path = if committed { "a.jar.disabled" } else { "a.jar" };
            assert_eq!(
                fs::read(f.game.join("mods").join(path)).unwrap(),
                b"synthetic jar bytes"
            );
        }
    }

    #[test]
    fn index_write_failure_rolls_back_the_file_and_revision() {
        let (f, store) = Fixture::new();
        let digest = f.digest();
        fs::create_dir(f.root.join("index/.state.json.tmp")).unwrap();
        assert!(store
            .toggle_mod(
                "toggle",
                "op",
                "school",
                &f.base,
                Revision::new(1),
                "a.jar",
                false,
                &digest,
                2
            )
            .is_err());
        assert_eq!(store.get("school").unwrap().revision, 1);
        assert_eq!(
            fs::read(f.game.join("mods/a.jar")).unwrap(),
            b"synthetic jar bytes"
        );
        assert!(!f.game.join("mods/a.jar.disabled").exists());
        fs::remove_dir(f.root.join("index/.state.json.tmp")).unwrap();
        drop(store);
        assert_eq!(f.reopen().unwrap().get("school").unwrap().revision, 1);
    }

    #[test]
    fn aborted_mod_intent_cannot_roll_back_a_later_successful_edit() {
        let (f, store) = Fixture::new();
        let digest = f.digest();
        fs::create_dir(f.root.join("index/.state.json.tmp")).unwrap();
        assert!(store
            .toggle_mod(
                "toggle",
                "op",
                "school",
                &f.base,
                Revision::new(1),
                "a.jar",
                false,
                &digest,
                2
            )
            .is_err());
        fs::remove_dir(f.root.join("index/.state.json.tmp")).unwrap();
        store
            .update(
                "rename",
                "school",
                Revision::new(1),
                &InstancePatch {
                    name: Some("Later name".into()),
                    ..Default::default()
                },
                3,
            )
            .unwrap();
        drop(store);
        let reopened = f.reopen().unwrap();
        assert_eq!(reopened.get("school").unwrap().name, "Later name");
        assert_eq!(reopened.get("school").unwrap().revision, 2);
    }

    #[test]
    fn unresolved_intent_blocks_launch_until_recovery_can_confirm_commit() {
        let (f, store) = Fixture::new();
        let digest = f.digest();
        fs::create_dir(f.root.join("index/.recovery.json.tmp")).unwrap();
        assert!(store
            .toggle_mod(
                "toggle",
                "op",
                "school",
                &f.base,
                Revision::new(1),
                "a.jar",
                false,
                &digest,
                2
            )
            .is_err());
        assert_eq!(store.get("school").unwrap().state, InstanceState::Broken);
        assert!(store
            .acquire_running_lease("school", "game", "run", 3, 100)
            .is_err());
        assert!(f.game.join("mods/a.jar.disabled").exists());
        drop(store);
        assert!(f.reopen().is_err());
        fs::remove_dir(f.root.join("index/.recovery.json.tmp")).unwrap();
        let reopened = f.reopen().unwrap();
        assert_eq!(reopened.get("school").unwrap().revision, 2);
    }

    #[test]
    fn recovery_does_not_delete_an_unrelated_same_content_target() {
        let (f, store) = Fixture::new();
        drop(store);
        interrupted(&f, 0);
        fs::write(f.game.join("mods/a.jar.disabled"), b"synthetic jar bytes").unwrap();
        assert!(f.reopen().is_err());
        assert!(f.game.join("mods/a.jar").exists() && f.game.join("mods/a.jar.disabled").exists());
    }

    #[cfg(unix)]
    #[test]
    fn linked_journal_temporary_file_is_rejected_without_writing_outside_storage() {
        let (f, store) = Fixture::new();
        let digest = f.digest();
        let outside = f.root.join("preserve.txt");
        fs::write(&outside, b"preserve").unwrap();
        std::os::unix::fs::symlink(&outside, f.root.join("index/.mod-transaction.json.tmp"))
            .unwrap();
        assert!(store
            .toggle_mod(
                "toggle",
                "op",
                "school",
                &f.base,
                Revision::new(1),
                "a.jar",
                false,
                &digest,
                2
            )
            .is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"preserve");
        assert!(f.game.join("mods/a.jar").exists());
        assert!(!f.game.join("mods/a.jar.disabled").exists());
    }
}

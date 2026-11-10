//! Pure-local filesystem staging for the fixed NeoForge 1.20.4/20.4.253 client pilot.
//!
//! This module only performs bounded local filesystem work: no network, no
//! process spawning, no scheduler and no instance mutation. Planning happens
//! fully before any filesystem mutation. The destination is created with a
//! single `create_dir` call and is never pre-existing; a rollback guard attempts to remove the newly-created tree on failure or
//! cancellation. A pre-existing directory is never owned by that guard.
//! Filesystem cleanup failures can leave a staging tree for later recovery.
//!
//! Trust model: the caller supplies the trusted installer SHA256 identity and
//! a `VerifiedInput` for the Minecraft jar whose SHA1/size are trusted here.
//! All pinned tool libraries and the NeoForm MCP_DATA zip are verified against
//! the embedded pins or the verified installer profile, not caller-supplied hashes.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha1::Sha1;
use sha2::{Digest, Sha256};

use super::neoforge_plan::{
    plan_client_staging, StagingArgument, StagingPlanError, StagingStep, INSTALLER_INPUT,
    MINECRAFT_INPUT,
};
use super::neoforge_processor_allowlist::ARTIFACTS;
use super::neoforge_profile::{
    maven_path, read_installer_archive, InstallerResource, NeoForgeInstallerProfile,
};
use super::paths::normalise_content_path;
use super::zipindex::{read_entry, read_index, ZipBudget, ZipEntry, ZipIndex};

const PILOT_GAME_VERSION: &str = "1.20.4";
const PILOT_LOADER_VERSION: &str = "20.4.253";
const COPY_BUFFER: usize = 64 * 1024;
const RESOURCE_PREFIX: &str = "installer-resources/data/";

/// Trusted Minecraft input jar: path, size and SHA1 are asserted by the caller.
#[derive(Debug, Clone)]
pub struct VerifiedInput {
    pub path: PathBuf,
    pub size: u64,
    pub sha1: String,
}

/// One artifact the later processor/scheduler stage expects to be staged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagingKind {
    /// A library jar from the fixed pin list or a profile library.
    Library,
    /// The NeoForm MCP data zip taken from MCP_DATA.
    McpData,
    /// An installer resource extracted from the installer archive.
    InstallerResource { sha256: String },
    /// The copied installer jar.
    Installer,
    /// The copied Minecraft jar.
    Minecraft,
}

/// One deterministic, already-finalized staging step (files are only listed in
/// preparation order; processors run in a later install flow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagingFootprint {
    /// Normalised, stage-relative content path.
    pub path: String,
    /// Expected on-disk size after staging.
    pub size: u64,
    /// Kind of staging performed for this path.
    pub kind: StagingKind,
}

/// Completed staging result. `root` is the newly-created destination.
#[derive(Debug)]
pub struct PreparedStage {
    pub root: PathBuf,
    pub steps: Vec<StagingStep>,
    pub version_metadata: serde_json::Value,
    /// Files materialised by this module, in preparation order (deduplicated).
    pub staged: Vec<StagingFootprint>,
}

#[derive(Debug)]
pub enum StageError {
    Io(io::Error),
    Invalid(String),
    Cancelled,
}

impl From<io::Error> for StageError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Invalid(m) => write!(f, "invalid staging input: {m}"),
            Self::Cancelled => write!(f, "staging cancelled"),
        }
    }
}

impl std::error::Error for StageError {}

impl From<StagingPlanError> for StageError {
    fn from(e: StagingPlanError) -> Self {
        Self::Invalid(format!("plan rejected: {e}"))
    }
}

/// Removes exactly the newly-created stage root on drop unless disarmed.
struct StageGuard {
    root: PathBuf,
    armed: bool,
}

impl StageGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        if self.armed {
            // Only ever removes the tree we created with `create_dir`; a
            // pre-existing destination was refused before the guard existed.
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

/// Prepare a brand-new client staging directory.
///
/// `installer_sha256` is the caller's trusted identity for `installer_bytes`.
/// `resolve_library` maps a normalised Maven-relative path (without `libraries/`)
/// to a local cached library path; it is never consulted for the network.
/// `cancelled` is polled during copies, extraction and before completion.
#[allow(clippy::too_many_arguments)]
pub fn prepare_client_stage(
    destination: &Path,
    installer_bytes: &[u8],
    installer_sha256: &str,
    minecraft: &VerifiedInput,
    mut resolve_library: impl FnMut(&str) -> Result<PathBuf, StageError>,
    cancelled: impl Fn() -> bool,
) -> Result<PreparedStage, StageError> {
    if cancelled() {
        return Err(StageError::Cancelled);
    }
    verify_memory_sha256(installer_bytes, installer_sha256)?;
    let parsed = read_installer_archive(PILOT_GAME_VERSION, PILOT_LOADER_VERSION, installer_bytes)
        .map_err(|m| StageError::Invalid(format!("installer archive: {m}")))?;
    let steps = plan_client_staging(&parsed.profile)?;
    let plan = assemble_requirements(&parsed.profile, &steps, &parsed.resources)?;
    let index = read_index(installer_bytes, &ZipBudget::default())
        .map_err(|e| StageError::Invalid(format!("installer zip index: {e}")))?;
    let resource_entries = match_resource_entries(&plan.resources, &index)?;
    if !destination.is_absolute() {
        return Err(StageError::Invalid(
            "absolute staging destination required".into(),
        ));
    }
    reject_existing_destination(destination)?;
    check_ancestors(destination)?;
    check_source(&minecraft.path, "minecraft input")?;
    if minecraft.sha1.len() != 40 || !minecraft.sha1.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(StageError::Invalid(
            "invalid Minecraft SHA1 identity".into(),
        ));
    }
    let mut sources = PreparedSources::default();
    sources.push_source(
        MINECRAFT_INPUT,
        minecraft.path.clone(),
        minecraft.size,
        SourceHash::Sha1(minecraft.sha1.clone()),
    )?;
    for need in &plan.pinned {
        if cancelled() {
            return Err(StageError::Cancelled);
        }
        let raw_maven = need
            .rel
            .strip_prefix("libraries/")
            .ok_or_else(|| StageError::Invalid("library prefix missing".into()))?;
        let local = resolve_library(raw_maven)?;
        check_source(&local, "library input")?;
        sources.push_source(&need.rel, local, need.size, need.hash.clone())?;
    }
    if cancelled() {
        return Err(StageError::Cancelled);
    }
    fs::create_dir(destination)?;
    let mut guard = StageGuard {
        root: destination.to_path_buf(),
        armed: true,
    };
    let mut staged = Vec::new();
    write_bytes(destination, INSTALLER_INPUT, installer_bytes, &cancelled)?;
    staged.push(StagingFootprint {
        path: INSTALLER_INPUT.into(),
        size: installer_bytes.len() as u64,
        kind: StagingKind::Installer,
    });
    for source in &sources.sources {
        ensure_parent(destination, &source.rel)?;
        stream_copy(
            &source.local,
            &destination.join(&source.rel),
            source.size,
            &source.hash,
            &cancelled,
        )?;
        staged.push(StagingFootprint {
            path: source.rel.clone(),
            size: source.size,
            kind: if source.rel == MINECRAFT_INPUT {
                StagingKind::Minecraft
            } else if source.rel.ends_with(".zip") {
                StagingKind::McpData
            } else {
                StagingKind::Library
            },
        });
    }
    for (resource, entry) in plan.resources.iter().zip(resource_entries) {
        if cancelled() {
            return Err(StageError::Cancelled);
        }
        let data = read_entry(installer_bytes, &entry, &ZipBudget::default())
            .map_err(|e| StageError::Invalid(format!("resource extraction: {e}")))?;
        if data.len() as u64 != resource.size || hex(&Sha256::digest(&data)) != resource.sha256 {
            return Err(StageError::Invalid(
                "installer resource identity mismatch".into(),
            ));
        }
        let rel = format!("installer-resources/{}", resource.path);
        write_bytes(destination, &rel, &data, &cancelled)?;
        staged.push(StagingFootprint {
            path: rel,
            size: resource.size,
            kind: StagingKind::InstallerResource {
                sha256: resource.sha256.clone(),
            },
        });
    }
    if cancelled() {
        return Err(StageError::Cancelled);
    }
    guard.disarm();
    Ok(PreparedStage {
        root: destination.to_path_buf(),
        steps,
        version_metadata: parsed.version_metadata,
        staged,
    })
}

fn write_bytes(
    root: &Path,
    rel: &str,
    bytes: &[u8],
    cancelled: &impl Fn() -> bool,
) -> Result<(), StageError> {
    normalise_exact(rel)?;
    ensure_parent(root, rel)?;
    let mut file = create_new_file(&root.join(rel))?;
    for chunk in bytes.chunks(COPY_BUFFER) {
        if cancelled() {
            return Err(StageError::Cancelled);
        }
        file.write_all(chunk)?;
    }
    file.sync_all()?;
    Ok(())
}

#[derive(Debug, Clone)]
enum SourceHash {
    Sha1(String),
    Sha256(String),
}

#[derive(Debug, Default)]
struct PreparedSources {
    sources: Vec<PlannedSource>,
}

#[derive(Debug, Clone)]
struct PlannedSource {
    rel: String,
    local: PathBuf,
    size: u64,
    hash: SourceHash,
}

impl PreparedSources {
    fn push_source(
        &mut self,
        rel: &str,
        local: PathBuf,
        size: u64,
        hash: SourceHash,
    ) -> Result<(), StageError> {
        let rel = normalise_exact(rel)?;
        self.sources.push(PlannedSource {
            rel,
            local,
            size,
            hash,
        });
        Ok(())
    }
}

#[derive(Debug, Default)]
struct StagePlan {
    pinned: Vec<PinnedNeed>,
    resources: Vec<InstallerResource>,
}

#[derive(Debug)]
struct PinnedNeed {
    rel: String,
    hash: SourceHash,
    size: u64,
}

fn assemble_requirements(
    profile: &NeoForgeInstallerProfile,
    steps: &[StagingStep],
    resources: &[InstallerResource],
) -> Result<StagePlan, StageError> {
    let mut plan = StagePlan::default();
    let mut paths = BTreeSet::new();
    for pin in ARTIFACTS {
        let rel = normalise_exact(&format!("libraries/{}", pin.path))?;
        paths.insert(rel.clone());
        plan.pinned.push(PinnedNeed {
            rel,
            hash: SourceHash::Sha256(pin.sha256.into()),
            size: pin.size,
        });
    }
    for step in steps {
        if step.operation == super::neoforge_profile::ProcessorOperation::McpData {
            let input = step
                .args
                .windows(2)
                .find_map(|pair| match (&pair[0], &pair[1]) {
                    (StagingArgument::Literal(option), StagingArgument::StagedPath(path))
                        if option == "--input" =>
                    {
                        Some(path)
                    }
                    _ => None,
                })
                .ok_or_else(|| StageError::Invalid("MCP data input missing".into()))?;
            if paths.insert(input.clone()) {
                let raw = input
                    .strip_prefix("libraries/")
                    .ok_or_else(|| StageError::Invalid("MCP data path prefix".into()))?;
                let mut matching = profile
                    .libraries
                    .iter()
                    .filter(|library| library.path == raw);
                let library = matching
                    .next()
                    .ok_or_else(|| StageError::Invalid("MCP data identity missing".into()))?;
                if matching.next().is_some()
                    || maven_path(&library.coordinate).ok().as_deref() != Some(raw)
                {
                    return Err(StageError::Invalid("MCP data identity ambiguous".into()));
                }
                normalise_exact(input)?;
                plan.pinned.push(PinnedNeed {
                    rel: input.clone(),
                    hash: SourceHash::Sha1(library.sha1.clone()),
                    size: library.size,
                });
            }
        }
    }
    let needed: BTreeSet<_> = steps
        .iter()
        .flat_map(|step| &step.args)
        .filter_map(|arg| match arg {
            StagingArgument::StagedPath(path) if path.starts_with(RESOURCE_PREFIX) => {
                path.strip_prefix("installer-resources/")
            }
            _ => None,
        })
        .collect();
    for path in needed {
        normalise_exact(path)?;
        let mut matches = resources.iter().filter(|resource| resource.path == path);
        let resource = matches
            .next()
            .ok_or_else(|| StageError::Invalid("installer resource missing".into()))?;
        if matches.next().is_some() {
            return Err(StageError::Invalid("installer resource duplicated".into()));
        }
        plan.resources.push(resource.clone());
    }
    Ok(plan)
}

fn match_resource_entries(
    resources: &[InstallerResource],
    index: &ZipIndex,
) -> Result<Vec<ZipEntry>, StageError> {
    resources
        .iter()
        .map(|resource| {
            let mut entries = index
                .entries
                .iter()
                .filter(|entry| entry.name == resource.path);
            let entry = entries
                .next()
                .ok_or_else(|| StageError::Invalid("ZIP resource missing".into()))?;
            if entries.next().is_some() {
                return Err(StageError::Invalid("ZIP resource duplicated".into()));
            }
            Ok(entry.clone())
        })
        .collect()
}

fn verify_memory_sha256(bytes: &[u8], expected: &str) -> Result<(), StageError> {
    let expected = expected.trim().to_ascii_lowercase();
    if expected.len() != 64 {
        return Err(StageError::Invalid(
            "installer sha256 not 64 hex chars".into(),
        ));
    }
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let got = hex(&hasher.finalize());
    if got != expected {
        return Err(StageError::Invalid("installer sha256 mismatch".into()));
    }
    Ok(())
}

fn reject_existing_destination(destination: &Path) -> Result<(), StageError> {
    match fs::symlink_metadata(destination) {
        Ok(_) => Err(StageError::Invalid(
            "destination already exists; refusing to overwrite".into(),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StageError::Io(e)),
    }
}

fn check_source(path: &Path, what: &str) -> Result<(), StageError> {
    check_ancestors(path)?;
    let meta = fs::symlink_metadata(path)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Err(StageError::Invalid(format!("{what} is a symlink")));
    }
    if !ft.is_file() {
        return Err(StageError::Invalid(format!("{what} is not a regular file")));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(StageError::Invalid(format!("{what} is a reparse point")));
        }
    }
    Ok(())
}

fn check_ancestors(destination: &Path) -> Result<(), StageError> {
    let parent = destination
        .parent()
        .ok_or_else(|| StageError::Invalid("destination has no parent".into()))?;
    for ancestor in parent.ancestors() {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        let meta = fs::symlink_metadata(ancestor)?;
        let ft = meta.file_type();
        if !ft.is_dir() || ft.is_symlink() {
            return Err(StageError::Invalid(format!(
                "destination ancestor is a symlink: {}",
                ancestor.display()
            )));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(StageError::Invalid(format!(
                    "destination ancestor is a reparse point: {}",
                    ancestor.display()
                )));
            }
        }
    }
    Ok(())
}

fn normalise_exact(raw: &str) -> Result<String, StageError> {
    let normalised = normalise_content_path(raw)
        .map_err(|e| StageError::Invalid(format!("unsafe path {raw}: {e}")))?;
    if normalised != raw {
        return Err(StageError::Invalid(format!(
            "path not in exact normalised form: {raw}"
        )));
    }
    Ok(normalised)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn create_new_file(path: &Path) -> Result<File, StageError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(StageError::Io)
}

fn ensure_parent(root: &Path, rel: &str) -> Result<(), StageError> {
    normalise_exact(rel)?;

    let parent = Path::new(rel)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut current = root.to_path_buf();
    for component in parent.components() {
        current.push(component.as_os_str());
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                check_ancestors(&current.join("probe"))?;
            }
            Err(e) => return Err(StageError::Io(e)),
        }
    }
    Ok(())
}

/// Stream `source` into `dest` writing exactly `expected_size` bytes, verifying
/// `hash` while copying. Refuses size overflow/shortfall and never overwrites.
fn stream_copy(
    source: &Path,
    dest: &Path,
    expected_size: u64,
    hash: &SourceHash,
    cancelled: &impl Fn() -> bool,
) -> Result<(), StageError> {
    check_source(source, "copy source")?;
    let mut input = File::open(source)?;
    let mut output = create_new_file(dest)?;
    let mut buf = vec![0u8; COPY_BUFFER];
    let mut total: u64 = 0;
    let mut sha256 = Sha256::new();
    let mut sha1 = Sha1::new();
    loop {
        if cancelled() {
            return Err(StageError::Cancelled);
        }
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .ok_or_else(|| StageError::Invalid("source size overflow".into()))?;
        if total > expected_size {
            return Err(StageError::Invalid(
                "source larger than expected size".into(),
            ));
        }
        match hash {
            SourceHash::Sha256(_) => sha256.update(&buf[..n]),
            SourceHash::Sha1(_) => sha1.update(&buf[..n]),
        }
        output.write_all(&buf[..n])?;
    }
    output.flush()?;
    if total != expected_size {
        return Err(StageError::Invalid(format!(
            "source size mismatch: expected {expected_size}, got {total}"
        )));
    }
    let got = match hash {
        SourceHash::Sha256(expected) => {
            let got = hex(&sha256.finalize());
            if !got.eq_ignore_ascii_case(expected) {
                return Err(StageError::Invalid("source sha256 mismatch".into()));
            }
            got
        }
        SourceHash::Sha1(expected) => {
            let got = hex(&sha1.finalize());
            if !got.eq_ignore_ascii_case(expected) {
                return Err(StageError::Invalid("source sha1 mismatch".into()));
            }
            got
        }
    };
    drop(got);
    output.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_root(tag: &str) -> PathBuf {
        let base = std::env::var_os("SHTMCL_NEOFORGE_STAGE_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "shtmcl-stage-{}-{}-{}",
                tag,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::create_dir(&base).unwrap();
        base
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        let mut h = Sha1::new();
        h.update(bytes);
        hex(&h.finalize())
    }

    #[test]
    fn stream_copy_verifies_and_rejects_tamper() {
        let base = temp_root("copy");
        let src = base.join("src.bin");
        let data: Vec<u8> = (0..(COPY_BUFFER * 2 + 7))
            .map(|i| (i % 251) as u8)
            .collect();
        write(&src, &data);
        let dst = base.join("out.bin");
        let ok = SourceHash::Sha1(sha1_hex(&data));
        stream_copy(&src, &dst, data.len() as u64, &ok, &|| false).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), data);

        let tampered = base.join("bad.bin");
        let bad = SourceHash::Sha1("00".repeat(20));
        match stream_copy(&src, &tampered, data.len() as u64, &bad, &|| false) {
            Err(StageError::Invalid(m)) => assert!(m.contains("sha1 mismatch")),
            other => panic!("expected tamper rejection: {other:?}"),
        }
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn stream_copy_rejects_size_mismatch_both_ways() {
        let base = temp_root("size");
        let src = base.join("src.bin");
        write(&src, b"short");
        let h = SourceHash::Sha256("00".repeat(32));
        let out = base.join("short.out");
        match stream_copy(&src, &out, 4, &h, &|| false) {
            Err(StageError::Invalid(m)) => assert!(m.contains("larger than expected")),
            other => panic!("expected oversize rejection: {other:?}"),
        }
        let out2 = base.join("big.out");
        match stream_copy(&src, &out2, 50, &h, &|| false) {
            Err(StageError::Invalid(m)) => assert!(m.contains("size mismatch")),
            other => panic!("expected undersize rejection: {other:?}"),
        }
        assert!(create_new_file(&out2).is_err() || fs::remove_file(&out2).is_ok());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cancellation_mid_copy_errors() {
        let base = temp_root("cancel");
        let src = base.join("src.bin");
        write(&src, &vec![7u8; COPY_BUFFER * 3]);
        let count = AtomicUsize::new(0);
        let cancel = || count.fetch_add(1, Ordering::SeqCst) >= 1;
        let out = base.join("out.bin");
        let h = SourceHash::Sha256("00".repeat(32));
        match stream_copy(&src, &out, COPY_BUFFER as u64 * 3, &h, &cancel) {
            Err(StageError::Cancelled) => {}
            other => panic!("expected cancellation: {other:?}"),
        }
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn existing_destination_is_preserved_and_guard_is_not_created() {
        let base = temp_root("existing");
        let dest = base.join("stage");
        fs::create_dir(&dest).unwrap();
        write(&dest.join("keep.txt"), b"keep");
        let err = prepare_client_stage(
            &dest,
            b"not a jar",
            &"00".repeat(32),
            &VerifiedInput {
                path: base.join("missing.jar"),
                size: 0,
                sha1: "00".repeat(20),
            },
            |_| Err(StageError::Invalid("no network".into())),
            || false,
        )
        .unwrap_err();
        assert!(matches!(err, StageError::Invalid(_)));
        assert!(dest.join("keep.txt").exists());
        assert_eq!(fs::read(dest.join("keep.txt")).unwrap(), b"keep");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn bad_input_leaves_no_root() {
        let base = temp_root("badinput");
        let dest = base.join("fresh-stage");
        let err = prepare_client_stage(
            &dest,
            b"definitely not a zip",
            &"ab".repeat(32),
            &VerifiedInput {
                path: base.join("missing.jar"),
                size: 0,
                sha1: "00".repeat(20),
            },
            |_| Err(StageError::Invalid("no network".into())),
            || false,
        )
        .unwrap_err();
        assert!(matches!(err, StageError::Invalid(_) | StageError::Io(_)));
        assert!(!dest.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn guard_removes_only_created_tree() {
        let base = temp_root("guard");
        let dest = base.join("created");
        fs::create_dir(&dest).unwrap();
        {
            let _guard = StageGuard {
                root: dest.clone(),
                armed: true,
            };
        }
        assert!(!dest.exists());
        assert!(base.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn normalise_exact_rejects_and_accepts() {
        assert!(normalise_exact("a/b.jar").is_ok());
        for raw in ["a//b", "a/./b", "../a", "/a", "a\\b", ""] {
            assert!(normalise_exact(raw).is_err(), "{raw}");
        }
    }

    /// Real local inputs; no Java execution or network requests in this test.
    #[test]
    #[ignore = "requires captured installer, Minecraft jar and libraries"]
    fn captured_full_staging() {
        let input = |name| PathBuf::from(std::env::var_os(name).expect(name));
        let installer = input("SHTMCL_NEOFORGE_INSTALLER");
        let mc = input("SHTMCL_MINECRAFT_JAR");
        let artifacts = input("SHTMCL_NEOFORGE_ARTIFACTS");
        let extra = input("SHTMCL_NEOFORGE_EXTRA_ARTIFACTS");
        let expected = std::env::var("SHTMCL_NEOFORGE_INSTALLER_SHA256").unwrap();
        let minecraft = VerifiedInput {
            path: mc.clone(),
            size: fs::metadata(&mc).unwrap().len(),
            sha1: std::env::var("SHTMCL_MINECRAFT_JAR_SHA1").unwrap(),
        };
        let bytes = fs::read(&installer).unwrap();
        let resolver = |rel: &str| {
            let p = artifacts.join(rel);
            Ok(if p.is_file() { p } else { extra.join(rel) })
        };
        let base = temp_root("captured");
        let dest = base.join("stage");
        let stage =
            prepare_client_stage(&dest, &bytes, &expected, &minecraft, resolver, || false).unwrap();
        assert_eq!(stage.steps.len(), 6);
        assert_eq!(stage.staged.len(), 37);
        assert_eq!(stage.version_metadata["id"], "neoforge-20.4.253");
        for file in &stage.staged {
            assert_eq!(
                fs::metadata(stage.root.join(&file.path)).unwrap().len(),
                file.size
            );
        }
        assert!(dest.join("installer-resources/data/client.lzma").is_file());
        assert!(!dest.join("installer-resources/data/server.lzma").exists());
        // Existing destination check reaches the staging branch with valid inputs.
        fs::write(dest.join("keep.txt"), b"keep").unwrap();
        assert!(
            prepare_client_stage(&dest, &bytes, &expected, &minecraft, resolver, || false).is_err()
        );
        assert_eq!(fs::read(dest.join("keep.txt")).unwrap(), b"keep");
        let cancelled_dest = base.join("cancelled");
        let should_cancel = || {
            fs::metadata(cancelled_dest.join(INSTALLER_INPUT))
                .is_ok_and(|m| m.len() > COPY_BUFFER as u64)
        };
        assert!(matches!(
            prepare_client_stage(
                &cancelled_dest,
                &bytes,
                &expected,
                &minecraft,
                resolver,
                should_cancel
            ),
            Err(StageError::Cancelled)
        ));
        assert!(!cancelled_dest.exists());
        let bad_dest = base.join("bad-dependency");
        // Feed real but wrong bytes as the first dependency: hash/size verification must reject and remove the stage.
        assert!(prepare_client_stage(
            &bad_dest,
            &bytes,
            &expected,
            &minecraft,
            |_| Ok(mc.clone()),
            || false
        )
        .is_err());
        assert!(!bad_dest.exists());
        println!("stage={} files={} steps={} cancellation_cleanup=true wrong_dependency_cleanup=true existing_preserved=true",dest.display(),stage.staged.len(),stage.steps.len());
        // Keep successful staged files only when an explicit evidence root was supplied.
        if std::env::var_os("SHTMCL_NEOFORGE_STAGE_TEST_ROOT").is_none() {
            fs::remove_dir_all(base).unwrap();
        }
    }
}

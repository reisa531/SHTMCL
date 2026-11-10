//! NeoForge prerequisite installation transaction coordinator.
//!
//! Scope boundary: this coordinates preparation of the local NeoForge runtime
//! prerequisite (stage + mappings + ordered client steps + runtime libraries).
//! It is NOT a full game/instance install. No network, scheduler, DTO, or
//! instance writes happen here; the caller's exclusive directory lease applies
//! throughout.
//!
//! Ownership: only the brand-new stage root returned by `prepare_client_stage`
//! is mutated. Partial journals (`root/pipeline-journal/`) are evidence only;
//! there is no automatic resume/reuse. A caller must restart into a fresh
//! attempt directory. `launchReady` is always false in the completion marker.
//!
//! If a runner step reports `StopFailed` nothing is removed: the process may
//! still be alive and the whole root is retained for diagnostics.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use shtmcl_platform::{ProcessLauncher, ProcessSupervisor};

use super::neoforge_mappings::{parse_verified_metadata, MappingsError};
use super::neoforge_profile::read_installer_archive;
use super::neoforge_runner::{
    bind, execute_client_step, regular_file, RunnerError, StepLogs, StepOutput,
};
use super::neoforge_runtime::plan_runtime;
use super::neoforge_stage::{prepare_client_stage, StageError, VerifiedInput};

const MAX_BASE_METADATA_BYTES: u64 = 8 * 1024 * 1024;
const MAX_INSTALLER_BYTES: u64 = 64 * 1024 * 1024;
const GAME_VERSION: &str = "1.20.4";
const LOADER_VERSION: &str = "20.4.253";
const JOURNAL_DIR: &str = "pipeline-journal";
const MARKER_FILE: &str = "neoforge-complete.json";
const METADATA_RELATIVE: &str = "versions/1.20.4/1.20.4.json";
const STEP_ORDER: [(usize, &str); 6] = [
    (3, "processor-3"),
    (4, "mappings"),
    (5, "processor-5"),
    (6, "processor-6"),
    (8, "processor-8"),
    (9, "processor-9"),
];
const COPY_CHUNK: usize = 64 * 1024;

/// Inputs required to coordinate a local NeoForge client prerequisite install.
pub struct ClientInstallInputs<'a> {
    pub destination: &'a Path,
    pub installer_bytes: &'a [u8],
    pub installer_sha256: &'a str,
    pub base_metadata_bytes: &'a [u8],
    pub base_metadata_sha1: &'a str,
    pub minecraft: &'a VerifiedInput,
    pub java: &'a Path,
    pub cached_mappings: &'a Path,
}

/// Result of a successful coordination run.
pub struct CompletedClientStage {
    pub root: PathBuf,
    pub metadata: Value,
    pub manifest_path: PathBuf,
}

/// Errors returned by the prerequisite pipeline.
#[derive(Debug)]
pub enum PipelineError {
    Io(io::Error),
    Invalid(String),
    Cancelled,
    StopFailed,
    Runner(RunnerError),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineError::Io(e) => write!(f, "pipeline io error: {e}"),
            PipelineError::Invalid(m) => write!(f, "pipeline invalid input: {m}"),
            PipelineError::Cancelled => write!(f, "pipeline cancelled"),
            PipelineError::StopFailed => {
                write!(f, "pipeline stop failed; process may still be alive")
            }
            PipelineError::Runner(RunnerError::Process { code, .. }) => {
                write!(f, "pipeline processor exited with code {code}")
            }
            PipelineError::Runner(e) => write!(f, "pipeline runner error: {e}"),
        }
    }
}

impl Error for PipelineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            PipelineError::Io(e) => Some(e),
            PipelineError::Runner(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for PipelineError {
    fn from(e: io::Error) -> Self {
        PipelineError::Io(e)
    }
}

impl From<RunnerError> for PipelineError {
    fn from(e: RunnerError) -> Self {
        match e {
            RunnerError::Cancelled => PipelineError::Cancelled,
            RunnerError::StopFailed => PipelineError::StopFailed,
            other => PipelineError::Runner(other),
        }
    }
}

fn stage_error_to(err: StageError) -> PipelineError {
    match err {
        StageError::Cancelled => PipelineError::Cancelled,
        StageError::Io(e) => PipelineError::Io(e),
        StageError::Invalid(m) => PipelineError::Invalid(m),
    }
}

fn mappings_error_to(err: MappingsError) -> PipelineError {
    match err {
        MappingsError::Cancelled => PipelineError::Cancelled,
        MappingsError::Io(e) => PipelineError::Io(e),
        MappingsError::Invalid(m) => PipelineError::Invalid(m),
        MappingsError::Path(e) => PipelineError::from(e),
    }
}

fn hex_lower_valid(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(bytes);
    hex_encode(&h.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex_encode(&h.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.as_bytes()
            .iter()
            .zip(b.as_bytes())
            .all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Lightweight preflight: base metadata and Minecraft input identity agree,
/// before any mutation.
fn preflight(inputs: &ClientInstallInputs<'_>) -> Result<Value, PipelineError> {
    if !hex_lower_valid(inputs.base_metadata_sha1, 40) {
        return Err(PipelineError::Invalid(
            "metadata sha1 must be 40 hex chars".into(),
        ));
    }
    let bytes = inputs.base_metadata_bytes;
    if bytes.len() as u64 > MAX_BASE_METADATA_BYTES {
        return Err(PipelineError::Invalid("metadata too large".into()));
    }
    if !eq_ignore_case(&sha1_hex(bytes), inputs.base_metadata_sha1) {
        return Err(PipelineError::Invalid("metadata sha1 mismatch".into()));
    }
    let base: Value = serde_json::from_slice(bytes)
        .map_err(|e| PipelineError::Invalid(format!("metadata json: {e}")))?;
    let id = base.get("id").and_then(Value::as_str).unwrap_or("");
    if id != GAME_VERSION {
        return Err(PipelineError::Invalid(format!(
            "metadata id {id:?} != {GAME_VERSION}"
        )));
    }
    let client = base
        .pointer("/downloads/client")
        .ok_or_else(|| PipelineError::Invalid("missing downloads.client".into()))?;
    let want_sha1 = client.get("sha1").and_then(Value::as_str).unwrap_or("");
    let want_size = client.get("size").and_then(Value::as_u64);
    if !hex_lower_valid(want_sha1, 40) {
        return Err(PipelineError::Invalid(
            "downloads.client.sha1 invalid".into(),
        ));
    }
    match want_size {
        Some(sz) if sz == inputs.minecraft.size => {}
        _ => {
            return Err(PipelineError::Invalid(
                "minecraft size != downloads.client.size".into(),
            ))
        }
    }
    if !eq_ignore_case(want_sha1, &inputs.minecraft.sha1) {
        return Err(PipelineError::Invalid(
            "minecraft sha1 != downloads.client.sha1".into(),
        ));
    }
    Ok(base)
}

fn verify_installer_and_plan(
    inputs: &ClientInstallInputs<'_>,
    base: &Value,
) -> Result<super::neoforge_runtime::RuntimePlan, PipelineError> {
    if !hex_lower_valid(inputs.installer_sha256, 64) {
        return Err(PipelineError::Invalid(
            "installer sha256 must be 64 hex".into(),
        ));
    }
    if inputs.installer_bytes.len() as u64 > MAX_INSTALLER_BYTES {
        return Err(PipelineError::Invalid("installer too large".into()));
    }
    if !eq_ignore_case(inputs.installer_sha256, inputs.installer_sha256) {
        return Err(PipelineError::Invalid("installer sha256 mismatch".into()));
    }
    let parsed = read_installer_archive(GAME_VERSION, LOADER_VERSION, inputs.installer_bytes)
        .map_err(|e| PipelineError::Invalid(format!("installer archive: {e}")))?;
    plan_runtime(&parsed.profile, &parsed.version_metadata, base)
        .map_err(|e| PipelineError::Invalid(format!("runtime plan: {e}")))
}

// ---------------------------------------------------------------------------
// Journal
// ---------------------------------------------------------------------------

struct Journal {
    dir: PathBuf,
    sequence: u32,
    installer_sha256: String,
    metadata_sha1: String,
}

impl Journal {
    fn open(root: &Path, inputs: &ClientInstallInputs) -> Result<Self, PipelineError> {
        let dir = bind(root, JOURNAL_DIR)?;
        fs::create_dir(&dir)?;
        Ok(Journal {
            dir,
            sequence: 0,
            installer_sha256: inputs.installer_sha256.to_ascii_lowercase(),
            metadata_sha1: inputs.base_metadata_sha1.to_ascii_lowercase(),
        })
    }

    fn append(&mut self, phase: &str, status: &str, extra: Value) -> Result<(), PipelineError> {
        let seq = self.sequence;
        self.sequence = self.sequence.checked_add(1).ok_or_else(|| {
            PipelineError::Io(io::Error::new(
                io::ErrorKind::Other,
                "journal sequence overflow",
            ))
        })?;
        let mut obj = serde_json::Map::new();
        obj.insert("sequence".into(), json!(seq));
        obj.insert("inputInstallerSha256".into(), json!(self.installer_sha256));
        obj.insert("inputMetadataSha1".into(), json!(self.metadata_sha1));
        obj.insert("phase".into(), json!(phase));
        obj.insert("status".into(), json!(status));
        if let Value::Object(m) = extra {
            for (k, v) in m {
                obj.insert(k, v);
            }
        }
        let bytes = serde_json::to_vec(&Value::Object(obj))
            .map_err(|e| PipelineError::Invalid(format!("journal encode: {e}")))?;
        let name = format!("{seq:06}.json");
        let path = bind(&self.dir, &name)?;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;

        Ok(())
    }

    fn step_logs(&self, index: usize, logs: &StepLogs) -> Result<Value, PipelineError> {
        let so = self.write_log(index, "stdout", &logs.stdout.bytes, logs.stdout.truncated)?;
        let se = self.write_log(index, "stderr", &logs.stderr.bytes, logs.stderr.truncated)?;
        Ok(
            json!({ "stdoutSha256": so.0, "stdoutSize": so.1, "stdoutTruncated": logs.stdout.truncated,
                   "stderrSha256": se.0, "stderrSize": se.1, "stderrTruncated": logs.stderr.truncated }),
        )
    }

    fn write_log(
        &self,
        index: usize,
        kind: &str,
        bytes: &[u8],
        _truncated: bool,
    ) -> Result<(String, u64), PipelineError> {
        let name = format!("step-{index}-{kind}.log");
        let path = bind(&self.dir, &name)?;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok((sha256_hex(bytes), bytes.len() as u64))
    }
}

fn outputs_json(root: &Path, outputs: &[StepOutput]) -> Result<Value, PipelineError> {
    let arr = outputs
        .iter()
        .map(|o| {
            let relative = o.path.strip_prefix(root)
                .map_err(|_| PipelineError::Invalid("processor output outside staging".into()))?;
            Ok(json!({"path":relative.to_string_lossy().replace('\\', "/"),"size":o.size,"sha256":o.sha256}))
        })
        .collect::<Result<Vec<_>, PipelineError>>()?;
    Ok(Value::Array(arr))
}

fn terminal_error_status(err: &PipelineError) -> &'static str {
    match err {
        PipelineError::StopFailed => "quarantined",
        PipelineError::Cancelled => "cancelled",
        _ => "failed",
    }
}

// ---------------------------------------------------------------------------
// Stage execution
// ---------------------------------------------------------------------------

struct StageOutcome {
    copied: Vec<Value>,
    generated: Vec<(String, u64, String)>,
    metadata: Value,
    metadata_sha256: String,
}

fn run_prepared(
    journal: &mut Journal,
    prepared: &super::neoforge_stage::PreparedStage,
    inputs: &ClientInstallInputs<'_>,
    plan: &super::neoforge_runtime::RuntimePlan,
    resolve_library: &mut impl FnMut(&str) -> Result<PathBuf, StageError>,
    launcher: &dyn ProcessLauncher,
    supervisor: &dyn ProcessSupervisor,
    timeout: Duration,
    cancelled: &impl Fn() -> bool,
    progress: &mut impl FnMut(&str),
) -> Result<StageOutcome, PipelineError> {
    let root = prepared.root.clone();
    let mut step_outputs: Vec<StepOutput> = Vec::new();

    for &(index, phase) in STEP_ORDER.iter() {
        if cancelled() {
            return Err(PipelineError::Cancelled);
        }
        journal.append(phase, "running", json!({ "index": index }))?;
        progress(phase);
        if index == 4 {
            parse_verified_metadata(GAME_VERSION, inputs.base_metadata_bytes)
                .map_err(mappings_error_to)?
                .stage_cached(&root, inputs.cached_mappings, || cancelled())
                .map_err(mappings_error_to)?;
        } else {
            let result = match execute_client_step(
                &root,
                inputs.installer_sha256,
                index,
                inputs.java,
                launcher,
                supervisor,
                timeout,
                || cancelled(),
            ) {
                Ok(result) => result,
                Err(RunnerError::Process { code, logs }) => {
                    journal.step_logs(index, &logs)?;
                    return Err(PipelineError::Runner(RunnerError::Process { code, logs }));
                }
                Err(error) => return Err(error.into()),
            };
            let logs = journal.step_logs(index, &result.logs)?;
            let outs = result.outputs;
            let mut extra = serde_json::Map::new();
            extra.insert("outputs".into(), outputs_json(&root, &outs)?);
            if let Value::Object(m) = logs {
                for (k, v) in m {
                    extra.insert(k, v);
                }
            }
            journal.append(phase, "completed", Value::Object(extra))?;
            step_outputs.extend(outs);
            continue;
        }
        let mappings = parse_verified_metadata(GAME_VERSION, inputs.base_metadata_bytes)
            .map_err(mappings_error_to)?;
        journal.append(phase, "completed", json!({"path":super::neoforge_mappings::MAPPINGS_PATH,"sha1":mappings.sha1(),"size":mappings.size()}))?;
    }

    // Copy 50 runtime libraries from resolver.
    journal.append(
        "runtime",
        "running",
        json!({ "count": plan.downloads.len() }),
    )?;
    progress("runtime");
    let mut copied: Vec<Value> = Vec::new();
    for dl in &plan.downloads {
        if cancelled() {
            return Err(PipelineError::Cancelled);
        }
        let src = resolve_library(&dl.path).map_err(stage_error_to)?;
        let rel = format!("libraries/{}", dl.path);
        let dest = bind(&root, &rel)?;
        let (sha1, size) = copy_one(&src, &dest, &dl.sha1, dl.size, cancelled)?;
        copied.push(json!({ "path": rel, "sha1": sha1, "size": size }));
    }
    journal.append("runtime", "completed", json!({ "files": copied }))?;

    // Verify the three generated paths against runner receipts.
    journal.append("verify-generated", "running", json!({}))?;
    progress("verify-generated");
    let mut expected: Vec<(&str, u64, &str)> = Vec::new();
    for path in &plan.generated_paths {
        let needle = path.trim_start_matches('/');
        let rec = step_outputs.iter().find(|o| o.path == root.join(needle));
        match rec {
            Some(o) => expected.push((path.as_str(), o.size, o.sha256.as_str())),
            None => {
                return Err(PipelineError::Invalid(format!(
                    "no runner receipt for generated path {path}"
                )))
            }
        }
    }
    let mut generated: Vec<(String, u64, String)> = Vec::new();
    for (rel, size, sha) in expected {
        if cancelled() {
            return Err(PipelineError::Cancelled);
        }
        let abs = bind(&root, rel)?;
        let actual_size = regular_file_size(&abs)?;
        if actual_size == 0 || actual_size != size {
            return Err(PipelineError::Invalid(format!(
                "generated size mismatch for {rel}"
            )));
        }
        let hash = sha256_file(&abs, cancelled)?;
        if !eq_ignore_case(&hash, sha) {
            return Err(PipelineError::Invalid(format!(
                "generated sha256 mismatch for {rel}"
            )));
        }
        generated.push((rel.trim_start_matches('/').to_string(), actual_size, hash));
    }

    journal.append("verify-generated", "completed", json!({"files": generated.iter().map(|(p,n,h)|json!({"path":p,"size":n,"sha256":h})).collect::<Vec<_>>()}))?;

    // Save launch metadata.
    journal.append("metadata", "running", json!({}))?;
    progress("metadata");
    let meta_path = bind(&root, METADATA_RELATIVE)?;
    if let Some(parent) = meta_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let meta_bytes = serde_json::to_vec(&plan.metadata)
        .map_err(|e| PipelineError::Invalid(format!("metadata encode: {e}")))?;
    let meta_path = bind(&root, METADATA_RELATIVE)?;
    let mut mf = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&meta_path)?;
    mf.write_all(&meta_bytes)?;
    mf.sync_all()?;
    drop(mf);
    let metadata_sha256 = sha256_hex(&meta_bytes);
    journal.append(
        "metadata",
        "completed",
        json!({ "metadataPath": METADATA_RELATIVE, "sha256": metadata_sha256 }),
    )?;

    Ok(StageOutcome {
        copied,
        generated,
        metadata: plan.metadata.clone(),
        metadata_sha256,
    })
}

fn copy_one(
    src: &Path,
    dest: &Path,
    want_sha1: &str,
    want_size: u64,
    cancelled: &impl Fn() -> bool,
) -> Result<(String, u64), PipelineError> {
    let src_size = regular_file_size(src)?;
    if src_size != want_size {
        return Err(PipelineError::Invalid(format!(
            "runtime lib size mismatch for {src:?}"
        )));
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    // Existing destination: verify against expected, reuse else reject.
    if dest.symlink_metadata().is_ok() {
        let sz = regular_file_size(dest)?;
        if sz != want_size {
            return Err(PipelineError::Invalid(format!(
                "existing dest size mismatch {dest:?}"
            )));
        }
        let h = sha1_file(dest, cancelled)?;
        if !eq_ignore_case(&h, want_sha1) {
            return Err(PipelineError::Invalid(format!(
                "existing dest sha1 mismatch {dest:?}"
            )));
        }
        return Ok((h, sz));
    }
    let mut input = fs::File::open(src)?;
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    let mut hasher = Sha1::new();
    let mut total: u64 = 0;
    let mut buf = vec![0u8; COPY_CHUNK];
    let result = (|| -> io::Result<()> {
        loop {
            if cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if total + n as u64 > want_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "runtime input grew",
                ));
            }
            out.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
            total += n as u64;
        }
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        out.sync_all()
    })();
    drop(out);
    match result {
        Ok(()) => {
            let digest = hex_encode(&hasher.finalize());
            if total != want_size || !eq_ignore_case(&digest, want_sha1) {
                let _ = fs::remove_file(dest);
                return Err(PipelineError::Invalid(format!(
                    "runtime lib hash mismatch {src:?}"
                )));
            }
            Ok((digest, total))
        }
        Err(e) if e.kind() == io::ErrorKind::Interrupted => {
            let _ = fs::remove_file(dest);
            Err(PipelineError::Cancelled)
        }
        Err(e) => {
            let _ = fs::remove_file(dest);
            Err(PipelineError::Io(e))
        }
    }
}

fn regular_file_size(path: &Path) -> Result<u64, PipelineError> {
    let size = regular_file(path)?;
    if size > 512 * 1024 * 1024 {
        return Err(PipelineError::Invalid("file budget exceeded".into()));
    }
    Ok(size)
}
fn sha1_file(path: &Path, cancelled: &impl Fn() -> bool) -> Result<String, PipelineError> {
    hash_file::<Sha1>(path, cancelled)
}
fn sha256_file(path: &Path, cancelled: &impl Fn() -> bool) -> Result<String, PipelineError> {
    hash_file::<Sha256>(path, cancelled)
}
fn hash_file<D: Digest>(
    path: &Path,
    cancelled: &impl Fn() -> bool,
) -> Result<String, PipelineError> {
    let size = regular_file_size(path)?;
    let mut file = fs::File::open(path)?;
    let mut hash = D::new();
    let mut buf = [0u8; COPY_CHUNK];
    let mut total = 0;
    loop {
        if cancelled() {
            return Err(PipelineError::Cancelled);
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > size {
            return Err(PipelineError::Invalid("input grew during hash".into()));
        }
        hash.update(&buf[..n]);
    }
    if total != size {
        return Err(PipelineError::Invalid("input changed during hash".into()));
    }
    Ok(hex_encode(&hash.finalize()))
}

fn write_marker(
    root: &Path,
    outcome: &StageOutcome,
    inputs: &ClientInstallInputs<'_>,
    cancelled: &impl Fn() -> bool,
) -> Result<PathBuf, PipelineError> {
    let mut files: Vec<Value> = Vec::new();
    for c in &outcome.copied {
        files.push(c.clone());
    }
    for (rel, size, sha) in &outcome.generated {
        files.push(json!({ "path": rel, "size": size, "sha256": sha }));
    }
    let marker = json!({
        "schemaVersion": 1,
        "gameVersion": GAME_VERSION,
        "loaderVersion": LOADER_VERSION,
        "installerSha256": inputs.installer_sha256,
        "metadataSha1": inputs.base_metadata_sha1,
        "files": files,
        "metadataPath": METADATA_RELATIVE,
        "metadataSha256": outcome.metadata_sha256,
        "launchReady": false,
    });
    let bytes = serde_json::to_vec(&marker)
        .map_err(|e| PipelineError::Invalid(format!("marker encode: {e}")))?;
    let tmp = bind(root, "neoforge-complete.json.tmp")?;
    let final_path = bind(root, MARKER_FILE)?;
    if final_path.symlink_metadata().is_ok() {
        return Err(PipelineError::Invalid(
            "completion marker already exists".into(),
        ));
    }
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    if cancelled() {
        return Err(PipelineError::Cancelled);
    }
    fs::rename(&tmp, &final_path)?;
    Ok(final_path)
}

/// Coordinate a local NeoForge prerequisite installation transaction.
pub fn install_client(
    inputs: ClientInstallInputs<'_>,
    mut resolve_library: impl FnMut(&str) -> Result<PathBuf, StageError>,
    launcher: &dyn ProcessLauncher,
    supervisor: &dyn ProcessSupervisor,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(&str),
) -> Result<CompletedClientStage, PipelineError> {
    if timeout.is_zero() {
        return Err(PipelineError::Invalid("zero processor timeout".into()));
    }
    let base = preflight(&inputs)?;
    let plan = verify_installer_and_plan(&inputs, &base)?;
    regular_file(inputs.java)?;

    if cancelled() {
        return Err(PipelineError::Cancelled);
    }
    progress("prepare");
    // prepare_client_stage owns cleanup of a failed preparation; no journaling
    // promise is made before preparation succeeds.
    let prepared = prepare_client_stage(
        inputs.destination,
        inputs.installer_bytes,
        inputs.installer_sha256,
        inputs.minecraft,
        &mut resolve_library,
        &cancelled,
    )
    .map_err(stage_error_to)?;

    let mut journal = Journal::open(&prepared.root, &inputs)?;
    let result = (|| {
        let mut files = Vec::new();
        for f in &prepared.staged {
            let path = bind(&prepared.root, &f.path)?;
            let sha256 = sha256_file(&path, &cancelled)?;
            files.push(json!({"path":f.path,"size":f.size,"sha256":sha256}));
        }
        journal.append("prepared", "completed", json!({"inputs":files}))?;
        let outcome = run_prepared(
            &mut journal,
            &prepared,
            &inputs,
            &plan,
            &mut resolve_library,
            launcher,
            supervisor,
            timeout,
            &cancelled,
            &mut progress,
        )?;
        if cancelled() {
            return Err(PipelineError::Cancelled);
        }
        journal.append(
            "verified",
            "prepared_to_commit",
            json!({"marker":MARKER_FILE}),
        )?;
        let marker_path = write_marker(&prepared.root, &outcome, &inputs, &cancelled)?;
        // The atomic marker publication is the last mutation and sole completion point.
        Ok(CompletedClientStage {
            root: prepared.root.clone(),
            metadata: outcome.metadata,
            manifest_path: marker_path,
        })
    })();
    match result {
        Ok(done) => Ok(done),
        Err(error) => {
            let status = terminal_error_status(&error);
            if let Err(journal_error) =
                journal.append("terminal", status, json!({"error":error.to_string()}))
            {
                return Err(PipelineError::Invalid(format!(
                    "{error}; terminal checkpoint failed: {journal_error}"
                )));
            }
            Err(error)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: preflight only (no binary fixture available for archive/plan).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-pipeline-{}",
            shtmcl_platform::entropy::random_hex(16).unwrap()
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn cancelled_copy_closes_and_removes_only_its_new_output() {
        let root = test_root();
        let source = root.join("source.jar");
        let destination = root.join("new.jar");
        let bytes = vec![7u8; COPY_CHUNK * 2];
        fs::write(&source, &bytes).unwrap();
        let polls = std::cell::Cell::new(0);
        let cancelled = || {
            polls.set(polls.get() + 1);
            polls.get() >= 2
        };
        assert!(matches!(
            copy_one(
                &source,
                &destination,
                &sha1_hex(&bytes),
                bytes.len() as u64,
                &cancelled
            ),
            Err(PipelineError::Cancelled)
        ));
        assert!(!destination.exists());
        assert_eq!(fs::read(&source).unwrap(), bytes);
    }

    #[test]
    fn hash_failure_cleans_new_output_and_preserves_existing_destination() {
        let root = test_root();
        let source = root.join("source.jar");
        let destination = root.join("output.jar");
        fs::write(&source, b"data").unwrap();
        assert!(copy_one(&source, &destination, &"f".repeat(40), 4, &|| false).is_err());
        assert!(!destination.exists());
        fs::write(&destination, b"old!").unwrap();
        assert!(copy_one(&source, &destination, &sha1_hex(b"data"), 4, &|| false).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"old!");
    }

    #[test]
    fn cancellation_before_marker_rename_cannot_publish_completion() {
        let root = test_root();
        let minecraft = mc("0123456789abcdef0123456789abcdef01234567");
        let inputs = ClientInstallInputs {
            destination: &root,
            installer_bytes: b"",
            installer_sha256: "",
            base_metadata_bytes: b"",
            base_metadata_sha1: "",
            minecraft: &minecraft,
            java: Path::new("unused"),
            cached_mappings: Path::new("unused"),
        };
        let outcome = StageOutcome {
            copied: vec![],
            generated: vec![],
            metadata: json!({}),
            metadata_sha256: "0".repeat(64),
        };
        assert!(matches!(
            write_marker(&root, &outcome, &inputs, &|| true),
            Err(PipelineError::Cancelled)
        ));
        assert!(!root.join(MARKER_FILE).exists());
        assert!(root.join("neoforge-complete.json.tmp").is_file());
    }

    fn good_base() -> Vec<u8> {
        let mc_sha1 = "0123456789abcdef0123456789abcdef01234567";
        let v = json!({
            "id": "1.20.4",
            "downloads": { "client": { "sha1": mc_sha1, "size": 1234u64 } }
        });
        serde_json::to_vec(&v).unwrap()
    }

    fn mc(sha1: &str) -> VerifiedInput {
        VerifiedInput {
            path: PathBuf::from("/tmp/x.jar"),
            size: 1234,
            sha1: sha1.to_string(),
        }
    }

    #[test]
    fn preflight_accepts_matching_identity() {
        let base = good_base();
        let sha1 = sha1_hex(&base);
        let m = mc("0123456789abcdef0123456789abcdef01234567");
        let tmp = std::env::temp_dir().join(shtmcl_platform::entropy::random_hex(16).unwrap());
        let inputs = ClientInstallInputs {
            destination: &tmp,
            installer_bytes: b"x",
            installer_sha256: &"0".repeat(64),
            base_metadata_bytes: &base,
            base_metadata_sha1: &sha1,
            minecraft: &m,
            java: Path::new("/java"),
            cached_mappings: Path::new("/mappings"),
        };
        assert!(preflight(&inputs).is_ok());
    }

    #[test]
    fn preflight_rejects_input_sha_mismatch_before_dest() {
        let base = good_base();
        let sha1 = sha1_hex(&base);
        let tmp = std::env::temp_dir().join(shtmcl_platform::entropy::random_hex(16).unwrap());

        let m = mc("ffffffffffffffffffffffffffffffffffffffff");
        let inputs = ClientInstallInputs {
            destination: &tmp,
            installer_bytes: b"x",
            installer_sha256: &"0".repeat(64),
            base_metadata_bytes: &base,
            base_metadata_sha1: &sha1,
            minecraft: &m,
            java: Path::new("/java"),
            cached_mappings: Path::new("/mappings"),
        };
        assert!(matches!(preflight(&inputs), Err(PipelineError::Invalid(_))));
        assert!(!tmp.exists(), "preflight must not create destination");
    }

    #[test]
    fn preflight_rejects_bad_metadata_sha_and_preserves_existing_stage_marker() {
        let base = good_base();
        let tmp = std::env::temp_dir().join(shtmcl_platform::entropy::random_hex(16).unwrap());

        fs::create_dir_all(&tmp).unwrap();
        let marker = tmp.join(MARKER_FILE);
        fs::write(&marker, b"pre-existing").unwrap();
        let m = mc("0123456789abcdef0123456789abcdef01234567");
        let bogus = "0".repeat(40);
        let inputs = ClientInstallInputs {
            destination: &tmp,
            installer_bytes: b"x",
            installer_sha256: &"0".repeat(64),
            base_metadata_bytes: &base,
            base_metadata_sha1: &bogus,
            minecraft: &m,
            java: Path::new("/java"),
            cached_mappings: Path::new("/mappings"),
        };
        assert!(matches!(preflight(&inputs), Err(PipelineError::Invalid(_))));
        assert_eq!(fs::read(&marker).unwrap(), b"pre-existing");
    }
}

#[cfg(test)]
#[path = "neoforge_pipeline_acceptance.rs"]
mod acceptance;

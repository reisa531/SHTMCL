//! Standalone verifier for a prepared NeoForge pilot attempt directory.
//!
//! Trust boundary: the caller supplies `expected_prepared_sha256`, the pinned
//! installer identity (this file), and (via the manifest bindings below) the
//! fixed pilot versions/classloader identity. The prepared marker
//! `neoforge-prepared.json` is app-owned identity, NOT a cryptographic
//! signature; it is authenticated only by the caller-held digest plus the
//! SHA-1/SHA-256 bindings to `neoforge-base.json`, the pipeline marker, and the
//! merged version JSON. This verifier must be run under the same exclusive
//! lease that guarded preparation; it cannot defend against concurrent writers
//! that modify files between the size check, streaming hash, EOF check, and
//! `path` revalidation. It performs no filesystem writes and never publishes.
//!
//! Failure is chosen over partial acceptance: any corrupt data returns `Err`
//! before the caller can publish or record an installation.

use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Take};
use std::path::{Path, PathBuf};

const GAME_VERSION: &str = "1.20.4";
const LOADER_VERSION: &str = "20.4.253";
const INSTALLER_SHA256: &str = "8f1413379067059acc0151444b17536cfad97756d99fc70e45587bf229c986fa";
const BOOTSTRAP_MAIN_CLASS: &str = "cpw.mods.bootstraplauncher.BootstrapLauncher";
const PREPARED_NAME: &str = "neoforge-prepared.json";
const BASE_NAME: &str = "neoforge-base.json";
const COMPLETE_NAME: &str = "neoforge-complete.json";
const MERGED_RELATIVE: &str = "versions/1.20.4/1.20.4.json";
const PREPARED_LIMIT: u64 = 8 * 1024 * 1024;
const BASE_LIMIT: u64 = 8 * 1024 * 1024;
const COMPLETE_LIMIT: u64 = 8 * 1024 * 1024;
const MERGED_LIMIT: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
const MAX_FILE_SIZE: u64 = 2 * 1024 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) struct VerifiedPrepared {
    pub base_metadata: Value,
    pub metadata: Value,
    pub files: Vec<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Algo {
    Sha1,
    Sha256,
}

impl Algo {
    fn expect_len(self) -> usize {
        match self {
            Algo::Sha1 => 40,
            Algo::Sha256 => 64,
        }
    }
}

#[derive(Clone)]
struct ExpectedFile {
    relative: String,
    size: u64,
    algo: Algo,
    digest: String,
}

enum DigestState {
    Sha1(Sha1),
    Sha256(Sha256),
}

impl DigestState {
    fn new(algo: Algo) -> Self {
        match algo {
            Algo::Sha1 => DigestState::Sha1(Sha1::new()),
            Algo::Sha256 => DigestState::Sha256(Sha256::new()),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            DigestState::Sha1(d) => d.update(bytes),
            DigestState::Sha256(d) => d.update(bytes),
        }
    }

    fn finish_hex(self) -> String {
        match self {
            DigestState::Sha1(d) => format!("{:x}", d.finalize()),
            DigestState::Sha256(d) => format!("{:x}", d.finalize()),
        }
    }
}

// The caller supplies the identity recorded by the owning installation operation.
pub(crate) fn verify(
    root: &Path,
    expected_prepared_sha256: &str,
    cancelled: &dyn Fn() -> bool,
) -> Result<VerifiedPrepared, String> {
    if cancelled() {
        return Err("cancelled".into());
    }
    if !root.is_absolute()
        || root.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err("root_not_absolute".into());
    }
    let expected = normalize_sha256(expected_prepared_sha256)?;

    reject_link(root)?;
    if !root.is_dir() {
        return Err("root_not_directory".into());
    }
    reject_ancestors(root)?;

    // --- Prepared marker (root of trust for this stage) ---
    let prepared_path = root.join(PREPARED_NAME);
    let prepared_bytes = read_leaf(
        &prepared_path,
        PREPARED_LIMIT,
        Algo::Sha256,
        Some(&expected),
        cancelled,
    )?;
    verify_document(root, &prepared_bytes, &expected, cancelled)
}

/// Verify the app's in-memory marker before publishing it to disk.
pub(crate) fn verify_document(
    root: &Path,
    prepared_bytes: &[u8],
    expected: &str,
    cancelled: &dyn Fn() -> bool,
) -> Result<VerifiedPrepared, String> {
    if cancelled() {
        return Err("cancelled".into());
    }
    if !root.is_absolute()
        || root.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err("root_not_absolute".into());
    }
    reject_ancestors(root)?;
    if !root.is_dir() {
        return Err("root_not_directory".into());
    }
    let expected = normalize_sha256(expected)?;
    if prepared_bytes.len() as u64 > PREPARED_LIMIT {
        return Err("input_size".into());
    }
    if format!("{:x}", Sha256::digest(prepared_bytes)) != expected {
        return Err("input_digest".into());
    }
    let prepared: Value =
        serde_json::from_slice(prepared_bytes).map_err(|_| "prepared_json".to_string())?;
    let prepared = prepared
        .as_object()
        .ok_or_else(|| "prepared_object".to_string())?;

    if prepared.get("schemaVersion").and_then(Value::as_u64) != Some(1) {
        return Err("prepared_schema".into());
    }
    if prepared.get("gameVersion").and_then(Value::as_str) != Some(GAME_VERSION) {
        return Err("prepared_game_version".into());
    }
    if prepared.get("loaderVersion").and_then(Value::as_str) != Some(LOADER_VERSION) {
        return Err("prepared_loader_version".into());
    }
    if prepared.get("installed").and_then(Value::as_bool) != Some(false) {
        return Err("prepared_installed".into());
    }
    if prepared.get("launchReady").and_then(Value::as_bool) != Some(false) {
        return Err("prepared_launch_ready".into());
    }

    let base_name = prepared
        .get("baseMetadataPath")
        .and_then(Value::as_str)
        .ok_or_else(|| "prepared_base_path".to_string())?;
    if base_name != BASE_NAME {
        return Err("prepared_base_path".into());
    }
    let base_sha1_expected = prepared
        .get("baseMetadataSha1")
        .and_then(Value::as_str)
        .ok_or_else(|| "prepared_base_sha1".to_string())?;
    let base_sha1_expected = normalize_sha1(base_sha1_expected)?;
    let merged_sha256_expected = prepared
        .get("metadataSha256")
        .and_then(Value::as_str)
        .map(normalize_sha256)
        .transpose()?
        .ok_or_else(|| "prepared_metadata_sha256".to_string())?;
    let complete_sha256_expected = prepared
        .get("pipelineMarkerSha256")
        .and_then(Value::as_str)
        .map(normalize_sha256)
        .transpose()?
        .ok_or_else(|| "prepared_pipeline_sha256".to_string())?;

    // --- Raw base metadata, bound by SHA-1 from the prepared marker ---
    let base_path = root.join(base_name);
    let base_bytes = read_leaf(
        &base_path,
        BASE_LIMIT,
        Algo::Sha1,
        Some(&base_sha1_expected),
        cancelled,
    )?;
    let base: Value = serde_json::from_slice(&base_bytes).map_err(|_| "base_json".to_string())?;
    if base.get("id").and_then(Value::as_str) != Some(GAME_VERSION) {
        return Err("base_id".into());
    }

    // --- Pipeline completion marker (neoforge-complete.json) ---
    let complete_path = root.join(COMPLETE_NAME);
    let complete_bytes = read_leaf(
        &complete_path,
        COMPLETE_LIMIT,
        Algo::Sha256,
        Some(&complete_sha256_expected),
        cancelled,
    )?;
    let complete: Value =
        serde_json::from_slice(&complete_bytes).map_err(|_| "complete_json".to_string())?;
    let complete = complete
        .as_object()
        .ok_or_else(|| "complete_object".to_string())?;

    if complete.get("schemaVersion").and_then(Value::as_u64) != Some(1) {
        return Err("complete_schema".into());
    }
    if complete.get("gameVersion").and_then(Value::as_str) != Some(GAME_VERSION) {
        return Err("complete_game_version".into());
    }
    if complete.get("loaderVersion").and_then(Value::as_str) != Some(LOADER_VERSION) {
        return Err("complete_loader_version".into());
    }
    if complete.get("installerSha256").and_then(Value::as_str) != Some(INSTALLER_SHA256) {
        return Err("complete_installer_sha256".into());
    }
    let complete_base_sha1 = complete
        .get("metadataSha1")
        .and_then(Value::as_str)
        .ok_or_else(|| "complete_metadata_sha1".to_string())?;
    if normalize_sha1(complete_base_sha1)? != base_sha1_expected {
        return Err("base_metadata_sha1_unbound".into());
    }
    if complete.get("metadataPath").and_then(Value::as_str) != Some(MERGED_RELATIVE) {
        return Err("complete_metadata_path".into());
    }
    if complete.get("launchReady").and_then(Value::as_bool) != Some(false) {
        return Err("complete_launch_ready".into());
    }

    // --- Merged version JSON, bound by SHA-256 from the prepared marker ---
    let merged_path = root.join(MERGED_RELATIVE);
    let merged_bytes = read_leaf(
        &merged_path,
        MERGED_LIMIT,
        Algo::Sha256,
        Some(&merged_sha256_expected),
        cancelled,
    )?;
    let merged: Value =
        serde_json::from_slice(&merged_bytes).map_err(|_| "merged_json".to_string())?;
    if merged.get("id").and_then(Value::as_str) != Some(GAME_VERSION) {
        return Err("merged_id".into());
    }
    if merged.get("mainClass").and_then(Value::as_str) != Some(BOOTSTRAP_MAIN_CLASS) {
        return Err("merged_main_class".into());
    }
    check_loader_identity(&merged)?;
    if complete
        .get("metadataSha256")
        .and_then(Value::as_str)
        .map(normalize_sha256)
        .transpose()?
        .as_deref()
        != Some(merged_sha256_expected.as_str())
    {
        return Err("complete_metadata_sha256".into());
    }
    if merged.get("inheritsFrom").is_some() {
        return Err("unresolved_version_parent".into());
    }

    // --- Artifact lists ---
    let mut expected_files: BTreeMap<String, ExpectedFile> = BTreeMap::new();

    let base_artifacts = prepared
        .get("baseArtifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| "prepared_base_artifacts".to_string())?;
    if base_artifacts.is_empty() {
        return Err("prepared_base_artifacts_empty".into());
    }
    for artifact in base_artifacts {
        let file = expect_entry(artifact, Algo::Sha1, "sha1")?;
        insert_expected(&mut expected_files, file)?;
    }

    let pipeline_files = complete
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| "complete_files".to_string())?;
    if pipeline_files.is_empty() {
        return Err("complete_files_empty".into());
    }
    for artifact in pipeline_files {
        let file = expect_pipeline_entry(artifact)?;
        insert_expected(&mut expected_files, file)?;
    }

    let native_files = prepared
        .get("nativeFiles")
        .and_then(Value::as_array)
        .ok_or_else(|| "prepared_native_files".to_string())?;
    for artifact in native_files {
        let file = expect_entry(artifact, Algo::Sha256, "sha256")?;
        insert_expected(&mut expected_files, file)?;
    }

    if base_artifacts
        .len()
        .saturating_add(pipeline_files.len())
        .saturating_add(native_files.len())
        > MAX_ENTRIES
    {
        return Err("entry_count".into());
    }

    // --- Stream and hash each expected file ---
    let mut files_out: Vec<PathBuf> = Vec::with_capacity(expected_files.len());
    let mut checked_parents = BTreeSet::new();
    for expected_file in expected_files.values() {
        validate_relative(&expected_file.relative)?;
        let path = root.join(&expected_file.relative);
        reject_link_chain(root, &expected_file.relative, &mut checked_parents)?;
        stream_and_verify(&path, expected_file, cancelled)?;
        files_out.push(path);
    }

    if cancelled() {
        return Err("cancelled".into());
    }

    Ok(VerifiedPrepared {
        base_metadata: base,
        metadata: merged,
        files: files_out,
    })
}

fn check_loader_identity(merged: &Value) -> Result<(), String> {
    if merged.pointer("/shtmclLoader/kind").and_then(Value::as_str) != Some("neoforge")
        || merged
            .pointer("/shtmclLoader/version")
            .and_then(Value::as_str)
            != Some(LOADER_VERSION)
    {
        return Err("merged_loader_identity".into());
    }
    Ok(())
}

fn insert_expected(
    map: &mut BTreeMap<String, ExpectedFile>,
    file: ExpectedFile,
) -> Result<(), String> {
    validate_relative(&file.relative)?;
    if file.size > MAX_FILE_SIZE {
        return Err("entry_size".into());
    }
    let key = ascii_casefold(&file.relative);
    if let Some(previous) = map.get(&key) {
        if previous.algo == file.algo
            && previous.digest == file.digest
            && previous.size == file.size
        {
            return Ok(());
        }
        return Err("path_conflict".into());
    }
    map.insert(key, file);
    Ok(())
}

fn expect_entry(value: &Value, algo: Algo, key: &str) -> Result<ExpectedFile, String> {
    let relative = value
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "entry_path".to_string())?
        .to_string();
    let size = value
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| "entry_size".to_string())?;
    let digest = value
        .get(key)
        .and_then(Value::as_str)
        .map(|d| match algo {
            Algo::Sha1 => normalize_sha1(d),
            Algo::Sha256 => normalize_sha256(d),
        })
        .transpose()?
        .ok_or_else(|| "entry_digest".to_string())?;
    Ok(ExpectedFile {
        relative,
        size,
        algo,
        digest,
    })
}

fn expect_pipeline_entry(value: &Value) -> Result<ExpectedFile, String> {
    let relative = value
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "entry_path".to_string())?
        .to_string();
    let size = value
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| "entry_size".to_string())?;
    let sha1 = value.get("sha1").and_then(Value::as_str);
    let sha256 = value.get("sha256").and_then(Value::as_str);
    let (algo, raw) = match (sha1, sha256) {
        (Some(s1), None) => (Algo::Sha1, s1),
        (None, Some(s2)) => (Algo::Sha256, s2),
        _ => return Err("pipeline_digest_arity".into()),
    };
    let digest = match algo {
        Algo::Sha1 => normalize_sha1(raw),
        Algo::Sha256 => normalize_sha256(raw),
    }?;
    Ok(ExpectedFile {
        relative,
        size,
        algo,
        digest,
    })
}

fn read_leaf(
    path: &Path,
    limit: u64,
    algo: Algo,
    expected: Option<&str>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<u8>, String> {
    if cancelled() {
        return Err("cancelled".into());
    }
    reject_ancestors(path)?;
    let file = File::open(path).map_err(|_| "input_open".to_string())?;
    let meta = file.metadata().map_err(|_| "input_metadata".to_string())?;
    if !meta.is_file() {
        return Err("input_not_file".into());
    }
    if meta.len() > limit {
        return Err("input_size".into());
    }
    let mut bytes = Vec::new();
    let mut reader: Take<File> = file.take(limit + 1);
    let mut buffer = vec![0u8; CHUNK];
    loop {
        if cancelled() {
            return Err("cancelled".into());
        }
        let count = reader
            .read(&mut buffer)
            .map_err(|_| "input_read".to_string())?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() as u64 > limit {
            return Err("input_size".into());
        }
    }
    if bytes.len() as u64 != meta.len() {
        return Err("input_size_changed".into());
    }
    if let Some(expected) = expected {
        let actual = match algo {
            Algo::Sha1 => format!("{:x}", Sha1::digest(&bytes)),
            Algo::Sha256 => format!("{:x}", Sha256::digest(&bytes)),
        };
        if actual != expected {
            return Err("input_digest".into());
        }
    }
    Ok(bytes)
}

fn stream_and_verify(
    path: &Path,
    expected: &ExpectedFile,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    if expected.size > MAX_FILE_SIZE {
        return Err("file_size_limit".into());
    }
    if cancelled() {
        return Err("cancelled".into());
    }
    reject_link(path)?;
    let file = File::open(path).map_err(|_| "file_open".to_string())?;
    let meta = file.metadata().map_err(|_| "file_metadata".to_string())?;
    if !meta.is_file() {
        return Err("file_not_regular".into());
    }
    if meta.len() != expected.size {
        return Err("file_size".into());
    }
    let mut digest = DigestState::new(expected.algo);
    let mut reader = file;
    let mut buffer = vec![0u8; CHUNK];
    let mut total: u64 = 0;
    loop {
        if cancelled() {
            return Err("cancelled".into());
        }
        let count = reader
            .read(&mut buffer)
            .map_err(|_| "file_read".to_string())?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > expected.size {
            return Err("file_size".into());
        }
        digest.update(&buffer[..count]);
    }
    if total != expected.size {
        return Err("file_size_eof".into());
    }
    if digest.finish_hex() != expected.digest {
        return Err("file_digest".into());
    }
    Ok(())
}

fn reject_link(path: &Path) -> Result<(), String> {
    crate::game_install::reject_link(path)
}

fn reject_ancestors(root: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in root.components() {
        current.push(component);
        reject_link(&current)?;
    }
    Ok(())
}

fn reject_link_chain(
    root: &Path,
    relative: &str,
    checked: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    let mut current = root.to_path_buf();
    let parts: Vec<&str> = relative.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        current.push(part);
        let is_file = index + 1 == parts.len();
        if !is_file && !checked.insert(current.clone()) {
            continue;
        }
        reject_link(&current)?;
        let meta = current
            .symlink_metadata()
            .map_err(|_| "file_metadata".to_string())?;
        if is_file {
            if !meta.is_file() {
                return Err("file_not_regular".into());
            }
        } else if !meta.is_dir() {
            return Err("file_parent_not_directory".into());
        }
    }
    Ok(())
}

fn normalize_sha1(value: &str) -> Result<String, String> {
    normalize_hex(value, Algo::Sha1)
}

fn normalize_sha256(value: &str) -> Result<String, String> {
    normalize_hex(value, Algo::Sha256)
}

fn normalize_hex(value: &str, algo: Algo) -> Result<String, String> {
    if value.len() != algo.expect_len() || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("digest_format".into());
    }
    Ok(value.to_ascii_lowercase())
}

fn ascii_casefold(value: &str) -> String {
    value.to_ascii_lowercase()
}

fn validate_relative(relative: &str) -> Result<(), String> {
    if !shtmcl_core::launch::install::safe_path(relative)
        || relative
            .chars()
            .any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Err("path_invalid".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let name = format!("neo-verifier-{}", shtmcl_platform::random_hex(16).unwrap());
            let root = std::env::temp_dir().join(name);
            fs::create_dir_all(&root).unwrap();
            Temp(root)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha1::digest(bytes))
    }
    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn write_file(root: &Path, relative: &str, bytes: &[u8]) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    struct Fixture {
        temp: Temp,
        prepared_sha256: String,
    }

    fn build_fixture() -> Fixture {
        let temp = Temp::new();
        let root = temp.path();

        let base = br#"{"id":"1.20.4","libraries":[]}"#.to_vec();
        let base_sha1 = sha1_hex(&base);
        write_file(root, "neoforge-base.json", &base);

        write_file(root, "libraries/net/neoforged/client.jar", b"client-bytes");
        write_file(root, "assets/indexes/5.json", b"index");
        write_file(root, "natives/foo.dll", b"native-bytes");

        let client_sha1 = sha1_hex(b"client-bytes");
        let index_sha1 = sha1_hex(b"index");
        let native_sha256 = sha256_hex(b"native-bytes");

        let base_artifacts = vec![
            serde_json::json!({"path":"libraries/net/neoforged/client.jar","sha1":client_sha1,"size":12,"url":"https://libraries.minecraft.net/client.jar"}),
            serde_json::json!({"path":"assets/indexes/5.json","sha1":index_sha1,"size":5,"url":"https://piston-data.mojang.com/5.json"}),
        ];
        let native_files =
            vec![serde_json::json!({"path":"natives/foo.dll","size":12,"sha256":native_sha256})];

        let merged = serde_json::json!({
            "id":"1.20.4",
            "mainClass": BOOTSTRAP_MAIN_CLASS,
            "shtmclLoader":{"kind":"neoforge","version":"20.4.253"},
            "libraries": []
        });
        let merged_bytes = serde_json::to_vec_pretty(&merged).unwrap();
        write_file(root, "versions/1.20.4/1.20.4.json", &merged_bytes);
        let merged_sha256 = sha256_hex(&merged_bytes);

        let pipeline = serde_json::json!({
            "schemaVersion":1,
            "gameVersion":"1.20.4",
            "loaderVersion":"20.4.253",
            "installerSha256": INSTALLER_SHA256,
            "metadataSha1": base_sha1,
            "metadataPath": "versions/1.20.4/1.20.4.json",
            "metadataSha256": merged_sha256,
            "launchReady": false,
            "files": [
                {"path":"libraries/net/neoforged/client.jar","size":12,"sha1": client_sha1}
            ]
        });
        let pipeline_bytes = serde_json::to_vec(&pipeline).unwrap();
        write_file(root, "neoforge-complete.json", &pipeline_bytes);
        let pipeline_sha256 = sha256_hex(&pipeline_bytes);

        let prepared = serde_json::json!({
            "schemaVersion":1,
            "gameVersion":"1.20.4",
            "loaderVersion":"20.4.253",
            "baseMetadataPath":"neoforge-base.json",
            "baseMetadataSha1": base_sha1,
            "metadataSha256": merged_sha256,
            "pipelineMarkerSha256": pipeline_sha256,
            "baseArtifacts": base_artifacts,
            "nativeFiles": native_files,
            "installed": false,
            "launchReady": false
        });
        let prepared_bytes = serde_json::to_vec_pretty(&prepared).unwrap();
        write_file(root, "neoforge-prepared.json", &prepared_bytes);
        let prepared_sha256 = sha256_hex(&prepared_bytes);

        Fixture {
            temp,
            prepared_sha256,
        }
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn synthetic_valid_fixture_verifies() {
        let fixture = build_fixture();
        let verified = verify(fixture.temp.path(), &fixture.prepared_sha256, &never).unwrap();
        assert_eq!(verified.files.len(), 3);
        assert_eq!(verified.metadata["id"], "1.20.4");
        assert_eq!(verified.metadata["mainClass"], BOOTSTRAP_MAIN_CLASS);
    }

    #[test]
    fn wrong_digest_rejected() {
        let fixture = build_fixture();
        let wrong = sha256_hex(b"different");
        assert_eq!(
            verify(fixture.temp.path(), &wrong, &never).unwrap_err(),
            "input_digest"
        );
    }

    #[test]
    fn tampered_file_same_length_rejected() {
        let fixture = build_fixture();
        write_file(
            fixture.temp.path(),
            "libraries/net/neoforged/client.jar",
            b"clienT-bytes",
        );
        // Same length, different bytes.
        assert_eq!(
            fs::metadata(
                fixture
                    .temp
                    .path()
                    .join("libraries/net/neoforged/client.jar")
            )
            .unwrap()
            .len(),
            12
        );
        let error = verify(fixture.temp.path(), &fixture.prepared_sha256, &never).unwrap_err();
        assert!(error == "file_digest" || error == "input_digest", "{error}");
    }

    #[test]
    fn missing_file_rejected() {
        let fixture = build_fixture();
        fs::remove_file(fixture.temp.path().join("natives/foo.dll")).unwrap();
        let error = verify(fixture.temp.path(), &fixture.prepared_sha256, &never).unwrap_err();
        assert!(error == "file_metadata" || error == "file_open", "{error}");
    }

    #[test]
    fn unsafe_path_rejected() {
        for relative in [
            "../escape.txt",
            "a/./b",
            "a//b",
            "C:/x",
            "a\\b",
            "a/NUL.jar",
            "a/x.",
            "a/x ",
            "a/evil\0",
        ] {
            assert!(validate_relative(relative).is_err(), "{relative:?}");
        }
    }

    #[test]
    fn cross_list_conflicting_path_rejected() {
        let fixture = build_fixture();
        // Rewrite prepared marker so a native entry shadows a base artifact with
        // a different digest but the same (casefolded) path.
        let root = fixture.temp.path();
        let mut prepared: Value =
            serde_json::from_slice(&fs::read(root.join("neoforge-prepared.json")).unwrap())
                .unwrap();
        prepared["nativeFiles"] = serde_json::json!([
            {"path":"LIBRARIES/net/neoforged/client.jar","size":12,"sha256": sha256_hex(b"other-bytes")}
        ]);
        let prepared_bytes = serde_json::to_vec_pretty(&prepared).unwrap();
        write_file(root, "neoforge-prepared.json", &prepared_bytes);
        let prepared_sha256 = sha256_hex(&prepared_bytes);
        let error = verify(root, &prepared_sha256, &never).unwrap_err();
        assert_eq!(error, "path_conflict");
    }

    #[test]
    fn cancellation_aborts() {
        let fixture = build_fixture();
        assert_eq!(
            verify(fixture.temp.path(), &fixture.prepared_sha256, &|| true).unwrap_err(),
            "cancelled"
        );
    }

    #[test]
    fn cancellation_during_large_file_hash_retains_input() {
        let temp = Temp::new();
        let bytes = vec![7u8; CHUNK * 4];
        write_file(temp.path(), "large.jar", &bytes);
        let expected = ExpectedFile {
            relative: "large.jar".into(),
            size: bytes.len() as u64,
            algo: Algo::Sha256,
            digest: sha256_hex(&bytes),
        };
        let polls = std::cell::Cell::new(0);
        let result = stream_and_verify(&temp.path().join("large.jar"), &expected, &|| {
            polls.set(polls.get() + 1);
            polls.get() >= 3
        });
        assert_eq!(result.unwrap_err(), "cancelled");
        assert_eq!(fs::read(temp.path().join("large.jar")).unwrap(), bytes);
    }

    #[test]
    fn sha256_native_corruption_is_rejected() {
        let fixture = build_fixture();
        write_file(fixture.temp.path(), "natives/foo.dll", b"NATIVE-bytes");
        assert_eq!(
            verify(fixture.temp.path(), &fixture.prepared_sha256, &never).unwrap_err(),
            "file_digest"
        );
    }

    #[cfg(windows)]
    #[test]
    fn merged_metadata_parent_junction_is_rejected() {
        use std::os::windows::process::CommandExt;
        let fixture = build_fixture();
        let external = Temp::new();
        let link = fixture.temp.path().join("versions");
        let target = external.path().join("versions");
        fs::rename(&link, &target).unwrap();
        let result = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        let checked = verify(fixture.temp.path(), &fixture.prepared_sha256, &never);
        fs::remove_dir(&link).unwrap();
        assert_eq!(checked.unwrap_err(), "linked_install_path");
        assert!(target.join("1.20.4/1.20.4.json").is_file());
    }

    #[test]
    #[ignore = "read-only verification of a retained full prepared game; requires explicit paths"]
    fn retained_full_preparation_matches_captured_identity() {
        let root = PathBuf::from(
            std::env::var_os("SHTMCL_NEOFORGE_PREPARED_ROOT").expect("prepared root"),
        );
        let expected = std::env::var("SHTMCL_NEOFORGE_PREPARED_SHA256").expect("captured digest");
        let started = std::time::Instant::now();
        let verified = verify(&root, &expected, &never).unwrap();
        assert!(verified.files.len() > 3800);
        assert!(!root.join("shtmcl-install.json").exists());
        let report = serde_json::json!({"preparedRoot":root,"preparedSha256":expected,
            "verifiedUniqueFiles":verified.files.len(),"mainClass":verified.metadata["mainClass"],
            "elapsedSeconds":started.elapsed().as_secs_f64(),"readOnly":true,
            "gameLaunched":false,"instanceRegistered":false});
        let output =
            PathBuf::from(std::env::var_os("SHTMCL_NEOFORGE_VERIFY_REPORT").expect("new report"));
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)
            .unwrap();
        file.write_all(&serde_json::to_vec_pretty(&report).unwrap())
            .unwrap();
        println!("{report}");
    }
}

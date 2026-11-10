//! Opt-in real-network runtime assembly evidence; not a production installer.
use super::*;
use crate::downloads::{ArtifactHash, FileDisk, FileStore, HttpDownloads, HttpSource, SystemClock};
use sha1::{Digest as Sha1Digest, Sha1};
use sha2::Sha256;
use shtmcl_domain::{CommandResponse, OperationId, RequestId, TaskState};
use shtmcl_platform::PlatformDirs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RUN_ID: &str = "official-neoforge-runtime-assembly";

fn env_path(key: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(key).unwrap_or_else(|| panic!("missing env {key}")))
}
fn env_str(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("missing env {key}"))
}
fn hex_sha1(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}
fn hex_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}
fn stream_sha1_and_len(path: &Path) -> std::io::Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}
fn stream_sha256_and_len(path: &Path) -> std::io::Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}
/// create_new output, then stream-verify source into it; refuses overwrite.
fn copy_new(src: &Path, dst: &Path) -> std::io::Result<u64> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut input = std::fs::File::open(src)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n])?;
        total += n as u64;
    }
    output.sync_all()?;
    Ok(total)
}

#[test]
#[ignore = "real network+installer evidence; run via SHTMCL_* env"]
fn official_neoforge_runtime_assembly() {
    let root = env_path("SHTMCL_NEOFORGE_RUNTIME_ROOT");
    std::fs::create_dir_all(&root).expect("root");
    let evidence = |name: &str| root.join(name);
    let result: std::result::Result<(), String> = (|| {
        // 0. Identity-verify installer + game metadata before touching IO.
        let installer_path = env_path("SHTMCL_NEOFORGE_RUNTIME_INSTALLER");
        let installer_bytes = std::fs::read(&installer_path).map_err(|e| e.to_string())?;
        if installer_bytes.len() > 32 * 1024 * 1024 {
            return Err("installer_too_large".into());
        }
        let expected_installer = env_str("SHTMCL_NEOFORGE_INSTALLER_SHA256");
        if hex_sha256(&installer_bytes) != expected_installer {
            return Err("installer_sha256_mismatch".into());
        }
        let metadata_path = env_path("SHTMCL_NEOFORGE_GAME_METADATA");
        let metadata_bytes = std::fs::read(&metadata_path).map_err(|e| e.to_string())?;
        let expected_metadata = env_str("SHTMCL_NEOFORGE_GAME_METADATA_SHA1");
        if hex_sha1(&metadata_bytes) != expected_metadata {
            return Err("metadata_sha1_mismatch".into());
        }
        std::fs::write(evidence("game-metadata.json"), &metadata_bytes).unwrap();
        let base: Value = serde_json::from_slice(&metadata_bytes).map_err(|e| e.to_string())?;

        // 1. Real installer profile + real child version from the archive.
        let parsed = super::super::neoforge_profile::read_installer_archive(
            "1.20.4",
            "20.4.253",
            &installer_bytes,
        )
        .map_err(str::to_owned)?;
        let profile = parsed.profile;
        let child = parsed.version_metadata;
        std::fs::write(
            evidence("child-version.json"),
            serde_json::to_vec_pretty(&child).unwrap(),
        )
        .unwrap();
        let plan = plan_runtime(&profile, &child, &base).map_err(|e| format!("plan: {e}"))?;
        std::fs::write(
            evidence("merged-version.json"),
            serde_json::to_vec_pretty(&plan.metadata).unwrap(),
        )
        .unwrap();

        // 2. Drive the shared downloads module over the planner output.
        let cache = root.join("cache");
        let sources = vec![
            HttpSource::new("neo-runtime", "https://maven.neoforged.net/releases/")
                .map_err(|e| e.to_string())?,
            HttpSource::new("mojang-libraries", "https://libraries.minecraft.net/")
                .map_err(|e| e.to_string())?,
        ];
        let http = Arc::new(HttpDownloads::new(&cache, sources).map_err(|e| e.to_string())?);
        let mut app = crate::CoreApp::new(PlatformDirs::under(&root));
        app.register(Arc::new(crate::downloads::DownloadsModule::new(
            app.context(),
            http.clone(),
            Arc::new(FileDisk::new(&root)),
            Arc::new(SystemClock),
        )))
        .map_err(|e| format!("register: {e:?}"))?;

        let response = plan
            .start_download(
                &app,
                RequestId::new(format!("req-{RUN_ID}")).map_err(|e| format!("{e:?}"))?,
                OperationId::new(RUN_ID).map_err(|e| format!("{e:?}"))?,
            )
            .map_err(|e| format!("start: {e:?}"))?;
        let CommandResponse::Accepted { task_id, .. } = response else {
            return Err("expected accepted download task".into());
        };
        let deadline = Instant::now() + Duration::from_secs(240);
        let task = loop {
            let snapshot = app.snapshot();
            let task = snapshot
                .into_iter()
                .find(|t| t.id == task_id)
                .ok_or_else(|| "download task vanished".to_string())?;
            if matches!(
                task.state,
                TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
            ) {
                break task;
            }
            if Instant::now() >= deadline {
                return Err("download timed out after 240s".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        std::fs::write(
            evidence("download-task.json"),
            serde_json::to_vec_pretty(&task).unwrap(),
        )
        .unwrap();
        if task.state != TaskState::Succeeded {
            return Err(format!(
                "download termninal: {:?} {:?}",
                task.state, task.error
            ));
        }

        // 3. Independently re-verify all planned cache objects.
        let store = FileStore::new(&cache).map_err(|e| e.to_string())?;
        let objects = cache.join("objects/sha1");
        let stage = root.join("runtime-stage");
        std::fs::create_dir(&stage).map_err(|e| format!("stage must be new: {e}"))?;
        let stage_libraries = stage.join("libraries");
        std::fs::create_dir_all(&stage_libraries).unwrap();
        let mut manifest_entries: Vec<Value> = Vec::new();
        for library in &plan.downloads {
            let object = objects.join(&library.sha1[..2]).join(&library.sha1[..40]);
            let store_path = object;
            let verified = store
                .verify_cached(&ArtifactHash::new("sha1", &library.sha1).unwrap())
                .map_err(|e| e.to_string())?;
            if verified != Some(library.size) {
                return Err(format!("verify_cached mismatch {}", library.sha1));
            }
            let (digest, size) = stream_sha1_and_len(&store_path).map_err(|e| e.to_string())?;
            if digest != library.sha1 || size != library.size {
                return Err(format!("independent sha1 mismatch {}", library.sha1));
            }
            let dst = stage_libraries.join(&library.path);
            copy_new(&store_path, &dst).map_err(|e| format!("copy {}: {e}", library.path))?;
            let dst_canon = std::fs::canonicalize(&dst).unwrap_or(dst.clone());
            manifest_entries.push(serde_json::json!({
                "path": format!("libraries/{}", library.path),
                "absolutePath": dst_canon,
                "size": library.size,
                "sha1": library.sha1
            }));
        }

        // 4. Generated-3: existing evidence dir, no expectation baked in.
        let completed = env_path("SHTMCL_NEOFORGE_COMPLETED_STAGE");
        for path in &plan.generated_paths {
            let src = completed.join(path);
            if !src.exists() {
                return Err(format!("missing generated {}", path));
            }
            let dst = stage.join(path);
            copy_new(&src, &dst).map_err(|e| format!("copy generated {}: {e}", path))?;
            let (sha256, size) = stream_sha256_and_len(&dst).map_err(|e| e.to_string())?;
            let dst_canon = std::fs::canonicalize(&dst).unwrap_or(dst.clone());
            manifest_entries.push(serde_json::json!({
                "path": path,
                "absolutePath": dst_canon,
                "size": size,
                "sha256": sha256,
                "origin": "SHTMCL_NEOFORGE_COMPLETED_STAGE"
            }));
        }

        std::fs::write(
            evidence("runtime-manifest.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "stageRoot": std::fs::canonicalize(&stage).unwrap_or(stage.clone()),
                "entries": manifest_entries
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            evidence("result.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "runId": RUN_ID,
                "installerSha256": expected_installer,
                "gameMetadataSha1": expected_metadata,
                "downloadTaskId": format!("{:?}", task_id),
                "downloadState": "Succeeded",
                "libraryCount": plan.downloads.len(),
                "generatedCount": plan.generated_paths.len(),
                "instanceCommitted": false,
                "gameLaunched": false,
                "note": "runtime-stage assembled and verified; independent integrator verification pending"
            }))
            .unwrap(),
        )
        .unwrap();
        Ok(())
    })();
    if let Err(detail) = result {
        let _ = std::fs::write(
            evidence("failure.json"),
            serde_json::to_vec_pretty(&serde_json::json!({"runId": RUN_ID, "error": detail}))
                .unwrap(),
        );
        panic!("official_neoforge_runtime_assembly failed: {detail}");
    }
}

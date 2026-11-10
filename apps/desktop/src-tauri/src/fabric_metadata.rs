//! Fabric's HTTP metadata is resolved to pinned library identities before 03 downloads.
use crate::game_install::{checked_directory, reject_link};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use shtmcl_core::{
    content::fabric::{parse_profile, valid_version},
    launch::catalog::VersionMetadata,
};
use shtmcl_platform::PlatformDirs;
use std::{fs, io::Read, path::Path, sync::OnceLock, time::Duration};

fn client() -> Result<reqwest::blocking::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::blocking::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .user_agent(concat!("SHTMCL/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(20))
                .pool_idle_timeout(Duration::from_secs(90))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| "fabric_http_client".into())
        })
        .clone()
}
fn fetch(url: &str, max: usize) -> Result<Vec<u8>, String> {
    match fetch_once(url, max) {
        Err(error)
            if matches!(error.as_str(), "fabric_http_request" | "fabric_http_read")
                && url.starts_with("https://meta.fabricmc.net/") =>
        {
            eprintln!("SHTMCL: Fabric metadata source fallback: official -> BMCLAPI");
            fetch_once(
                &url.replacen(
                    "https://meta.fabricmc.net/",
                    "https://bmclapi2.bangbang93.com/fabric-meta/",
                    1,
                ),
                max,
            )
        }
        result => result,
    }
}
fn fetch_once(url: &str, max: usize) -> Result<Vec<u8>, String> {
    let mut response = client()?
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|error| {
            eprintln!("SHTMCL: Fabric metadata request failed: {error:?}");
            "fabric_http_request"
        })?;
    let mut data = vec![];
    response
        .by_ref()
        .take(max as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|_| "fabric_http_read")?;
    if data.len() > max {
        return Err("fabric_response_budget".into());
    }
    Ok(data)
}
fn endpoint(game: &str, loader: Option<&str>) -> Result<String, String> {
    if game.is_empty()
        || game.len() > 256
        || game.contains(['/', '\\', '\0'])
        || game.chars().any(char::is_control)
    {
        return Err("fabric_game_version".into());
    }
    let mut url = reqwest::Url::parse("https://meta.fabricmc.net/v2/versions/loader/").unwrap();
    {
        let mut segments = url.path_segments_mut().map_err(|_| "fabric_url")?;
        segments.pop_if_empty().push(game);
        if let Some(loader) = loader {
            if !valid_version(loader) {
                return Err("fabric_loader_version".into());
            }
            segments.push(loader).push("profile").push("json");
        }
    }
    Ok(url.to_string())
}
pub(crate) fn versions(game: &str) -> Result<Vec<Value>, String> {
    let data = fetch(&endpoint(game, None)?, 8 * 1024 * 1024)?;
    let raw: Value = serde_json::from_slice(&data).map_err(|_| "fabric_versions_json")?;
    let entries = raw
        .as_array()
        .filter(|items| items.len() <= 4096)
        .ok_or("fabric_versions")?;
    let mut seen = std::collections::BTreeSet::new();
    let mut result = vec![];
    for item in entries {
        let version = item["loader"]["version"]
            .as_str()
            .filter(|v| valid_version(v))
            .ok_or("fabric_loader_version")?;
        let stable = item["loader"]["stable"]
            .as_bool()
            .ok_or("fabric_loader_stability")?;
        if item["intermediary"]["version"] != game {
            return Err("fabric_game_mismatch".into());
        }
        if seen.insert(version.to_string()) {
            result.push(json!({"version":version,"stable":stable}));
        }
    }
    Ok(result)
}

pub(crate) fn prepare(
    base: &VersionMetadata,
    loader: &str,
    cancelled: &dyn Fn() -> bool,
) -> Result<VersionMetadata, String> {
    if cancelled() {
        return Err("cancelled".into());
    }
    let bytes = fetch(&endpoint(&base.id, Some(loader))?, 1024 * 1024)?;
    let raw: Value = serde_json::from_slice(&bytes).map_err(|_| "fabric_profile_json")?;
    let mut profile = parse_profile(&raw, &base.id, loader)?;
    let http = client()?;
    for library in &mut profile.libraries {
        if cancelled() {
            return Err("cancelled".into());
        }
        if library.sha1.is_none() {
            let data = fetch(&format!("{}.sha1", library.url), 256)?;
            let hash = std::str::from_utf8(&data)
                .map_err(|_| "fabric_checksum")?
                .trim();
            if hash.len() != 40 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err("fabric_checksum".into());
            }
            library.sha1 = Some(hash.to_ascii_lowercase());
        }
        if library.size.is_none() {
            let response = http
                .head(&library.url)
                .send()
                .and_then(|r| r.error_for_status())
                .map_err(|_| "fabric_library_head")?;
            library.size = Some(
                response
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .filter(|s| *s > 0 && *s <= 256 * 1024 * 1024)
                    .ok_or("fabric_library_size")?,
            );
        }
    }
    VersionMetadata::from_value(profile.merge(&base.raw)?, Some(&base.id))
        .map_err(|_| "fabric_merged_metadata".into())
}

pub(crate) fn persist(dirs: &PlatformDirs, bytes: &[u8]) -> Result<String, String> {
    let hash = format!("{:x}", Sha1::digest(bytes));
    let folder = dirs.cache.join("fabric-metadata");
    checked_directory(&folder)?;
    let path = folder.join(format!("{hash}.json"));
    reject_link(&path)?;
    if path.exists() {
        if fs::read(&path).map_err(|_| "fabric_cache_read")? != bytes {
            return Err("fabric_cache_conflict".into());
        }
        return Ok(hash);
    }
    let temp = folder.join(format!(
        ".{}.tmp",
        shtmcl_platform::entropy::random_hex(12).map_err(|_| "fabric_entropy")?
    ));
    fs::write(&temp, bytes).map_err(|_| "fabric_cache_write")?;
    if fs::rename(&temp, &path).is_err() {
        let _ = fs::remove_file(&temp);
        if fs::read(&path).ok().as_deref() != Some(bytes) {
            return Err("fabric_cache_publish".into());
        }
    }
    Ok(hash)
}
pub(crate) fn pinned(
    dirs: &PlatformDirs,
    root: &Path,
    receipt: &Value,
    game: &str,
    loader: &str,
) -> Result<VersionMetadata, String> {
    let hash = receipt["metadataSha1"]
        .as_str()
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("fabric_metadata_hash")?;
    let cache = dirs
        .cache
        .join("fabric-metadata")
        .join(format!("{}.json", hash.to_ascii_lowercase()));
    for path in [cache, root.join(format!("versions/{game}/{game}.json"))] {
        if path.ancestors().any(|p| reject_link(p).is_err()) {
            continue;
        }
        let Ok(file) = fs::File::open(path) else {
            continue;
        };
        let mut bytes = vec![];
        if file
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() > 8 * 1024 * 1024
            || !format!("{:x}", Sha1::digest(&bytes)).eq_ignore_ascii_case(hash)
        {
            continue;
        }
        let meta =
            VersionMetadata::parse(&bytes, Some(game)).map_err(|_| "fabric_metadata_json")?;
        if meta.raw["shtmclLoader"]["kind"] != "fabric"
            || meta.raw["shtmclLoader"]["version"] != loader
        {
            return Err("fabric_metadata_identity".into());
        }
        return Ok(meta);
    }
    Err("fabric_pinned_metadata_missing".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_encodes_game_and_rejects_loader_paths() {
        assert!(endpoint("1.14 Pre-Release 5", Some("0.16.10"))
            .unwrap()
            .contains("1.14%20Pre-Release%205"));
        assert!(endpoint("../file", None).is_err());
        assert!(endpoint("1.20.4", Some("../bad")).is_err());
    }
    #[test]
    fn pinned_metadata_recovers_from_one_corrupt_copy_and_checks_loader_identity() {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-fabric-cache-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        let dirs = PlatformDirs::under(&root);
        dirs.create_all().unwrap();
        let instance = dirs.instances.join("fabric-test");
        let versions = instance.join("versions/1.20.4");
        fs::create_dir_all(&versions).unwrap();
        let bytes=serde_json::to_vec(&json!({"id":"1.20.4","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[],"arguments":{"game":[]},"shtmclLoader":{"kind":"fabric","version":"0.16.10"}})).unwrap();
        let hash = persist(&dirs, &bytes).unwrap();
        let receipt = json!({"metadataSha1":hash});
        fs::write(versions.join("1.20.4.json"), &bytes).unwrap();
        assert!(pinned(&dirs, &instance, &receipt, "1.20.4", "0.16.10").is_ok());
        assert!(pinned(&dirs, &instance, &receipt, "1.20.4", "0.16.11").is_err());
        fs::write(
            dirs.cache
                .join("fabric-metadata")
                .join(format!("{hash}.json")),
            b"damaged",
        )
        .unwrap();
        assert!(pinned(&dirs, &instance, &receipt, "1.20.4", "0.16.10").is_ok());
        fs::write(versions.join("1.20.4.json"), b"damaged too").unwrap();
        assert!(pinned(&dirs, &instance, &receipt, "1.20.4", "0.16.10").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}

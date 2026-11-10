//! Bounded parser for Mojang's Java runtime catalog and per-runtime file list.
//!
//! This module only turns official metadata into a safe install plan. File
//! transfer and cache publication remain owned by downloads (03).

use serde_json::Value;
use sha1::{Digest, Sha1};

pub const RUNTIME_CATALOG_URL: &str = "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
pub const MAX_RUNTIME_CATALOG_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RUNTIME_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RUNTIME_FILES: usize = 10_000;
pub const MAX_RUNTIME_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

const ALLOWED_DOWNLOAD_HOSTS: &[&str] = &["piston-data.mojang.com", "launcher.mojang.com"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRelease {
    pub family: String,
    pub version: String,
    pub released: String,
    pub manifest_url: String,
    pub manifest_sha1: String,
    pub manifest_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeFile {
    Directory {
        path: String,
    },
    File {
        path: String,
        url: String,
        sha1: String,
        size: u64,
        executable: bool,
    },
    Link {
        path: String,
        target: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInstallPlan {
    pub files: Vec<RuntimeFile>,
    pub total_bytes: u64,
    pub java_executable: String,
    pub javaw_executable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeManifestError {
    TooLarge,
    InvalidJson,
    InvalidCatalog,
    UnsupportedPlatform(String),
    NoCompatibleRuntime { platform: String, major: u32 },
    InvalidRelease,
    ManifestDigestMismatch,
    InvalidFiles,
    TooManyFiles,
    TooLargeRuntime,
    UnsafePath(String),
    UnsupportedFileType(String),
    InvalidFile(String),
    UnsafeUrl(String),
    MissingJavaExecutable,
}

/// Select the newest Mojang runtime release matching the requested major.
/// Platform keys use Mojang's names, for example `windows-x64`.
pub fn select_release(
    bytes: &[u8],
    platform: &str,
    required_major: u32,
) -> Result<RuntimeRelease, RuntimeManifestError> {
    if bytes.len() > MAX_RUNTIME_CATALOG_BYTES {
        return Err(RuntimeManifestError::TooLarge);
    }
    let root: Value =
        serde_json::from_slice(bytes).map_err(|_| RuntimeManifestError::InvalidJson)?;
    let platform_value = root
        .get(platform)
        .and_then(Value::as_object)
        .ok_or_else(|| RuntimeManifestError::UnsupportedPlatform(platform.to_owned()))?;

    let mut candidates = Vec::new();
    for (family, releases) in platform_value {
        let Some(releases) = releases.as_array() else {
            return Err(RuntimeManifestError::InvalidCatalog);
        };
        for release in releases {
            let version = release
                .pointer("/version/name")
                .and_then(Value::as_str)
                .ok_or(RuntimeManifestError::InvalidRelease)?;
            if parse_major(version) != Some(required_major) {
                continue;
            }
            let manifest = release
                .get("manifest")
                .ok_or(RuntimeManifestError::InvalidRelease)?;
            let sha1 = manifest
                .get("sha1")
                .and_then(Value::as_str)
                .filter(|digest| is_hex_digest(digest, 40))
                .ok_or(RuntimeManifestError::InvalidRelease)?
                .to_ascii_lowercase();
            let expected_url =
                format!("https://piston-meta.mojang.com/v1/packages/{sha1}/manifest.json");
            let url = manifest
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| *url == expected_url)
                .ok_or(RuntimeManifestError::InvalidRelease)?;
            let released = release
                .pointer("/version/released")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let size = manifest
                .get("size")
                .and_then(Value::as_u64)
                .ok_or(RuntimeManifestError::InvalidRelease)?;
            candidates.push(RuntimeRelease {
                family: family.clone(),
                version: version.to_owned(),
                released: released.to_owned(),
                manifest_url: url.to_owned(),
                manifest_sha1: sha1,
                manifest_size: size,
            });
        }
    }

    candidates
        .into_iter()
        .max_by(|left, right| {
            left.released
                .cmp(&right.released)
                .then_with(|| left.family.cmp(&right.family))
        })
        .ok_or_else(|| RuntimeManifestError::NoCompatibleRuntime {
            platform: platform.to_owned(),
            major: required_major,
        })
}

/// Validate a runtime file manifest against the catalog's SHA-1 before
/// returning a bounded plan. Each downloaded file is independently pinned by
/// its raw SHA-1 and exact byte size.
pub fn parse_install_plan(
    bytes: &[u8],
    expected_manifest_sha1: &str,
    platform: &str,
) -> Result<RuntimeInstallPlan, RuntimeManifestError> {
    if bytes.len() > MAX_RUNTIME_MANIFEST_BYTES {
        return Err(RuntimeManifestError::TooLarge);
    }
    if !is_hex_digest(expected_manifest_sha1, 40) {
        return Err(RuntimeManifestError::InvalidRelease);
    }
    let actual = hex(&Sha1::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected_manifest_sha1) {
        return Err(RuntimeManifestError::ManifestDigestMismatch);
    }
    let root: Value =
        serde_json::from_slice(bytes).map_err(|_| RuntimeManifestError::InvalidJson)?;
    let file_map = root
        .get("files")
        .and_then(Value::as_object)
        .ok_or(RuntimeManifestError::InvalidFiles)?;
    if file_map.len() > MAX_RUNTIME_FILES {
        return Err(RuntimeManifestError::TooManyFiles);
    }

    let mut files = Vec::with_capacity(file_map.len());
    let mut total_bytes = 0u64;
    let mut has_java = false;
    let mut has_javaw = false;
    for (path, entry) in file_map {
        validate_relative_path(path)?;
        let kind = entry
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeManifestError::InvalidFile(path.clone()))?;
        let file = match kind {
            "directory" => RuntimeFile::Directory { path: path.clone() },
            "link" => {
                let target = entry
                    .get("target")
                    .and_then(Value::as_str)
                    .filter(|target| !target.is_empty())
                    .ok_or_else(|| RuntimeManifestError::InvalidFile(path.clone()))?;
                validate_link_target(path, target)?;
                RuntimeFile::Link {
                    path: path.clone(),
                    target: target.to_owned(),
                }
            }
            "file" => {
                let raw = entry
                    .pointer("/downloads/raw")
                    .ok_or_else(|| RuntimeManifestError::InvalidFile(path.clone()))?;
                let sha1 = raw
                    .get("sha1")
                    .and_then(Value::as_str)
                    .filter(|digest| is_hex_digest(digest, 40))
                    .ok_or_else(|| RuntimeManifestError::InvalidFile(path.clone()))?
                    .to_ascii_lowercase();
                let size = raw
                    .get("size")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| RuntimeManifestError::InvalidFile(path.clone()))?;
                let url = raw
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|url| is_allowed_download_url(url))
                    .ok_or_else(|| RuntimeManifestError::UnsafeUrl(path.clone()))?
                    .to_owned();
                total_bytes = total_bytes
                    .checked_add(size)
                    .filter(|total| *total <= MAX_RUNTIME_TOTAL_BYTES)
                    .ok_or(RuntimeManifestError::TooLargeRuntime)?;
                has_java |= path.eq_ignore_ascii_case("bin/java.exe");
                has_javaw |= path.eq_ignore_ascii_case("bin/javaw.exe");
                RuntimeFile::File {
                    path: path.clone(),
                    url,
                    sha1,
                    size,
                    executable: entry
                        .get("executable")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }
            }
            unsupported => {
                return Err(RuntimeManifestError::UnsupportedFileType(
                    unsupported.to_owned(),
                ));
            }
        };
        files.push(file);
    }
    if platform.starts_with("windows-") && !has_java {
        return Err(RuntimeManifestError::MissingJavaExecutable);
    }

    Ok(RuntimeInstallPlan {
        files,
        total_bytes,
        java_executable: if platform.starts_with("windows-") {
            "bin/java.exe".to_owned()
        } else {
            "bin/java".to_owned()
        },
        javaw_executable: has_javaw.then(|| "bin/javaw.exe".to_owned()),
    })
}

fn parse_major(version: &str) -> Option<u32> {
    let version = version.strip_prefix("1.").unwrap_or(version);
    let digits = version.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0)
        .then(|| version[..digits].parse().ok())
        .flatten()
}

fn is_hex_digest(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_allowed_download_url(url: &str) -> bool {
    let Some(authority_and_path) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = authority_and_path.split('/').next().unwrap_or_default();
    !authority.contains('@')
        && !authority.contains(':')
        && ALLOWED_DOWNLOAD_HOSTS.contains(&authority.to_ascii_lowercase().as_str())
        && authority_and_path.contains('/')
}

fn validate_relative_path(path: &str) -> Result<(), RuntimeManifestError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path.contains('\0')
        || path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(RuntimeManifestError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

fn validate_link_target(path: &str, target: &str) -> Result<(), RuntimeManifestError> {
    if target.starts_with('/')
        || target.contains('\\')
        || target.contains(':')
        || target.contains('\0')
    {
        return Err(RuntimeManifestError::UnsafePath(path.to_owned()));
    }
    let parent = path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or_default();
    let mut depth = parent.split('/').filter(|part| !part.is_empty()).count() as isize;
    for component in target.split('/') {
        match component {
            "" | "." => return Err(RuntimeManifestError::UnsafePath(path.to_owned())),
            ".." => depth -= 1,
            _ => depth += 1,
        }
        if depth < 0 {
            return Err(RuntimeManifestError::UnsafePath(path.to_owned()));
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_java_version_names_use_the_feature_version() {
        assert_eq!(parse_major("1.8.0_51"), Some(8));
        assert_eq!(parse_major("8u51-cacert462b08"), Some(8));
        assert_eq!(parse_major("21.0.7"), Some(21));
        assert_eq!(parse_major("unknown"), None);
    }

    fn catalog() -> Vec<u8> {
        br#"{"windows-x64":{"java-runtime-delta":[{"version":{"name":"21.0.7","released":"2025-05-19T08:34:42+00:00"},"manifest":{"sha1":"cb4394a27089d19f65d5baa6cf0482c27c3c7865","size":148529,"url":"https://piston-meta.mojang.com/v1/packages/cb4394a27089d19f65d5baa6cf0482c27c3c7865/manifest.json"}}],"java-runtime-gamma":[{"version":{"name":"17.0.15","released":"2025-05-19T08:28:47+00:00"},"manifest":{"sha1":"89ce85ccb518c62e18b4b58d63399ba2d9611426","size":148571,"url":"https://piston-meta.mojang.com/v1/packages/89ce85ccb518c62e18b4b58d63399ba2d9611426/manifest.json"}}]}}"#.to_vec()
    }

    fn runtime_files() -> Vec<u8> {
        br#"{"files":{"bin":{"type":"directory"},"bin/java.exe":{"type":"file","executable":true,"downloads":{"raw":{"sha1":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":10,"url":"https://piston-data.mojang.com/v1/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/java.exe"}}},"bin/javaw.exe":{"type":"file","executable":true,"downloads":{"raw":{"sha1":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size":12,"url":"https://piston-data.mojang.com/v1/objects/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/javaw.exe"}}}}}"#.to_vec()
    }

    #[test]
    fn selects_compatible_runtime_and_validates_bounded_file_plan() {
        let selected = select_release(&catalog(), "windows-x64", 21).unwrap();
        assert_eq!(selected.family, "java-runtime-delta");
        assert_eq!(selected.version, "21.0.7");

        let bytes = runtime_files();
        let digest = hex(&Sha1::digest(&bytes));
        let plan = parse_install_plan(&bytes, &digest, "windows-x64").unwrap();
        assert_eq!(plan.files.len(), 3);
        assert_eq!(plan.total_bytes, 22);
        assert_eq!(plan.java_executable, "bin/java.exe");
        assert_eq!(plan.javaw_executable.as_deref(), Some("bin/javaw.exe"));
    }

    #[test]
    fn rejects_wrong_catalog_digest_traversal_and_non_tls_files() {
        let bytes = runtime_files();
        assert_eq!(
            parse_install_plan(&bytes, &"0".repeat(40), "windows-x64"),
            Err(RuntimeManifestError::ManifestDigestMismatch)
        );
        let unsafe_path = br#"{"files":{"../outside":{"type":"directory"}}}"#;
        let digest = hex(&Sha1::digest(unsafe_path));
        assert!(matches!(
            parse_install_plan(unsafe_path, &digest, "windows-x64"),
            Err(RuntimeManifestError::UnsafePath(_))
        ));
        assert!(matches!(
            select_release(&catalog(), "windows-x64", 8),
            Err(RuntimeManifestError::NoCompatibleRuntime { .. })
        ));

        let non_tls = String::from_utf8(runtime_files()).unwrap().replace(
            "https://piston-data.mojang.com",
            "http://piston-data.mojang.com",
        );
        let digest = hex(&Sha1::digest(non_tls.as_bytes()));
        assert!(matches!(
            parse_install_plan(non_tls.as_bytes(), &digest, "windows-x64"),
            Err(RuntimeManifestError::UnsafeUrl(_))
        ));

        let escaping_link =
            br#"{"files":{"bin/outside":{"type":"link","target":"../../outside"}}}"#;
        let digest = hex(&Sha1::digest(escaping_link));
        assert!(matches!(
            parse_install_plan(escaping_link, &digest, "windows-x64"),
            Err(RuntimeManifestError::UnsafePath(_))
        ));
    }

    #[test]
    #[ignore = "contacts the official Mojang runtime metadata service"]
    fn official_windows_x64_runtime_manifest_parses_with_catalog_digest() {
        use std::io::Read;

        fn bounded_get(url: &str, limit: usize) -> Vec<u8> {
            let client = reqwest::blocking::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client");
            let response = client.get(url).send().expect("official metadata request");
            assert!(
                response.status().is_success(),
                "status {}",
                response.status()
            );
            let mut body = Vec::new();
            response
                .take(limit as u64 + 1)
                .read_to_end(&mut body)
                .expect("bounded metadata body");
            assert!(body.len() <= limit, "metadata exceeds limit");
            body
        }

        let catalog = bounded_get(RUNTIME_CATALOG_URL, MAX_RUNTIME_CATALOG_BYTES);
        let release = select_release(&catalog, "windows-x64", 21).expect("Java 21 runtime");
        let manifest = bounded_get(&release.manifest_url, MAX_RUNTIME_MANIFEST_BYTES);
        let plan = parse_install_plan(&manifest, &release.manifest_sha1, "windows-x64")
            .expect("verified runtime file list");
        println!(
            "platform=windows-x64 family={} version={} manifest_bytes={} files={} total_bytes={} java={} javaw={}",
            release.family,
            release.version,
            manifest.len(),
            plan.files.len(),
            plan.total_bytes,
            plan.java_executable,
            plan.javaw_executable.as_deref().unwrap_or("missing")
        );
        assert_eq!(release.version, "21.0.7");
        assert_eq!(manifest.len() as u64, release.manifest_size);
        assert!(plan.files.len() > 100);
        assert!(plan.total_bytes > 50 * 1024 * 1024);
        assert_eq!(plan.java_executable, "bin/java.exe");
    }
}

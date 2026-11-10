//! Runtime compatibility and install-source boundary.
//!
//! Java discovery/probing is implemented in `super::java`; this module maps
//! official version metadata to requirements and describes verified install
//! payloads. A network adapter is intentionally injected by L03/L00.

pub mod manifest;

use std::path::Path;

use sha2::{Digest, Sha256};

use super::catalog::VersionMetadata;
use super::java::{Arch, HostPlatform, JavaRequirement, JavaRuntime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementSource {
    OfficialMetadata,
    KnownVersionFallback,
    UnknownVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRequirement {
    pub java: JavaRequirement,
    pub source: RequirementSource,
    pub reason: Option<&'static str>,
}

/// Prefer the authoritative `javaVersion.majorVersion` from the resolved game
/// metadata. Keep an explicit, visible fallback only for older version files.
pub fn requirement_for(metadata: &VersionMetadata) -> RuntimeRequirement {
    if let Some(major) = metadata.java_major.filter(|major| *major > 0) {
        return RuntimeRequirement {
            java: JavaRequirement::AtLeast(major),
            source: RequirementSource::OfficialMetadata,
            reason: None,
        };
    }
    let (java, verified) = super::java::requirement_for_game_version(&metadata.id);
    RuntimeRequirement {
        java,
        source: if verified {
            RequirementSource::KnownVersionFallback
        } else {
            RequirementSource::UnknownVersion
        },
        reason: if verified {
            Some("java_version_metadata_missing_fallback_used")
        } else {
            Some("java_version_metadata_missing_unknown_version")
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeCompatibility {
    Compatible,
    WrongPlatform,
    WrongArchitecture,
    WrongJavaVersion,
    Damaged,
}

pub fn check_runtime(
    runtime: &JavaRuntime,
    platform: HostPlatform,
    arch: Arch,
    requirement: &JavaRequirement,
    executable_exists: bool,
) -> RuntimeCompatibility {
    if !executable_exists {
        return RuntimeCompatibility::Damaged;
    }
    if runtime.platform != platform {
        return RuntimeCompatibility::WrongPlatform;
    }
    if runtime.arch != arch {
        return RuntimeCompatibility::WrongArchitecture;
    }
    if !requirement.matches(runtime.version.major) {
        return RuntimeCompatibility::WrongJavaVersion;
    }
    RuntimeCompatibility::Compatible
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeArchive {
    pub vendor: String,
    pub version: String,
    pub platform: HostPlatform,
    pub arch: Arch,
    pub download_url: String,
    pub sha256: String,
    pub archive_format: String,
    pub relative_java_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeInstallError {
    InvalidDigest,
    DigestMismatch,
    UnsafeExecutablePath,
    Io(String),
}

/// The transport is supplied by the download subsystem. The installer accepts
/// only bytes that match the signed metadata digest and an executable path
/// strictly contained in the staged extraction directory.
pub fn verify_archive(bytes: &[u8], expected_sha256: &str) -> Result<(), RuntimeInstallError> {
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(RuntimeInstallError::InvalidDigest);
    }
    let digest = Sha256::digest(bytes);
    let actual = format!("{digest:x}");
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(RuntimeInstallError::DigestMismatch);
    }
    Ok(())
}

pub fn executable_path(
    staging_directory: &Path,
    relative: &Path,
) -> Result<std::path::PathBuf, RuntimeInstallError> {
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::Prefix(_)
                    | std::path::Component::RootDir
            )
        })
    {
        return Err(RuntimeInstallError::UnsafeExecutablePath);
    }
    let result = staging_directory.join(relative);
    if !result.starts_with(staging_directory) {
        return Err(RuntimeInstallError::UnsafeExecutablePath);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::java::{JavaVendor, JavaVersion};
    use shtmcl_platform::fs::FileIdentity;

    fn runtime(major: u32, arch: Arch) -> JavaRuntime {
        JavaRuntime {
            id: "fixture-java".into(),
            path: Path::new("runtime/bin/java").into(),
            platform: HostPlatform::Windows,
            arch,
            vendor: JavaVendor::Temurin,
            version: JavaVersion {
                major,
                minor: 0,
                security: 0,
                update: None,
                raw: major.to_string(),
            },
            verified_at_unix_ms: 1,
            file_identity: FileIdentity {
                len: 10,
                modified_unix_ms: Some(1),
            },
        }
    }

    #[test]
    fn official_requirement_wins_and_legacy_fallback_is_disclosed() {
        let official = VersionMetadata::from_value(
            serde_json::json!({"id":"1.20.1","javaVersion":{"majorVersion":17}}),
            None,
        )
        .unwrap();
        assert_eq!(
            requirement_for(&official).source,
            RequirementSource::OfficialMetadata
        );
        assert!(matches!(
            requirement_for(&official).java,
            JavaRequirement::AtLeast(17)
        ));
        let historical =
            VersionMetadata::from_value(serde_json::json!({"id":"1.20.1"}), None).unwrap();
        assert_eq!(
            requirement_for(&historical).source,
            RequirementSource::KnownVersionFallback
        );
    }

    #[test]
    fn architecture_java_version_and_damaged_runtime_have_distinct_results() {
        assert_eq!(
            check_runtime(
                &runtime(21, Arch::X64),
                HostPlatform::Windows,
                Arch::X64,
                &JavaRequirement::AtLeast(17),
                true
            ),
            RuntimeCompatibility::Compatible
        );
        assert_eq!(
            check_runtime(
                &runtime(21, Arch::X86),
                HostPlatform::Windows,
                Arch::X64,
                &JavaRequirement::AtLeast(17),
                true
            ),
            RuntimeCompatibility::WrongArchitecture
        );
        assert_eq!(
            check_runtime(
                &runtime(8, Arch::X64),
                HostPlatform::Windows,
                Arch::X64,
                &JavaRequirement::AtLeast(17),
                true
            ),
            RuntimeCompatibility::WrongJavaVersion
        );
        assert_eq!(
            check_runtime(
                &runtime(21, Arch::X64),
                HostPlatform::Windows,
                Arch::X64,
                &JavaRequirement::AtLeast(17),
                false
            ),
            RuntimeCompatibility::Damaged
        );
    }

    #[test]
    fn archive_digest_and_install_path_are_checked() {
        let bytes = b"archive";
        let digest = format!("{:x}", Sha256::digest(bytes));
        assert_eq!(verify_archive(bytes, &digest), Ok(()));
        assert_eq!(
            verify_archive(b"different", &digest),
            Err(RuntimeInstallError::DigestMismatch)
        );
        assert_eq!(
            executable_path(Path::new("data/runtime.tmp"), Path::new("bin/java.exe")).unwrap(),
            Path::new("data/runtime.tmp/bin/java.exe")
        );
        assert_eq!(
            executable_path(Path::new("data/runtime.tmp"), Path::new("../java.exe")),
            Err(RuntimeInstallError::UnsafeExecutablePath)
        );
    }
}

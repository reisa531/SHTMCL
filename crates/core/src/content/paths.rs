//! Content path normalisation and the documented parse budgets.
//!
//! Every path that comes from a pack, a lock, an archive entry or an instance
//! snapshot passes through [`normalise_content_path`] before it can reach a plan.
//! The rules exist because pack archives are untrusted input: they must not write
//! outside the instance root, collide on case-insensitive filesystems, or smuggle
//! Windows reserved names.

use super::error::ContentError;

/// `docs/development/04-content-and-modpacks.md` §6 initial archive budgets.
///
/// The entry ceiling equals the observable end-of-central-directory limit (a u16
/// count) because zip64 is rejected: a larger number would be a budget that no
/// input can ever reach.
pub const MAX_ARCHIVE_ENTRIES: usize = u16::MAX as usize;
pub const MAX_PATH_DEPTH: usize = 32;
pub const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_TOTAL_UNCOMPRESSED: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_ENTRY_UNCOMPRESSED: u64 = 512 * 1024 * 1024;
pub const MAX_PATH_BYTES: usize = 512;

const RESERVED_NAMES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Paths that must never leave the machine when a pack is exported.
const EXPORT_EXCLUDED_PREFIXES: [&str; 6] = [
    "logs/",
    "crash-reports/",
    ".microsoft/",
    "launcher_accounts/",
    "screenshots/",
    "debug/",
];

const EXPORT_EXCLUDED_FILES: [&str; 5] = [
    "usercache.json",
    "launcher_profiles.json",
    "launcher_accounts.json",
    "realms_persistence.json",
    "launcher_accounts_microsoft_store.json",
];

/// Normalise a pack-relative path to `/`-separated form, or reject it.
pub fn normalise_content_path(raw: &str) -> Result<String, ContentError> {
    if raw.is_empty() {
        return Err(ContentError::PathEscape {
            path: raw.to_string(),
        });
    }
    if raw.len() > MAX_PATH_BYTES {
        return Err(ContentError::PathEscape {
            path: raw.to_string(),
        });
    }
    if raw.contains('\\') {
        // A backslash is a separator on Windows and an escape on other systems;
        // packs must use `/` so the target path stays unambiguous.
        return Err(ContentError::PathEscape {
            path: raw.to_string(),
        });
    }
    if raw.starts_with('/') || raw.starts_with('~') {
        return Err(ContentError::PathEscape {
            path: raw.to_string(),
        });
    }
    if raw.len() > 1 && raw.as_bytes().get(1) == Some(&b':') {
        return Err(ContentError::PathEscape {
            path: raw.to_string(),
        });
    }
    if raw.contains(':') {
        // A colon anywhere opens an NTFS alternate data stream (`a.jar:stream`),
        // which would land beside the real file instead of replacing it.
        return Err(ContentError::PathEscape {
            path: raw.to_string(),
        });
    }
    if raw.chars().any(|ch| ch.is_control()) {
        return Err(ContentError::PathEscape {
            path: raw.to_string(),
        });
    }

    let mut parts: Vec<&str> = Vec::new();
    for component in raw.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err(ContentError::PathEscape {
                path: raw.to_string(),
            });
        }
        if component.ends_with(' ') || component.ends_with('.') {
            // Windows silently strips these, which would create two names for one
            // file and let a second entry overwrite the first.
            return Err(ContentError::PathEscape {
                path: raw.to_string(),
            });
        }
        let stem = component
            .split('.')
            .next()
            .unwrap_or(component)
            .to_ascii_lowercase();
        if RESERVED_NAMES.contains(&stem.as_str()) {
            return Err(ContentError::PathEscape {
                path: raw.to_string(),
            });
        }
        parts.push(component);
    }

    if parts.is_empty() {
        return Err(ContentError::PathEscape {
            path: raw.to_string(),
        });
    }
    if parts.len() > MAX_PATH_DEPTH {
        return Err(ContentError::BudgetExceeded {
            budget: "path_depth",
            limit: MAX_PATH_DEPTH as u64,
            observed: parts.len() as u64,
        });
    }
    Ok(parts.join("/"))
}

/// Case-insensitive collision key used for duplicate detection.
pub fn collision_key(normalised: &str) -> String {
    normalised.to_lowercase()
}

/// `true` when a path must be excluded from an export candidate set even if the
/// user selects "everything": credentials, launcher state and logs never travel.
pub fn is_excluded_from_export(normalised: &str) -> bool {
    let lower = normalised.to_lowercase();
    if EXPORT_EXCLUDED_FILES.iter().any(|name| lower == *name) {
        return true;
    }
    if EXPORT_EXCLUDED_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return true;
    }
    lower.ends_with(".log") || lower.ends_with(".log.gz")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_nested_pack_paths() {
        assert_eq!(
            normalise_content_path("mods/sodium-0.5.3.jar").expect("ok"),
            "mods/sodium-0.5.3.jar"
        );
        assert_eq!(
            normalise_content_path("./config/a/b.json").expect("ok"),
            "config/a/b.json"
        );
    }

    #[test]
    fn rejects_traversal_absolute_and_windows_escapes() {
        for bad in [
            "../outside.jar",
            "mods/../../outside.jar",
            "/etc/passwd",
            "C:/windows/system32/x.jar",
            "mods\\evil.jar",
            "mods/evil.jar ",
            "mods/con.jar",
            "",
            "   ",
            // NTFS alternate data stream: would create `a.jar` plus a hidden stream.
            "mods/a.jar:stream",
        ] {
            assert!(
                normalise_content_path(bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_over_deep_paths() {
        let deep = vec!["a"; MAX_PATH_DEPTH + 1].join("/");
        assert!(matches!(
            normalise_content_path(&deep),
            Err(ContentError::BudgetExceeded { .. })
        ));
    }

    #[test]
    fn excludes_credentials_and_logs_from_exports() {
        assert!(is_excluded_from_export("logs/latest.log"));
        assert!(is_excluded_from_export("usercache.json"));
        assert!(is_excluded_from_export(".microsoft/account.json"));
        assert!(!is_excluded_from_export("mods/sodium.jar"));
        assert!(!is_excluded_from_export("config/fabric.json"));
    }
}

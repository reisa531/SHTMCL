//! Bounded, read-only local Mod inventory for a single instance.
//!
//! The scan never executes, writes, or touches the network. It only enumerates
//! immediate regular files under `<root>/mods`, reports `.jar` / `.jar.disabled`
//! case-insensitively, and returns an explicit status so callers can distinguish
//! a genuinely empty directory from an unknown or oversized one.
use serde_json::{json, Value};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

/// Upper bound on directory entries inspected during a single scan.
const MAX_ENUMERATED: usize = 4096;
/// Upper bound on entries returned to the caller.
const MAX_RETURNED: usize = 512;

fn unavailable() -> Value {
    json!({ "status": "unavailable", "files": [] })
}

fn limited() -> Value {
    json!({ "status": "limited", "files": [] })
}

/// Every ancestor of `path`, including `path` itself, must be an existing symlink
/// free path. A missing tail is tolerated: ancestors that do exist are checked.
fn ancestors_checked(path: &Path) -> bool {
    let mut any = false;
    for part in path.ancestors() {
        any = true;
        if crate::game_install::reject_link(part).is_err() {
            return false;
        }
    }
    // `ancestors` always yields at least the path itself.
    any
}

/// Classify an entry file name. Only `.jar` (enabled) and `.jar.disabled`
/// (disabled) are accepted, case-insensitively. Returns `None` for ignored files.
fn classify(name: &str) -> Option<bool> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".jar.disabled") {
        // Base name must be non-empty before the `.jar.disabled` suffix.
        let stem = &name[..name.len() - ".jar.disabled".len()];
        if stem.is_empty() {
            return None;
        }
        Some(false)
    } else if lower.ends_with(".jar") {
        let stem = &name[..name.len() - ".jar".len()];
        if stem.is_empty() {
            return None;
        }
        Some(true)
    } else {
        None
    }
}

/// Validate a raw file name for presentation: valid UTF-8, no control chars,
/// no separators, and at most 255 characters.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 255
        && !name.chars().any(|c| c.is_control())
        && !name.contains('/')
        && !name.contains('\\')
}

/// Scan `<root>/mods` and produce a stable, read-only inventory description.
///
/// * `root` must be an existing, real (non-symlink, non-reparse) directory.
/// * A genuinely missing `mods` directory yields `ready` with an empty list.
/// * Permission or I/O errors yield `unavailable` rather than a false empty.
/// * An over-large directory yields `limited`.
pub(crate) fn scan(root: &Path) -> Value {
    // Root must be a real directory with link-free ancestors.
    if !root.is_dir() || !ancestors_checked(root) {
        return unavailable();
    }

    let mods = root.join("mods");
    if !ancestors_checked(&mods) {
        return unavailable();
    }

    let entries = match fs::read_dir(&mods) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Only a true missing path is a legitimate empty inventory.
            return json!({ "status": "ready", "files": [] });
        }
        Err(_) => return unavailable(),
    };

    let mut files: Vec<(String, bool, u64)> = Vec::new();
    let mut enumerated = 0usize;
    for entry in entries {
        if enumerated >= MAX_ENUMERATED {
            return limited();
        }
        enumerated += 1;

        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return unavailable(),
        };

        let path: PathBuf = entry.path();

        // Reject symlinks / reparse points on the entry itself.
        if crate::game_install::reject_link(&path).is_err() {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => return unavailable(),
        };
        if !file_type.is_file() {
            // Directories, sockets, etc. are ignored; only regular files count.
            continue;
        }

        let entry_name = entry.file_name();
        let file_name: &OsStr = entry_name.as_os_str();
        let name = match file_name.to_str() {
            Some(name) => name,
            None => return unavailable(),
        };
        if !valid_name(name) {
            return unavailable();
        }
        let enabled = match classify(name) {
            Some(enabled) => enabled,
            None => continue,
        };

        let bytes = match entry.metadata() {
            Ok(metadata) => metadata.len(),
            Err(_) => return unavailable(),
        };

        files.push((name.to_owned(), enabled, bytes));
    }

    if files.len() > MAX_RETURNED {
        return limited();
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));

    let items: Vec<Value> = files
        .into_iter()
        .map(|(file_name, enabled, bytes)| {
            json!({ "fileName": file_name, "enabled": enabled, "bytes": bytes })
        })
        .collect();

    json!({ "status": "ready", "files": items })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shtmcl-mod-inventory-{tag}-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn file_names(value: &Value) -> Vec<String> {
        value["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["fileName"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn known_disabled_uppercase_and_ignored_nested() {
        let root = scratch("mixed");
        let mods = root.join("mods");
        fs::create_dir_all(mods.join("nested")).unwrap();
        for name in ["a.jar", "B.JAR", "c.Jar.Disabled", "notes.txt"] {
            fs::write(mods.join(name), b"fixture").unwrap();
        }
        fs::write(mods.join("nested").join("ignored.jar"), b"x").unwrap();
        fs::create_dir_all(mods.join("adir.jar")).unwrap();

        let value = scan(&root);
        assert_eq!(value["status"], "ready");
        let names = file_names(&value);
        assert_eq!(names, vec!["B.JAR", "a.jar", "c.Jar.Disabled"]);
        let files = value["files"].as_array().unwrap();
        let enabled = |name: &str| -> bool {
            files
                .iter()
                .find(|item| item["fileName"] == name)
                .unwrap()["enabled"]
                .as_bool()
                .unwrap()
        };
        assert!(enabled("a.jar"));
        assert!(enabled("B.JAR"));
        assert!(!enabled("c.Jar.Disabled"));
        for item in files {
            assert_eq!(item["bytes"], 7);
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_mods_dir_is_ready_empty() {
        let root = scratch("missing");
        let value = scan(&root);
        assert_eq!(value["status"], "ready");
        assert!(value["files"].as_array().unwrap().is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn invalid_root_is_unavailable() {
        let missing = std::env::temp_dir().join(format!(
            "shtmcl-mod-inventory-absent-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        let value = scan(&missing);
        assert_eq!(value["status"], "unavailable");
        assert!(value["files"].as_array().unwrap().is_empty());
    }

    #[test]
    fn oversized_directory_is_limited() {
        let root = scratch("oversize");
        let mods = root.join("mods");
        fs::create_dir_all(&mods).unwrap();
        for index in 0..(MAX_RETURNED + 16) {
            fs::write(mods.join(format!("mod-{index:04}.jar")), b"z").unwrap();
        }
        let value = scan(&root);
        assert_eq!(value["status"], "limited");
        assert!(value["files"].as_array().unwrap().is_empty());
        fs::remove_dir_all(&root).unwrap();
    }
}

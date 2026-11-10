use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use shtmcl_platform::{probe_file, safe_join, PathError, PlatformDirs};

#[test]
fn safe_join_accepts_nested_relative_paths() {
    let joined = safe_join(Path::new("base"), Path::new("a/b.txt")).expect("relative path");
    assert_eq!(joined, PathBuf::from("base").join("a").join("b.txt"));
}

#[test]
fn safe_join_rejects_absolute_and_parent_paths() {
    assert_eq!(
        safe_join(Path::new("base"), Path::new("../escape")),
        Err(PathError::EscapesBase)
    );
    assert_eq!(
        safe_join(Path::new("base"), Path::new("a/../../escape")),
        Err(PathError::EscapesBase)
    );
}

#[test]
fn platform_dirs_create_the_expected_layout() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "shtmcl-platform-test-{}-{nonce}",
        std::process::id()
    ));
    let dirs = PlatformDirs::under(&root);
    dirs.create_all().expect("create platform directories");
    assert!(dirs.data.is_dir());
    assert!(dirs.config.is_dir());
    assert!(dirs.cache.is_dir());
    assert!(dirs.logs.is_dir());
    assert!(dirs.instances.is_dir());
    std::fs::remove_dir_all(&root).expect("clean up test directories");
}

#[test]
fn probes_an_existing_file() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let identity = probe_file(&path).expect("probe manifest");
    assert!(identity.len > 0);
    assert!(identity.modified_unix_ms.is_some());
}

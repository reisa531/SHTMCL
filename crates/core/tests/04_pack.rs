//! 04 · modpack parsing (`.mrpack`), override semantics, plan mapping and the
//! export format.
//!
//! The `.mrpack` files under `tests/modules/04/fixtures/` are real stored-zip
//! archives produced by `node tests/modules/04/make-fixtures.mjs`; the JS test in
//! `tests/modules/04/modpack.test.mjs` cross-checks the same bytes.

use std::path::PathBuf;

use shtmcl_core::content::loader::{GameTarget, LoaderKind};
use shtmcl_core::content::modpack::{
    build_modrinth_index, classify_override, parse_modrinth_index, parse_modrinth_index_value,
    plan_export, read_mrpack, ExportCandidate, Hashes, OverrideScope, PackFormat, SideSupport,
};
use shtmcl_core::content::plan::{plan_changes, ContentLock, InstanceContentView};
use shtmcl_domain::{InstanceId, Revision};

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/modules/04/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read {}: {error}; run `node tests/modules/04/make-fixtures.mjs`",
            path.display()
        )
    })
}

fn view(files: Vec<(&str, &str)>) -> InstanceContentView {
    InstanceContentView::new(
        InstanceId::new("instance-1").expect("instance id"),
        Revision::new(1),
        files
            .into_iter()
            .map(|(path, digest)| shtmcl_core::content::plan::InstanceFile::new(path, digest, 10))
            .collect(),
    )
}

#[test]
fn reads_the_committed_mrpack_fixture() {
    let manifest = read_mrpack(&fixture("sample.mrpack")).expect("manifest");
    assert_eq!(manifest.name, "SHTMCL fixture pack");
    assert_eq!(manifest.version_id, "1.0.0");
    assert_eq!(manifest.files.len(), 2);
    assert_eq!(manifest.restricted_count, 0);

    let target = manifest.target().expect("target");
    assert_eq!(target.game_version, "1.20.1");
    assert_eq!(target.loader, LoaderKind::Fabric);
    assert_eq!(target.loader_version.as_deref(), Some("0.15.11"));

    // overrides/ + client-overrides/ + server-overrides/ all land on instance paths.
    let mut overrides: Vec<(OverrideScope, String)> = manifest
        .overrides
        .iter()
        .map(|entry| (entry.scope, entry.target_path.clone()))
        .collect();
    overrides.sort_by(|left, right| left.1.cmp(&right.1));
    assert_eq!(
        overrides,
        vec![
            (OverrideScope::Client, "config/client-only.json".to_string()),
            (OverrideScope::Server, "config/server-only.json".to_string()),
            (
                OverrideScope::Shared,
                "config/sodium-options.json".to_string()
            ),
        ]
    );

    // Client-side installation targets exclude the server-only override.
    let client_paths: Vec<String> = manifest
        .targets(shtmcl_core::content::modpack::PackSide::Client)
        .into_iter()
        .map(|target| target.path)
        .collect();
    assert_eq!(
        client_paths,
        vec![
            "config/client-only.json".to_string(),
            "config/sodium-options.json".to_string(),
            "mods/lithium-0.11.2.jar".to_string(),
            "mods/sodium-0.5.3.jar".to_string(),
        ]
    );
}

#[test]
fn rejects_an_archive_entry_that_escapes_the_root() {
    let error = read_mrpack(&fixture("traversal.mrpack")).expect_err("traversal must fail");
    assert_eq!(error.code(), "content.pack.path_escape");
    assert!(error.to_envelope().detail_ref.is_some());
}

#[test]
fn reads_the_deflated_mrpack_fixture() {
    // Real `.mrpack` archives are deflated; the reader must inflate under budget.
    let manifest = read_mrpack(&fixture("sample-deflate.mrpack")).expect("manifest");
    assert_eq!(manifest.name, "SHTMCL fixture pack");
    assert_eq!(manifest.files.len(), 2);
    assert_eq!(manifest.overrides.len(), 3);
}

#[test]
fn rejects_a_manifest_with_missing_required_fields() {
    let error = read_mrpack(&fixture("incomplete.mrpack")).expect_err("missing fields");
    assert_eq!(error.code(), "content.pack.invalid_manifest");
    assert!(error.to_string().contains("name") || error.to_string().contains("versionId"));
}

#[test]
fn rejects_manifest_level_path_escapes_and_absolute_paths() {
    for bad in [
        "../escape.jar",
        "/etc/passwd",
        "mods/../../escape.jar",
        "C:/evil.jar",
    ] {
        let text = format!(
            r#"{{"formatVersion":1,"game":"minecraft","name":"t","versionId":"1",
                 "files":[{{"path":"{bad}","hashes":{{"sha1":"{}"}},
                 "downloads":["https://example.invalid/a.jar"]}}],
                 "dependencies":{{"minecraft":"1.20.1"}}}}"#,
            "a".repeat(40)
        );
        let error = parse_modrinth_index(&text).expect_err("path must be rejected");
        assert_eq!(
            error.code(),
            "content.pack.path_escape",
            "expected rejection for {bad}"
        );
    }
}

#[test]
fn rejects_a_manifest_file_without_a_usable_hash() {
    let text = r#"{"formatVersion":1,"game":"minecraft","name":"t","versionId":"1",
        "files":[{"path":"mods/a.jar","hashes":{},"downloads":[]}],
        "dependencies":{"minecraft":"1.20.1"}}"#;
    let error = parse_modrinth_index(text).expect_err("hash required");
    assert_eq!(error.code(), "content.pack.invalid_manifest");
}

#[test]
fn duplicate_entries_are_deduped_or_rejected_by_identity() {
    let document = |first: &str, second: &str| {
        serde_json::json!({
            "formatVersion": 1,
            "game": "minecraft",
            "name": "t",
            "versionId": "1",
            "files": [
                {
                    "path": "mods/a.jar",
                    "hashes": { "sha1": first },
                    "downloads": ["https://example.invalid/a.jar"]
                },
                {
                    "path": "MODS/A.jar",
                    "hashes": { "sha1": second },
                    "downloads": ["https://example.invalid/a.jar"]
                }
            ],
            "dependencies": { "minecraft": "1.20.1" }
        })
    };
    let same = "a".repeat(40);
    let deduped =
        parse_modrinth_index_value(&document(&same, &same)).expect("identical entries collapse");
    assert_eq!(deduped.files.len(), 1);

    let error = parse_modrinth_index_value(&document(&same, &"b".repeat(40)))
        .expect_err("case-insensitive duplicate with another identity");
    assert_eq!(error.code(), "content.pack.duplicate_path");
}

#[test]
fn lock_and_plan_map_the_manifest_onto_instance_changes() {
    let manifest = read_mrpack(&fixture("sample.mrpack")).expect("manifest");
    let lock =
        ContentLock::from_manifest(&manifest, shtmcl_core::content::modpack::PackSide::Client)
            .expect("lock");
    assert_eq!(
        lock.files.len(),
        4,
        "2 downloads + 2 client-visible overrides"
    );

    // Already-present mod is not relisted; the rest is added.
    let current = view(vec![("mods/sodium-0.5.3.jar", &"a".repeat(40))]);
    let plan = plan_changes("plan-1", &current, &lock).expect("plan");
    let actions: Vec<(String, &str)> = plan
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.action.as_str()))
        .collect();
    assert_eq!(
        actions,
        vec![
            ("config/client-only.json".to_string(), "add"),
            ("config/sodium-options.json".to_string(), "add"),
            ("mods/lithium-0.11.2.jar".to_string(), "add"),
        ],
        "unchanged files stay out of the confirmation list"
    );
    assert_eq!(plan.counts.add, 3);
    assert_eq!(plan.counts.delete, 0);

    // Determinism: the same lock built in a different order gives one plan digest.
    let reversed = ContentLock::new(
        lock.target.clone(),
        lock.side,
        lock.files.iter().rev().cloned().collect(),
    )
    .expect("lock");
    let again = plan_changes("plan-1", &current, &reversed).expect("plan");
    assert_eq!(plan.plan_digest, again.plan_digest);
    assert_eq!(plan.entries, again.entries);
}

#[test]
fn export_round_trips_and_drops_credentials_and_logs() {
    let sha = "c".repeat(40);
    let candidate = |path: &str| ExportCandidate {
        path: path.to_string(),
        hashes: Hashes {
            sha1: Some(sha.clone()),
            sha512: None,
            sha256: None,
        },
        downloads: vec!["https://example.invalid/f.jar".to_string()],
        file_size: Some(123),
        client: SideSupport::Required,
        server: SideSupport::Optional,
    };
    let spec = plan_export(
        PackFormat::Modrinth,
        "Export fixture",
        "2.0.0",
        GameTarget::new("1.20.1", LoaderKind::Fabric).with_loader_version("0.15.11"),
        vec![
            candidate("mods/sodium-0.5.3.jar"),
            candidate("logs/latest.log"),
            candidate("usercache.json"),
            candidate(".microsoft/account.json"),
            candidate("saves/world/level.dat"),
            candidate("mods/SODIUM-0.5.3.jar"),
        ],
        false,
    )
    .expect("export plan");

    assert_eq!(spec.files.len(), 1, "only the mod survives the filters");
    assert!(spec
        .excluded
        .iter()
        .any(|(path, reason)| path == "logs/latest.log" && *reason == "credentials_or_logs"));
    assert!(spec
        .excluded
        .iter()
        .any(|(path, reason)| path == "saves/world/level.dat" && *reason == "saves_not_selected"));
    assert!(spec
        .excluded
        .iter()
        .any(|(path, reason)| path == "mods/SODIUM-0.5.3.jar" && *reason == "duplicate_path"));

    let document = build_modrinth_index(&spec).expect("index document");
    let reparsed = parse_modrinth_index(&document).expect("round trip");
    assert_eq!(reparsed.name, "Export fixture");
    assert_eq!(reparsed.files.len(), 1);
    assert_eq!(reparsed.files[0].path, "mods/sodium-0.5.3.jar");
    assert_eq!(reparsed.files[0].hashes.sha1.as_deref(), Some(sha.as_str()));
    assert_eq!(
        reparsed.target().expect("target").loader,
        LoaderKind::Fabric
    );
    assert!(
        !document.contains("token"),
        "no credential ever reaches a pack"
    );
}

#[test]
fn override_prefixes_are_classified_and_escapes_refused() {
    assert_eq!(
        classify_override("overrides/config/a.json")
            .expect("ok")
            .expect("some")
            .scope,
        OverrideScope::Shared
    );
    assert!(classify_override("modrinth.index.json")
        .expect("ok")
        .is_none());
    let error = classify_override("server-overrides/../../evil.txt").expect_err("escape");
    assert_eq!(error.code(), "content.pack.path_escape");
}

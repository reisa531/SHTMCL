//! NeoForge source aliases and the fixed pilot installer payload.
//!
//! This module is pure composition: it only builds `HttpSource` aliases over
//! the official NeoForge Maven releases root and a pinned downloader request.
//! It performs no networking, execution, filesystem access, scheduling, or UI
//! configuration. Wiring into the application is the integrator's concern.

use shtmcl_core::downloads::{standard_sources, HttpSource};

/// Official NeoForge Maven releases root (HTTPS only).
const NEOFORGE_RELEASES_ROOT: &str = "https://maven.neoforged.net/releases/";

/// Source id used by the installer payload (also an alias in [`sources`]).
const NEOFORGE_MAVEN_ID: &str = "neoforge-maven";

/// Pilot pin identity.
///
/// This pin originates from a captured pilot acceptance run for exactly
/// Minecraft `1.20.4` with NeoForge loader `20.4.253`. Its presence here does
/// **not** imply general NeoForge loader support; any other game or loader
/// combination is rejected by [`installer_payload`].
const PILOT_GAME: &str = "1.20.4";
const PILOT_LOADER: &str = "20.4.253";
const PILOT_INSTALLER_URL: &str = "https://maven.neoforged.net/releases/net/neoforged/neoforge/20.4.253/neoforge-20.4.253-installer.jar";
const PILOT_INSTALLER_SIZE: u64 = 6_540_143;
const PILOT_INSTALLER_SHA256: &str =
    "8f1413379067059acc0151444b17536cfad97756d99fc70e45587bf229c986fa";
const PILOT_ARTIFACT_ID: &str = "neoforge-installer";

/// Returns the standard sources extended with the NeoForge Maven aliases.
///
/// `neoforge-maven` and `neo-runtime` both resolve against the same fixed
/// official HTTPS releases root. No mirrors are added and the standard
/// sources (e.g. `mojang-libraries`, `mojang-data`) are preserved.
pub(crate) fn sources() -> Vec<HttpSource> {
    let mut sources = standard_sources();

    for id in [NEOFORGE_MAVEN_ID, "neo-runtime"] {
        sources.push(
            HttpSource::new(id, NEOFORGE_RELEASES_ROOT)
                .expect("constant NeoForge source must be valid"),
        );
    }

    sources
}

/// Builds the pinned installer download request for the pilot only.
///
/// Accepts exactly Minecraft `1.20.4` and NeoForge loader `20.4.253`; every
/// other combination is rejected. The returned `serde_json::Value` matches the
/// downloader command payload shape and uses the `neoforge-maven` source.
pub(crate) fn installer_payload(
    game: &str,
    loader: &str,
) -> Result<serde_json::Value, &'static str> {
    if game != PILOT_GAME || loader != PILOT_LOADER {
        return Err("neoforge_install_version_unsupported");
    }

    Ok(serde_json::json!({
        "command": "downloads.fetch",
        "artifacts": [{
            "artifactId": PILOT_ARTIFACT_ID,
            "sourceId": NEOFORGE_MAVEN_ID,
            "url": PILOT_INSTALLER_URL,
            "size": PILOT_INSTALLER_SIZE,
            "hash": {
                "algorithm": "sha256",
                "digest": PILOT_INSTALLER_SHA256,
            },
        }],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(sources: &'a [HttpSource], id: &str) -> &'a HttpSource {
        sources
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("missing source {id}"))
    }

    #[test]
    fn source_ids_are_unique() {
        let sources = sources();
        let mut ids: Vec<&str> = sources.iter().map(|s| s.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate source ids: {ids:?}");
    }

    #[test]
    fn standard_sources_preserved() {
        let sources = sources();
        // Mojang libraries and data must still be present.
        find(&sources, "mojang-libraries");
        find(&sources, "mojang-data");
    }

    #[test]
    fn both_aliases_use_official_releases_root() {
        let sources = sources();
        for id in [NEOFORGE_MAVEN_ID, "neo-runtime"] {
            let source = find(&sources, id);
            assert_eq!(source.base_url.scheme(), "https", "{id} must be HTTPS");
            assert_eq!(
                source.base_url.as_str(),
                NEOFORGE_RELEASES_ROOT,
                "{id} must use the official releases root"
            );
            assert_eq!(source.base_url.host_str(), Some("maven.neoforged.net"));
        }
    }

    #[test]
    fn foreign_hosts_are_rejected_before_networking() {
        use shtmcl_core::downloads::{DownloadPort, HttpDownloads};
        let root = std::env::temp_dir().join(format!(
            "neo-source-{}-{}",
            std::process::id(),
            shtmcl_platform::random_hex(16).unwrap()
        ));
        let transport = HttpDownloads::new(&root, sources()).unwrap();
        let foreign = [
            "https://evil.example.com/net/neoforged/neoforge/20.4.253/neoforge-20.4.253-installer.jar",
            "http://maven.neoforged.net/releases/net/neoforged/neoforge/20.4.253/neoforge-20.4.253-installer.jar",
            "https://maven.neoforged.net.evil.example.com/releases/x.jar",
        ];
        for url in foreign {
            assert!(
                transport
                    .probe_url(NEOFORGE_MAVEN_ID, "rejected", url)
                    .is_err(),
                "expected rejection of foreign url: {url}"
            );
        }

        drop(transport);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn accepts_exact_pilot_and_identity() {
        let payload =
            installer_payload(PILOT_GAME, PILOT_LOADER).expect("exact pilot must be accepted");

        assert_eq!(payload["command"], serde_json::json!("downloads.fetch"));

        let artifacts = payload["artifacts"]
            .as_array()
            .expect("artifacts must be an array");
        assert_eq!(artifacts.len(), 1, "exactly one artifact expected");
        let artifact = &artifacts[0];

        assert_eq!(artifact["artifactId"], serde_json::json!(PILOT_ARTIFACT_ID));
        assert_eq!(artifact["sourceId"], serde_json::json!(NEOFORGE_MAVEN_ID));
        assert_eq!(artifact["url"], serde_json::json!(PILOT_INSTALLER_URL));
        assert_eq!(artifact["size"], serde_json::json!(PILOT_INSTALLER_SIZE));
        assert_eq!(artifact["hash"]["algorithm"], serde_json::json!("sha256"));
        assert_eq!(
            artifact["hash"]["digest"],
            serde_json::json!(PILOT_INSTALLER_SHA256)
        );

        // The payload source id must be one of the registered aliases and
        // must resolve the pinned installer under the official root.
        let sources = sources();
        let source = find(&sources, NEOFORGE_MAVEN_ID);
        assert!(artifact["url"]
            .as_str()
            .unwrap()
            .starts_with(source.base_url.as_str()));
    }

    #[test]
    fn rejects_unsupported_combinations() {
        let cases = [
            ("1.20.4", "20.4.252"),
            ("1.20.4", "20.4.254"),
            ("1.20.4", "21.1.0"),
            ("1.20.1", "20.4.253"),
            ("1.21.1", "20.4.253"),
            ("1.20.4", ""),
            ("", "20.4.253"),
            ("", ""),
        ];
        for (game, loader) in cases {
            assert!(
                installer_payload(game, loader).is_err(),
                "expected rejection of {game}/{loader}"
            );
        }
    }
}

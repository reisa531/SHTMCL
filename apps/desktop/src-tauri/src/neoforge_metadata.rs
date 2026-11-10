//! Bounded NeoForge version-catalog retrieval for the existing launch read path.
//! Installer acquisition and execution are intentionally handled elsewhere.

use serde_json::{json, Value};
use shtmcl_core::content::neoforge::{catalog_urls, parse_maven_versions};
use std::{io::Read, sync::OnceLock, time::Duration};

const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;

fn client() -> Result<reqwest::blocking::Client, &'static str> {
    static CLIENT: OnceLock<Result<reqwest::blocking::Client, &'static str>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .user_agent(concat!("SHTMCL/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(20))
                .pool_idle_timeout(Duration::from_secs(90))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| "neoforge_http_client")
        })
        .clone()
}

pub(crate) fn versions(game_version: &str) -> Result<Vec<Value>, &'static str> {
    let urls = catalog_urls(game_version)?;
    let bytes = fetch(&urls, MAX_METADATA_BYTES)?;
    parse_maven_versions(game_version, &bytes).map(|releases| {
        releases
            .into_iter()
            .map(|release| json!({"version":release.loader_version,"stable":release.stable}))
            .collect()
    })
}

fn fetch(urls: &[String; 2], max_bytes: usize) -> Result<Vec<u8>, &'static str> {
    fetch_with(urls, max_bytes, fetch_once)
}

fn fetch_with<F>(
    urls: &[String; 2],
    max_bytes: usize,
    mut fetch_once: F,
) -> Result<Vec<u8>, &'static str>
where
    F: FnMut(&str, usize) -> Result<Vec<u8>, &'static str>,
{
    for (index, url) in urls.iter().enumerate() {
        validate_catalog_url(url, index)?;
        match fetch_once(url, max_bytes) {
            Ok(bytes) => return Ok(bytes),
            Err("neoforge_http_request" | "neoforge_http_read") if index == 0 => {
                eprintln!("SHTMCL: NeoForge metadata source fallback: official -> BMCLAPI");
            }
            Err(error) => return Err(error),
        }
    }
    Err("neoforge_http_request")
}

fn validate_catalog_url(value: &str, index: usize) -> Result<(), &'static str> {
    let url = reqwest::Url::parse(value).map_err(|_| "neoforge_catalog_url")?;
    let expected_host = match index {
        0 => "maven.neoforged.net",
        1 => "bmclapi2.bangbang93.com",
        _ => return Err("neoforge_catalog_url"),
    };
    if url.scheme() != "https"
        || url.host_str() != Some(expected_host)
        || url.port().is_some_and(|port| port != 443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().ends_with("/maven-metadata.xml")
    {
        return Err("neoforge_catalog_url");
    }
    Ok(())
}

fn fetch_once(url: &str, max_bytes: usize) -> Result<Vec<u8>, &'static str> {
    let mut response = client()?.get(url).send().map_err(|error| {
        eprintln!("SHTMCL: NeoForge metadata request failed: {error:?}");
        "neoforge_http_request"
    })?;
    if !response.status().is_success() {
        return Err("neoforge_http_request");
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err("neoforge_response_too_large");
    }

    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "neoforge_http_read")?;
    if bytes.len() > max_bytes {
        return Err("neoforge_response_too_large");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_urls_fixture() -> [String; 2] {
        catalog_urls("1.20.4").expect("supported game version")
    }

    #[test]
    fn retries_the_mirror_only_after_official_transport_failure() {
        let urls = catalog_urls_fixture();
        let mut requested = Vec::new();
        let data = fetch_with(&urls, 1024, |url, _| {
            requested.push(url.to_owned());
            if requested.len() == 1 {
                Err("neoforge_http_request")
            } else {
                Ok(b"metadata".to_vec())
            }
        })
        .expect("mirror should return the catalog");

        assert_eq!(data, b"metadata");
        assert_eq!(requested, urls);
    }

    #[test]
    fn does_not_retry_on_oversized_or_invalid_catalog_data() {
        for failure in ["neoforge_response_too_large", "neoforge_metadata_invalid"] {
            let urls = catalog_urls_fixture();
            let mut attempts = 0;
            let result = fetch_with(&urls, 1024, |_, _| {
                attempts += 1;
                Err(failure)
            });
            assert_eq!(result, Err(failure));
            assert_eq!(attempts, 1);
        }
    }

    #[test]
    fn rejects_urls_outside_the_two_fixed_https_sources() {
        let mut urls = catalog_urls_fixture();
        urls[0] =
            "http://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml".into();
        assert_eq!(
            fetch_with(&urls, 1024, |_, _| Ok(Vec::new())),
            Err("neoforge_catalog_url")
        );

        let mut urls = catalog_urls_fixture();
        urls[1] = "https://evil.example/maven/net/neoforged/neoforge/maven-metadata.xml".into();
        assert_eq!(
            fetch_with(&urls, 1024, |_, _| Err("neoforge_http_request")),
            Err("neoforge_catalog_url")
        );
    }

    #[test]
    #[ignore = "requires access to the NeoForged Maven repository or BMCLAPI"]
    fn reads_a_live_catalog_through_the_production_source_chain() {
        let entries = versions("1.20.4").expect("live NeoForge catalog");
        assert!(!entries.is_empty());
        assert!(entries.iter().all(|entry| {
            entry["version"].as_str().is_some() && entry["stable"].as_bool().is_some()
        }));
    }
}

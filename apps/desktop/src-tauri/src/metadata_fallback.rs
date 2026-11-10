//! One explicit mirror attempt for transport failure. Version SHA-1 validation
//! stays with the metadata cache; invalid content never triggers another fetch.
pub(crate) fn fetch(
    url: &str,
    max_bytes: usize,
    mut request: impl FnMut(&str, usize) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "metadata.url_invalid")?;
    if parsed.scheme() != "https"
        || parsed.port_or_known_default() != Some(443)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(
            parsed.host_str(),
            Some(
                "piston-meta.mojang.com"
                    | "launchermeta.mojang.com"
                    | "launcher.mojang.com"
                    | "piston-data.mojang.com"
            )
        )
    {
        return Err("metadata.url_not_allowed".into());
    }
    match request(url, max_bytes) {
        Ok(bytes) => Ok(bytes),
        Err(error)
            if matches!(
                error.as_str(),
                "metadata.http.request_failed" | "metadata.http.read_failed"
            ) =>
        {
            let mut mirror = parsed;
            mirror
                .set_host(Some("bmclapi2.bangbang93.com"))
                .map_err(|_| "metadata.url_invalid")?;
            eprintln!("SHTMCL: metadata source fallback: official -> bmclapi");
            request(mirror.as_str(), max_bytes)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fallback_preserves_path_budget_and_has_only_two_attempts() {
        let mut seen = Vec::new();
        let result = fetch(
            "https://piston-meta.mojang.com/v1/packages/hash/game.json",
            1024,
            |url, limit| {
                seen.push((url.to_owned(), limit));
                if seen.len() == 1 {
                    Err("metadata.http.request_failed".into())
                } else {
                    Ok(b"pinned downstream".to_vec())
                }
            },
        )
        .unwrap();
        assert_eq!(result, b"pinned downstream");
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[1],
            (
                "https://bmclapi2.bangbang93.com/v1/packages/hash/game.json".into(),
                1024
            )
        );
        let mut attempts = 0;
        assert!(fetch("https://launcher.mojang.com/file", 32, |_, _| {
            attempts += 1;
            Err("metadata.http.read_failed".into())
        })
        .is_err());
        assert_eq!(attempts, 2);
    }
    #[test]
    fn invalid_urls_and_content_budget_failures_do_not_retry() {
        for url in [
            "https://evil.example/file",
            "http://piston-meta.mojang.com/file",
            "https://piston-meta.mojang.com/file?token=private",
        ] {
            assert!(fetch(url, 1, |_, _| panic!("invalid URL must not be fetched")).is_err());
        }
        let mut attempts = 0;
        let error = fetch("https://piston-meta.mojang.com/file", 1, |_, _| {
            attempts += 1;
            Err("metadata.http.response_too_large".into())
        })
        .unwrap_err();
        assert_eq!(error, "metadata.http.response_too_large");
        assert_eq!(attempts, 1);
    }
}

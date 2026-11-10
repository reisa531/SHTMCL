//! Reused blocking HTTP transport for the bounded download worker pool.
//!
//! `source_id` is a lookup key in the explicitly supplied source table. The
//! artifact ID is a relative URL path; it is never interpreted as a filesystem
//! path or as an arbitrary URL.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, RANGE, RETRY_AFTER};
use reqwest::redirect::Policy;
use reqwest::{StatusCode, Url};

use super::ports::{DownloadPort, FetchOutcome};
use super::state::{ArtifactHash, ByteRange};
use super::storage::FileStore;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20 * 60);
// Bound stalled headers/body reads without imposing this short limit on the
// complete transfer of a large file that continues making progress.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_BUFFER_SIZE: usize = 64 * 1024;
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(50);

static HTTP_RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct HttpSource {
    pub id: String,
    pub base_url: Url,
    allowed_upstream_hosts: Vec<String>,
}

impl HttpSource {
    pub fn new(id: impl Into<String>, base_url: &str) -> Result<Self, String> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err("downloads.source.id_invalid".into());
        }
        let base_url = Url::parse(base_url).map_err(|_| "downloads.source.url_invalid")?;
        let local_http =
            base_url.scheme() == "http" && base_url.host_str().is_some_and(is_loopback_host);
        if base_url.scheme() != "https" && !local_http {
            return Err("downloads.source.https_required".into());
        }
        if base_url.username() != ""
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err("downloads.source.url_invalid".into());
        }
        let host = base_url
            .host_str()
            .ok_or("downloads.source.url_invalid")?
            .to_ascii_lowercase();
        Ok(Self {
            id,
            base_url,
            allowed_upstream_hosts: vec![host],
        })
    }

    pub fn mirror(
        id: impl Into<String>,
        base_url: &str,
        allowed_upstream_hosts: &[&str],
    ) -> Result<Self, String> {
        let mut source = Self::new(id, base_url)?;
        source.allowed_upstream_hosts = allowed_upstream_hosts
            .iter()
            .map(|host| host.to_ascii_lowercase())
            .collect();
        Ok(source)
    }

    fn resolve(&self, artifact_id: &str) -> Result<Url, String> {
        if artifact_id.is_empty()
            || artifact_id.starts_with('/')
            || artifact_id.starts_with('\\')
            || artifact_id.contains(['?', '#', '\\', '\r', '\n'])
        {
            return Err("downloads.artifact.path_invalid".into());
        }
        let segments = artifact_id.split('/').collect::<Vec<_>>();
        if segments
            .iter()
            .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
        {
            return Err("downloads.artifact.path_invalid".into());
        }
        let mut url = self.base_url.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| "downloads.source.url_invalid")?;
            path.pop_if_empty();
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url)
    }

    fn resolve_url(&self, raw_url: &str) -> Result<Url, String> {
        let upstream = Url::parse(raw_url).map_err(|_| "downloads.source.url_invalid")?;
        let local_http =
            upstream.scheme() == "http" && upstream.host_str().is_some_and(is_loopback_host);
        if (upstream.scheme() != "https" && !local_http)
            || upstream.username() != ""
            || upstream.password().is_some()
            || upstream.fragment().is_some()
        {
            return Err("downloads.source.url_invalid".into());
        }
        let host = upstream.host_str().ok_or("downloads.source.url_invalid")?;
        if !self
            .allowed_upstream_hosts
            .iter()
            .any(|allowed| host.eq_ignore_ascii_case(allowed))
        {
            return Err("downloads.source.host_not_allowed".into());
        }
        if host.eq_ignore_ascii_case(self.base_url.host_str().unwrap_or_default()) {
            return Ok(upstream);
        }
        let path = upstream.path().trim_start_matches('/');
        let mut mapped = self
            .base_url
            .join(path)
            .map_err(|_| "downloads.source.url_invalid")?;
        mapped.set_query(upstream.query());
        Ok(mapped)
    }
}

#[derive(Clone)]
pub struct HttpDownloads {
    client: reqwest::Client,
    runtime: &'static tokio::runtime::Runtime,
    sources: Arc<BTreeMap<String, HttpSource>>,
    store: FileStore,
}

impl HttpDownloads {
    pub fn new(
        cache_root: impl Into<std::path::PathBuf>,
        sources: impl IntoIterator<Item = HttpSource>,
    ) -> Result<Self, String> {
        let mut source_map = BTreeMap::new();
        for source in sources {
            if source_map.insert(source.id.clone(), source).is_some() {
                return Err("downloads.source.duplicate".into());
            }
        }
        let redirect_hosts = source_map
            .values()
            .flat_map(|source| {
                source
                    .allowed_upstream_hosts
                    .iter()
                    .cloned()
                    .chain(source.base_url.host_str().map(str::to_ascii_lowercase))
            })
            .collect::<std::collections::BTreeSet<_>>();
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_IDLE_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(90))
            .redirect(Policy::custom(move |attempt| {
                let from = attempt.previous().last();
                let downgrade = from.is_some_and(|url| {
                    url.scheme() == "https" && attempt.url().scheme() != "https"
                });
                if downgrade
                    || attempt.previous().len() >= 10
                    || !redirect_allowed(from, attempt.url(), &redirect_hosts)
                {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|_| "downloads.http.client_create_failed")?;
        let runtime = shared_http_runtime()?;
        let store = FileStore::new(cache_root).map_err(|_| "downloads.disk.cache_create_failed")?;
        Ok(Self {
            client,
            runtime,
            sources: Arc::new(source_map),
            store,
        })
    }

    fn source_url(&self, source_id: &str, artifact_id: &str) -> Result<Url, String> {
        self.sources
            .get(source_id)
            .ok_or_else(|| "downloads.source.unknown".to_string())?
            .resolve(artifact_id)
    }

    fn exact_source_url(&self, source_id: &str, url: &str) -> Result<Url, String> {
        self.sources
            .get(source_id)
            .ok_or_else(|| "downloads.source.unknown".to_string())?
            .resolve_url(url)
    }
}

/// Official Mojang endpoints and the documented BMCLAPI equivalents. The
/// provider is encoded in the ID so callers and the UI can show which source
/// served a file; mirror use must remain visible to users.
pub fn standard_sources() -> Vec<HttpSource> {
    let mut sources = [
        ("mojang-meta", "https://piston-meta.mojang.com/"),
        ("mojang-legacy-meta", "https://launchermeta.mojang.com/"),
        ("mojang-launcher", "https://launcher.mojang.com/"),
        ("mojang-data", "https://piston-data.mojang.com/"),
        ("mojang-assets", "https://resources.download.minecraft.net/"),
        ("mojang-libraries", "https://libraries.minecraft.net/"),
        ("forge-maven", "https://files.minecraftforge.net/maven/"),
        ("fabric-meta", "https://meta.fabricmc.net/"),
        ("fabric-maven", "https://maven.fabricmc.net/"),
    ]
    .into_iter()
    .filter_map(|(id, url)| HttpSource::new(id, url).ok())
    .collect::<Vec<_>>();
    let metadata_hosts = [
        "piston-meta.mojang.com",
        "launchermeta.mojang.com",
        "launcher.mojang.com",
        "piston-data.mojang.com",
    ];
    sources.extend(
        [
            HttpSource::mirror(
                "bmclapi-meta",
                "https://bmclapi2.bangbang93.com/",
                &metadata_hosts,
            ),
            HttpSource::mirror(
                "bmclapi-launcher",
                "https://bmclapi2.bangbang93.com/",
                &metadata_hosts,
            ),
            HttpSource::mirror(
                "bmclapi-assets",
                "https://bmclapi2.bangbang93.com/assets/",
                &["resources.download.minecraft.net"],
            ),
            HttpSource::mirror(
                "bmclapi-maven",
                "https://bmclapi2.bangbang93.com/maven/",
                &[
                    "libraries.minecraft.net",
                    "files.minecraftforge.net",
                    "maven.fabricmc.net",
                ],
            ),
            HttpSource::mirror(
                "bmclapi-fabric",
                "https://bmclapi2.bangbang93.com/fabric-meta/",
                &["meta.fabricmc.net"],
            ),
        ]
        .into_iter()
        .filter_map(Result::ok),
    );
    sources
}

impl HttpDownloads {
    fn probe_at(&self, url: Url, cancelled: &dyn Fn() -> bool) -> Result<Option<u64>, String> {
        let client = self.client.clone();
        self.runtime.block_on(async {
            let response = tokio::select! {
                biased;
                _ = wait_for_cancel(cancelled) => return Err("downloads.task.cancelled".into()),
                result = client.head(url.clone()).send() => result
                    .map_err(|_| "downloads.http.probe_failed")?,
            };
            if response.status().is_success() {
                if let Some(length) = response
                    .headers()
                    .get(CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                {
                    return Ok(Some(length));
                }
            } else if !matches!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
            ) {
                if response.status() == StatusCode::TOO_MANY_REQUESTS {
                    return Err("downloads.http.throttled".into());
                }
                return Err(format!(
                    "downloads.http.probe_status:{}",
                    response.status().as_u16()
                ));
            }

            let response = tokio::select! {
                biased;
                _ = wait_for_cancel(cancelled) => return Err("downloads.task.cancelled".into()),
                result = client.get(url)
                    .header(RANGE, "bytes=0-0")
                    .send() => result.map_err(|_| "downloads.http.probe_failed")?,
            };
            if response.status() == StatusCode::PARTIAL_CONTENT {
                return Ok(parse_content_range(
                    response
                        .headers()
                        .get(CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or(""),
                )?
                .1);
            }
            if response.status().is_success() {
                return Ok(response
                    .headers()
                    .get(CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok()));
            }
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                return Err("downloads.http.throttled".into());
            }
            Err(format!(
                "downloads.http.probe_status:{}",
                response.status().as_u16()
            ))
        })
    }

    fn fetch_at(
        &self,
        artifact_id: &str,
        url: Url,
        range: ByteRange,
        etag: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> FetchOutcome {
        if cancelled() {
            return FetchOutcome::Cancelled;
        }
        if range.is_empty() {
            return FetchOutcome::TransportFailed("downloads.range.invalid".into());
        }
        let client = self.client.clone();
        let store = self.store.clone();
        let artifact_id = artifact_id.to_owned();
        let etag = etag.map(str::to_owned);
        self.runtime.block_on(async move {
            let mut request = client
                .get(url)
                .header(RANGE, format!("bytes={}-{}", range.start, range.end - 1));
            if let Some(etag) = etag.as_deref() {
                request = request.header(IF_RANGE, etag);
            }
            let mut response = tokio::select! {
                biased;
                _ = wait_for_cancel(cancelled) => return FetchOutcome::Cancelled,
                result = request.send() => match result {
                    Ok(response) => response,
                    Err(error) => return FetchOutcome::TransportFailed(format!(
                        "downloads.http.request_failed:{}",
                        request_error_kind(&error)
                    )),
                },
            };

            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                return FetchOutcome::Throttled {
                    retry_at: retry_at(
                        response
                            .headers()
                            .get(RETRY_AFTER)
                            .and_then(|value| value.to_str().ok()),
                    ),
                };
            }
            if status == StatusCode::RANGE_NOT_SATISFIABLE || status == StatusCode::OK {
                return FetchOutcome::RangeUnsupported;
            }
            if status != StatusCode::PARTIAL_CONTENT {
                return FetchOutcome::TransportFailed(format!(
                    "downloads.http.status:{}",
                    status.as_u16()
                ));
            }

            let (served, _) = match parse_content_range(
                response
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or(""),
            ) {
                Ok(parsed) => parsed,
                Err(error) => return FetchOutcome::TransportFailed(error),
            };
            if served != range {
                return FetchOutcome::Bytes {
                    range: served,
                    etag: None,
                };
            }
            let response_etag = response
                .headers()
                .get(ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if etag.is_some() && response_etag.as_deref() != etag.as_deref() {
                return FetchOutcome::RangeUnsupported;
            }
            if response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .is_some_and(|length| length != range.len())
            {
                return FetchOutcome::TransportFailed("downloads.http.length_mismatch".into());
            }

            let mut file = match store.open_partial(&artifact_id, range.start) {
                Ok(file) => file,
                Err(_) => {
                    return FetchOutcome::TransportFailed("downloads.disk.write_failed".into())
                }
            };
            let mut received = 0u64;
            loop {
                let chunk = tokio::select! {
                    biased;
                    _ = wait_for_cancel(cancelled) => return FetchOutcome::Cancelled,
                    result = response.chunk() => match result {
                        Ok(Some(chunk)) => chunk,
                        Ok(None) => break,
                        Err(_) => return FetchOutcome::TransportFailed(
                            "downloads.http.body_read_failed".into()
                        ),
                    },
                };
                let Some(next) = received.checked_add(chunk.len() as u64) else {
                    return FetchOutcome::TransportFailed("downloads.http.body_too_large".into());
                };
                if next > range.len() {
                    return FetchOutcome::TransportFailed("downloads.http.body_too_large".into());
                }
                for block in chunk.chunks(STREAM_BUFFER_SIZE) {
                    if cancelled() {
                        return FetchOutcome::Cancelled;
                    }
                    if file.write_all(block).is_err() {
                        return FetchOutcome::TransportFailed("downloads.disk.write_failed".into());
                    }
                }
                received = next;
            }
            if received != range.len() {
                return FetchOutcome::TransportFailed("downloads.http.body_truncated".into());
            }

            FetchOutcome::Bytes {
                range,
                etag: response_etag,
            }
        })
    }

    fn fetch_full_at(
        &self,
        artifact_id: &str,
        url: Url,
        cancelled: &dyn Fn() -> bool,
    ) -> FetchOutcome {
        if cancelled() {
            return FetchOutcome::Cancelled;
        }
        let client = self.client.clone();
        let store = self.store.clone();
        let artifact_id = artifact_id.to_owned();
        self.runtime.block_on(async move {
            let mut response = tokio::select! {
                biased;
                _ = wait_for_cancel(cancelled) => return FetchOutcome::Cancelled,
                result = client.get(url).send() => match result {
                    Ok(response) => response,
                    Err(error) => return FetchOutcome::TransportFailed(format!(
                        "downloads.http.request_failed:{}",
                        request_error_kind(&error)
                    )),
                },
            };
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                return FetchOutcome::Throttled {
                    retry_at: retry_at(
                        response
                            .headers()
                            .get(RETRY_AFTER)
                            .and_then(|value| value.to_str().ok()),
                    ),
                };
            }
            if response.status() != StatusCode::OK {
                return FetchOutcome::TransportFailed(format!(
                    "downloads.http.full_status:{}",
                    response.status().as_u16()
                ));
            }
            let expected_length = response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let response_etag = response
                .headers()
                .get(ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let mut file = match store.reset_partial(&artifact_id) {
                Ok(file) => file,
                Err(_) => {
                    return FetchOutcome::TransportFailed("downloads.disk.write_failed".into())
                }
            };
            let mut received = 0u64;
            loop {
                let chunk = tokio::select! {
                    biased;
                    _ = wait_for_cancel(cancelled) => return FetchOutcome::Cancelled,
                    result = response.chunk() => match result {
                        Ok(Some(chunk)) => chunk,
                        Ok(None) => break,
                        Err(_) => return FetchOutcome::TransportFailed(
                            "downloads.http.body_read_failed".into()
                        ),
                    },
                };
                let Some(next) = received.checked_add(chunk.len() as u64) else {
                    return FetchOutcome::TransportFailed("downloads.http.body_too_large".into());
                };
                if expected_length.is_some_and(|length| next > length) {
                    return FetchOutcome::TransportFailed("downloads.http.body_too_large".into());
                }
                for block in chunk.chunks(STREAM_BUFFER_SIZE) {
                    if cancelled() {
                        return FetchOutcome::Cancelled;
                    }
                    if file.write_all(block).is_err() {
                        return FetchOutcome::TransportFailed("downloads.disk.write_failed".into());
                    }
                }
                received = next;
            }
            if received == 0 {
                return FetchOutcome::TransportFailed("downloads.http.empty_body".into());
            }
            if expected_length.is_some_and(|length| length != received) {
                return FetchOutcome::TransportFailed("downloads.http.body_truncated".into());
            }
            FetchOutcome::Bytes {
                range: ByteRange {
                    start: 0,
                    end: received,
                },
                etag: response_etag,
            }
        })
    }
}

impl DownloadPort for HttpDownloads {
    fn probe(&self, source_id: &str, artifact_id: &str) -> Result<Option<u64>, String> {
        self.probe_at(self.source_url(source_id, artifact_id)?, &|| false)
    }

    fn probe_cancellable(
        &self,
        source_id: &str,
        artifact_id: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<u64>, String> {
        self.probe_at(self.source_url(source_id, artifact_id)?, cancelled)
    }

    fn probe_url(
        &self,
        source_id: &str,
        _artifact_id: &str,
        url: &str,
    ) -> Result<Option<u64>, String> {
        self.probe_at(self.exact_source_url(source_id, url)?, &|| false)
    }

    fn probe_url_cancellable(
        &self,
        source_id: &str,
        _artifact_id: &str,
        url: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<u64>, String> {
        self.probe_at(self.exact_source_url(source_id, url)?, cancelled)
    }

    fn fetch(
        &self,
        source_id: &str,
        artifact_id: &str,
        range: ByteRange,
        etag: Option<&str>,
    ) -> FetchOutcome {
        match self.source_url(source_id, artifact_id) {
            Ok(url) => self.fetch_at(artifact_id, url, range, etag, &|| false),
            Err(error) => FetchOutcome::TransportFailed(error),
        }
    }

    fn fetch_cancellable(
        &self,
        source_id: &str,
        artifact_id: &str,
        range: ByteRange,
        etag: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> FetchOutcome {
        match self.source_url(source_id, artifact_id) {
            Ok(url) => self.fetch_at(artifact_id, url, range, etag, cancelled),
            Err(error) => FetchOutcome::TransportFailed(error),
        }
    }

    fn fetch_url(
        &self,
        source_id: &str,
        artifact_id: &str,
        url: &str,
        range: ByteRange,
        etag: Option<&str>,
    ) -> FetchOutcome {
        match self.exact_source_url(source_id, url) {
            Ok(url) => self.fetch_at(artifact_id, url, range, etag, &|| false),
            Err(error) => FetchOutcome::TransportFailed(error),
        }
    }

    fn fetch_url_cancellable(
        &self,
        source_id: &str,
        artifact_id: &str,
        url: &str,
        range: ByteRange,
        etag: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> FetchOutcome {
        match self.exact_source_url(source_id, url) {
            Ok(url) => self.fetch_at(artifact_id, url, range, etag, cancelled),
            Err(error) => FetchOutcome::TransportFailed(error),
        }
    }

    fn fetch_full(&self, source_id: &str, artifact_id: &str) -> FetchOutcome {
        match self.source_url(source_id, artifact_id) {
            Ok(url) => self.fetch_full_at(artifact_id, url, &|| false),
            Err(error) => FetchOutcome::TransportFailed(error),
        }
    }

    fn fetch_full_cancellable(
        &self,
        source_id: &str,
        artifact_id: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> FetchOutcome {
        match self.source_url(source_id, artifact_id) {
            Ok(url) => self.fetch_full_at(artifact_id, url, cancelled),
            Err(error) => FetchOutcome::TransportFailed(error),
        }
    }

    fn fetch_full_url(&self, source_id: &str, artifact_id: &str, url: &str) -> FetchOutcome {
        match self.exact_source_url(source_id, url) {
            Ok(url) => self.fetch_full_at(artifact_id, url, &|| false),
            Err(error) => FetchOutcome::TransportFailed(error),
        }
    }

    fn fetch_full_url_cancellable(
        &self,
        source_id: &str,
        artifact_id: &str,
        url: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> FetchOutcome {
        match self.exact_source_url(source_id, url) {
            Ok(url) => self.fetch_full_at(artifact_id, url, cancelled),
            Err(error) => FetchOutcome::TransportFailed(error),
        }
    }

    fn hash_bytes(&self, artifact_id: &str, algorithm: &str) -> Result<String, String> {
        self.store
            .hash_partial(artifact_id, algorithm)
            .map_err(|_| "downloads.disk.hash_failed".into())
    }

    fn prepare_empty(&self, artifact_id: &str) -> Result<(), String> {
        self.store
            .reset_partial(artifact_id)
            .and_then(|file| file.sync_all())
            .map_err(|_| "downloads.disk.empty_failed".into())
    }

    fn commit_range(&self, artifact_id: &str, _range: ByteRange) -> Result<(), String> {
        self.store
            .commit_partial(artifact_id)
            .map_err(|_| "downloads.disk.sync_failed".into())
    }

    fn publish(&self, hash: &ArtifactHash, artifact_id: &str) -> Result<u64, String> {
        self.store
            .publish(hash, artifact_id)
            .map_err(|_| "downloads.disk.publish_failed".into())
    }

    fn materialize_cached(
        &self,
        hash: &ArtifactHash,
        artifact_id: &str,
    ) -> Result<Option<u64>, String> {
        self.store
            .materialize(hash, artifact_id)
            .map_err(|_| "downloads.disk.cache_read_failed".into())
    }

    fn recover_cached(
        &self,
        hash: &ArtifactHash,
        artifact_id: &str,
    ) -> Result<Option<u64>, String> {
        // Recovery must not follow linked cache objects or index unverified bytes.
        match self
            .store
            .verify_cached(hash)
            .map_err(|_| "downloads.disk.cache_read_failed".to_string())?
        {
            Some(_) => self.materialize_cached(hash, artifact_id),
            None => Ok(None),
        }
    }

    fn remove_cached(&self, hash: &ArtifactHash) -> Result<(), String> {
        self.store
            .remove_cached(hash)
            .map_err(|_| "downloads.disk.cache_remove_failed".into())
    }

    fn discard(&self, artifact_id: &str) -> Result<(), String> {
        self.store
            .discard(artifact_id)
            .map_err(|_| "downloads.disk.partial_remove_failed".into())
    }
}

fn shared_http_runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    HTTP_RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|_| "downloads.http.runtime_create_failed".to_string())
        })
        .as_ref()
        .map_err(Clone::clone)
}

async fn wait_for_cancel(cancelled: &dyn Fn() -> bool) {
    loop {
        if cancelled() {
            return;
        }
        tokio::time::sleep(CANCELLATION_POLL_INTERVAL).await;
    }
}

fn parse_content_range(value: &str) -> Result<(ByteRange, Option<u64>), String> {
    let value = value
        .strip_prefix("bytes ")
        .ok_or("downloads.http.content_range_invalid")?;
    let (range, total) = value
        .split_once('/')
        .ok_or("downloads.http.content_range_invalid")?;
    let (start, inclusive_end) = range
        .split_once('-')
        .ok_or("downloads.http.content_range_invalid")?;
    let start = start
        .parse::<u64>()
        .map_err(|_| "downloads.http.content_range_invalid")?;
    let inclusive_end = inclusive_end
        .parse::<u64>()
        .map_err(|_| "downloads.http.content_range_invalid")?;
    let end = inclusive_end
        .checked_add(1)
        .ok_or("downloads.http.content_range_invalid")?;
    if end <= start {
        return Err("downloads.http.content_range_invalid".into());
    }
    let total = if total == "*" {
        None
    } else {
        Some(
            total
                .parse::<u64>()
                .map_err(|_| "downloads.http.content_range_invalid")?,
        )
    };
    if total.is_some_and(|total| end > total) {
        return Err("downloads.http.content_range_invalid".into());
    }
    Ok((ByteRange { start, end }, total))
}

fn retry_at(header: Option<&str>) -> u64 {
    let now = now_ms();
    let retry = header.and_then(|value| {
        if let Ok(seconds) = value.trim().parse::<u64>() {
            return Some(now.saturating_add(seconds.saturating_mul(1000)));
        }
        httpdate::parse_http_date(value)
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
    });
    retry.unwrap_or_else(|| now.saturating_add(10_000))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn request_error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_redirect() {
        "redirect"
    } else if error.is_body() {
        "body"
    } else {
        "request"
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn redirect_allowed(
    from: Option<&Url>,
    to: &Url,
    allowed_hosts: &std::collections::BTreeSet<String>,
) -> bool {
    let Some(target_host) = to.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    if allowed_hosts.contains(&target_host) {
        return true;
    }
    // BMCLAPI redirects to signed download nodes and may rotate between those
    // hosts. Keep the exception scoped to its known CDN host families and the
    // same `/download/<id>` object path with signed `s`/`e` query keys.
    let signed_target = is_bmclapi_signed_download(to);
    from.is_some_and(|source| {
        to.scheme() == "https"
            && signed_target
            && (source.host_str() == Some("bmclapi2.bangbang93.com")
                || (is_bmclapi_signed_download(source) && source.path() == to.path()))
    })
}

fn is_bmclapi_signed_download(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let known_cdn = host.eq_ignore_ascii_case("18.749333.xyz")
        || host.to_ascii_lowercase().ends_with(".sakura.sld.tw");
    let download_path = url.path_segments().is_some_and(|mut segments| {
        segments.next() == Some("download") && segments.next().is_some()
    });
    let query_keys = url
        .query_pairs()
        .map(|(key, _)| key.into_owned())
        .collect::<std::collections::BTreeSet<_>>();
    known_cdn && download_path && query_keys.contains("s") && query_keys.contains("e")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::thread;

    fn temp_root() -> PathBuf {
        let nonce = shtmcl_platform::entropy::random_hex(16)
            .expect("failed to generate HTTP test root nonce");
        std::env::temp_dir().join(format!(
            "shtmcl-http-test-{}-{nonce}",
            std::process::id()
        ))
    }

    fn server(response: Vec<u8>) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            stream.write_all(&response).unwrap();
            first
        });
        (format!("http://{address}/"), thread)
    }

    fn range_probe_server() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            drop(reader);
            drop(stream);

            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/*\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx").unwrap();
        });
        (format!("http://{address}/"), thread)
    }

    fn redirect_server() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            drop(reader);
            drop(stream);

            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/3\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc").unwrap();
        });
        (format!("http://{address}/"), thread)
    }

    fn range_fallback_server() -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    headers.push_str(&line.to_ascii_lowercase());
                }
                requests.push(format!(
                    "{}{}",
                    first.trim(),
                    if headers.contains("range:") {
                        " range"
                    } else {
                        ""
                    }
                ));
                let response = if index == 0 {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n".as_slice()
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: \"full\"\r\nConnection: close\r\n\r\nhello".as_slice()
                };
                stream.write_all(response).unwrap();
            }
            requests
        });
        (format!("http://{address}/"), thread)
    }

    fn adapter(root: &Path, base: &str) -> HttpDownloads {
        HttpDownloads::new(root, [HttpSource::new("test", base).unwrap()]).unwrap()
    }

    #[test]
    fn validates_and_streams_exact_partial_range() {
        let (base, server) = server(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-4/5\r\nContent-Length: 3\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nllo".to_vec());
        let root = temp_root();
        let downloads = adapter(&root, &base);
        assert_eq!(
            downloads.fetch("test", "object", ByteRange { start: 2, end: 5 }, None),
            FetchOutcome::Bytes {
                range: ByteRange { start: 2, end: 5 },
                etag: Some("\"v1\"".into())
            }
        );
        assert_eq!(server.join().unwrap(), "GET /object HTTP/1.1\r\n");
        assert_eq!(
            downloads.hash_bytes("object", "sha1").unwrap(),
            "2d38115e39e437c1bb4c673fb0925d3d9d4d89fa"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn full_response_to_range_is_not_written() {
        let (base, server) = server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_vec(),
        );
        let root = temp_root();
        let downloads = adapter(&root, &base);
        assert_eq!(
            downloads.fetch("test", "object", ByteRange { start: 0, end: 5 }, None),
            FetchOutcome::RangeUnsupported
        );
        server.join().unwrap();
        assert_eq!(
            downloads.hash_bytes("object", "sha1"),
            Err("downloads.disk.hash_failed".into())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn full_restart_streams_to_zero_and_reports_received_length() {
        let (base, server) = server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: \"v2\"\r\nConnection: close\r\n\r\nhello".to_vec(),
        );
        let root = temp_root();
        let downloads = adapter(&root, &base);
        assert_eq!(
            downloads.fetch_full("test", "object"),
            FetchOutcome::Bytes {
                range: ByteRange { start: 0, end: 5 },
                etag: Some("\"v2\"".into()),
            }
        );
        server.join().unwrap();
        assert_eq!(
            downloads.hash_bytes("object", "sha1").unwrap(),
            "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn module_zero_length_files_are_verified_without_network() {
        use super::super::service::DownloadsModule;
        use crate::{CoreApp, CoreModule};
        use serde_json::json;
        use shtmcl_domain::{CommandRequest, CommandResponse, OperationId, RequestId, TaskState};
        use shtmcl_platform::PlatformDirs;
        use std::time::Instant;

        // No server accepts requests. A mistaken network call fails this test.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let root = temp_root();
        let cache = root.join("cache");
        let app = CoreApp::new(PlatformDirs::under(root.join("app")));
        app.context().dirs().create_all().unwrap();
        let transport = Arc::new(adapter(&cache, &base));
        let module = DownloadsModule::new(
            app.context(),
            transport.clone(),
            Arc::new(super::super::storage::FileDisk::new(&cache)),
            Arc::new(super::super::ports::SystemClock),
        );
        let empty_hash = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        for (id, digest, expected_state) in [
            ("empty-good", empty_hash, TaskState::Succeeded),
            (
                "empty-bad",
                "0000000000000000000000000000000000000000",
                TaskState::Failed,
            ),
        ] {
            // Stale content must be truncated before verifying known emptiness.
            transport
                .store
                .open_partial(id, 0)
                .unwrap()
                .write_all(b"stale")
                .unwrap();
            let request = CommandRequest {
                request_id: RequestId::new(id).unwrap(),
                operation_id: Some(OperationId::new(id).unwrap()),
                entity_id: None,
                expected_revision: None,
                payload: json!({"command":"downloads.fetch","artifact":{
                    "artifactId":id,"sourceId":"test","url":format!("{base}empty"),
                    "size":0,"hash":{"algorithm":"sha1","digest":digest}
                }}),
            };
            let task_id = match module.handle(&request).unwrap() {
                CommandResponse::Accepted { task_id, .. } => task_id,
                _ => panic!("expected accepted task"),
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            let task = loop {
                let task = module.task_service().snapshot(&task_id).unwrap();
                if matches!(
                    task.state,
                    TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
                ) {
                    break task;
                }
                assert!(Instant::now() < deadline, "zero length task did not settle");
                thread::sleep(Duration::from_millis(10));
            };
            assert_eq!(task.state, expected_state, "{task:?}");
            if expected_state == TaskState::Failed {
                assert_eq!(task.error.unwrap().code.as_str(), "downloads.hash.mismatch");
                assert_eq!(
                    transport
                        .materialize_cached(
                            &ArtifactHash::new("sha1", digest).unwrap(),
                            "bad-inspect"
                        )
                        .unwrap(),
                    None
                );
            }
        }
        let hash = ArtifactHash::new("sha1", empty_hash).unwrap();
        assert_eq!(
            transport.materialize_cached(&hash, "inspect").unwrap(),
            Some(0)
        );
        assert_eq!(transport.hash_bytes("inspect", "sha1").unwrap(), empty_hash);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(module);
        drop(transport);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn module_restarts_with_one_stream_when_ranges_are_ignored() {
        use super::super::service::DownloadsModule;
        use crate::{CoreApp, CoreModule};
        use serde_json::json;
        use shtmcl_domain::{CommandRequest, CommandResponse, OperationId, RequestId, TaskState};
        use shtmcl_platform::PlatformDirs;
        use std::sync::Arc;
        use std::time::Instant;

        let (base, server) = range_fallback_server();
        let root = temp_root();
        let cache = root.join("cache");
        let app = CoreApp::new(PlatformDirs::under(root.join("app")));
        app.context().dirs().create_all().unwrap();
        let transport = Arc::new(adapter(&cache, &base));
        let module = DownloadsModule::new(
            app.context(),
            transport.clone(),
            Arc::new(super::super::storage::FileDisk::new(&cache)),
            Arc::new(super::super::ports::SystemClock),
        );
        let mut payload = json!({
            "artifact": {
                "artifactId": "fallback-resource",
                "sourceId": "test",
                "url": format!("{base}object"),
                "size": 5,
                "hash": { "algorithm": "sha1", "digest": "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d" }
            },
            "chunkBytes": 5
        });
        payload["command"] = json!("downloads.fetch");
        let request = CommandRequest {
            request_id: RequestId::new("request-full-fallback").unwrap(),
            operation_id: Some(OperationId::new("operation-full-fallback").unwrap()),
            entity_id: None,
            expected_revision: None,
            payload,
        };
        let response = module.handle(&request).unwrap();
        let task_id = match response {
            CommandResponse::Accepted { task_id, .. } => task_id,
            CommandResponse::Completed { .. } => panic!("expected accepted task"),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let task = loop {
            if let Some(task) = module.task_service().snapshot(&task_id) {
                if matches!(
                    task.state,
                    TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
                ) {
                    break task;
                }
            }
            assert!(Instant::now() < deadline, "download worker did not settle");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(task.state, TaskState::Succeeded);
        assert!(task.evidence.contains(&"completed_artifacts".to_string()));
        assert_eq!(
            server.join().unwrap(),
            vec!["GET /object HTTP/1.1 range", "GET /object HTTP/1.1"]
        );
        let hash = ArtifactHash::new("sha1", "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        assert_eq!(
            transport.materialize_cached(&hash, "cached-copy").unwrap(),
            Some(5)
        );
        assert_eq!(
            transport.hash_bytes("cached-copy", "sha1").unwrap(),
            hash.digest
        );
        drop(module);
        drop(transport);
        let mut cleaned = false;
        for attempt in 0..5 {
            match std::fs::remove_dir_all(&root) {
                Ok(()) => {
                    cleaned = true;
                    break;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::PermissionDenied && attempt < 4 =>
                {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => panic!("failed to remove HTTP test root: {error}"),
            }
        }
        assert!(cleaned, "HTTP test root cleanup did not complete");
    }

    #[test]
    fn task_cancel_interrupts_a_stalled_http_body_and_resume_verifies_the_file() {
        stalled_cancel_and_resume(false);
    }

    #[test]
    fn explicit_stop_cleans_partial_before_terminal_and_can_redownload() {
        stalled_cancel_and_resume(true);
    }

    fn stalled_cancel_and_resume(discard_on_cancel: bool) {
        use super::super::service::DownloadsModule;
        use crate::{CoreApp, CoreModule};
        use serde_json::json;
        use shtmcl_domain::{
            AttemptGeneration, CancelTaskRequest, CommandRequest, CommandResponse, OperationId,
            RequestId, TaskState,
        };
        use shtmcl_platform::PlatformDirs;
        use std::sync::mpsc;
        use std::time::Instant;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (first_chunk_tx, first_chunk_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            let (mut first, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(first.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let first_request = line.trim().to_owned();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header.is_empty() {
                    break;
                }
            }
            requests.push(first_request);
            first
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-7/8\r\nContent-Length: 8\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nab",
                )
                .unwrap();
            first.flush().unwrap();
            first_chunk_tx.send(()).unwrap();
            thread::sleep(Duration::from_secs(2));
            let _ = first.write_all(b"cdefgh");

            let (mut second, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(second.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let second_request = line.trim().to_owned();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header.is_empty() {
                    break;
                }
            }
            requests.push(second_request);
            second
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-7/8\r\nContent-Length: 8\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabcdefgh",
                )
                .unwrap();
            requests
        });

        let root = temp_root();
        let cache = root.join("cache");
        let app = CoreApp::new(PlatformDirs::under(root.join("app")));
        app.context().dirs().create_all().unwrap();
        let transport = Arc::new(adapter(&cache, &format!("http://{address}/")));
        let module = DownloadsModule::new(
            app.context(),
            transport.clone(),
            Arc::new(super::super::storage::FileDisk::new(&cache)),
            Arc::new(super::super::ports::SystemClock),
        );
        let digest = "425af12a0743502b322e93a015bcf868e324d56a";
        let make_request = |request_id: &str, operation_id: &str, command: &str| {
            let mut payload = json!({
                "artifact": {
                    "artifactId": "slow-resource",
                    "sourceId": "test",
                    "url": format!("http://{address}/object"),
                    "size": 8,
                    "hash": { "algorithm": "sha1", "digest": digest }
                },
                "chunkBytes": 8
            });
            payload["command"] = json!(command);
            payload["discardOnCancel"] = json!(discard_on_cancel);
            CommandRequest {
                request_id: RequestId::new(request_id).unwrap(),
                operation_id: Some(OperationId::new(operation_id).unwrap()),
                entity_id: None,
                expected_revision: None,
                payload,
            }
        };
        let first = module
            .handle(&make_request(
                "request-slow",
                "operation-slow",
                "downloads.fetch",
            ))
            .unwrap();
        let task_id = match first {
            CommandResponse::Accepted { task_id, .. } => task_id,
            CommandResponse::Completed { .. } => panic!("expected accepted download"),
        };
        first_chunk_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("server sent the first body fragment");
        let started_cancel = Instant::now();
        let outcome = module.task_service().request_cancel(&CancelTaskRequest {
            task_id: task_id.clone(),
            attempt_generation: AttemptGeneration::INITIAL,
        });
        assert_eq!(outcome.decision, shtmcl_domain::CancelDecision::Requested);
        let deadline = Instant::now() + Duration::from_secs(2);
        let cancelled = loop {
            let task = module.task_service().snapshot(&task_id).unwrap();
            if matches!(
                task.state,
                TaskState::Cancelled | TaskState::Failed | TaskState::Succeeded
            ) {
                break task;
            }
            assert!(
                Instant::now() < deadline,
                "cancelled HTTP task did not settle"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(cancelled.state, TaskState::Cancelled);
        if discard_on_cancel {
            assert_eq!(std::fs::read_dir(cache.join("partial")).unwrap().count(), 0,
                "explicit stop must clean its partial before reporting cancelled");
            thread::sleep(Duration::from_millis(80));
            assert_eq!(std::fs::read_dir(cache.join("partial")).unwrap().count(), 0,
                "stopped writer must not recreate the partial");
        }
        let cancel_elapsed = started_cancel.elapsed();
        println!("stalled-response-cancel-ms={}", cancel_elapsed.as_millis());
        assert!(
            cancel_elapsed < Duration::from_millis(1500),
            "body cancellation should be observed after the bounded read wait"
        );

        let resumed = module
            .handle(&make_request(
                "request-slow-resume",
                "operation-slow-resume",
                "downloads.resume",
            ))
            .unwrap();
        let resumed_id = match resumed {
            CommandResponse::Accepted { task_id, .. } => task_id,
            CommandResponse::Completed { .. } => panic!("expected accepted resume"),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let completed = loop {
            let task = module.task_service().snapshot(&resumed_id).unwrap();
            if matches!(
                task.state,
                TaskState::Cancelled | TaskState::Failed | TaskState::Succeeded
            ) {
                break task;
            }
            assert!(
                Instant::now() < deadline,
                "resumed HTTP task did not settle"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(completed.state, TaskState::Succeeded);
        assert_eq!(module.cache_snapshot().0, 1);
        assert_eq!(
            server.join().unwrap(),
            vec!["GET /object HTTP/1.1", "GET /object HTTP/1.1"]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn task_cancel_interrupts_an_unknown_size_probe() {
        use std::io::Read;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Instant;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (request_started_tx, request_started_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request_line = line.trim().to_owned();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header.is_empty() {
                    break;
                }
            }
            request_started_tx.send(()).unwrap();
            let mut byte = [0u8; 1];
            let _ = stream.read(&mut byte);
            request_line
        });

        let root = temp_root();
        let downloads = adapter(&root, &format!("http://{address}/"));
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_flag = Arc::clone(&cancelled);
        let cancel_thread = thread::spawn(move || {
            request_started_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("probe request reached the controlled server");
            thread::sleep(Duration::from_millis(20));
            cancel_flag.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let outcome =
            downloads.probe_cancellable("test", "object", &|| cancelled.load(Ordering::Acquire));
        cancel_thread.join().unwrap();
        assert_eq!(outcome, Err("downloads.task.cancelled".into()));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(server.join().unwrap(), "HEAD /object HTTP/1.1");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_etag_rejects_a_range_before_writing() {
        let (base, server) = server(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/3\r\nContent-Length: 3\r\nETag: \"new\"\r\nConnection: close\r\n\r\nabc".to_vec());
        let root = temp_root();
        let downloads = adapter(&root, &base);
        assert_eq!(
            downloads.fetch(
                "test",
                "object",
                ByteRange { start: 0, end: 3 },
                Some("\"old\"")
            ),
            FetchOutcome::RangeUnsupported
        );
        server.join().unwrap();
        assert_eq!(
            downloads.hash_bytes("object", "sha1"),
            Err("downloads.disk.hash_failed".into())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn throttling_respects_retry_after_seconds() {
        let (base, server) = server(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec());
        let root = temp_root();
        let downloads = adapter(&root, &base);
        let before = now_ms();
        let outcome = downloads.fetch("test", "object", ByteRange { start: 0, end: 1 }, None);
        let FetchOutcome::Throttled { retry_at } = outcome else {
            panic!("expected throttle")
        };
        assert!(retry_at >= before + 2_800 && retry_at <= before + 3_200);
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn probe_supports_unknown_length_sources() {
        let (base, server) = range_probe_server();
        let root = temp_root();
        let downloads = adapter(&root, &base);
        assert_eq!(downloads.probe("test", "metadata.json").unwrap(), None);
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn truncated_stream_is_not_reported_as_a_completed_range() {
        let (base, server) = server(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/4\r\nContent-Length: 4\r\nConnection: close\r\n\r\nab".to_vec());
        let root = temp_root();
        let downloads = adapter(&root, &base);
        assert!(matches!(
            downloads.fetch("test", "object", ByteRange { start: 0, end: 4 }, None),
            FetchOutcome::TransportFailed(_)
        ));
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn follows_a_same_origin_redirect() {
        let (base, server) = redirect_server();
        let root = temp_root();
        let downloads = adapter(&root, &base);
        assert!(matches!(
            downloads.fetch("test", "object", ByteRange { start: 0, end: 3 }, None),
            FetchOutcome::Bytes {
                range: ByteRange { start: 0, end: 3 },
                ..
            }
        ));
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_untrusted_source_and_traversal_path() {
        assert!(HttpSource::new("bad", "http://example.com/").is_err());
        let source = HttpSource::new("test", "https://example.com/base/").unwrap();
        assert!(source.resolve("../escape").is_err());
        assert!(source.resolve("https://evil.example/file").is_err());
    }

    #[test]
    fn source_url_mapping_preserves_signed_query_parameters() {
        let source = HttpSource::mirror(
            "mirror",
            "https://bmclapi2.bangbang93.com/",
            &["piston-meta.mojang.com"],
        )
        .unwrap();
        let mapped = source
            .resolve_url("https://piston-meta.mojang.com/mc/game/manifest.json?signature=a%2Fb")
            .unwrap();
        assert_eq!(
            mapped.as_str(),
            "https://bmclapi2.bangbang93.com/mc/game/manifest.json?signature=a%2Fb"
        );
        assert!(source
            .resolve_url("https://untrusted.example/file")
            .is_err());
    }

    #[test]
    fn redirects_are_limited_to_sources_and_the_bmclapi_signed_cdn() {
        let allowed = ["piston-meta.mojang.com".to_string()].into_iter().collect();
        let bmcl = Url::parse("https://bmclapi2.bangbang93.com/file").unwrap();
        let signed_cdn =
            Url::parse("https://18.749333.xyz/download/hash?name=file&s=y&e=token").unwrap();
        let sakura_cdn =
            Url::parse("https://bmclapi18.sakura.sld.tw/download/hash?name=file&s=y&e=token")
                .unwrap();
        let rotated_cdn =
            Url::parse("https://bmclapi17.sakura.sld.tw/download/hash?name=file&s=z&e=next")
                .unwrap();
        let other_object =
            Url::parse("https://bmclapi17.sakura.sld.tw/download/other?name=file&s=z&e=next")
                .unwrap();
        let untrusted = Url::parse("https://evil.example/download").unwrap();
        assert!(redirect_allowed(Some(&bmcl), &signed_cdn, &allowed));
        assert!(redirect_allowed(Some(&bmcl), &sakura_cdn, &allowed));
        assert!(redirect_allowed(Some(&sakura_cdn), &rotated_cdn, &allowed));
        assert!(!redirect_allowed(Some(&untrusted), &signed_cdn, &allowed));
        assert!(!redirect_allowed(Some(&bmcl), &untrusted, &allowed));
        assert!(!redirect_allowed(
            Some(&sakura_cdn),
            &other_object,
            &allowed
        ));
    }

    #[test]
    #[ignore = "real Mojang HTTPS and SHA-256 check; run explicitly for release evidence"]
    fn downloads_real_official_version_manifest() {
        let url = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
        let artifact_id = "mc/game/version_manifest_v2.json";
        for source_id in ["mojang-meta", "bmclapi-meta"] {
            let root = temp_root();
            let downloads = HttpDownloads::new(&root, standard_sources()).unwrap();
            let size = downloads
                .probe_url(source_id, artifact_id, url)
                .unwrap()
                .expect("source response length");
            let range = ByteRange {
                start: 0,
                end: size,
            };
            let outcome = downloads.fetch_url(source_id, artifact_id, url, range, None);
            println!("{source_id}: fetch={outcome:?}");
            assert!(matches!(
                outcome,
                FetchOutcome::Bytes { range: served, .. } if served == range
            ));
            downloads.commit_range(artifact_id, range).unwrap();
            let digest = downloads.hash_bytes(artifact_id, "sha256").unwrap();
            println!("{source_id}: bytes={size}, sha256={digest}");
            assert_eq!(
                digest,
                "1e9f0052c6f385576f0effac072f2b32278346ce647d681e396dcf7b6f7d738a"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

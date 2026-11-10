//! Ports 03 needs from the outside world, and the default **fail-closed**
//! implementations used when the app layer has not wired a real one.
//!
//! The module never opens a socket itself. It asks a [`DownloadPort`] for bytes,
//! a [`DiskPort`] for space and a [`ClockPort`] for time, so a test can drive
//! interruption, throttling and disk exhaustion deterministically instead of by
//! sleeping.

use std::sync::Mutex;

use crate::downloads::state::{ArtifactHash, ByteRange};

/// What one fetch attempt produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    /// These bytes arrived. `etag` identifies the version, so a resume against a
    /// changed file is detected instead of stitching two revisions together.
    Bytes {
        range: ByteRange,
        etag: Option<String>,
    },
    /// The source does not serve ranges; the transfer must restart from zero.
    RangeUnsupported,
    /// The source is rate limiting; `retry_at` is when to try again (ms).
    Throttled { retry_at: u64 },
    /// Retryable transport failure.
    TransportFailed(String),
    /// The source served a body whose hash does not match.
    HashMismatch {
        expected: ArtifactHash,
        actual: String,
    },
    /// The owning task requested cancellation while this response was active.
    Cancelled,
}

/// Byte source for this module.
pub trait DownloadPort: Send + Sync {
    /// Total size, when the source reports one.
    fn probe(&self, source_id: &str, artifact_id: &str) -> Result<Option<u64>, String>;

    fn probe_cancellable(
        &self,
        source_id: &str,
        artifact_id: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<u64>, String> {
        if cancelled() {
            return Err("downloads.task.cancelled".to_string());
        }
        self.probe(source_id, artifact_id)
    }

    /// Probe an exact URL while retaining `source_id` for policy and scheduling.
    /// Legacy fixtures may use the source-relative path contract above.
    fn probe_url(
        &self,
        source_id: &str,
        artifact_id: &str,
        _url: &str,
    ) -> Result<Option<u64>, String> {
        self.probe(source_id, artifact_id)
    }

    fn probe_url_cancellable(
        &self,
        source_id: &str,
        artifact_id: &str,
        url: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<u64>, String> {
        if cancelled() {
            return Err("downloads.task.cancelled".to_string());
        }
        self.probe_url(source_id, artifact_id, url)
    }

    /// Fetch one range. Implementations must honour the range exactly; a source
    /// that cannot is reported as [`FetchOutcome::RangeUnsupported`].
    fn fetch(
        &self,
        source_id: &str,
        artifact_id: &str,
        range: ByteRange,
        etag: Option<&str>,
    ) -> FetchOutcome;

    /// Fetch a range while observing cooperative task cancellation. Legacy
    /// transports retain their previous behavior and check once before I/O.
    fn fetch_cancellable(
        &self,
        source_id: &str,
        artifact_id: &str,
        range: ByteRange,
        etag: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> FetchOutcome {
        if cancelled() {
            return FetchOutcome::Cancelled;
        }
        self.fetch(source_id, artifact_id, range, etag)
    }

    /// Fetch an exact URL. Implementations that do not need URL routing can
    /// retain the simple source-relative behavior.
    fn fetch_url(
        &self,
        source_id: &str,
        artifact_id: &str,
        _url: &str,
        range: ByteRange,
        etag: Option<&str>,
    ) -> FetchOutcome {
        self.fetch(source_id, artifact_id, range, etag)
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
        if cancelled() {
            return FetchOutcome::Cancelled;
        }
        self.fetch_url(source_id, artifact_id, url, range, etag)
    }

    /// Restart a resource as one streamed response when a source rejects byte
    /// ranges. Implementations may return `RangeUnsupported` if no safe full
    /// request is available.
    fn fetch_full(&self, source_id: &str, _artifact_id: &str) -> FetchOutcome {
        FetchOutcome::TransportFailed(format!("source:{source_id}:full_fetch_unavailable"))
    }

    fn fetch_full_cancellable(
        &self,
        source_id: &str,
        artifact_id: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> FetchOutcome {
        if cancelled() {
            return FetchOutcome::Cancelled;
        }
        self.fetch_full(source_id, artifact_id)
    }

    fn fetch_full_url(&self, source_id: &str, artifact_id: &str, _url: &str) -> FetchOutcome {
        self.fetch_full(source_id, artifact_id)
    }

    fn fetch_full_url_cancellable(
        &self,
        source_id: &str,
        artifact_id: &str,
        url: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> FetchOutcome {
        if cancelled() {
            return FetchOutcome::Cancelled;
        }
        self.fetch_full_url(source_id, artifact_id, url)
    }

    /// Hash the bytes already written for this artifact.
    fn hash_bytes(&self, artifact_id: &str, algorithm: &str) -> Result<String, String>;

    /// Create a durable empty partial for a resource with a known zero length.
    /// Verification and publication still follow the normal digest path.
    fn prepare_empty(&self, _artifact_id: &str) -> Result<(), String> {
        Err("downloads.disk.empty_unavailable".into())
    }

    /// Persist the completed range before it is counted as downloaded. Durability
    /// first is what makes an interruption resumable instead of a restart.
    fn commit_range(&self, artifact_id: &str, range: ByteRange) -> Result<(), String>;

    /// Move verified bytes into the shared cache under their content key.
    fn publish(&self, hash: &ArtifactHash, artifact_id: &str) -> Result<u64, String>;

    /// Materialize a previously verified cache entry for this request. Returns
    /// `None` when the content is absent, so the caller may fetch it normally.
    fn materialize_cached(
        &self,
        _hash: &ArtifactHash,
        _artifact_id: &str,
    ) -> Result<Option<u64>, String> {
        Err("downloads.cache.materialize.unavailable".to_string())
    }

    /// Recover a verified disk object absent from the in-memory cache index.
    /// Optional: ports without persistent caches continue with normal fetches.
    /// `Some(size)` requires digest verification and materialization for this
    /// artifact ID. Missing objects return `None`; unreadable/invalid ones fail.
    fn recover_cached(
        &self,
        _hash: &ArtifactHash,
        _artifact_id: &str,
    ) -> Result<Option<u64>, String> {
        Ok(None)
    }

    /// Remove a content-addressed cache file. Implementations must treat an
    /// already missing file as success so index recovery can be retried safely.
    fn remove_cached(&self, _hash: &ArtifactHash) -> Result<(), String> {
        Err("downloads.cache.remove.unavailable".to_string())
    }

    /// Drop partial bytes for an artifact.
    fn discard(&self, artifact_id: &str) -> Result<(), String>;
}

/// Space checks, kept separate from transfers.
pub trait DiskPort: Send + Sync {
    /// Free bytes at the cache location.
    fn available(&self) -> Result<u64, String>;
}

/// Time source; injected so lease/backoff behaviour is testable.
pub trait ClockPort: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Default clock: real wall time.
pub struct SystemClock;

impl ClockPort for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Default download port: refuses every transfer.
///
/// This is the fail-closed default. A build that ships without a transport wired
/// must answer "source unavailable" rather than pretend a download succeeded —
/// otherwise the UX layer would show a completed install with no files on disk.
pub struct UnavailableDownloads;

impl DownloadPort for UnavailableDownloads {
    fn probe(&self, _source_id: &str, _artifact_id: &str) -> Result<Option<u64>, String> {
        Err("downloads.transport.unavailable".to_string())
    }

    fn fetch(
        &self,
        source_id: &str,
        _artifact_id: &str,
        _range: ByteRange,
        _etag: Option<&str>,
    ) -> FetchOutcome {
        FetchOutcome::TransportFailed(format!("source:{source_id}:unavailable"))
    }

    fn hash_bytes(&self, _artifact_id: &str, _algorithm: &str) -> Result<String, String> {
        Err("downloads.transport.unavailable".to_string())
    }

    fn commit_range(&self, _artifact_id: &str, _range: ByteRange) -> Result<(), String> {
        Err("downloads.transport.unavailable".to_string())
    }

    fn publish(&self, _hash: &ArtifactHash, _artifact_id: &str) -> Result<u64, String> {
        Err("downloads.transport.unavailable".to_string())
    }

    fn discard(&self, _artifact_id: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Default disk port: reports zero free space.
///
/// Zero rather than a large number on purpose: an unbounded guess would let a
/// transfer start and fail after writing gigabytes. Zero makes the pre-flight
/// check refuse the request with a clear code instead.
pub struct UnknownDisk;

impl DiskPort for UnknownDisk {
    fn available(&self) -> Result<u64, String> {
        Ok(0)
    }
}

/// Test/embedded download port that serves a scripted byte stream.
///
/// Not used in production wiring; it exists so behaviour (resume, throttle,
/// hash mismatch, disk exhaustion) is exercised by the crate's own tests without
/// a network or a filesystem.
#[derive(Default)]
pub struct FixtureDownloads {
    inner: Mutex<FixtureState>,
}

#[derive(Default)]
struct FixtureState {
    /// Size of each artifact.
    pub sizes: std::collections::BTreeMap<String, u64>,
    /// Bytes committed per artifact.
    pub written: std::collections::BTreeMap<String, Vec<ByteRange>>,
    /// Next fetch answers, consumed in order.
    pub script: Vec<FetchOutcome>,
    /// Content hash reported by `hash_bytes`.
    pub digests: std::collections::BTreeMap<String, String>,
    /// Artifacts published to the cache.
    pub published: Vec<String>,
    /// Deterministic cache metadata used by the fixture port.
    pub cached_sizes: std::collections::BTreeMap<String, u64>,
    pub fetch_count: usize,
    pub fetch_delay_ms: u64,
}

impl FixtureDownloads {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare an artifact of `size` bytes with digest `digest`.
    pub fn with_artifact(self, id: &str, size: u64, digest: &str) -> Self {
        {
            let mut inner = self.inner.lock().expect("lock");
            inner.sizes.insert(id.to_string(), size);
            inner.digests.insert(id.to_string(), digest.to_string());
        }
        self
    }

    /// Queue the next fetch outcome.
    pub fn scripted(self, outcome: FetchOutcome) -> Self {
        self.inner.lock().expect("lock").script.push(outcome);
        self
    }

    /// Delay fetch responses so concurrency and cancellation tests can place a
    /// second subscriber while the first transfer is still active.
    pub fn with_fetch_delay(self, delay_ms: u64) -> Self {
        self.inner.lock().expect("lock").fetch_delay_ms = delay_ms;
        self
    }

    pub fn written_len(&self, id: &str) -> u64 {
        self.inner
            .lock()
            .expect("lock")
            .written
            .get(id)
            .map(|ranges| ranges.iter().map(|r| r.len()).sum())
            .unwrap_or(0)
    }

    pub fn published_count(&self) -> usize {
        self.inner.lock().expect("lock").published.len()
    }

    pub fn fetch_count(&self) -> usize {
        self.inner.lock().expect("lock").fetch_count
    }

    pub fn cached_count(&self) -> usize {
        self.inner.lock().expect("lock").cached_sizes.len()
    }
}

impl DownloadPort for FixtureDownloads {
    fn probe(&self, _source_id: &str, artifact_id: &str) -> Result<Option<u64>, String> {
        Ok(self
            .inner
            .lock()
            .expect("lock")
            .sizes
            .get(artifact_id)
            .copied())
    }

    fn fetch(
        &self,
        _source_id: &str,
        artifact_id: &str,
        range: ByteRange,
        _etag: Option<&str>,
    ) -> FetchOutcome {
        let mut inner = self.inner.lock().expect("lock");
        inner.fetch_count += 1;
        if inner.fetch_delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(inner.fetch_delay_ms));
        }
        if !inner.script.is_empty() {
            return inner.script.remove(0);
        }
        // Default: serve the requested range.
        inner
            .written
            .entry(artifact_id.to_string())
            .or_default()
            .push(range);
        FetchOutcome::Bytes {
            range,
            etag: Some("fixture".to_string()),
        }
    }

    fn hash_bytes(&self, artifact_id: &str, _algorithm: &str) -> Result<String, String> {
        Ok(self
            .inner
            .lock()
            .expect("lock")
            .digests
            .get(artifact_id)
            .cloned()
            .unwrap_or_default())
    }

    fn commit_range(&self, artifact_id: &str, range: ByteRange) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("lock");
        let entry = inner.written.entry(artifact_id.to_string()).or_default();
        if !entry.contains(&range) {
            entry.push(range);
        }
        Ok(())
    }

    fn publish(&self, hash: &ArtifactHash, artifact_id: &str) -> Result<u64, String> {
        let mut inner = self.inner.lock().expect("lock");
        inner.published.push(hash.key());
        let size = inner.sizes.get(artifact_id).copied().unwrap_or(0);
        inner.cached_sizes.insert(hash.key(), size);
        Ok(size)
    }

    fn materialize_cached(
        &self,
        hash: &ArtifactHash,
        artifact_id: &str,
    ) -> Result<Option<u64>, String> {
        let mut inner = self.inner.lock().expect("lock");
        let Some(size) = inner.cached_sizes.get(&hash.key()).copied() else {
            return Ok(None);
        };
        inner.sizes.insert(artifact_id.to_string(), size);
        inner.written.insert(
            artifact_id.to_string(),
            if size > 0 {
                vec![ByteRange {
                    start: 0,
                    end: size,
                }]
            } else {
                Vec::new()
            },
        );
        Ok(Some(size))
    }

    fn remove_cached(&self, hash: &ArtifactHash) -> Result<(), String> {
        self.inner
            .lock()
            .expect("lock")
            .cached_sizes
            .remove(&hash.key());
        Ok(())
    }

    fn discard(&self, artifact_id: &str) -> Result<(), String> {
        self.inner.lock().expect("lock").written.remove(artifact_id);
        Ok(())
    }
}

/// Fixture disk that reports a fixed free size.
pub struct FixtureDisk {
    pub free: u64,
}

impl DiskPort for FixtureDisk {
    fn available(&self) -> Result<u64, String> {
        Ok(self.free)
    }
}

/// Fixture clock that returns a scripted sequence of times.
pub struct FixtureClock {
    ticks: Mutex<Vec<u64>>,
    fallback: u64,
}

impl FixtureClock {
    pub fn new(ticks: Vec<u64>) -> Self {
        let fallback = ticks.last().copied().unwrap_or(0);
        Self {
            ticks: Mutex::new(ticks),
            fallback,
        }
    }
}

impl ClockPort for FixtureClock {
    fn now_ms(&self) -> u64 {
        let mut ticks = self.ticks.lock().expect("lock");
        if ticks.len() > 1 {
            ticks.remove(0)
        } else {
            self.fallback
        }
    }
}

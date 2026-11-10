//! Artifact identity, the shared cache index and the bounded scheduler.
//!
//! Three ideas keep downloads fast and safe at the same time:
//!
//! 1. **Identity is the hash, not the URL.** Two mods that ship the same jar, or
//!    the same library requested by two instances, are one artifact. The cache is
//!    keyed by `(algorithm, digest)`, so deduplication happens by content.
//! 2. **Concurrency is bounded per source, not per request.** A host that answers
//!    twelve parallel connections with 429 is slower than one that uses three, so
//!    the scheduler holds a slot per source and queues the rest.
//! 3. **Progress is durable.** Resumable byte ranges are persisted before the
//!    bytes are counted as downloaded, so an interruption resumes instead of
//!    restarting. Only a verified artifact is published to the cache.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::downloads::error::DownloadsError;

/// Default number of parallel connections to one source.
pub const DEFAULT_SOURCE_CONCURRENCY: usize = 3;
/// Default number of artifacts in flight across all sources.
pub const DEFAULT_TOTAL_CONCURRENCY: usize = 6;
/// Retained completed ranges, bounded so the index cannot grow without limit.
/// Bound the retained transfer index while leaving enough room for fragmented
/// servers and out-of-order range responses. 4096 ranges keep the in-memory
/// representation below roughly 64 KiB per active artifact.
const MAX_RANGES: usize = 4096;

/// Content identity of an artifact.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactHash {
    /// `sha1` or `sha256`; lowercase, no prefix.
    pub algorithm: String,
    /// Lowercase hex digest.
    pub digest: String,
}

impl ArtifactHash {
    /// Build and validate. A malformed digest is rejected at the boundary instead
    /// of becoming a cache key that never matches.
    pub fn new(algorithm: &str, digest: &str) -> Result<Self, DownloadsError> {
        let algorithm = algorithm.trim().to_ascii_lowercase();
        let digest = digest.trim().to_ascii_lowercase();
        let expected_len = match algorithm.as_str() {
            "sha1" => 40,
            "sha256" => 64,
            _ => return Err(DownloadsError::InvalidRequest("algorithm")),
        };
        if digest.len() != expected_len || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(DownloadsError::InvalidRequest("digest"));
        }
        Ok(Self { algorithm, digest })
    }

    /// Cache key: the same content in two different URLs maps to one entry.
    pub fn key(&self) -> String {
        format!("{}:{}", self.algorithm, self.digest)
    }
}

impl fmt::Display for ArtifactHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key())
    }
}

/// What a caller asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRequest {
    /// Caller-chosen id, stable across retries.
    pub artifact_id: String,
    /// Expected content hash. Absent means "take whatever the source serves",
    /// which is allowed for small metadata files but never for a game library.
    pub expected: Option<ArtifactHash>,
    /// Which mirror/source family to use. Opaque to this module.
    pub source_id: String,
    /// Exact signed or canonical URL. Kept private to the download worker and
    /// never copied into ordinary task/event errors.
    pub source_url: Option<String>,
    /// Known size, when the caller has it. Used for the disk-space check.
    pub size: Option<u64>,
}

/// Lifecycle of one artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactState {
    /// Waiting for a source slot.
    Queued,
    Downloading,
    /// Bytes are on disk but the hash has not been confirmed.
    Verifying,
    /// Verified and published to the cache.
    Ready,
    /// Stopped by cancellation; the ranges are kept for a resume.
    Interrupted,
    Failed,
}

impl ArtifactState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Verifying => "verifying",
            Self::Ready => "ready",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
        }
    }

    /// A terminal state never reports progress again; a new attempt needs an
    /// explicit resume, which creates a new generation.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Ready | Self::Failed)
    }
}

/// A half-open byte range `[start, end)` that is present on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(self) -> bool {
        self.end <= self.start
    }

    /// Merge two ranges when they overlap or touch, so a resumed download keeps a
    /// short range list instead of one entry per chunk.
    fn merge(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    fn touches(self, other: Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }
}

/// Persisted progress of one artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    pub artifact_id: String,
    pub source_id: String,
    pub hash: Option<ArtifactHash>,
    pub total: Option<u64>,
    pub ranges: Vec<ByteRange>,
    pub state: ArtifactState,
    /// Attempt generation the last write belongs to. A late write from an older
    /// generation is dropped instead of corrupting the range list.
    pub generation: u32,
    /// Unique owner for one accepted operation. Task attempt generations are
    /// local to a task and can both be `1`, so they cannot fence cross-operation
    /// late writes on their own.
    #[serde(default)]
    pub operation_epoch: u64,
}

impl Transfer {
    pub fn new(request: &ArtifactRequest, generation: u32) -> Self {
        Self {
            artifact_id: request.artifact_id.clone(),
            source_id: request.source_id.clone(),
            hash: request.expected.clone(),
            total: request.size,
            ranges: Vec::new(),
            state: ArtifactState::Queued,
            generation,
            operation_epoch: 0,
        }
    }

    /// Fence a write by both task attempt and the operation that currently owns
    /// this artifact record.
    pub fn record_for(
        &mut self,
        range: ByteRange,
        generation: u32,
        operation_epoch: u64,
    ) -> Result<(), DownloadsError> {
        if operation_epoch != self.operation_epoch {
            return Err(DownloadsError::IdempotencyConflict);
        }
        self.record(range, generation)
    }

    /// Bytes already present, computed from the merged range list.
    pub fn downloaded(&self) -> u64 {
        self.ranges.iter().map(|range| range.len()).sum()
    }

    /// Add a completed range without ever treating a gap as downloaded. Ranges
    /// remain sorted and merged, and a write from an older generation is refused.
    pub fn record(&mut self, range: ByteRange, generation: u32) -> Result<(), DownloadsError> {
        if generation < self.generation {
            return Err(DownloadsError::IdempotencyConflict);
        }
        if range.is_empty() {
            return Err(DownloadsError::RangeInvalid(self.artifact_id.clone()));
        }
        if let Some(total) = self.total {
            if range.end > total {
                return Err(DownloadsError::RangeInvalid(self.artifact_id.clone()));
            }
        }
        let mut merged = range;
        let first = self
            .ranges
            .partition_point(|current| current.end < merged.start);
        let mut after = first;
        while after < self.ranges.len() && self.ranges[after].start <= merged.end {
            merged = merged.merge(self.ranges[after]);
            after += 1;
        }

        let resulting_len = self.ranges.len() - (after - first) + 1;
        if resulting_len > MAX_RANGES {
            return Err(DownloadsError::RangeLimit(self.artifact_id.clone()));
        }

        self.ranges.splice(first..after, [merged]);
        self.generation = generation;
        Ok(())
    }

    /// True when every byte is present.
    pub fn is_complete(&self) -> bool {
        match self.total {
            Some(total) if total > 0 => {
                self.ranges.len() == 1 && self.ranges[0].start == 0 && self.ranges[0].end >= total
            }
            _ => false,
        }
    }

    /// The next range to fetch, so a resume continues instead of restarting.
    pub fn next_gap(&self, chunk: u64) -> Option<ByteRange> {
        let total = self.total?;
        let mut cursor = 0;
        for range in &self.ranges {
            if range.start > cursor {
                let end = range.start.min(cursor.saturating_add(chunk)).min(total);
                return (end > cursor).then_some(ByteRange { start: cursor, end });
            }
            cursor = cursor.max(range.end);
            if cursor >= total {
                return None;
            }
        }
        let end = cursor.saturating_add(chunk).min(total);
        (end > cursor).then_some(ByteRange { start: cursor, end })
    }

    /// Evidence markers for the task, so UX only claims what is true.
    pub fn evidence(&self) -> Vec<&'static str> {
        let mut markers = Vec::new();
        if !self.ranges.is_empty() && !self.is_complete() {
            markers.push(crate::downloads::error::evidence::RESUMABLE_RANGES);
        }
        if self.state == ArtifactState::Ready {
            markers.push(crate::downloads::error::evidence::COMPLETED_ARTIFACTS);
        }
        markers
    }
}

/// Merge a sorted range list, joining overlapping and adjacent spans.
pub fn merge_ranges(ranges: Vec<ByteRange>) -> Vec<ByteRange> {
    let mut merged: Vec<ByteRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if range.is_empty() {
            continue;
        }
        match merged.last_mut() {
            Some(last) if last.touches(range) => *last = last.merge(range),
            _ => merged.push(range),
        }
    }
    merged
}

/// One bounded scheduler over sources.
#[derive(Debug, Clone)]
pub struct Scheduler {
    total_limit: usize,
    source_limit: usize,
    active: BTreeMap<String, usize>,
    active_total: usize,
    /// Sources that answered with a rate limit; they get no slot until `retry_at`.
    throttled: BTreeMap<String, u64>,
    /// Artifacts already claimed, so two requests for one hash share one transfer.
    claimed: BTreeSet<String>,
}

/// Why a source could not be used right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    Granted,
    /// The global cap is reached; the caller should queue, not fail.
    GlobalBusy,
    /// This source's cap is reached.
    SourceBusy,
    /// The source asked us to back off until `retry_at` (ms since epoch).
    Throttled {
        retry_at: u64,
    },
}

impl Scheduler {
    pub fn new(total_limit: usize, source_limit: usize) -> Self {
        Self {
            total_limit: total_limit.max(1),
            source_limit: source_limit.max(1),
            active: BTreeMap::new(),
            active_total: 0,
            throttled: BTreeMap::new(),
            claimed: BTreeSet::new(),
        }
    }

    pub fn default_limits() -> Self {
        Self::new(DEFAULT_TOTAL_CONCURRENCY, DEFAULT_SOURCE_CONCURRENCY)
    }

    /// Ask for a slot. Never blocks and never panics; the caller decides whether
    /// to queue or surface a message.
    pub fn try_acquire(&mut self, source_id: &str, now: u64) -> Slot {
        if let Some(retry_at) = self.throttled.get(source_id).copied() {
            if now < retry_at {
                return Slot::Throttled { retry_at };
            }
            self.throttled.remove(source_id);
        }
        if self.active_total >= self.total_limit {
            return Slot::GlobalBusy;
        }
        let used = self.active.get(source_id).copied().unwrap_or(0);
        if used >= self.source_limit {
            return Slot::SourceBusy;
        }
        *self.active.entry(source_id.to_string()).or_insert(0) += 1;
        self.active_total += 1;
        Slot::Granted
    }

    /// Return a slot. Releasing a source with no active slot is a no-op, so a
    /// double release after an error cannot drive the counter negative.
    pub fn release(&mut self, source_id: &str) {
        if let Some(used) = self.active.get_mut(source_id) {
            if *used > 0 {
                *used -= 1;
                self.active_total = self.active_total.saturating_sub(1);
            }
            if *used == 0 {
                self.active.remove(source_id);
            }
        }
    }

    /// Record a rate-limit answer so the next request for that source waits.
    pub fn throttle(&mut self, source_id: &str, until: u64) {
        self.throttled.insert(source_id.to_string(), until);
    }

    /// Claim an artifact key for this operation. Returns `false` when another
    /// transfer already owns it, which is how two instances asking for the same
    /// library produce one download instead of two.
    pub fn claim(&mut self, key: &str) -> bool {
        self.claimed.insert(key.to_string())
    }

    pub fn release_claim(&mut self, key: &str) {
        self.claimed.remove(key);
    }

    pub fn active_total(&self) -> usize {
        self.active_total
    }
}

/// Cache entry for a verified artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntry {
    pub hash: ArtifactHash,
    pub size: u64,
    /// How many live consumers point at this entry. Zero means it is collectable.
    pub refs: u32,
}

/// The shared cache index: content-addressed, reference counted.
///
/// Collectable does not mean deleted on release: a zero-ref entry stays until a
/// sweep, so the next instance that needs the same jar does not download it again.
#[derive(Debug, Default)]
pub struct CacheIndex {
    entries: BTreeMap<String, CacheEntry>,
}

impl CacheIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a verified artifact. Republishing the same hash with the same size
    /// is idempotent; a different size for one hash is a corrupt cache and is
    /// refused rather than silently overwritten.
    ///
    /// Returns the entry by value: returning a reference would keep the map
    /// mutably borrowed and make the two-branch insert below impossible.
    pub fn publish(
        &mut self,
        hash: &ArtifactHash,
        size: u64,
    ) -> Result<CacheEntry, DownloadsError> {
        let key = hash.key();
        match self.entries.get(&key) {
            Some(entry) if entry.size == size => Ok(entry.clone()),
            Some(_) => Err(DownloadsError::CacheCorrupt(key)),
            None => {
                let entry = CacheEntry {
                    hash: hash.clone(),
                    size,
                    refs: 0,
                };
                self.entries.insert(key, entry.clone());
                Ok(entry)
            }
        }
    }

    pub fn lookup(&self, hash: &ArtifactHash) -> Option<&CacheEntry> {
        self.entries.get(&hash.key())
    }

    /// Remove an entry after its file has been deleted successfully. Live
    /// references cannot be collected.
    pub fn remove_collectable(&mut self, hash: &ArtifactHash) -> bool {
        let key = hash.key();
        if self.entries.get(&key).is_some_and(|entry| entry.refs == 0) {
            self.entries.remove(&key);
            true
        } else {
            false
        }
    }

    /// Stable collectable entries, including the identity needed by the disk
    /// port to remove each content-addressed file.
    pub fn collectable_entries(&self) -> Vec<CacheEntry> {
        self.entries
            .values()
            .filter(|entry| entry.refs == 0)
            .cloned()
            .collect()
    }

    /// Take a reference: an instance now depends on this artifact.
    pub fn retain(&mut self, hash: &ArtifactHash) -> Result<u32, DownloadsError> {
        let key = hash.key();
        let entry = self
            .entries
            .get_mut(&key)
            .ok_or_else(|| DownloadsError::CacheMiss(key.clone()))?;
        entry.refs = entry.refs.saturating_add(1);
        Ok(entry.refs)
    }

    /// Drop a reference. Saturating, so an unbalanced release cannot wrap.
    pub fn release(&mut self, hash: &ArtifactHash) -> Result<u32, DownloadsError> {
        let key = hash.key();
        let entry = self
            .entries
            .get_mut(&key)
            .ok_or_else(|| DownloadsError::CacheMiss(key.clone()))?;
        entry.refs = entry.refs.saturating_sub(1);
        Ok(entry.refs)
    }

    /// Keys with no live consumer, in stable order.
    pub fn collectable(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.refs == 0)
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries.values().map(|entry| entry.size).sum()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: &str, size: u64) -> ArtifactRequest {
        ArtifactRequest {
            artifact_id: id.into(),
            expected: Some(ArtifactHash::new("sha256", &"a".repeat(64)).expect("hash")),
            source_id: "mirror-a".into(),
            source_url: None,
            size: Some(size),
        }
    }

    #[test]
    fn hashes_validate_length_and_hex() {
        assert!(ArtifactHash::new("sha256", &"a".repeat(64)).is_ok());
        assert!(ArtifactHash::new("SHA1", &"b".repeat(40)).is_ok());
        assert!(ArtifactHash::new("sha256", &"a".repeat(40)).is_err());
        assert!(ArtifactHash::new("md5", &"a".repeat(32)).is_err());
        assert!(ArtifactHash::new("sha256", &"z".repeat(64)).is_err());
    }

    #[test]
    fn the_same_content_from_two_urls_shares_one_cache_key() {
        let a = ArtifactHash::new("sha256", &"a".repeat(64)).expect("hash");
        let b = ArtifactHash::new("sha256", &"A".repeat(64)).expect("hash");
        assert_eq!(a.key(), b.key());
    }

    #[test]
    fn ranges_merge_when_they_overlap_or_touch() {
        let merged = merge_ranges(vec![
            ByteRange { start: 0, end: 10 },
            ByteRange { start: 10, end: 20 },
            ByteRange { start: 30, end: 40 },
            ByteRange { start: 35, end: 50 },
        ]);
        assert_eq!(
            merged,
            vec![
                ByteRange { start: 0, end: 20 },
                ByteRange { start: 30, end: 50 }
            ]
        );
    }

    #[test]
    fn fragmented_ranges_remain_exact_beyond_the_old_cap() {
        let mut transfer = Transfer::new(&request("jar", 5_000), 1);
        for index in 0..513 {
            transfer
                .record(
                    ByteRange {
                        start: index * 2,
                        end: index * 2 + 1,
                    },
                    1,
                )
                .expect("record range");
        }
        assert_eq!(transfer.downloaded(), 513);
        assert_eq!(transfer.ranges.len(), 513);
        assert_eq!(transfer.next_gap(10), Some(ByteRange { start: 1, end: 2 }));
    }

    #[test]
    fn range_limit_refuses_new_fragment_without_mutating_progress() {
        let mut transfer = Transfer::new(&request("jar", 20_000), 1);
        for index in 0..MAX_RANGES {
            transfer
                .record(
                    ByteRange {
                        start: (index as u64) * 2,
                        end: (index as u64) * 2 + 1,
                    },
                    1,
                )
                .expect("within range cap");
        }
        let before = transfer.clone();
        assert!(matches!(
            transfer.record(
                ByteRange {
                    start: MAX_RANGES as u64 * 2,
                    end: MAX_RANGES as u64 * 2 + 1,
                },
                1,
            ),
            Err(DownloadsError::RangeLimit(_))
        ));
        assert_eq!(transfer, before);
    }

    #[test]
    fn a_resume_asks_for_the_gap_not_the_whole_file() {
        let mut transfer = Transfer::new(&request("a", 100), 1);
        transfer
            .record(ByteRange { start: 0, end: 40 }, 1)
            .expect("range");
        let gap = transfer.next_gap(20).expect("gap");
        assert_eq!(gap, ByteRange { start: 40, end: 60 });
        assert_eq!(transfer.downloaded(), 40);
    }

    #[test]
    fn a_write_from_an_older_generation_is_refused() {
        let mut transfer = Transfer::new(&request("a", 100), 2);
        assert!(matches!(
            transfer.record(ByteRange { start: 0, end: 10 }, 1),
            Err(DownloadsError::IdempotencyConflict)
        ));
    }

    #[test]
    fn a_late_write_from_another_operation_is_refused_even_at_same_task_generation() {
        let mut transfer = Transfer::new(&request("a", 100), 1);
        transfer.operation_epoch = 42;
        let before = transfer.clone();
        assert!(matches!(
            transfer.record_for(ByteRange { start: 0, end: 10 }, 1, 41),
            Err(DownloadsError::IdempotencyConflict)
        ));
        assert_eq!(transfer, before);
        transfer
            .record_for(ByteRange { start: 0, end: 10 }, 1, 42)
            .expect("current operation may write");
    }

    #[test]
    fn a_range_past_the_known_size_is_refused() {
        let mut transfer = Transfer::new(&request("a", 100), 1);
        assert!(matches!(
            transfer.record(
                ByteRange {
                    start: 90,
                    end: 120
                },
                1
            ),
            Err(DownloadsError::RangeInvalid(_))
        ));
    }

    #[test]
    fn completeness_needs_one_covering_range() {
        let mut transfer = Transfer::new(&request("a", 100), 1);
        transfer
            .record(ByteRange { start: 0, end: 50 }, 1)
            .expect("r");
        assert!(!transfer.is_complete());
        transfer
            .record(
                ByteRange {
                    start: 50,
                    end: 100,
                },
                1,
            )
            .expect("r");
        assert!(transfer.is_complete());
        assert!(transfer.next_gap(10).is_none());
    }

    #[test]
    fn evidence_reports_resumable_only_while_incomplete() {
        let mut transfer = Transfer::new(&request("a", 100), 1);
        transfer
            .record(ByteRange { start: 0, end: 50 }, 1)
            .expect("r");
        assert_eq!(
            transfer.evidence(),
            vec![crate::downloads::error::evidence::RESUMABLE_RANGES]
        );
        transfer
            .record(
                ByteRange {
                    start: 50,
                    end: 100,
                },
                1,
            )
            .expect("r");
        transfer.state = ArtifactState::Ready;
        assert_eq!(
            transfer.evidence(),
            vec![crate::downloads::error::evidence::COMPLETED_ARTIFACTS]
        );
    }

    #[test]
    fn the_scheduler_caps_each_source_and_the_total() {
        let mut scheduler = Scheduler::new(3, 2);
        assert_eq!(scheduler.try_acquire("a", 0), Slot::Granted);
        assert_eq!(scheduler.try_acquire("a", 0), Slot::Granted);
        assert_eq!(scheduler.try_acquire("a", 0), Slot::SourceBusy);
        assert_eq!(scheduler.try_acquire("b", 0), Slot::Granted);
        assert_eq!(scheduler.try_acquire("c", 0), Slot::GlobalBusy);
        scheduler.release("a");
        assert_eq!(scheduler.try_acquire("c", 0), Slot::Granted);
        assert_eq!(scheduler.active_total(), 3);
    }

    #[test]
    fn a_throttled_source_is_skipped_until_its_deadline() {
        let mut scheduler = Scheduler::new(2, 1);
        scheduler.throttle("a", 1_000);
        assert_eq!(
            scheduler.try_acquire("a", 500),
            Slot::Throttled { retry_at: 1_000 }
        );
        assert_eq!(scheduler.try_acquire("a", 1_000), Slot::Granted);
    }

    #[test]
    fn releasing_an_unheld_slot_does_not_go_negative() {
        let mut scheduler = Scheduler::new(2, 1);
        scheduler.release("ghost");
        scheduler.release("ghost");
        assert_eq!(scheduler.active_total(), 0);
    }

    #[test]
    fn one_claim_per_content_prevents_a_duplicate_download() {
        let mut scheduler = Scheduler::default_limits();
        let key = ArtifactHash::new("sha256", &"a".repeat(64))
            .expect("hash")
            .key();
        assert!(scheduler.claim(&key));
        assert!(!scheduler.claim(&key));
        scheduler.release_claim(&key);
        assert!(scheduler.claim(&key));
    }

    #[test]
    fn the_cache_is_reference_counted_and_collectable_at_zero() {
        let mut cache = CacheIndex::new();
        let hash = ArtifactHash::new("sha256", &"a".repeat(64)).expect("hash");
        cache.publish(&hash, 100).expect("publish");
        assert_eq!(cache.collectable().len(), 1);
        assert_eq!(cache.retain(&hash).expect("retain"), 1);
        assert!(cache.collectable().is_empty());
        assert_eq!(cache.release(&hash).expect("release"), 0);
        assert_eq!(cache.collectable().len(), 1);
        assert_eq!(cache.total_bytes(), 100);
    }

    #[test]
    fn republishing_one_hash_with_a_different_size_is_a_corrupt_cache() {
        let mut cache = CacheIndex::new();
        let hash = ArtifactHash::new("sha256", &"a".repeat(64)).expect("hash");
        cache.publish(&hash, 100).expect("publish");
        assert!(cache.publish(&hash, 100).is_ok());
        assert!(matches!(
            cache.publish(&hash, 200),
            Err(DownloadsError::CacheCorrupt(_))
        ));
    }

    #[test]
    fn an_unknown_hash_is_a_miss_not_a_panic() {
        let mut cache = CacheIndex::new();
        let hash = ArtifactHash::new("sha256", &"b".repeat(64)).expect("hash");
        assert!(cache.lookup(&hash).is_none());
        assert!(matches!(
            cache.retain(&hash),
            Err(DownloadsError::CacheMiss(_))
        ));
    }
}

//! Game log pipeline.
//!
//! Module 02 is the *only* reader of the game's stdout/stderr. Everything else
//! (08's LogStore, diagnostics export, the UI log page) consumes bounded pages
//! through [`LogSink`] / [`LogPipeline::page`]. Two rules make that safe:
//!
//! 1. the pump always keeps draining the child pipes — its speed is never
//!    coupled to the UI, a disk flush or a diagnostic scan;
//! 2. retention is bounded by records *and* bytes; the oldest records are
//!    dropped and counted, never the newest.
//!
//! A single line is capped (initial 64 KiB) and truncated on a character
//! boundary with the omitted byte count recorded, so one hostile line cannot
//! exhaust memory or split a UTF-8 sequence.

use std::collections::{BTreeMap, VecDeque};
use std::io::{ErrorKind, Read};
use std::sync::{Arc, Mutex};

use shtmcl_domain::LaunchSessionId;

/// Initial per-line cap (64 KiB).
pub const DEFAULT_MAX_LINE_BYTES: usize = 64 * 1024;
/// Initial in-memory retention cap (8 MiB).
pub const DEFAULT_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Initial retained record count (~5,000 lines of history for the UI).
pub const DEFAULT_MAX_RECORDS: usize = 5_000;
/// Upper bound for one `launch.logs` page.
pub const MAX_PAGE_LIMIT: usize = 1_000;
/// Default page size when the caller does not ask for one.
pub const DEFAULT_PAGE_LIMIT: usize = 200;
/// Tail retention used when a paging sink (08's LogStore) owns the data: 64 KiB.
pub const DEFAULT_TAIL_BYTES: usize = 64 * 1024;
/// Tail retention in records for the same mode.
pub const DEFAULT_TAIL_RECORDS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogStream {
    Stdout,
    Stderr,
}

impl LogStream {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// One retained log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    pub sequence: u64,
    pub stream: LogStream,
    pub line: String,
    /// Bytes dropped from this line by the per-line cap (never the whole line).
    pub omitted_bytes: usize,
}

impl LogRecord {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "sequence": self.sequence,
            "stream": self.stream.as_str(),
            "line": self.line,
            "omittedBytes": self.omitted_bytes,
        })
    }
}

/// A bounded window of records plus the cursor to continue from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPage {
    pub records: Vec<LogRecord>,
    pub next_cursor: u64,
    /// Bytes discarded because retention was full (cumulative for the session).
    pub dropped_bytes: u64,
    /// Lines discarded for the same reason (cumulative for the session).
    pub dropped_lines: u64,
    /// True when more records are already retained after `next_cursor`.
    pub has_more: bool,
}

impl LogPage {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "records": self.records.iter().map(LogRecord::to_json).collect::<Vec<_>>(),
            "nextCursor": self.next_cursor,
            "droppedBytes": self.dropped_bytes,
            "droppedLines": self.dropped_lines,
            "hasMore": self.has_more,
        })
    }
}

/// Sink implemented by module 08's LogStore (and by test doubles).
///
/// Implementations must be non-blocking: they are called from the pipe pump, and
/// a slow append must never stall the game's stdout.
///
/// 08's LogStore is the single diagnostic store (00 ruling, D02-6), so a sink may
/// also serve pages. When it does, 02 stops keeping a second full ring and only
/// retains a small tail for immediate context.
pub trait LogSink: Send + Sync {
    /// Consume records; returns how many were accepted.
    fn append(&self, session: &LaunchSessionId, records: &[LogRecord]) -> usize;

    /// Record retention losses so diagnostics can report them.
    fn note_dropped(&self, session: &LaunchSessionId, bytes: u64, lines: u64);

    /// True when this sink is the primary store and can serve log pages itself.
    fn supports_paging(&self) -> bool {
        false
    }

    /// Page from the sink's own store. `None` when it does not support paging (or
    /// has nothing yet), in which case 02 falls back to its local ring.
    fn page(
        &self,
        _session: &LaunchSessionId,
        _cursor: Option<u64>,
        _limit: usize,
    ) -> Option<LogPage> {
        None
    }
}

/// Sink used until module 08 delivers a LogStore.
#[derive(Debug, Default)]
pub struct NoopLogSink;

impl LogSink for NoopLogSink {
    fn append(&self, _session: &LaunchSessionId, records: &[LogRecord]) -> usize {
        records.len()
    }

    fn note_dropped(&self, _session: &LaunchSessionId, _bytes: u64, _lines: u64) {}
}

/// Bounded ring of log lines for one session.
///
/// Two modes share one implementation: the **primary** ring (no paging sink) keeps
/// the documented 5,000 lines / 8 MiB and counts every eviction as a retention
/// loss; the **tail** ring (a paging sink owns the data, D02-6) keeps only a small
/// window for immediate context and does not report evictions as lost output,
/// because the store still has them.
#[derive(Debug)]
pub struct BoundedLogRing {
    max_records: usize,
    max_bytes: usize,
    max_line_bytes: usize,
    records: VecDeque<LogRecord>,
    bytes: usize,
    dropped_bytes: u64,
    dropped_lines: u64,
    count_evictions: bool,
    next_sequence: u64,
}

impl Default for BoundedLogRing {
    fn default() -> Self {
        Self::with_limits(
            DEFAULT_MAX_RECORDS,
            DEFAULT_MAX_BYTES,
            DEFAULT_MAX_LINE_BYTES,
        )
    }
}

impl BoundedLogRing {
    pub fn with_limits(max_records: usize, max_bytes: usize, max_line_bytes: usize) -> Self {
        Self::with_mode(max_records, max_bytes, max_line_bytes, true)
    }

    /// `count_evictions = false` selects the tail mode described on the struct.
    pub fn with_mode(
        max_records: usize,
        max_bytes: usize,
        max_line_bytes: usize,
        count_evictions: bool,
    ) -> Self {
        Self {
            max_records: max_records.max(1),
            max_bytes: max_bytes.max(1),
            max_line_bytes: max_line_bytes.max(1),
            records: VecDeque::new(),
            bytes: 0,
            dropped_bytes: 0,
            dropped_lines: 0,
            count_evictions,
            next_sequence: 0,
        }
    }

    /// Append one line. `extra_omitted` counts bytes the reader already had to
    /// discard before this call (a line longer than the pending buffer).
    ///
    /// `omitted_bytes` is measured in **raw input bytes**: the kept prefix is cut
    /// on a UTF-8 boundary of the raw slice, so `kept + omitted == raw.len()` even
    /// when a multi-byte character straddles the cut (D02-5).
    pub fn push(&mut self, stream: LogStream, raw: &[u8], extra_omitted: u64) -> LogRecord {
        // Decode a bounded prefix so a 1 GiB line cannot be materialised whole.
        let decode_cap = self.max_line_bytes.saturating_mul(4);
        let head = &raw[..raw.len().min(decode_cap)];
        let mut end = head.len().min(self.max_line_bytes);
        // Never cut inside a UTF-8 sequence: step back to a start byte (0b10xxxxxx
        // marks a continuation byte).
        while end > 0 && end < head.len() && (head[end] & 0xC0) == 0x80 {
            end -= 1;
        }
        let line = String::from_utf8_lossy(&head[..end]).to_string();
        let omitted_bytes = (raw.len() - end) as u64 + extra_omitted;

        self.next_sequence += 1;
        let record = LogRecord {
            sequence: self.next_sequence,
            stream,
            line,
            omitted_bytes: omitted_bytes as usize,
        };
        self.bytes += record.line.len();
        self.records.push_back(record.clone());
        self.evict();
        record
    }

    /// Convenience for tests and for callers that already decoded a line.
    pub fn push_line(&mut self, stream: LogStream, line: &str) -> LogRecord {
        self.push(stream, line.as_bytes(), 0)
    }

    fn evict(&mut self) {
        while self.records.len() > self.max_records || self.bytes > self.max_bytes {
            match self.records.pop_front() {
                Some(record) => {
                    self.bytes = self.bytes.saturating_sub(record.line.len());
                    if self.count_evictions {
                        self.dropped_lines += 1;
                        self.dropped_bytes += record.line.len() as u64;
                    }
                }
                None => break,
            }
        }
    }

    /// Records after `cursor`, newest-first retention preserved in read order.
    pub fn page(&self, cursor: Option<u64>, limit: usize) -> LogPage {
        let limit = limit.clamp(1, MAX_PAGE_LIMIT);
        let after = cursor.unwrap_or(0);
        let mut records = Vec::new();
        let mut has_more = false;
        for record in &self.records {
            if record.sequence <= after {
                continue;
            }
            if records.len() == limit {
                has_more = true;
                break;
            }
            records.push(record.clone());
        }
        let next_cursor = records.last().map_or(after, |record| record.sequence);
        LogPage {
            records,
            next_cursor,
            dropped_bytes: self.dropped_bytes,
            dropped_lines: self.dropped_lines,
            has_more,
        }
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn dropped(&self) -> (u64, u64) {
        (self.dropped_bytes, self.dropped_lines)
    }

    pub fn last_sequence(&self) -> u64 {
        self.next_sequence
    }
}

/// Per-session rings plus the single downstream sink.
///
/// When the sink is the primary store (it supports paging, i.e. 08's LogStore),
/// this pipeline keeps only a tail window and forwards pages to the sink. With no
/// paging sink it is the only store and keeps the full documented ring.
pub struct LogPipeline {
    rings: Mutex<BTreeMap<LaunchSessionId, BoundedLogRing>>,
    sink: Arc<dyn LogSink>,
    max_records: usize,
    max_bytes: usize,
    max_line_bytes: usize,
    /// True when the sink owns retention and this pipeline is only a tail cache.
    secondary: bool,
}

impl LogPipeline {
    pub fn new(sink: Arc<dyn LogSink>) -> Self {
        let secondary = sink.supports_paging();
        let (max_records, max_bytes) = if secondary {
            (DEFAULT_TAIL_RECORDS, DEFAULT_TAIL_BYTES)
        } else {
            (DEFAULT_MAX_RECORDS, DEFAULT_MAX_BYTES)
        };
        Self {
            rings: Mutex::new(BTreeMap::new()),
            sink,
            max_records,
            max_bytes,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            secondary,
        }
    }

    pub fn with_limits(
        sink: Arc<dyn LogSink>,
        max_records: usize,
        max_bytes: usize,
        max_line_bytes: usize,
    ) -> Self {
        let secondary = sink.supports_paging();
        Self {
            rings: Mutex::new(BTreeMap::new()),
            sink,
            max_records,
            max_bytes,
            max_line_bytes,
            secondary,
        }
    }

    pub fn max_line_bytes(&self) -> usize {
        self.max_line_bytes
    }

    /// True when the pipeline only keeps a tail because the sink is the store.
    pub fn is_secondary(&self) -> bool {
        self.secondary
    }

    /// Retention actually held in memory by 02 `(max records, max bytes)`.
    pub fn local_capacity(&self) -> (usize, usize) {
        (self.max_records, self.max_bytes)
    }

    fn new_ring(&self) -> BoundedLogRing {
        BoundedLogRing::with_mode(
            self.max_records,
            self.max_bytes,
            self.max_line_bytes,
            !self.secondary,
        )
    }

    pub fn ensure(&self, session: &LaunchSessionId) {
        if let Ok(mut rings) = self.rings.lock() {
            rings
                .entry(session.clone())
                .or_insert_with(|| self.new_ring());
        }
    }

    /// Ingest one raw line (bytes as read from the pipe).
    pub fn ingest(
        &self,
        session: &LaunchSessionId,
        stream: LogStream,
        raw: &[u8],
        extra_omitted: u64,
    ) {
        let mut record = None;
        let mut dropped = (0u64, 0u64);
        if let Ok(mut rings) = self.rings.lock() {
            let secondary = self.secondary;
            let (max_records, max_bytes, max_line_bytes) =
                (self.max_records, self.max_bytes, self.max_line_bytes);
            let ring = rings.entry(session.clone()).or_insert_with(|| {
                BoundedLogRing::with_mode(max_records, max_bytes, max_line_bytes, !secondary)
            });
            let before = ring.dropped();
            record = Some(ring.push(stream, raw, extra_omitted));
            let after = ring.dropped();
            dropped = (after.0 - before.0, after.1 - before.1);
        }
        if let Some(record) = record {
            self.sink.append(session, std::slice::from_ref(&record));
        }
        if dropped.0 > 0 || dropped.1 > 0 {
            self.sink.note_dropped(session, dropped.0, dropped.1);
        }
    }

    pub fn ingest_line(&self, session: &LaunchSessionId, stream: LogStream, line: &str) {
        self.ingest(session, stream, line.as_bytes(), 0);
    }

    /// Page the session log.
    ///
    /// A paging sink (08's LogStore) is the authoritative store, so its page wins;
    /// the local tail is only a fallback for the moment just before/after exit.
    pub fn page(
        &self,
        session: &LaunchSessionId,
        cursor: Option<u64>,
        limit: usize,
    ) -> Option<LogPage> {
        if self.secondary {
            if let Some(page) = self.sink.page(session, cursor, limit) {
                return Some(page);
            }
        }
        self.rings
            .lock()
            .ok()
            .and_then(|rings| rings.get(session).map(|ring| ring.page(cursor, limit)))
    }

    pub fn dropped(&self, session: &LaunchSessionId) -> (u64, u64) {
        self.rings
            .lock()
            .ok()
            .and_then(|rings| rings.get(session).map(BoundedLogRing::dropped))
            .unwrap_or((0, 0))
    }

    /// Highest line sequence ingested for the session. A consumer that reconnects
    /// compares its last applied cursor with this value to detect a gap.
    pub fn last_sequence(&self, session: &LaunchSessionId) -> u64 {
        self.rings
            .lock()
            .ok()
            .and_then(|rings| rings.get(session).map(BoundedLogRing::last_sequence))
            .unwrap_or(0)
    }

    pub fn retained(&self, session: &LaunchSessionId) -> (usize, usize) {
        self.rings
            .lock()
            .ok()
            .and_then(|rings| rings.get(session).map(|ring| (ring.len(), ring.bytes())))
            .unwrap_or((0, 0))
    }

    pub fn forget(&self, session: &LaunchSessionId) {
        if let Ok(mut rings) = self.rings.lock() {
            rings.remove(session);
        }
    }
}

/// Longest partial line the pump keeps before it starts counting dropped bytes.
fn pending_cap(max_line_bytes: usize) -> usize {
    max_line_bytes.saturating_mul(4)
}

/// Drain one pipe, splitting on `\n` without ever blocking on a line length.
///
/// Returns the number of bytes the pump had to discard because a single line
/// exceeded [`pending_cap`]; that count is reported through the record's
/// `omitted_bytes` instead of growing memory.
pub fn drain_reader(
    reader: &mut dyn Read,
    max_line_bytes: usize,
    mut on_line: impl FnMut(&[u8], u64),
) -> u64 {
    let cap = pending_cap(max_line_bytes);
    let mut chunk = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::with_capacity(8192);
    let mut overflow: u64 = 0;
    let mut total_overflow: u64 = 0;

    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                for byte in &chunk[..read] {
                    if *byte == b'\n' {
                        if pending.last() == Some(&b'\r') {
                            pending.pop();
                        }
                        on_line(&pending, overflow);
                        total_overflow += overflow;
                        overflow = 0;
                        pending.clear();
                    } else if pending.len() < cap {
                        pending.push(*byte);
                    } else {
                        overflow += 1;
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    if !pending.is_empty() || overflow > 0 {
        on_line(&pending, overflow);
        total_overflow += overflow;
    }
    total_overflow
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_is_bounded_and_counts_drops() {
        let mut ring = BoundedLogRing::with_limits(10, 1024, 64);
        for index in 0..100 {
            ring.push_line(LogStream::Stdout, &format!("line-{index}"));
        }
        assert!(ring.len() <= 10);
        assert!(ring.bytes() <= 1024);
        let (bytes, lines) = ring.dropped();
        assert!(lines >= 90, "old lines are dropped, not newest: {lines}");
        assert!(bytes > 0);
        let page = ring.page(None, 5);
        assert_eq!(page.records.len(), 5);
        assert!(page.has_more);
        // The newest line survives.
        let last = ring.page(None, MAX_PAGE_LIMIT);
        assert_eq!(
            last.records.last().map(|record| record.line.as_str()),
            Some("line-99")
        );
    }

    #[test]
    fn long_line_is_truncated_on_a_char_boundary() {
        let mut ring = BoundedLogRing::with_limits(10, 1 << 20, 16);
        let line = "日本語のテキストです";
        let record = ring.push_line(LogStream::Stderr, line);
        assert!(record.line.is_char_boundary(record.line.len()));
        assert!(record.line.len() <= 16);
        assert!(record.omitted_bytes > 0);
        // Round-trips as valid UTF-8.
        assert!(std::str::from_utf8(record.line.as_bytes()).is_ok());
    }

    #[test]
    fn drain_reader_splits_lines_and_caps_pending() {
        let mut input: &[u8] = b"hello\nworld\r\nlong-line-without-newline\n";
        let mut lines = Vec::new();
        drain_reader(&mut input, 8, |line, omitted| {
            lines.push((String::from_utf8_lossy(line).to_string(), omitted));
        });
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].0, "hello");
        assert_eq!(lines[1].0, "world");
        // 25 bytes with an 8-byte line cap: 4x8 = 32 pending cap, so nothing lost.
        assert_eq!(lines[2].1, 0);
    }
}

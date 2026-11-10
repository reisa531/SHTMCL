//! The shared, bounded log store owned by module 08.
//!
//! Module 02 is the only reader of the game pipes; it hands every retained line
//! to the [`LogSink`] implemented here. This store is therefore the single
//! durable/diagnostic copy: the UI page, the diagnostic engine and the export
//! path all read it through a cursor, and nobody reads stdout/stderr a second
//! time.
//!
//! Budgets (initial, from `docs/development/08-diagnostics-and-delivery.md` §4):
//!
//! | Budget | Value | Where it is enforced |
//! | --- | --- | --- |
//! | memory ring | 8 MiB / 5,000 lines per session | [`BoundedLogRing`] reused from 02 |
//! | one displayed line | 64 KiB, cut on a character boundary | same ring, `omitted_bytes` recorded |
//! | rolling file | 8 MiB per part | [`WriterState::write_chunk`] rotates on a line boundary |
//! | one session on disk | 64 MiB | [`WriterState::make_room`] |
//! | all app logs on disk | 256 MiB | [`WriterState::enforce_quota`] evicts ended sessions first |
//! | write queue | 4 MiB | [`LogStore::append`] drops the fragment and counts it |
//!
//! `append` never blocks on the disk: when the queue is full or a write fails it
//! discards *that* fragment, increments the dropped counters and keeps accepting
//! lines, so the game's stdout is always drained. Retention loss is reported in
//! [`SessionStatus`] so diagnostics can say "this log was not saved completely"
//! instead of implying a clean log.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use shtmcl_domain::{InstanceId, LaunchSessionId};

use super::error::DiagnosticsError;
use crate::launch::logs::{
    BoundedLogRing, LogPage, LogRecord, LogSink, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINE_BYTES,
    DEFAULT_MAX_RECORDS, MAX_PAGE_LIMIT,
};

/// In-memory ring cap per session (8 MiB, the same budget 02 uses).
pub const DEFAULT_RING_BYTES: usize = DEFAULT_MAX_BYTES;
/// Retained lines per session (~5,000 for the UI page).
pub const DEFAULT_RING_RECORDS: usize = DEFAULT_MAX_RECORDS;
/// Per-line display cap (64 KiB).
pub const DEFAULT_LINE_BYTES: usize = DEFAULT_MAX_LINE_BYTES;
/// Rolling file size (8 MiB per part).
pub const DEFAULT_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Per-session disk budget (64 MiB).
pub const DEFAULT_SESSION_BYTES: u64 = 64 * 1024 * 1024;
/// Whole-application log budget (256 MiB).
pub const DEFAULT_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// Write queue (4 MiB).
pub const DEFAULT_QUEUE_BYTES: usize = 4 * 1024 * 1024;
/// A buffered chunk is sealed when it reaches this size instead of per line.
const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogLimits {
    pub ring_bytes: usize,
    pub ring_records: usize,
    pub line_bytes: usize,
    pub file_bytes: u64,
    pub session_bytes: u64,
    pub total_bytes: u64,
    pub queue_bytes: usize,
}

impl Default for LogLimits {
    fn default() -> Self {
        Self {
            ring_bytes: DEFAULT_RING_BYTES,
            ring_records: DEFAULT_RING_RECORDS,
            line_bytes: DEFAULT_LINE_BYTES,
            file_bytes: DEFAULT_FILE_BYTES,
            session_bytes: DEFAULT_SESSION_BYTES,
            total_bytes: DEFAULT_TOTAL_BYTES,
            queue_bytes: DEFAULT_QUEUE_BYTES,
        }
    }
}

impl LogLimits {
    /// Clamp to something a store can actually run with (never zero).
    fn sane(mut self) -> Self {
        self.ring_bytes = self.ring_bytes.max(1024);
        self.ring_records = self.ring_records.max(1);
        self.line_bytes = self.line_bytes.max(64);
        self.file_bytes = self.file_bytes.max(256);
        self.session_bytes = self.session_bytes.max(self.file_bytes);
        self.total_bytes = self.total_bytes.max(self.session_bytes);
        self.queue_bytes = self.queue_bytes.max(1024);
        self
    }
}

/// Per-session counters. Loss is always reported, never hidden.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStatus {
    pub records: usize,
    pub ring_bytes: usize,
    pub last_sequence: u64,
    /// Evicted from the memory ring (oldest lines first).
    pub ring_dropped_bytes: u64,
    pub ring_dropped_lines: u64,
    /// Discarded because the write queue was full or a disk write failed.
    pub disk_dropped_bytes: u64,
    pub disk_dropped_lines: u64,
    /// Reported by the pipe pump itself (02's own ring eviction).
    pub pump_dropped_bytes: u64,
    pub pump_dropped_lines: u64,
    pub persisted_bytes: u64,
    pub parts: usize,
    pub queued_bytes: usize,
    pub ended: bool,
    /// `true` once any fragment of this session was not written to disk.
    pub not_persisted: bool,
}

impl SessionStatus {
    pub fn lossy(&self) -> bool {
        self.ring_dropped_lines > 0
            || self.disk_dropped_lines > 0
            || self.pump_dropped_lines > 0
            || self.not_persisted
    }

    pub fn to_value(&self) -> Value {
        json!({
            "records": self.records,
            "ringBytes": self.ring_bytes,
            "lastSequence": self.last_sequence,
            "ringDroppedBytes": self.ring_dropped_bytes,
            "ringDroppedLines": self.ring_dropped_lines,
            "diskDroppedBytes": self.disk_dropped_bytes,
            "diskDroppedLines": self.disk_dropped_lines,
            "pumpDroppedBytes": self.pump_dropped_bytes,
            "pumpDroppedLines": self.pump_dropped_lines,
            "persistedBytes": self.persisted_bytes,
            "parts": self.parts,
            "queuedBytes": self.queued_bytes,
            "ended": self.ended,
            "notPersisted": self.not_persisted,
            "lossy": self.lossy(),
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// Session ids are opaque; on disk they become one directory component. Anything
/// outside `[A-Za-z0-9._-]` is escaped as `_<hex>`, so two different ids can never
/// share a directory and `..` cannot escape the log root.
fn sanitise_component(id: &str) -> String {
    let mut out = String::with_capacity(id.len() + 8);
    for (index, character) in id.chars().enumerate() {
        let safe = character.is_ascii_alphanumeric()
            || matches!(character, '-' | '_')
            || (character == '.' && index > 0);
        if safe {
            out.push(character);
        } else {
            out.push('_');
            out.push_str(&format!("{:x}", character as u32));
        }
    }
    if out.is_empty() || out == "." || out == ".." {
        out.push_str("_session");
    }
    out
}

struct SessionState {
    ring: BoundedLogRing,
    instance_id: Option<InstanceId>,
    ended: bool,
    queued_bytes: usize,
    disk_dropped_bytes: u64,
    disk_dropped_lines: u64,
    pump_dropped_bytes: u64,
    pump_dropped_lines: u64,
    not_persisted: bool,
}

impl SessionState {
    fn new(limits: LogLimits, instance_id: Option<InstanceId>) -> Self {
        Self {
            ring: BoundedLogRing::with_limits(
                limits.ring_records,
                limits.ring_bytes,
                limits.line_bytes,
            ),
            instance_id,
            ended: false,
            queued_bytes: 0,
            disk_dropped_bytes: 0,
            disk_dropped_lines: 0,
            pump_dropped_bytes: 0,
            pump_dropped_lines: 0,
            not_persisted: false,
        }
    }
}

struct Buffered {
    bytes: Vec<u8>,
    records: u64,
}

#[derive(Default)]
struct DiskQueue {
    buffers: VecDeque<(LaunchSessionId, Buffered)>,
    bytes: usize,
}

struct StoreState {
    sessions: BTreeMap<LaunchSessionId, SessionState>,
    next_session: u64,
}

struct DiskPart {
    path: PathBuf,
    bytes: u64,
}

struct DiskSession {
    dir: PathBuf,
    /// Closed parts, oldest first.
    parts: VecDeque<DiskPart>,
    /// Index of the part that is (or will be) opened next.
    next_index: u32,
    current: Option<File>,
    current_bytes: u64,
    total: u64,
    ended: bool,
    created: u64,
}

impl DiskSession {
    fn new(dir: PathBuf, created: u64) -> Self {
        Self {
            dir,
            parts: VecDeque::new(),
            next_index: 1,
            current: None,
            current_bytes: 0,
            total: 0,
            ended: false,
            created,
        }
    }

    fn current_path(&self) -> PathBuf {
        self.dir.join(format!("{:05}.log", self.next_index))
    }
}

struct WriterState {
    sessions: BTreeMap<LaunchSessionId, DiskSession>,
    total_bytes: u64,
    next_created: u64,
}

struct Inner {
    limits: LogLimits,
    sessions_dir: PathBuf,
    state: Mutex<StoreState>,
    queue: Mutex<DiskQueue>,
    wake: Condvar,
    writer: Mutex<WriterState>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    stalled: AtomicBool,
    stopped: AtomicBool,
    write_failures: AtomicU64,
    evicted_sessions: AtomicU64,
    evicted_files: AtomicU64,
}

impl Inner {
    fn drain_one(&self) -> bool {
        let (session, buffered) = {
            let mut queue = lock(&self.queue);
            match queue.buffers.pop_front() {
                Some((session, buffered)) => {
                    queue.bytes = queue.bytes.saturating_sub(buffered.bytes.len());
                    (session, buffered)
                }
                None => return false,
            }
        };
        let size = buffered.bytes.len();
        let failed = {
            let mut writer = lock(&self.writer);
            let written =
                writer.write_chunk(&self.limits, &self.sessions_dir, &session, &buffered.bytes);
            writer.enforce_quota(&self.limits, &self.evicted_sessions, &self.evicted_files);
            written.is_err()
        };
        if failed {
            self.write_failures.fetch_add(1, Ordering::Relaxed);
        }
        let mut state = lock(&self.state);
        if let Some(session_state) = state.sessions.get_mut(&session) {
            session_state.queued_bytes = session_state.queued_bytes.saturating_sub(size);
            if failed {
                session_state.not_persisted = true;
                session_state.disk_dropped_bytes += size as u64;
                session_state.disk_dropped_lines += buffered.records;
            }
        }
        true
    }

    fn drain_all(&self) -> usize {
        let mut drained = 0usize;
        while self.drain_one() {
            drained += 1;
            if drained > 1_000_000 {
                break;
            }
        }
        drained
    }
}

impl WriterState {
    fn ensure(&mut self, session: &LaunchSessionId, root: &Path) -> &mut DiskSession {
        let created = self.next_created;
        if !self.sessions.contains_key(session) {
            self.next_created += 1;
            let dir = root.join(sanitise_component(session.as_str()));
            self.sessions
                .insert(session.clone(), DiskSession::new(dir, created));
        }
        self.sessions
            .get_mut(session)
            .expect("session was just inserted")
    }

    fn open_current(&mut self, session: &LaunchSessionId) -> std::io::Result<()> {
        let entry = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| std::io::Error::other("unknown log session"))?;
        if entry.current.is_some() {
            return Ok(());
        }
        fs::create_dir_all(&entry.dir)?;
        let path = entry.current_path();
        entry.current = Some(OpenOptions::new().create(true).append(true).open(&path)?);
        entry.current_bytes = 0;
        Ok(())
    }

    /// Close the open part so it becomes evictable. An empty part is removed
    /// instead of being recorded, so a rotation never leaves a zero-byte file.
    fn rotate(&mut self, session: &LaunchSessionId) -> std::io::Result<()> {
        let Some(entry) = self.sessions.get_mut(session) else {
            return Ok(());
        };
        let Some(file) = entry.current.take() else {
            return Ok(());
        };
        let bytes = entry.current_bytes;
        drop(file);
        if bytes == 0 {
            let _ = fs::remove_file(entry.current_path());
            return Ok(());
        }
        let path = entry.current_path();
        entry.parts.push_back(DiskPart { path, bytes });
        entry.next_index += 1;
        entry.current_bytes = 0;
        Ok(())
    }

    /// Guarantee `need` more bytes fit inside the session budget by evicting the
    /// oldest closed part. Returns `false` only when a single line cannot fit at
    /// all (then the caller reports the loss instead of looping).
    fn make_room(&mut self, session: &LaunchSessionId, need: u64, session_bytes: u64) -> bool {
        {
            let Some(entry) = self.sessions.get(session) else {
                return false;
            };
            if entry.total + need <= session_bytes {
                return true;
            }
        }
        let _ = self.rotate(session);
        let Some(entry) = self.sessions.get_mut(session) else {
            return false;
        };
        while entry.total + need > session_bytes {
            let Some(part) = entry.parts.pop_front() else {
                return false;
            };
            let _ = fs::remove_file(&part.path);
            entry.total = entry.total.saturating_sub(part.bytes);
            self.total_bytes = self.total_bytes.saturating_sub(part.bytes);
        }
        true
    }

    /// Write one buffered chunk, rotating on a line boundary so a part never
    /// exceeds `file_bytes`. A chunk is always a whole number of lines.
    fn write_chunk(
        &mut self,
        limits: &LogLimits,
        root: &Path,
        session: &LaunchSessionId,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        self.ensure(session, root);
        self.open_current(session)?;
        let mut remaining = bytes;
        let mut spins = 0u32;

        while !remaining.is_empty() {
            spins += 1;
            if spins > 4096 {
                return Err(std::io::Error::other("log rotation did not converge"));
            }
            let space = {
                let entry = self
                    .sessions
                    .get(session)
                    .ok_or_else(|| std::io::Error::other("unknown log session"))?;
                limits.file_bytes.saturating_sub(entry.current_bytes)
            };
            if space == 0 {
                self.rotate(session)?;
                self.open_current(session)?;
                continue;
            }

            let mut take = remaining.len().min(space as usize);
            if take < remaining.len() {
                match remaining[..take].iter().rposition(|byte| *byte == b'\n') {
                    Some(position) => take = position + 1,
                    None => {
                        self.rotate(session)?;
                        self.open_current(session)?;
                        continue;
                    }
                }
            }
            if !self.make_room(session, take as u64, limits.session_bytes) {
                return Err(std::io::Error::other("session log budget reached"));
            }

            let entry = self
                .sessions
                .get_mut(session)
                .ok_or_else(|| std::io::Error::other("unknown log session"))?;
            if entry.current.is_none() {
                self.open_current(session)?;
            }
            let entry = self
                .sessions
                .get_mut(session)
                .ok_or_else(|| std::io::Error::other("unknown log session"))?;
            let file = entry
                .current
                .as_mut()
                .ok_or_else(|| std::io::Error::other("log part is not open"))?;
            file.write_all(&remaining[..take])?;
            entry.current_bytes += take as u64;
            entry.total += take as u64;
            self.total_bytes += take as u64;
            remaining = &remaining[take..];
        }
        Ok(())
    }

    fn enforce_session_quota(&mut self, session: &LaunchSessionId, session_bytes: u64) {
        let Some(entry) = self.sessions.get_mut(session) else {
            return;
        };
        while entry.total > session_bytes {
            let Some(part) = entry.parts.pop_front() else {
                break;
            };
            let _ = fs::remove_file(&part.path);
            entry.total = entry.total.saturating_sub(part.bytes);
            self.total_bytes = self.total_bytes.saturating_sub(part.bytes);
        }
    }

    fn enforce_quota(
        &mut self,
        limits: &LogLimits,
        evicted_sessions: &AtomicU64,
        evicted_files: &AtomicU64,
    ) {
        let sessions: Vec<LaunchSessionId> = self.sessions.keys().cloned().collect();
        for session in sessions {
            self.enforce_session_quota(&session, limits.session_bytes);
        }
        if self.total_bytes <= limits.total_bytes {
            return;
        }

        // 1. Ended sessions go first, oldest first.
        let mut ended: Vec<(u64, LaunchSessionId)> = self
            .sessions
            .iter()
            .filter(|(_, entry)| entry.ended)
            .map(|(id, entry)| (entry.created, id.clone()))
            .collect();
        ended.sort();
        for (_, session) in ended {
            if self.total_bytes <= limits.total_bytes {
                return;
            }
            if let Some(mut entry) = self.sessions.remove(&session) {
                entry.current = None;
                for part in entry.parts.drain(..) {
                    let _ = fs::remove_file(&part.path);
                    self.total_bytes = self.total_bytes.saturating_sub(part.bytes);
                    evicted_files.fetch_add(1, Ordering::Relaxed);
                }
                let _ = fs::remove_dir_all(&entry.dir);
                evicted_sessions.fetch_add(1, Ordering::Relaxed);
            }
        }

        // 2. Still over budget: drop the oldest closed part of any session. The
        //    part that is still open is never deleted, so an active session keeps
        //    its newest lines even on a small budget.
        while self.total_bytes > limits.total_bytes {
            let mut candidate: Option<(u64, LaunchSessionId, usize)> = None;
            for (id, entry) in &self.sessions {
                for index in 0..entry.parts.len() {
                    let key = (entry.created, index as u64);
                    let better = match &candidate {
                        None => true,
                        Some((created, _, candidate_index)) => {
                            key < (*created, *candidate_index as u64)
                        }
                    };
                    if better {
                        candidate = Some((entry.created, id.clone(), index));
                    }
                }
            }
            let Some((_, session, index)) = candidate else {
                return;
            };
            if let Some(entry) = self.sessions.get_mut(&session) {
                if let Some(part) = entry.parts.remove(index) {
                    let _ = fs::remove_file(&part.path);
                    entry.total = entry.total.saturating_sub(part.bytes);
                    self.total_bytes = self.total_bytes.saturating_sub(part.bytes);
                    evicted_files.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// The module's single log store. Cloneable: 02's pump and 08's commands share
/// one instance through `Arc`.
#[derive(Clone)]
pub struct LogStore {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = lock(&self.inner.state);
        f.debug_struct("LogStore")
            .field("sessions", &state.sessions.len())
            .field("queueBytes", &lock(&self.inner.queue).bytes)
            .field("dir", &self.inner.sessions_dir)
            .finish()
    }
}

impl Drop for LogStore {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) != 1 {
            return;
        }
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.wake.notify_all();
        let handle = lock(&self.inner.handle).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl LogStore {
    pub fn new(logs_dir: impl AsRef<Path>) -> Self {
        Self::with_limits(logs_dir, LogLimits::default())
    }

    pub fn with_limits(logs_dir: impl AsRef<Path>, limits: LogLimits) -> Self {
        let limits = limits.sane();
        let sessions_dir = logs_dir.as_ref().join("sessions");
        let inner = Arc::new(Inner {
            limits,
            sessions_dir,
            state: Mutex::new(StoreState {
                sessions: BTreeMap::new(),
                next_session: 0,
            }),
            queue: Mutex::new(DiskQueue::default()),
            wake: Condvar::new(),
            writer: Mutex::new(WriterState {
                sessions: BTreeMap::new(),
                total_bytes: 0,
                next_created: 0,
            }),
            handle: Mutex::new(None),
            stalled: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            write_failures: AtomicU64::new(0),
            evicted_sessions: AtomicU64::new(0),
            evicted_files: AtomicU64::new(0),
        });

        let weak: Weak<Inner> = Arc::downgrade(&inner);
        let spawned = std::thread::Builder::new()
            .name("shtmcl-log-writer".to_string())
            .spawn(move || Self::writer_loop(weak));
        if let Ok(handle) = spawned {
            *lock(&inner.handle) = Some(handle);
        }

        Self { inner }
    }

    fn writer_loop(weak: Weak<Inner>) {
        loop {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if inner.stopped.load(Ordering::SeqCst) {
                return;
            }
            {
                let queue = lock(&inner.queue);
                if queue.bytes == 0 {
                    let _ = inner.wake.wait_timeout(queue, Duration::from_millis(25));
                }
            }
            if inner.stopped.load(Ordering::SeqCst) {
                return;
            }
            if inner.stalled.load(Ordering::SeqCst) {
                // Fault injection: a jammed disk. Appends keep accepting lines and
                // the queue fills up, which is exactly the budget under test.
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            inner.drain_all();
        }
    }

    pub fn limits(&self) -> LogLimits {
        self.inner.limits
    }

    pub fn logs_dir(&self) -> &Path {
        &self.inner.sessions_dir
    }

    /// Declare the instance a session belongs to (02 calls this when it opens a
    /// session). Purely informative: diagnostics never guesses it from the id.
    pub fn register_session(&self, session: &LaunchSessionId, instance_id: Option<InstanceId>) {
        let mut state = lock(&self.inner.state);
        if let Some(entry) = state.sessions.get_mut(session) {
            if entry.instance_id.is_none() {
                entry.instance_id = instance_id;
            }
            return;
        }
        state.next_session += 1;
        state.sessions.insert(
            session.clone(),
            SessionState::new(self.inner.limits, instance_id),
        );
    }

    /// Mark a session finished so the quota reaper may evict it first.
    pub fn end_session(&self, session: &LaunchSessionId) {
        {
            let mut state = lock(&self.inner.state);
            if let Some(entry) = state.sessions.get_mut(session) {
                entry.ended = true;
            }
        }
        let mut writer = lock(&self.inner.writer);
        // Create the disk record if it does not exist yet: the pipe pump may still
        // be holding the queued bytes, and a lost "ended" flag would make the quota
        // reaper evict an active session instead of this finished one.
        writer.ensure(session, &self.inner.sessions_dir).ended = true;
        let _ = writer.rotate(session);
        writer.enforce_quota(
            &self.inner.limits,
            &self.inner.evicted_sessions,
            &self.inner.evicted_files,
        );
    }

    pub fn has_session(&self, session: &LaunchSessionId) -> bool {
        lock(&self.inner.state).sessions.contains_key(session)
    }

    pub fn session_ids(&self) -> Vec<LaunchSessionId> {
        lock(&self.inner.state).sessions.keys().cloned().collect()
    }

    pub fn instance_of(&self, session: &LaunchSessionId) -> Option<InstanceId> {
        lock(&self.inner.state)
            .sessions
            .get(session)
            .and_then(|entry| entry.instance_id.clone())
    }

    /// Read one bounded page after `cursor`. The cursor is exclusive and stable:
    /// the next call with `page.next_cursor` continues exactly where this one
    /// stopped and never repeats a record.
    pub fn read_log(
        &self,
        session: &LaunchSessionId,
        cursor: Option<u64>,
        limit: usize,
    ) -> Result<LogPage, DiagnosticsError> {
        let state = lock(&self.inner.state);
        let entry = state
            .sessions
            .get(session)
            .ok_or_else(|| DiagnosticsError::LogsNotFound {
                session: session.as_str().to_string(),
            })?;
        Ok(entry.ring.page(cursor, limit.clamp(1, MAX_PAGE_LIMIT)))
    }

    /// Every retained record of a session, oldest first, read by paging so the
    /// result is *really* the whole retained window and not one capped page. The
    /// diagnostic engine and the exporter stream through [`LogStore::read_log`]
    /// directly; this convenience exists for callers that need the window at once
    /// and is bounded by the ring budget, never a second copy of the raw pipe.
    pub fn read_all(&self, session: &LaunchSessionId) -> Result<Vec<LogRecord>, DiagnosticsError> {
        let mut records = Vec::new();
        let mut cursor = 0u64;
        loop {
            let page = self.read_log(session, Some(cursor), MAX_PAGE_LIMIT)?;
            if page.records.is_empty() {
                return Ok(records);
            }
            cursor = page.next_cursor;
            records.extend(page.records);
            if !page.has_more {
                return Ok(records);
            }
        }
    }

    /// Page this store the way 02's log port does.
    ///
    /// [`LogSink::supports_paging`] returns `true` and [`LogSink::page`] delegates
    /// here, so the process pump, `launch.logs`, the UI page and diagnostics all
    /// read **one** copy. `None` means "no such session" (02's default method
    /// returns `None` for every sink that keeps no storage); `Some(page)` is the
    /// same page `diagnostics.logs` returns.
    pub fn page(
        &self,
        session: &LaunchSessionId,
        cursor: Option<u64>,
        limit: usize,
    ) -> Option<LogPage> {
        self.read_log(session, cursor, limit).ok()
    }

    pub fn status(&self, session: &LaunchSessionId) -> Option<SessionStatus> {
        let state = lock(&self.inner.state);
        let entry = state.sessions.get(session)?;
        let writer = lock(&self.inner.writer);
        let (persisted, parts) = writer
            .sessions
            .get(session)
            .map(|disk| {
                (
                    disk.total,
                    disk.parts.len() + usize::from(disk.current.is_some()),
                )
            })
            .unwrap_or((0, 0));
        let (ring_dropped_bytes, ring_dropped_lines) = entry.ring.dropped();
        Some(SessionStatus {
            records: entry.ring.len(),
            ring_bytes: entry.ring.bytes(),
            last_sequence: entry.ring.last_sequence(),
            ring_dropped_bytes,
            ring_dropped_lines,
            disk_dropped_bytes: entry.disk_dropped_bytes,
            disk_dropped_lines: entry.disk_dropped_lines,
            pump_dropped_bytes: entry.pump_dropped_bytes,
            pump_dropped_lines: entry.pump_dropped_lines,
            persisted_bytes: persisted,
            parts,
            queued_bytes: entry.queued_bytes,
            ended: entry.ended,
            not_persisted: entry.not_persisted,
        })
    }

    pub fn persisted_bytes(&self, session: &LaunchSessionId) -> u64 {
        lock(&self.inner.writer)
            .sessions
            .get(session)
            .map(|entry| entry.total)
            .unwrap_or(0)
    }

    pub fn total_persisted_bytes(&self) -> u64 {
        lock(&self.inner.writer).total_bytes
    }

    pub fn queued_bytes(&self) -> usize {
        lock(&self.inner.queue).bytes
    }

    pub fn write_failures(&self) -> u64 {
        self.inner.write_failures.load(Ordering::Relaxed)
    }

    pub fn evicted_sessions(&self) -> u64 {
        self.inner.evicted_sessions.load(Ordering::Relaxed)
    }

    pub fn evicted_files(&self) -> u64 {
        self.inner.evicted_files.load(Ordering::Relaxed)
    }

    /// Fault injection used by the disk-stall budget test: the writer stops
    /// draining, so the queue fills and appends begin dropping fragments while the
    /// pump keeps delivering lines.
    pub fn set_disk_stalled(&self, stalled: bool) {
        self.inner.stalled.store(stalled, Ordering::SeqCst);
        self.inner.wake.notify_all();
    }

    pub fn disk_is_stalled(&self) -> bool {
        self.inner.stalled.load(Ordering::SeqCst)
    }

    /// Drain the write queue now and wait until it is empty.
    pub fn flush_now(&self, timeout: Duration) -> Result<(), DiagnosticsError> {
        let deadline = Instant::now() + timeout;
        // Session accounting includes chunks already taken by the disk writer.
        // An empty queue alone does not mean those writes have completed.
        let drained = || {
            lock(&self.inner.state)
                .sessions
                .values()
                .all(|entry| entry.queued_bytes == 0)
        };
        loop {
            if drained() {
                return Ok(());
            }
            if self.disk_is_stalled() {
                return Err(DiagnosticsError::LogsStoreFailed {
                    detail: "disk_stalled".to_string(),
                });
            }
            self.inner.drain_all();
            if drained() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(DiagnosticsError::LogsStoreFailed {
                    detail: "flush_timeout".to_string(),
                });
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn enqueue(&self, session: &LaunchSessionId, rendered: &str) {
        let bytes = rendered.len();
        {
            let mut state = lock(&self.inner.state);
            if let Some(entry) = state.sessions.get_mut(session) {
                if entry.queued_bytes + bytes > self.inner.limits.queue_bytes {
                    // Disk is behind: drop this fragment, count it, keep draining.
                    entry.disk_dropped_bytes += bytes as u64;
                    entry.disk_dropped_lines += 1;
                    entry.not_persisted = true;
                    return;
                }
                entry.queued_bytes += bytes;
            }
        }
        let accepted = {
            let mut queue = lock(&self.inner.queue);
            if queue.bytes + bytes > self.inner.limits.queue_bytes {
                false
            } else {
                queue.bytes += bytes;
                match queue.buffers.back_mut() {
                    Some((last, buffered))
                        if last == session && buffered.bytes.len() < CHUNK_BYTES =>
                    {
                        buffered.bytes.extend_from_slice(rendered.as_bytes());
                        buffered.records += 1;
                    }
                    _ => queue.buffers.push_back((
                        session.clone(),
                        Buffered {
                            bytes: rendered.as_bytes().to_vec(),
                            records: 1,
                        },
                    )),
                }
                true
            }
        };
        if !accepted {
            let mut state = lock(&self.inner.state);
            if let Some(entry) = state.sessions.get_mut(session) {
                entry.queued_bytes = entry.queued_bytes.saturating_sub(bytes);
                entry.disk_dropped_bytes += bytes as u64;
                entry.disk_dropped_lines += 1;
                entry.not_persisted = true;
            }
            return;
        }
        self.inner.wake.notify_all();
    }
}

/// On-disk rendering of one retained line. The stream is not repeated per line
/// (the session status carries the counts); the omitted byte count is appended so
/// a reader of the file knows the text was cut.
fn render_record(record: &LogRecord) -> String {
    let mut rendered = String::with_capacity(record.line.len() + 32);
    rendered.push_str(&record.line);
    if record.omitted_bytes > 0 {
        rendered.push_str(&format!(" [... {} bytes omitted]", record.omitted_bytes));
    }
    rendered.push('\n');
    rendered
}

impl LogSink for LogStore {
    /// Non-blocking by contract: the ring always takes the line, the disk queue
    /// may drop the fragment (and records that it did).
    fn append(&self, session: &LaunchSessionId, records: &[LogRecord]) -> usize {
        if records.is_empty() {
            return 0;
        }
        self.register_session(session, None);
        let mut rendered: Vec<String> = Vec::with_capacity(records.len());
        {
            let mut state = lock(&self.inner.state);
            let limits = self.inner.limits;
            let entry = state
                .sessions
                .entry(session.clone())
                .or_insert_with(|| SessionState::new(limits, None));
            for record in records {
                entry.ring.push(
                    record.stream,
                    record.line.as_bytes(),
                    record.omitted_bytes as u64,
                );
                rendered.push(render_record(record));
            }
        }
        for line in rendered {
            self.enqueue(session, &line);
        }
        records.len()
    }

    fn note_dropped(&self, session: &LaunchSessionId, bytes: u64, lines: u64) {
        let mut state = lock(&self.inner.state);
        let limits = self.inner.limits;
        let entry = state
            .sessions
            .entry(session.clone())
            .or_insert_with(|| SessionState::new(limits, None));
        entry.pump_dropped_bytes += bytes;
        entry.pump_dropped_lines += lines;
    }

    /// This store owns the only full copy of a session, so the pipe pump must not
    /// keep a second 8 MiB ring: 02 switches to its small tail window
    /// (`LogPipeline::is_secondary()`), which is exactly the 00 ruling.
    fn supports_paging(&self) -> bool {
        true
    }

    /// Serve pages from the store itself. Delegates to the inherent
    /// [`LogStore::page`], which is the same page `diagnostics.logs` returns.
    fn page(
        &self,
        session: &LaunchSessionId,
        cursor: Option<u64>,
        limit: usize,
    ) -> Option<LogPage> {
        LogStore::page(self, session, cursor, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::logs::LogStream;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "shtmcl-08-logstore-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp dir");
        path
    }

    fn record(sequence: u64, line: impl Into<String>) -> LogRecord {
        LogRecord {
            sequence,
            stream: LogStream::Stdout,
            line: line.into(),
            omitted_bytes: 0,
        }
    }

    #[test]
    fn session_ids_cannot_escape_the_log_root() {
        assert_eq!(sanitise_component("session-1"), "session-1");
        assert!(!sanitise_component("..").contains(".."));
        assert!(!sanitise_component("../../etc").contains('/'));
        assert_ne!(sanitise_component("a/b"), sanitise_component("a_b"));
    }

    #[test]
    fn the_ring_is_bounded_and_reports_what_it_dropped() {
        let dir = temp_dir("ring");
        let store = LogStore::with_limits(
            &dir,
            LogLimits {
                ring_records: 8,
                ring_bytes: 4096,
                line_bytes: 64,
                ..LogLimits::default()
            },
        );
        let session = LaunchSessionId::new("s-1").expect("id");
        store.register_session(&session, None);
        for index in 0..200u64 {
            store.append(&session, &[record(index, format!("line-{index}"))]);
        }
        let status = store.status(&session).expect("status");
        assert!(status.records <= 8, "records bounded: {}", status.records);
        assert!(status.ring_dropped_lines > 0);
        let page = store.read_log(&session, None, 100).expect("page");
        assert_eq!(page.records.len(), status.records);
        assert_eq!(
            page.records.last().map(|record| record.line.as_str()),
            Some("line-199"),
            "the newest line survives"
        );
        store.flush_now(Duration::from_secs(5)).expect("drained");
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stalled_disk_drops_fragments_without_losing_the_ring() {
        let dir = temp_dir("stall");
        let store = LogStore::with_limits(
            &dir,
            LogLimits {
                queue_bytes: 2048,
                ring_records: 5000,
                ..LogLimits::default()
            },
        );
        store.set_disk_stalled(true);
        let session = LaunchSessionId::new("s-stall").expect("id");
        store.register_session(&session, None);
        for index in 0..400u64 {
            store.append(
                &session,
                &[record(
                    index,
                    format!("[12:00:00] [main/INFO] stalled line {index:04} padding padding"),
                )],
            );
        }
        let status = store.status(&session).expect("status");
        assert!(
            status.disk_dropped_lines > 0,
            "a jammed disk must count what was not saved"
        );
        assert!(status.not_persisted);
        assert_eq!(
            status.last_sequence, 400,
            "every line still reached the ring: the pump is never back-pressured"
        );
        assert!(store.queued_bytes() <= 2048);
        assert!(store.flush_now(Duration::from_millis(20)).is_err());
        store.set_disk_stalled(false);
        store.flush_now(Duration::from_secs(5)).expect("drained");
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rolling_parts_respect_the_part_and_session_budget() {
        let dir = temp_dir("roll");
        let store = LogStore::with_limits(
            &dir,
            LogLimits {
                file_bytes: 512,
                session_bytes: 2048,
                total_bytes: 4096,
                ..LogLimits::default()
            },
        );
        let session = LaunchSessionId::new("s-roll").expect("id");
        store.register_session(&session, None);
        for index in 0..500u64 {
            store.append(
                &session,
                &[record(
                    index,
                    format!("[main/INFO] roll {index:04} {}", "x".repeat(40)),
                )],
            );
        }
        store.flush_now(Duration::from_secs(5)).expect("drained");
        let status = store.status(&session).expect("status");
        assert!(
            status.persisted_bytes <= 2048,
            "session budget: {}",
            status.persisted_bytes
        );
        let mut parts = 0;
        for entry in fs::read_dir(dir.join("sessions").join("s-roll")).expect("dir") {
            let entry = entry.expect("entry");
            let length = entry.metadata().expect("meta").len();
            assert!(length <= 512, "part size {length} exceeds the budget");
            parts += 1;
        }
        assert!(parts >= 1, "at least one part was written");
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_waits_for_a_chunk_already_taken_by_the_writer() {
        let dir = temp_dir("in-flight");
        let store = LogStore::new(&dir);
        let session = LaunchSessionId::new("s-flight").expect("id");
        store.set_disk_stalled(true);
        store.register_session(&session, None);
        store.append(&session, &[record(1, "still writing")]);
        let writer = lock(&store.inner.writer);
        let inner = store.inner.clone();
        let worker = std::thread::spawn(move || inner.drain_one());
        let deadline = Instant::now() + Duration::from_secs(5);
        while store.queued_bytes() != 0 {
            assert!(Instant::now() < deadline, "writer did not take the chunk");
            std::thread::yield_now();
        }
        store.set_disk_stalled(false);
        assert!(store.flush_now(Duration::from_millis(20)).is_err());
        drop(writer);
        assert!(worker.join().expect("writer"));
        store.flush_now(Duration::from_secs(5)).expect("completed write");
        assert!(store.persisted_bytes(&session) > 0);
        drop(store);
        fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn an_ended_session_is_evicted_before_an_active_one() {
        let dir = temp_dir("quota");
        let store = LogStore::with_limits(
            &dir,
            LogLimits {
                file_bytes: 400,
                session_bytes: 400,
                total_bytes: 400,
                ..LogLimits::default()
            },
        );
        let old = LaunchSessionId::new("s-old").expect("id");
        let live = LaunchSessionId::new("s-live").expect("id");
        store.register_session(&old, None);
        store.register_session(&live, None);
        store.append(&old, &[record(1, format!("old {}", "o".repeat(300)))]);
        store.flush_now(Duration::from_secs(5)).expect("drained");
        store.end_session(&old);
        store.append(&live, &[record(1, format!("live {}", "n".repeat(300)))]);
        store.flush_now(Duration::from_secs(5)).expect("drained");
        assert!(
            store.evicted_sessions() >= 1,
            "the ended session is reaped first"
        );
        assert!(
            store.persisted_bytes(&live) > 0,
            "the active session keeps its lines"
        );
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Budget measurement (run with `--nocapture`): a sustained flood through the
    /// real store. The numbers go into the 08 handoff, not into a claim.
    #[test]
    fn budget_measurement_for_a_sustained_flood() {
        let dir = temp_dir("measure");
        let store = LogStore::with_limits(&dir, LogLimits::default());
        let session = LaunchSessionId::new("s-measure").expect("id");
        store.register_session(&session, None);
        let started = Instant::now();
        let lines = 50_000u32;
        for index in 0..lines {
            store.append(
                &session,
                &[record(
                    index as u64,
                    format!("[12:00:00] [main/INFO] flood {index:06} {}", "x".repeat(64)),
                )],
            );
        }
        let appended = started.elapsed();
        store.flush_now(Duration::from_secs(30)).expect("drained");
        let flushed = started.elapsed();
        let status = store.status(&session).expect("status");
        println!(
            "logstore budget: lines={lines} appended_in_ms={} flushed_in_ms={} records={} ring_bytes={} ring_dropped_lines={} disk_dropped_lines={} persisted_bytes={} parts={} queue_bytes={}",
            appended.as_millis(),
            flushed.as_millis(),
            status.records,
            status.ring_bytes,
            status.ring_dropped_lines,
            status.disk_dropped_lines,
            status.persisted_bytes,
            status.parts,
            store.queued_bytes()
        );
        assert!(status.records <= DEFAULT_RING_RECORDS);
        assert!(status.ring_bytes <= DEFAULT_RING_BYTES);
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }
}

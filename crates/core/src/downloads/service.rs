//! `CoreModule` implementation: command routing for module 03.
//!
//! Routing convention (frozen by 00): `payload.command`, first segment is the
//! module id (`downloads.`).
//!
//! Downloads are always long operations: `downloads.plan`, `downloads.fetch` and
//! `downloads.resume` accept a task and answer `accepted`, progress travels as
//! task payloads, and the "progress kept" / "files ready" markers land in
//! `Task.evidence` only after the corresponding fact is durable. That is what lets
//! the UX layer gate its copy on evidence instead of on optimism.
//!
//! Cache bookkeeping (`downloads.cache.*`) is a transaction and answers
//! `completed` immediately.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use serde_json::{json, Value};
use shtmcl_domain::{
    CommandRequest, CommandResponse, ErrorEnvelope, EventKind, ModuleId, OperationId, TaskPhase,
    TaskState,
};

use crate::downloads::error::{evidence, DownloadsError};
use crate::downloads::ports::{ClockPort, DiskPort, DownloadPort, FetchOutcome};
use crate::downloads::state::{
    ArtifactHash, ArtifactRequest, ArtifactState, ByteRange, CacheIndex, Scheduler, Slot, Transfer,
    DEFAULT_TOTAL_CONCURRENCY,
};
use crate::downloads::MODULE;
use crate::tasks::{bytes_progress, EventRequest, EventSink, TaskService, TaskSpec};
use crate::{CoreModule, ModuleContext};

/// Commands this module owns. Stable; registered in the 03 handoff.
pub mod actions {
    pub const PLAN: &str = "downloads.plan";
    pub const FETCH: &str = "downloads.fetch";
    pub const RESUME: &str = "downloads.resume";
    pub const STATUS: &str = "downloads.status";
    pub const CACHE_LIST: &str = "downloads.cache.list";
    pub const CACHE_COLLECT: &str = "downloads.cache.collect";
}

/// Event kinds published by this module.
pub mod events {
    pub fn progress() -> &'static str {
        "downloads.progress"
    }

    pub fn artifact_changed() -> &'static str {
        "downloads.artifact.changed"
    }
}

/// Bytes requested per fetch when the caller does not choose a chunk size.
const DEFAULT_CHUNK: u64 = 1 << 20;
/// Largest chunk accepted, so a caller cannot ask for one 64 MiB+ request.
const MAX_CHUNK: u64 = 8 << 20;
/// Pending requests are bounded independently from the six active workers.
const MAX_QUEUED_TASKS: usize = 64;
/// Bound per-task request metadata and the amount of work one UI command can add.
const MAX_ARTIFACTS_PER_TASK: usize = 256;
/// Admission waits are cancellable and cannot consume a worker indefinitely.
const SLOT_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);
const SLOT_WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Module state shared with worker threads.
///
/// Cloned (via `Arc`) into each worker so progress written by a background task
/// is the same state a `downloads.status` query reads. Keeping one owner here —
/// instead of a `OnceLock` per field — is what prevents a worker and the query
/// path from silently observing two different schedulers.
#[derive(Clone)]
struct Shared {
    scheduler: Arc<Mutex<Scheduler>>,
    transfers: Arc<Mutex<BTreeMap<String, Transfer>>>,
    cache: Arc<Mutex<CacheIndex>>,
    jobs: Arc<Mutex<BTreeMap<String, Arc<SharedJob>>>>,
    next_operation_epoch: Arc<AtomicU64>,
    admission: Arc<Mutex<AdmissionQueues>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            scheduler: Arc::new(Mutex::new(Scheduler::default_limits())),
            transfers: Arc::new(Mutex::new(BTreeMap::new())),
            cache: Arc::new(Mutex::new(CacheIndex::new())),
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
            next_operation_epoch: Arc::new(AtomicU64::new(1)),
            admission: Arc::new(Mutex::new(AdmissionQueues::default())),
        }
    }
}

#[derive(Default)]
struct AdmissionQueues {
    sources: BTreeMap<String, VecDeque<Arc<()>>>,
    len: usize,
}

/// One ticket per running worker, never per queued request. Rejoining for the
/// next artifact puts a busy batch behind workers already waiting on its source.
struct AdmissionTicket {
    queues: Arc<Mutex<AdmissionQueues>>,
    source: String,
    token: Arc<()>,
}

impl AdmissionTicket {
    fn new(shared: &Shared, source: &str) -> Result<Self, DownloadsError> {
        let mut queues = shared.admission.lock().unwrap_or_else(|p| p.into_inner());
        if queues.len >= DEFAULT_TOTAL_CONCURRENCY {
            return Err(DownloadsError::Internal);
        }
        let token = Arc::new(());
        queues
            .sources
            .entry(source.to_owned())
            .or_default()
            .push_back(token.clone());
        queues.len += 1;
        Ok(Self {
            queues: shared.admission.clone(),
            source: source.to_owned(),
            token,
        })
    }

    /// Lock order: jobs -> admission -> scheduler. No waiting holds these locks.
    fn try_acquire(&self, shared: &Shared, now: u64) -> Option<Slot> {
        let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
        let is_front = queues
            .sources
            .get(&self.source)
            .and_then(|queue| queue.front())
            .is_some_and(|token| Arc::ptr_eq(token, &self.token));
        if !is_front {
            return None;
        }
        let result = shared
            .scheduler
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .try_acquire(&self.source, now);
        if result == Slot::Granted {
            self.remove(&mut queues);
        }
        Some(result)
    }

    fn remove(&self, queues: &mut AdmissionQueues) {
        if let Some(queue) = queues.sources.get_mut(&self.source) {
            if let Some(index) = queue
                .iter()
                .position(|token| Arc::ptr_eq(token, &self.token))
            {
                queue.remove(index);
                queues.len -= 1;
            }
            if queue.is_empty() {
                queues.sources.remove(&self.source);
            }
        }
    }
}

impl Drop for AdmissionTicket {
    fn drop(&mut self) {
        let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
        self.remove(&mut queues);
    }
}

struct SharedJobState {
    subscribers: usize,
    outcome: Option<Result<u64, DownloadsError>>,
}

struct SharedJob {
    state: Mutex<SharedJobState>,
    changed: Condvar,
}

struct JobSubscription {
    key: String,
    job: Arc<SharedJob>,
    shared: Shared,
    leader: bool,
}

struct SchedulerSlot {
    shared: Shared,
    source_id: String,
    claim_key: String,
}

impl Drop for SchedulerSlot {
    fn drop(&mut self) {
        let mut scheduler = self
            .shared
            .scheduler
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        scheduler.release(&self.source_id);
        scheduler.release_claim(&self.claim_key);
    }
}

impl JobSubscription {
    fn complete(&self, outcome: Result<u64, DownloadsError>) {
        let mut state = self.job.state.lock().unwrap_or_else(|p| p.into_inner());
        state.outcome = Some(outcome);
        self.job.changed.notify_all();
    }
}

impl Drop for JobSubscription {
    fn drop(&mut self) {
        let remove = {
            let mut state = self.job.state.lock().unwrap_or_else(|p| p.into_inner());
            if self.leader && state.outcome.is_none() {
                state.outcome = Some(Err(DownloadsError::Internal));
                self.job.changed.notify_all();
            }
            state.subscribers = state.subscribers.saturating_sub(1);
            state.subscribers == 0 && state.outcome.is_some()
        };
        if remove {
            let mut jobs = self.shared.jobs.lock().unwrap_or_else(|p| p.into_inner());
            if jobs
                .get(&self.key)
                .is_some_and(|job| Arc::ptr_eq(job, &self.job))
            {
                jobs.remove(&self.key);
            }
        }
    }
}

/// The 03 module.
pub struct DownloadsModule {
    context: ModuleContext,
    downloads: Arc<dyn DownloadPort>,
    disk: Arc<dyn DiskPort>,
    clock: Arc<dyn ClockPort>,
    shared: Shared,
    workers: WorkerPool,
}

impl DownloadsModule {
    pub fn new(
        context: ModuleContext,
        downloads: Arc<dyn DownloadPort>,
        disk: Arc<dyn DiskPort>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        let workers = WorkerPool::new(DEFAULT_TOTAL_CONCURRENCY, MAX_QUEUED_TASKS);
        Self {
            context,
            downloads,
            disk,
            clock,
            shared: Shared::new(),
            workers,
        }
    }

    /// Wire the fail-closed defaults (no transport, no known disk).
    pub fn unavailable(context: ModuleContext) -> Self {
        Self::new(
            context,
            Arc::new(crate::downloads::ports::UnavailableDownloads),
            Arc::new(crate::downloads::ports::UnknownDisk),
            Arc::new(crate::downloads::ports::SystemClock),
        )
    }

    /// Read-only view of the cache index, for tests and diagnostics.
    pub fn cache_snapshot(&self) -> (usize, u64, Vec<String>) {
        let cache = self.shared.cache.lock().unwrap_or_else(|p| p.into_inner());
        (cache.len(), cache.total_bytes(), cache.collectable())
    }

    /// The task service this module accepted work on.
    ///
    /// Exposed so a caller can follow a task it started by id; the desktop layer
    /// already holds the same `Arc` through the app, but the isolated crate's
    /// tests build a module without an app.
    pub fn task_service(&self) -> &Arc<dyn TaskService> {
        self.context.tasks()
    }

    // ---------------------------------------------------------------- queries

    fn status(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let id = required_str(request, "artifactId")?;
        let transfers = self
            .shared
            .transfers
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let transfer = transfers
            .get(id)
            .ok_or_else(|| DownloadsError::NotFound(id.to_string()).to_envelope())?;
        Ok(completed(json!({
            "artifactId": transfer.artifact_id,
            "sourceId": transfer.source_id,
            "state": transfer.state,
            "downloaded": transfer.downloaded(),
            "total": transfer.total,
            "ranges": transfer.ranges,
            "generation": transfer.generation,
            "evidence": transfer.evidence(),
        })))
    }

    fn cache_list(&self, _request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let (count, total_bytes, collectable) = self.cache_snapshot();
        Ok(completed(json!({
            "count": count,
            "totalBytes": total_bytes,
            "collectable": collectable,
        })))
    }

    fn cache_collect(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let keep = optional_u64(request, "keep").unwrap_or(0) as usize;
        let mut cache = self.shared.cache.lock().unwrap_or_else(|p| p.into_inner());
        let collectable = cache.collectable_entries();
        let collectable_count = collectable.len();
        let remove_count = collectable.len().saturating_sub(keep);
        let mut collected = 0usize;
        for entry in collectable.into_iter().take(remove_count) {
            // Hold the index lock through deletion so a concurrent retain cannot
            // make a live resource disappear between the refcount check and unlink.
            self.downloads
                .remove_cached(&entry.hash)
                .map_err(|reference| DownloadsError::IoFailed(reference).to_envelope())?;
            if !cache.remove_collectable(&entry.hash) {
                return Err(DownloadsError::CacheCorrupt(entry.hash.key()).to_envelope());
            }
            collected += 1;
        }
        Ok(completed(json!({
            "collectable": collectable_count,
            "collected": collected,
            "kept": cache.collectable_entries().len(),
        })))
    }

    // ------------------------------------------------------------ long tasks

    /// `downloads.plan`, `downloads.fetch` and `downloads.resume` share one
    /// worker: a plan is the single-artifact subset of a set, so there is one code
    /// path to reason about and one place where evidence is attached.
    fn start(
        &self,
        request: &CommandRequest,
        resume_default: bool,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        let operation = require_operation(request)?;
        let requests = parse_requests(request)?;
        if requests.is_empty() {
            return Err(DownloadsError::InvalidRequest("artifacts").to_envelope());
        }
        if requests.len() > MAX_ARTIFACTS_PER_TASK {
            return Err(DownloadsError::InvalidRequest("artifacts").to_envelope());
        }
        validate_artifact_requests(&requests).map_err(|error| error.to_envelope())?;
        let discard_on_cancel = request.payload.get("discardOnCancel").and_then(Value::as_bool).unwrap_or(false);
        let resume = request
            .payload
            .get("resume")
            .and_then(Value::as_bool)
            .unwrap_or(resume_default);
        {
            let transfers = self
                .shared
                .transfers
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for request in &requests {
                if let Some(existing) = transfers.get(&request.artifact_id) {
                    let active = matches!(
                        existing.state,
                        ArtifactState::Queued
                            | ArtifactState::Downloading
                            | ArtifactState::Verifying
                    );
                    if (resume || active) && existing.hash != request.expected {
                        return Err(DownloadsError::IdempotencyConflict.to_envelope());
                    }
                }
            }
        }
        let chunk = optional_u64(request, "chunkBytes")
            .unwrap_or(DEFAULT_CHUNK)
            .clamp(1, MAX_CHUNK);
        // Pre-flight the disk: refusing before writing a byte is the difference
        // between a clear message and a half-filled cache.
        let needed = requests
            .iter()
            .filter_map(|request| request.size)
            .fold(0u64, u64::saturating_add);
        if needed > 0 {
            let available = self
                .disk
                .available()
                .map_err(|reference| DownloadsError::IoFailed(reference).to_envelope())?;
            if available < needed {
                return Err(DownloadsError::DiskFull { needed, available }.to_envelope());
            }
        }

        // Reserve bounded queue capacity before creating a task. This avoids
        // accepting work that can never be scheduled and avoids one OS thread
        // per request.
        let queue_permit = self
            .workers
            .reserve()
            .ok_or_else(|| DownloadsError::QueueFull.to_envelope())?;

        let spec = TaskSpec::new(
            operation.clone(),
            TaskPhase::new("downloads.fetch")
                .map_err(|_| DownloadsError::Internal.to_envelope())?,
        );
        let task = self.context.tasks().accept(spec)?;
        let generation = task.attempt_generation;
        let operation_epoch = self
            .shared
            .next_operation_epoch
            .fetch_add(1, Ordering::Relaxed);

        // Register the transfers before the worker starts so a status request that
        // arrives immediately already sees the queued artifacts.
        {
            let mut transfers = self
                .shared
                .transfers
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for artifact in &requests {
                let previous = transfers.get(&artifact.artifact_id).cloned();
                let shared_active = previous.as_ref().is_some_and(|transfer| {
                    matches!(
                        transfer.state,
                        ArtifactState::Queued
                            | ArtifactState::Downloading
                            | ArtifactState::Verifying
                    ) && transfer.hash == artifact.expected
                });
                if shared_active {
                    // Keep the current writer's epoch and ranges. This task will
                    // join its content-addressed job rather than replace state.
                    continue;
                }
                let mut transfer = match previous {
                    // Resume keeps the recorded ranges; a fresh start discards them.
                    Some(mut previous) if resume => {
                        previous.source_id = artifact.source_id.clone();
                        previous.hash = artifact.expected.clone();
                        previous.total = artifact.size.or(previous.total);
                        previous
                    }
                    _ => Transfer::new(artifact, generation.get()),
                };
                transfer.generation = generation.get();
                transfer.operation_epoch = operation_epoch;
                transfer.state = ArtifactState::Queued;
                transfers.insert(artifact.artifact_id.clone(), transfer);
            }
        }

        let worker = Worker {
            tasks: Arc::clone(self.context.tasks()),
            events: Arc::clone(self.context.events()),
            downloads: Arc::clone(&self.downloads),
            clock: Arc::clone(&self.clock),
            shared: self.shared.clone(),
            operation,
            task_id: task.id.clone(),
            generation,
            operation_epoch,
            requests,
            chunk,
            discard_on_cancel,
        };
        queue_permit.enqueue(worker);

        Ok(CommandResponse::Accepted {
            operation_id: task.operation_id,
            task_id: task.id,
        })
    }
}

// ---------------------------------------------------------------------------
// bounded worker pool
// ---------------------------------------------------------------------------

struct QueueState {
    pending: VecDeque<Worker>,
    reserved: usize,
    stopped: bool,
}

struct WorkQueue {
    state: Mutex<QueueState>,
    available: Condvar,
    capacity: usize,
}

/// A fixed set of threads drains a bounded FIFO queue. `QueuePermit` reserves a
/// pending slot before a Task is accepted, closing the race between capacity
/// checks and concurrent submissions.
struct WorkerPool {
    queue: Arc<WorkQueue>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl WorkerPool {
    fn new(worker_count: usize, capacity: usize) -> Self {
        let queue = Arc::new(WorkQueue {
            state: Mutex::new(QueueState {
                pending: VecDeque::new(),
                reserved: 0,
                stopped: false,
            }),
            available: Condvar::new(),
            capacity,
        });
        let threads = (0..worker_count.max(1))
            .map(|_| {
                let queue = Arc::clone(&queue);
                std::thread::spawn(move || loop {
                    let worker = {
                        let mut state = queue.state.lock().unwrap_or_else(|p| p.into_inner());
                        while state.pending.is_empty() && !state.stopped {
                            state = queue
                                .available
                                .wait(state)
                                .unwrap_or_else(|p| p.into_inner());
                        }
                        if state.pending.is_empty() && state.stopped {
                            return;
                        }
                        state.pending.pop_front()
                    };
                    if let Some(worker) = worker {
                        let tasks = Arc::clone(&worker.tasks);
                        let task_id = worker.task_id.clone();
                        let generation = worker.generation;
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run()))
                            .is_err()
                        {
                            let _ = tasks.fail(
                                &task_id,
                                generation,
                                DownloadsError::Internal.to_envelope(),
                            );
                        }
                    }
                })
            })
            .collect();
        Self {
            queue,
            threads: Mutex::new(threads),
        }
    }

    fn reserve(&self) -> Option<QueuePermit> {
        let mut state = self.queue.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.stopped
            || state.pending.len().saturating_add(state.reserved) >= self.queue.capacity
        {
            return None;
        }
        state.reserved += 1;
        Some(QueuePermit {
            queue: Arc::clone(&self.queue),
            committed: false,
        })
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        {
            let mut state = self.queue.state.lock().unwrap_or_else(|p| p.into_inner());
            state.stopped = true;
            self.queue.available.notify_all();
        }
        let threads = self.threads.get_mut().unwrap_or_else(|p| p.into_inner());
        for thread in threads.drain(..) {
            let _ = thread.join();
        }
    }
}

struct QueuePermit {
    queue: Arc<WorkQueue>,
    committed: bool,
}

impl QueuePermit {
    fn enqueue(mut self, worker: Worker) {
        let mut state = self.queue.state.lock().unwrap_or_else(|p| p.into_inner());
        state.reserved = state.reserved.saturating_sub(1);
        if !state.stopped {
            state.pending.push_back(worker);
            self.committed = true;
            self.queue.available.notify_one();
        }
    }
}

impl Drop for QueuePermit {
    fn drop(&mut self) {
        if !self.committed {
            let mut state = self.queue.state.lock().unwrap_or_else(|p| p.into_inner());
            state.reserved = state.reserved.saturating_sub(1);
        }
    }
}

impl CoreModule for DownloadsModule {
    fn id(&self) -> ModuleId {
        MODULE
    }

    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        let command = request.command()?;
        match command.as_str() {
            actions::PLAN | actions::FETCH => self.start(request, false),
            actions::RESUME => self.start(request, true),
            actions::STATUS => self.status(request),
            actions::CACHE_LIST => self.cache_list(request),
            actions::CACHE_COLLECT => self.cache_collect(request),
            _ => Err(DownloadsError::UnknownCommand.to_envelope()),
        }
    }
}

// ---------------------------------------------------------------------------
// worker
// ---------------------------------------------------------------------------

impl Shared {
    fn retire_failed_transfers(&self, operation_epoch: u64) {
        let mut transfers = self.transfers.lock().unwrap_or_else(|p| p.into_inner());
        for transfer in transfers.values_mut() {
            if transfer.operation_epoch == operation_epoch
                && matches!(
                    transfer.state,
                    ArtifactState::Queued | ArtifactState::Downloading | ArtifactState::Verifying
                )
            {
                let key = transfer
                    .hash
                    .as_ref()
                    .map(ArtifactHash::key)
                    .unwrap_or_else(|| transfer.artifact_id.clone());
                let active = self
                    .jobs
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&key)
                    .is_some_and(|job| {
                        job.state
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .outcome
                            .is_none()
                    });
                if !active {
                    transfer.state = ArtifactState::Failed;
                }
            }
        }
    }
}

/// Owns one download operation for the lifetime of its task.
struct Worker {
    tasks: Arc<dyn TaskService>,
    events: Arc<dyn EventSink>,
    downloads: Arc<dyn DownloadPort>,
    clock: Arc<dyn ClockPort>,
    shared: Shared,
    operation: OperationId,
    task_id: shtmcl_domain::TaskId,
    generation: shtmcl_domain::AttemptGeneration,
    operation_epoch: u64,
    requests: Vec<ArtifactRequest>,
    chunk: u64,
    discard_on_cancel: bool,
}

impl Worker {
    fn run(self) {
        // `accept` leaves the task `Queued`, and `Queued -> Succeeded` is not a
        // legal transition: the worker must mark itself `Running` before it may
        // settle. Skipping this is what makes a task hang forever in `queued`
        // while its work has already finished.
        if self
            .tasks
            .transition(&self.task_id, self.generation, TaskState::Running)
            .is_err()
        {
            return;
        }

        let total = self
            .requests
            .iter()
            .filter_map(|request| request.size)
            .fold(0u64, u64::saturating_add);
        let mut done: u64 = 0;

        for request in &self.requests {
            if self.cancelled() {
                self.finish_interrupted(&request.artifact_id);
                return;
            }
            match self.fetch_one(request) {
                Ok(bytes) => {
                    done = done.saturating_add(bytes);
                    // `bytes_progress` validates its own range; a rejected value
                    // must not fail the transfer that produced it.
                    if let Ok(progress) =
                        bytes_progress(done, if total > 0 { Some(total) } else { None })
                    {
                        let _ = self
                            .tasks
                            .set_progress(&self.task_id, self.generation, progress);
                    }
                }
                Err(DownloadsError::Cancelled) => {
                    self.finish_interrupted(&request.artifact_id);
                    return;
                }
                Err(error) => {
                    self.finish_failed_transfers();
                    self.fail(&error.to_envelope());
                    return;
                }
            }
        }
        if self.cancelled() {
            let _ = self.tasks.cancel_now(&self.task_id, self.generation);
            return;
        }
        let _ = self.tasks.succeed(
            &self.task_id,
            self.generation,
            Some(json!({
                "artifacts": self.requests.iter().map(|r| r.artifact_id.clone()).collect::<Vec<_>>(),
                "bytes": done,
            })),
        );
    }

    fn cancelled(&self) -> bool {
        self.tasks
            .is_cancel_requested(&self.task_id, self.generation)
    }

    fn cancelled_for_job(&self, key: &str) -> bool {
        if !self.cancelled() {
            return false;
        }
        self.shared
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(key)
            .map_or(true, |job| {
                job.state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .subscribers
                    <= 1
            })
    }

    /// Download one artifact to completion, respecting the per-source cap.
    fn fetch_one(&self, request: &ArtifactRequest) -> Result<u64, DownloadsError> {
        if let Some(hash) = request.expected.as_ref() {
            let mut cache = self.shared.cache.lock().unwrap_or_else(|p| p.into_inner());
            let indexed = cache.lookup(hash).is_some();
            let materialized = if indexed {
                self.downloads
                    .materialize_cached(hash, &request.artifact_id)
            } else {
                self.downloads.recover_cached(hash, &request.artifact_id)
            }
            .map_err(DownloadsError::IoFailed)?;
            if let Some(size) = materialized {
                if request.size.is_some_and(|expected| expected != size) {
                    return Err(DownloadsError::CacheCorrupt(hash.key()));
                }
                cache.publish(hash, size)?;
                let mut transfer = Transfer::new(request, self.generation.get());
                transfer.operation_epoch = self.operation_epoch;
                transfer.total = Some(size);
                if size > 0 {
                    transfer.record(
                        ByteRange {
                            start: 0,
                            end: size,
                        },
                        self.generation.get(),
                    )?;
                }
                transfer.state = ArtifactState::Ready;
                self.shared
                    .transfers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(request.artifact_id.clone(), transfer);
                return Ok(size);
            }
            // A stale index entry is removed only when it has no consumers.
            if indexed {
                cache.remove_collectable(hash);
            }
        }

        // One shared job per content hash. Each operation subscribes separately,
        // so cancelling one consumer does not stop the transfer for the others.
        let key = request
            .expected
            .as_ref()
            .map(ArtifactHash::key)
            .unwrap_or_else(|| request.artifact_id.clone());

        let admission_started = std::time::Instant::now();
        let mut admission_ticket = None;
        let (subscription, leader) = loop {
            if self.cancelled() {
                return Err(DownloadsError::Cancelled);
            }
            let mut jobs = self.shared.jobs.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(job) = jobs.get(&key) {
                let mut state = job.state.lock().unwrap_or_else(|p| p.into_inner());
                state.subscribers = state.subscribers.saturating_add(1);
                break (
                    JobSubscription {
                        key: key.clone(),
                        job: Arc::clone(job),
                        shared: self.shared.clone(),
                        leader: false,
                    },
                    false,
                );
            } else {
                if admission_ticket.is_none() {
                    admission_ticket =
                        Some(AdmissionTicket::new(&self.shared, &request.source_id)?);
                }
                match admission_ticket
                    .as_ref()
                    .unwrap()
                    .try_acquire(&self.shared, self.clock.now_ms())
                {
                    Some(Slot::Granted) => {}
                    None | Some(Slot::GlobalBusy | Slot::SourceBusy | Slot::Throttled { .. }) => {
                        // No job is published until its running worker owns a
                        // slot. A waiting worker therefore never blocks an owner
                        // sitting behind it in the pending worker queue.
                        drop(jobs);
                        if admission_started.elapsed() >= SLOT_WAIT_LIMIT {
                            return Err(DownloadsError::SourceBusy(request.source_id.clone()));
                        }
                        std::thread::sleep(SLOT_WAIT_POLL);
                        continue;
                    }
                }
                let mut scheduler = self
                    .shared
                    .scheduler
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if !scheduler.claim(&key) {
                    scheduler.release(&request.source_id);
                    return Err(DownloadsError::SourceBusy(request.source_id.clone()));
                }
                drop(scheduler);

                let job = Arc::new(SharedJob {
                    state: Mutex::new(SharedJobState {
                        subscribers: 1,
                        outcome: None,
                    }),
                    changed: Condvar::new(),
                });
                jobs.insert(key.clone(), Arc::clone(&job));
                break (
                    JobSubscription {
                        key: key.clone(),
                        job,
                        shared: self.shared.clone(),
                        leader: true,
                    },
                    true,
                );
            }
        };
        // Joining an existing shared job also relinquishes our queue position.
        drop(admission_ticket);

        if !leader {
            return self.wait_for_shared_job(request, key, subscription);
        }

        let slot = SchedulerSlot {
            shared: self.shared.clone(),
            source_id: request.source_id.clone(),
            claim_key: key.clone(),
        };
        let mut result = self.fetch_holding_slot(request, &key);
        // Only the current leader owns this partial. Keep the published job and
        // scheduler claim until cleanup ends, so followers cannot become writers.
        // Completed objects and unrelated batch members are never removed here.
        if self.discard_on_cancel && matches!(result, Err(DownloadsError::Cancelled)) {
            result = match self.downloads.discard(&request.artifact_id) {
                Ok(()) => {
                    let mut transfers = self.shared.transfers.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(transfer) = transfers.get_mut(&request.artifact_id) {
                        if transfer.operation_epoch == self.operation_epoch {
                            transfer.ranges.clear();
                        }
                    }
                    Err(DownloadsError::Cancelled)
                }
                Err(error) => Err(DownloadsError::IoFailed(error)),
            };
        }
        drop(slot);
        subscription.complete(result.clone());
        result
    }

    // The shared leader has settled before fetch_one returns an error.
    // Retire every unprocessed transfer reserved by this failed batch too;
    // otherwise a fresh operation mistakes stale Queued entries for writers.
    fn finish_failed_transfers(&self) {
        self.shared.retire_failed_transfers(self.operation_epoch);
    }

    fn wait_for_shared_job(
        &self,
        request: &ArtifactRequest,
        key: String,
        _subscription: JobSubscription,
    ) -> Result<u64, DownloadsError> {
        let Some(job) = self
            .shared
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .cloned()
        else {
            return Err(DownloadsError::Internal);
        };
        loop {
            if self.cancelled() {
                return Err(DownloadsError::Cancelled);
            }
            let mut state = job.state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(result) = state.outcome.clone() {
                drop(state);
                let size = result?;
                if let Some(hash) = request.expected.as_ref() {
                    let materialized = self
                        .downloads
                        .materialize_cached(hash, &request.artifact_id)
                        .map_err(DownloadsError::IoFailed)?;
                    if let Some(size) = materialized {
                        let mut transfer = Transfer::new(request, self.generation.get());
                        transfer.operation_epoch = self.operation_epoch;
                        transfer.total = Some(size);
                        if size > 0 {
                            transfer.record(
                                ByteRange {
                                    start: 0,
                                    end: size,
                                },
                                self.generation.get(),
                            )?;
                        }
                        transfer.state = ArtifactState::Ready;
                        self.shared
                            .transfers
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(request.artifact_id.clone(), transfer);
                        return Ok(size);
                    }
                    return Err(DownloadsError::CacheMiss(hash.key()));
                }
                return Ok(size);
            }
            let (next, _) = job
                .changed
                .wait_timeout(state, std::time::Duration::from_millis(40))
                .unwrap_or_else(|p| p.into_inner());
            state = next;
            drop(state);
        }
    }

    fn fetch_holding_slot(
        &self,
        request: &ArtifactRequest,
        job_key: &str,
    ) -> Result<u64, DownloadsError> {
        if self.cancelled_for_job(job_key) {
            return Err(DownloadsError::Cancelled);
        }
        let total = match request.size {
            Some(size) => Some(size),
            None => {
                let cancelled = || self.cancelled_for_job(job_key);
                let result = match request.source_url.as_deref() {
                    Some(url) => self.downloads.probe_url_cancellable(
                        &request.source_id,
                        &request.artifact_id,
                        url,
                        &cancelled,
                    ),
                    None => self.downloads.probe_cancellable(
                        &request.source_id,
                        &request.artifact_id,
                        &cancelled,
                    ),
                };
                match result {
                    Ok(total) => total,
                    Err(reference) if reference == "downloads.task.cancelled" => {
                        return Err(DownloadsError::Cancelled)
                    }
                    Err(reference) => return Err(DownloadsError::NetworkFailed(reference)),
                }
            }
        };

        {
            let mut transfers = self
                .shared
                .transfers
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let transfer = transfers
                .entry(request.artifact_id.clone())
                .or_insert_with(|| Transfer::new(request, self.generation.get()));
            if transfer.operation_epoch == 0 {
                transfer.operation_epoch = self.operation_epoch;
            }
            if transfer.operation_epoch != self.operation_epoch {
                return Err(DownloadsError::IdempotencyConflict);
            }
            transfer.total = total.or(transfer.total);
            transfer.state = ArtifactState::Downloading;
        }

        if total == Some(0) {
            // A range cannot represent an empty file. Materialize its known
            // contents locally, then use the same hash and cache commit path.
            self.downloads
                .prepare_empty(&request.artifact_id)
                .map_err(DownloadsError::IoFailed)?;
        }
        let mut etag: Option<String> = None;
        loop {
            if self.cancelled_for_job(job_key) {
                return Err(DownloadsError::Cancelled);
            }
            let next = {
                let transfers = self
                    .shared
                    .transfers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                transfers
                    .get(&request.artifact_id)
                    .and_then(|transfer| transfer.next_gap(self.chunk))
            };
            let Some(range) = next else { break };

            let cancelled = || self.cancelled_for_job(job_key);
            let outcome = match request.source_url.as_deref() {
                Some(url) => self.downloads.fetch_url_cancellable(
                    &request.source_id,
                    &request.artifact_id,
                    url,
                    range,
                    etag.as_deref(),
                    &cancelled,
                ),
                None => self.downloads.fetch_cancellable(
                    &request.source_id,
                    &request.artifact_id,
                    range,
                    etag.as_deref(),
                    &cancelled,
                ),
            };
            match outcome {
                FetchOutcome::Cancelled => return Err(DownloadsError::Cancelled),
                FetchOutcome::Bytes {
                    range: served,
                    etag: served_etag,
                } => {
                    if served != range {
                        // A source that answers a different range would corrupt the
                        // file silently; refuse instead of accepting it.
                        return Err(DownloadsError::RangeInvalid(request.artifact_id.clone()));
                    }
                    // Validate the bounded progress index before persisting bytes;
                    // if it cannot represent the range, leave both disk and index
                    // unchanged instead of writing untracked partial data.
                    let mut next_transfer = {
                        let transfers = self
                            .shared
                            .transfers
                            .lock()
                            .unwrap_or_else(|p| p.into_inner());
                        transfers
                            .get(&request.artifact_id)
                            .cloned()
                            .unwrap_or_else(|| {
                                let mut transfer = Transfer::new(request, self.generation.get());
                                transfer.operation_epoch = self.operation_epoch;
                                transfer
                            })
                    };
                    next_transfer.record_for(
                        served,
                        self.generation.get(),
                        self.operation_epoch,
                    )?;

                    // Durable before counted: this is what makes a resume possible.
                    self.downloads
                        .commit_range(&request.artifact_id, served)
                        .map_err(DownloadsError::IoFailed)?;
                    self.shared
                        .transfers
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(request.artifact_id.clone(), next_transfer);
                    etag = served_etag.or(etag);
                    self.publish_progress();
                }
                FetchOutcome::RangeUnsupported => {
                    // The source rejected a byte range. Restart as one streamed
                    // response; do not retain segments from the previous attempt.
                    self.downloads
                        .discard(&request.artifact_id)
                        .map_err(DownloadsError::IoFailed)?;
                    if let Some(transfer) = self
                        .shared
                        .transfers
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get_mut(&request.artifact_id)
                    {
                        transfer.ranges.clear();
                    }
                    let cancelled = || self.cancelled_for_job(job_key);
                    let full = match request.source_url.as_deref() {
                        Some(url) => self.downloads.fetch_full_url_cancellable(
                            &request.source_id,
                            &request.artifact_id,
                            url,
                            &cancelled,
                        ),
                        None => self.downloads.fetch_full_cancellable(
                            &request.source_id,
                            &request.artifact_id,
                            &cancelled,
                        ),
                    };
                    match full {
                        FetchOutcome::Cancelled => return Err(DownloadsError::Cancelled),
                        FetchOutcome::Bytes {
                            range: full_range,
                            etag: full_etag,
                        } if full_range.start == 0
                            && full_range.end > 0
                            && total.is_none_or(|expected| expected == full_range.end) =>
                        {
                            let mut transfer = {
                                let transfers = self
                                    .shared
                                    .transfers
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner());
                                transfers
                                    .get(&request.artifact_id)
                                    .cloned()
                                    .unwrap_or_else(|| {
                                        let mut transfer =
                                            Transfer::new(request, self.generation.get());
                                        transfer.operation_epoch = self.operation_epoch;
                                        transfer
                                    })
                            };
                            transfer.ranges.clear();
                            transfer.total = Some(full_range.end);
                            transfer.record_for(
                                full_range,
                                self.generation.get(),
                                self.operation_epoch,
                            )?;
                            self.downloads
                                .commit_range(&request.artifact_id, full_range)
                                .map_err(DownloadsError::IoFailed)?;
                            self.shared
                                .transfers
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .insert(request.artifact_id.clone(), transfer);
                            let _ = full_etag;
                            self.publish_progress();
                            break;
                        }
                        FetchOutcome::Throttled { retry_at } => {
                            let _ = self.downloads.discard(&request.artifact_id);
                            self.shared
                                .scheduler
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .throttle(&request.source_id, retry_at);
                            return Err(DownloadsError::SourceBusy(request.source_id.clone()));
                        }
                        FetchOutcome::TransportFailed(reference) => {
                            let _ = self.downloads.discard(&request.artifact_id);
                            return Err(DownloadsError::NetworkFailed(reference));
                        }
                        _ => {
                            let _ = self.downloads.discard(&request.artifact_id);
                            return Err(DownloadsError::RangeUnsupported(
                                request.artifact_id.clone(),
                            ));
                        }
                    }
                }
                FetchOutcome::Throttled { retry_at } => {
                    let mut scheduler = self
                        .shared
                        .scheduler
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    scheduler.throttle(&request.source_id, retry_at);
                    return Err(DownloadsError::SourceBusy(request.source_id.clone()));
                }
                FetchOutcome::TransportFailed(reference) => {
                    return Err(DownloadsError::NetworkFailed(reference));
                }
                FetchOutcome::HashMismatch { expected, actual } => {
                    let _ = expected;
                    let _ = actual;
                    return Err(DownloadsError::HashMismatch(request.artifact_id.clone()));
                }
            }
        }

        self.verify_and_publish(request)
    }

    fn verify_and_publish(&self, request: &ArtifactRequest) -> Result<u64, DownloadsError> {
        {
            let mut transfers = self
                .shared
                .transfers
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(transfer) = transfers.get_mut(&request.artifact_id) {
                transfer.state = ArtifactState::Verifying;
            }
        }

        let Some(expected) = request.expected.clone() else {
            // No expected hash: the artifact cannot be cached by content, so it is
            // reported as failed rather than published under a guessed key.
            return Err(DownloadsError::HashMissing(request.artifact_id.clone()));
        };

        let actual = self
            .downloads
            .hash_bytes(&request.artifact_id, &expected.algorithm)
            .map_err(DownloadsError::IoFailed)?;
        if !actual.eq_ignore_ascii_case(&expected.digest) {
            // Wrong bytes are worse than no bytes: discard so a later resume cannot
            // stitch a corrupt prefix onto a good suffix.
            let _ = self.downloads.discard(&request.artifact_id);
            {
                let mut transfers = self
                    .shared
                    .transfers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if let Some(transfer) = transfers.get_mut(&request.artifact_id) {
                    transfer.ranges.clear();
                    transfer.state = ArtifactState::Failed;
                }
            }
            return Err(DownloadsError::HashMismatch(request.artifact_id.clone()));
        }

        let size = self
            .downloads
            .publish(&expected, &request.artifact_id)
            .map_err(DownloadsError::IoFailed)?;

        self.shared
            .cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .publish(&expected, size)?;

        let downloaded = {
            let mut transfers = self
                .shared
                .transfers
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let transfer = transfers
                .get_mut(&request.artifact_id)
                .ok_or_else(|| DownloadsError::NotFound(request.artifact_id.clone()))?;
            transfer.state = ArtifactState::Ready;
            transfer.downloaded()
        };

        // Evidence only now: the bytes are verified and in the cache.
        let _ = self
            .tasks
            .add_evidence(&self.task_id, evidence::COMPLETED_ARTIFACTS);

        let _ = self.events.publish(
            EventRequest::new(
                self.operation.clone(),
                self.generation,
                EventKind::from_static(events::artifact_changed()),
            )
            .with_payload(json!({
                "artifactId": request.artifact_id,
                "sourceId": request.source_id,
                "state": "ready",
                "size": size,
            })),
        );

        Ok(downloaded)
    }

    /// Record that an interruption kept its ranges, so the UI may say so.
    fn finish_interrupted(&self, interrupted_artifact_id: &str) {
        let mut kept = false;
        for request in &self.requests {
            let artifact_id = request.artifact_id.as_str();
            let active = {
                let transfers = self
                    .shared
                    .transfers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                let key = transfers
                    .get(artifact_id)
                    .and_then(|transfer| transfer.hash.as_ref().map(ArtifactHash::key))
                    .unwrap_or_else(|| artifact_id.to_string());
                let active = self
                    .shared
                    .jobs
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&key)
                    .is_some_and(|job| {
                        job.state
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .outcome
                            .is_none()
                    });
                active
            };
            let request_kept = if active {
                false
            } else {
                let mut transfers = self
                    .shared
                    .transfers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                match transfers.get_mut(artifact_id) {
                    Some(transfer)
                        if transfer.operation_epoch == self.operation_epoch
                            && matches!(
                                transfer.state,
                                ArtifactState::Queued
                                    | ArtifactState::Downloading
                                    | ArtifactState::Verifying
                            ) =>
                    {
                        transfer.state = ArtifactState::Interrupted;
                        !transfer.ranges.is_empty() && !transfer.is_complete()
                    }
                    _ => false,
                }
            };
            if request.artifact_id == interrupted_artifact_id {
                kept |= request_kept;
            }
        }
        if kept {
            let _ = self
                .tasks
                .add_evidence(&self.task_id, evidence::RESUMABLE_RANGES);
        }
        let _ = self.tasks.cancel_now(&self.task_id, self.generation);
    }

    fn publish_progress(&self) {
        let payloads: Vec<Value> = {
            let transfers = self
                .shared
                .transfers
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            transfers
                .values()
                .map(|transfer| {
                    json!({
                        "artifactId": transfer.artifact_id,
                        "sourceId": transfer.source_id,
                        "state": transfer.state,
                        "downloaded": transfer.downloaded(),
                        "total": transfer.total,
                    })
                })
                .collect()
        };
        for payload in payloads {
            let _ = self.events.publish(
                EventRequest::new(
                    self.operation.clone(),
                    self.generation,
                    EventKind::from_static(events::progress()),
                )
                .with_payload(payload),
            );
        }
    }

    fn fail(&self, envelope: &ErrorEnvelope) {
        let _ = self
            .tasks
            .fail(&self.task_id, self.generation, envelope.clone());
    }
}

// ---------------------------------------------------------------------------
// request helpers
// ---------------------------------------------------------------------------

fn completed(result: Value) -> CommandResponse {
    CommandResponse::Completed { result }
}

fn require_operation(request: &CommandRequest) -> Result<OperationId, ErrorEnvelope> {
    request
        .operation_id
        .clone()
        .ok_or_else(ErrorEnvelope::command_operation_required)
}

fn optional_u64(request: &CommandRequest, key: &str) -> Option<u64> {
    request.payload.get(key).and_then(Value::as_u64)
}

fn required_str<'a>(
    request: &'a CommandRequest,
    key: &'static str,
) -> Result<&'a str, ErrorEnvelope> {
    request
        .payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| DownloadsError::InvalidRequest(key).to_envelope())
}

/// Read `payload.artifacts` (a list) or `payload.artifact` (one).
fn parse_requests(request: &CommandRequest) -> Result<Vec<ArtifactRequest>, ErrorEnvelope> {
    let items: Vec<&Value> = match request.payload.get("artifacts") {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(_) => return Err(DownloadsError::InvalidRequest("artifacts").to_envelope()),
        None => match request.payload.get("artifact") {
            Some(value) => vec![value],
            None => return Err(DownloadsError::InvalidRequest("artifacts").to_envelope()),
        },
    };
    if items.len() > MAX_ARTIFACTS_PER_TASK {
        return Err(DownloadsError::InvalidRequest("artifacts").to_envelope());
    }

    items
        .into_iter()
        .map(|item| {
            let artifact_id = item
                .get("artifactId")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| DownloadsError::InvalidRequest("artifactId").to_envelope())?;
            let source_id = item
                .get("sourceId")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| DownloadsError::InvalidRequest("sourceId").to_envelope())?;
            let source_url = item
                .get("url")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let expected = match item.get("hash") {
                None | Some(Value::Null) => None,
                Some(hash) => {
                    let algorithm = hash
                        .get("algorithm")
                        .and_then(Value::as_str)
                        .unwrap_or("sha256");
                    let digest = hash.get("digest").and_then(Value::as_str).unwrap_or("");
                    Some(ArtifactHash::new(algorithm, digest).map_err(|e| e.to_envelope())?)
                }
            };
            Ok(ArtifactRequest {
                artifact_id: artifact_id.to_string(),
                expected,
                source_id: source_id.to_string(),
                source_url,
                size: item.get("size").and_then(Value::as_u64),
            })
        })
        .collect()
}

fn validate_artifact_requests(requests: &[ArtifactRequest]) -> Result<(), DownloadsError> {
    let mut by_id =
        BTreeMap::<&str, (&Option<ArtifactHash>, &str, &Option<String>, Option<u64>)>::new();
    for request in requests {
        match by_id.get(request.artifact_id.as_str()) {
            Some((hash, source, source_url, size))
                if *hash != &request.expected
                    || *source != request.source_id
                    || *source_url != &request.source_url
                    || *size != request.size =>
            {
                return Err(DownloadsError::IdempotencyConflict);
            }
            Some(_) => {}
            None => {
                by_id.insert(
                    &request.artifact_id,
                    (
                        &request.expected,
                        &request.source_id,
                        &request.source_url,
                        request.size,
                    ),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod queue_tests {
    use super::*;

    fn wait_admission_count(module: &DownloadsModule, count: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if module.shared.admission.lock().unwrap().len == count {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "admission count did not reach {count}"
            );
            std::thread::sleep(SLOT_WAIT_POLL);
        }
    }

    #[test]
    fn continuous_leader_requeues_behind_existing_source_waiter() {
        let shared = Shared::new();
        *shared.scheduler.lock().unwrap() = Scheduler::new(6, 1);
        let mut leader = AdmissionTicket::new(&shared, "source").unwrap();
        assert_eq!(leader.try_acquire(&shared, 0), Some(Slot::Granted));
        // Drive repeated artifact completion/re-entry synchronously: the old
        // leader always runs before the waiter gets its next scheduling turn.
        for _ in 0..256 {
            let waiter = AdmissionTicket::new(&shared, "source").unwrap();
            assert_eq!(waiter.try_acquire(&shared, 0), Some(Slot::SourceBusy));
            shared.scheduler.lock().unwrap().release("source");
            drop(leader);
            leader = AdmissionTicket::new(&shared, "source").unwrap();
            assert_eq!(
                leader.try_acquire(&shared, 0),
                None,
                "leader overtook queued waiter"
            );
            assert_eq!(waiter.try_acquire(&shared, 0), Some(Slot::Granted));
            assert_eq!(shared.scheduler.lock().unwrap().active_total(), 1);
            shared.scheduler.lock().unwrap().release("source");
            assert_eq!(leader.try_acquire(&shared, 0), Some(Slot::Granted));
        }
        shared.scheduler.lock().unwrap().release("source");
        drop(leader);
        assert_eq!(shared.admission.lock().unwrap().len, 0);
        assert!(shared.admission.lock().unwrap().sources.is_empty());
    }

    #[test]
    fn blocked_source_does_not_block_other_source_and_ticket_count_is_bounded() {
        let shared = Shared::new();
        let first = AdmissionTicket::new(&shared, "a").unwrap();
        let second = AdmissionTicket::new(&shared, "a").unwrap();
        assert_eq!(second.try_acquire(&shared, 0), None);
        let other = AdmissionTicket::new(&shared, "b").unwrap();
        assert_eq!(other.try_acquire(&shared, 0), Some(Slot::Granted));
        shared.scheduler.lock().unwrap().release("b");
        let extras: Vec<_> = (0..4)
            .map(|_| AdmissionTicket::new(&shared, "a").unwrap())
            .collect();
        assert!(matches!(
            AdmissionTicket::new(&shared, "a"),
            Err(DownloadsError::Internal)
        ));
        drop(first);
        assert_eq!(second.try_acquire(&shared, 0), Some(Slot::Granted));
        shared.scheduler.lock().unwrap().release("a");
        drop(extras);
        drop(second);
        drop(other);
        assert_eq!(shared.admission.lock().unwrap().len, 0);
    }

    #[test]
    fn cancelling_source_queue_head_allows_next_worker_to_complete() {
        let (_app, module) = admission_fixture();
        let first = submit_admission(&module, 0);
        wait_admission_count(&module, 1);
        let second = submit_admission(&module, 1);
        wait_admission_count(&module, 2);
        let task = module.task_service().snapshot(&first).unwrap();
        module
            .task_service()
            .request_cancel(&shtmcl_domain::CancelTaskRequest {
                task_id: first.clone(),
                attempt_generation: task.attempt_generation,
            });
        assert_eq!(wait_terminal(&module, &first), TaskState::Cancelled);
        wait_admission_count(&module, 1);
        {
            let mut scheduler = module.shared.scheduler.lock().unwrap();
            for _ in 0..3 {
                scheduler.release("same-source");
            }
        }
        assert_eq!(wait_terminal(&module, &second), TaskState::Succeeded);
        wait_admission_count(&module, 0);
        assert!(module
            .task_service()
            .snapshot(&first)
            .unwrap()
            .error
            .is_none());
        assert_eq!(module.shared.scheduler.lock().unwrap().active_total(), 0);
    }

    fn admission_fixture() -> (crate::CoreApp, DownloadsModule) {
        use crate::downloads::ports::{FixtureClock, FixtureDisk, FixtureDownloads};
        let app = crate::CoreApp::new(shtmcl_platform::PlatformDirs::under(
            std::env::temp_dir().join("shtmcl-slot-admission"),
        ));
        let mut transport = FixtureDownloads::new();
        for index in 0..6 {
            transport =
                transport.with_artifact(&format!("admission-{index}"), 4, &format!("{index:040x}"));
        }
        let module = DownloadsModule::new(
            app.context(),
            Arc::new(transport),
            Arc::new(FixtureDisk { free: 1024 }),
            Arc::new(FixtureClock::new(vec![0])),
        );
        // Hold all three source slots so every submitted worker must queue.
        let mut scheduler = module.shared.scheduler.lock().unwrap();
        for _ in 0..3 {
            assert_eq!(scheduler.try_acquire("same-source", 0), Slot::Granted);
        }
        drop(scheduler);
        (app, module)
    }

    fn submit_admission(module: &DownloadsModule, index: usize) -> shtmcl_domain::TaskId {
        let id = format!("admission-{index}");
        let response = module
            .handle(&CommandRequest {
                request_id: shtmcl_domain::RequestId::new(id.clone()).unwrap(),
                operation_id: Some(OperationId::new(id.clone()).unwrap()),
                entity_id: None,
                expected_revision: None,
                payload: json!({"command":"downloads.fetch","artifacts":[{
                    "artifactId":id,"sourceId":"same-source","size":4,
                    "hash":{"algorithm":"sha1","digest":format!("{index:040x}")}
                }]}),
            })
            .unwrap();
        let CommandResponse::Accepted { task_id, .. } = response else {
            panic!("expected task")
        };
        task_id
    }

    fn wait_terminal(module: &DownloadsModule, id: &shtmcl_domain::TaskId) -> TaskState {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let task = module.task_service().snapshot(id).unwrap();
            if matches!(
                task.state,
                TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
            ) {
                return task.state;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "task did not settle: {task:?}"
            );
            std::thread::sleep(SLOT_WAIT_POLL);
        }
    }

    #[test]
    fn six_same_source_workers_wait_for_capacity_then_complete() {
        let (_app, module) = admission_fixture();
        let tasks: Vec<_> = (0..6)
            .map(|index| submit_admission(&module, index))
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(80));
        for id in &tasks {
            assert!(!matches!(
                module.task_service().snapshot(id).unwrap().state,
                TaskState::Failed
            ));
        }
        assert!(
            module.shared.jobs.lock().unwrap().is_empty(),
            "waiting workers published owners"
        );
        {
            let mut scheduler = module.shared.scheduler.lock().unwrap();
            for _ in 0..3 {
                scheduler.release("same-source");
            }
        }
        for id in &tasks {
            assert_eq!(wait_terminal(&module, id), TaskState::Succeeded);
        }
        assert_eq!(module.shared.scheduler.lock().unwrap().active_total(), 0);
    }

    #[test]
    fn failed_batch_preserves_an_active_shared_writer_with_the_same_epoch() {
        let shared = Shared::new();
        let request = ArtifactRequest {
            artifact_id: "same-epoch".into(),
            expected: Some(ArtifactHash::new("sha1", &"a".repeat(40)).unwrap()),
            source_id: "source".into(),
            source_url: None,
            size: Some(4),
        };
        let mut transfer = Transfer::new(&request, 1);
        transfer.operation_epoch = 42;
        transfer.state = ArtifactState::Queued;
        shared
            .transfers
            .lock()
            .unwrap()
            .insert(request.artifact_id.clone(), transfer);
        let key = request.expected.as_ref().unwrap().key();
        let job = Arc::new(SharedJob {
            state: Mutex::new(SharedJobState {
                subscribers: 2,
                outcome: None,
            }),
            changed: Condvar::new(),
        });
        shared.jobs.lock().unwrap().insert(key, job.clone());
        shared.retire_failed_transfers(42);
        assert_eq!(
            shared.transfers.lock().unwrap()["same-epoch"].state,
            ArtifactState::Queued
        );
        job.state.lock().unwrap().outcome = Some(Err(DownloadsError::Cancelled));
        shared.retire_failed_transfers(41);
        assert_eq!(
            shared.transfers.lock().unwrap()["same-epoch"].state,
            ArtifactState::Queued
        );
        shared.retire_failed_transfers(42);
        assert_eq!(
            shared.transfers.lock().unwrap()["same-epoch"].state,
            ArtifactState::Failed
        );
    }

    #[test]
    fn cancelled_batch_releases_unprocessed_transfer_epochs_for_retry() {
        let (_app, module) = admission_fixture();
        let submit = |operation: &str| {
            let request = CommandRequest {
                request_id: shtmcl_domain::RequestId::new(operation).unwrap(),
                operation_id: Some(OperationId::new(operation).unwrap()),
                entity_id: None,
                expected_revision: None,
                payload: json!({"command":"downloads.fetch", "artifacts":(0..2).map(|index| json!({
                    "artifactId":format!("admission-{index}"),"sourceId":"same-source","size":4,
                    "hash":{"algorithm":"sha1","digest":format!("{index:040x}")}
                })).collect::<Vec<_>>()}),
            };
            let CommandResponse::Accepted { task_id, .. } = module.handle(&request).unwrap() else {
                panic!("accepted")
            };
            task_id
        };
        let first = submit("cancelled-multi");
        wait_admission_count(&module, 1);
        let task = module.task_service().snapshot(&first).unwrap();
        module
            .task_service()
            .request_cancel(&shtmcl_domain::CancelTaskRequest {
                task_id: first.clone(),
                attempt_generation: task.attempt_generation,
            });
        assert_eq!(wait_terminal(&module, &first), TaskState::Cancelled);
        for transfer in module.shared.transfers.lock().unwrap().values() {
            assert_eq!(transfer.state, ArtifactState::Interrupted);
        }
        {
            let mut scheduler = module.shared.scheduler.lock().unwrap();
            for _ in 0..3 {
                scheduler.release("same-source");
            }
        }
        assert_eq!(
            wait_terminal(&module, &submit("retry-multi")),
            TaskState::Succeeded
        );
    }

    #[test]
    fn source_slot_wait_observes_cancellation_without_a_transport_call() {
        let (_app, module) = admission_fixture();
        let id = submit_admission(&module, 0);
        std::thread::sleep(std::time::Duration::from_millis(80));
        let task = module.task_service().snapshot(&id).unwrap();
        module
            .task_service()
            .request_cancel(&shtmcl_domain::CancelTaskRequest {
                task_id: id.clone(),
                attempt_generation: task.attempt_generation,
            });
        assert_eq!(wait_terminal(&module, &id), TaskState::Cancelled);
        assert!(module.task_service().snapshot(&id).unwrap().error.is_none());
        assert!(module.shared.jobs.lock().unwrap().is_empty());
    }

    #[test]
    fn worker_pool_and_pending_queue_have_fixed_upper_bounds() {
        let pool = WorkerPool::new(DEFAULT_TOTAL_CONCURRENCY, 2);
        assert_eq!(
            pool.threads.lock().unwrap_or_else(|p| p.into_inner()).len(),
            DEFAULT_TOTAL_CONCURRENCY
        );
        let first = pool.reserve().expect("first slot");
        let second = pool.reserve().expect("second slot");
        assert!(
            pool.reserve().is_none(),
            "queue exceeded configured capacity"
        );
        drop(first);
        assert!(
            pool.reserve().is_some(),
            "released reservation was not reusable"
        );
        drop(second);
    }
}

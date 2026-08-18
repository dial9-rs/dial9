use super::source::{FlushContext, Source};
#[cfg(not(tokio_unstable))]
use crate::primitives::sync::Weak;
use crate::primitives::sync::{Arc, Mutex};
use crate::telemetry::encoder::{Encodable, ThreadLocalEncoder};
use crate::telemetry::events::{SchedStat, clock_monotonic_ns};
use crate::telemetry::format::{
    PollEndEvent, PollStartEvent, RuntimeMetricsEvent, WorkerId, WorkerParkEvent, WorkerUnparkEvent,
};
#[cfg(tokio_unstable)]
use crate::telemetry::format::{TaskSpawnEvent, TaskTerminateEvent};
use crate::telemetry::task_metadata::TaskId;
use dial9_core::handle::{Dial9Handle, set_tl_handle};
use metrique_timesource::{Instant, time_source};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::runtime::RuntimeMetrics;

/// Per-runtime state captured at hook registration time.
///
/// All tokio-specific concepts live here rather than in `SharedState`.
/// Each `RuntimeContext` belongs to exactly one tokio runtime.
pub(crate) struct RuntimeContext {
    /// Identity for the per-thread worker caches.
    id: u64,
    /// The tokio runtime this context belongs to, set once the runtime is
    /// built.
    runtime_id: OnceLock<tokio::runtime::Id>,
    /// Optional human-readable name, set via `with_runtime_name`.
    pub runtime_name: Option<String>,
    /// Installed on each of this runtime's threads (TL) so
    /// `Dial9Handle::current()`, the tracing layer, and `dial9::spawn` resolve.
    pub recorder_handle: Dial9Handle,
    /// Global worker-ID counter, shared with every other runtime on this
    /// recorder so their blocks never overlap.
    worker_id_counter: WorkerIdCounter,
    /// Base worker ID for this runtime, reserved on the first worker resolve.
    #[cfg(tokio_unstable)]
    pub worker_id_base: OnceLock<u64>,
    /// Global worker IDs within this runtime.
    /// Populated lazily the first time each worker thread resolves its identity.
    pub worker_ids: Mutex<BTreeSet<u64>>,
}

thread_local! {
    /// Global worker ID for this thread, set on every `resolve_worker` call.
    /// Read by `current_worker_id()` for wake events.
    static GLOBAL_WORKER_ID: Cell<Option<u64>> = const { Cell::new(None) };
    /// Worker IDs claimed by this thread, keyed by `RuntimeContext::id`.
    ///
    /// The fallback path assigns IDs per thread-runtime pair. Retaining previous
    /// claims lets a thread reuse its ID when it returns to a runtime instead of
    /// reserving a new ID after every runtime switch.
    #[cfg(not(tokio_unstable))]
    static CLAIMED_WORKER_IDS: RefCell<Vec<(u64, u64)>> = const { RefCell::new(Vec::new()) };
    /// The runtime and worker ID this thread last enrolled with, so a thread
    /// that also drives a second runtime still joins that runtime's worker set
    /// instead of short-circuiting as already enrolled.
    static WORKER_REGISTERED: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
    /// The context this thread last resolved. Every traced task resolves on
    /// its first poll, and resolving takes the recorder's source lock.
    ///
    /// `Weak` because this cache is never cleared, and a thread can outlive
    /// the runtime it served.
    #[cfg(not(tokio_unstable))]
    static RUNTIME_CTX_CACHE: RefCell<Option<(tokio::runtime::Id, Weak<RuntimeContext>)>> =
        const { RefCell::new(None) };
    /// Keeps this thread enrolled with the recorder's per-thread sources.
    /// Dropped by the runtime's `on_thread_stop` hook, or by the TLS destructor
    /// for threads that get none (a `current_thread` runtime's driver).
    #[cfg(feature = "cpu-profiling")]
    static THREAD_TRACKING: RefCell<Option<dial9_core::thread::ThreadTrackingGuard>> =
        const { RefCell::new(None) };
    /// Whether this thread has tried to enroll. Set even when enrollment fails,
    /// so a thread the sources refuse is not retried on every poll.
    #[cfg(feature = "cpu-profiling")]
    static TRACKING_ATTEMPTED: Cell<bool> = const { Cell::new(false) };
    /// Monotonic timestamp captured in `on_before_task_poll`, cleared in
    /// `on_after_task_poll`. Allows code running inside a poll (e.g.
    /// `TaskDumped`, memory profiler) to reuse the timestamp without an extra
    /// clock read.
    static POLL_START_TS: Cell<Option<NonZeroU64>> = const { Cell::new(None) };
    /// Last timestamp returned by `poll_start_ts_monotonic`. Ensures strictly
    /// increasing values within a thread by bumping +1ns on ties.
    static LAST_TS: Cell<u64> = const { Cell::new(0) };
}

thread_local! {
    /// This worker's [`RuntimeMetrics`], cached so queue-depth reads on the poll
    /// and park paths cost only a thread-local read.
    static WORKER_METRICS: RefCell<Option<RuntimeMetrics>> = const { RefCell::new(None) };
}

/// Read this worker's runtime metrics. Call only from a Tokio runtime thread,
/// which every hook path already is.
#[cfg(tokio_unstable)]
fn worker_metrics<R>(f: impl FnOnce(&RuntimeMetrics) -> R) -> R {
    WORKER_METRICS.with(|cell| {
        let mut slot = cell.borrow_mut();
        f(slot.get_or_insert_with(|| tokio::runtime::Handle::current().metrics()))
    })
}

/// This thread's global worker ID, claiming one if it has none yet.
///
/// Without `worker_index()` there is no slot to offset from, so IDs follow
/// first-touch order rather than tokio's numbering. One at a time rather than a
/// `num_workers()`-sized block: `block_in_place` hands a worker's core to a
/// blocking-pool thread that then fires the park hooks, so the thread count is
/// unbounded.
///
/// Keyed by runtime: a thread that drives a second runtime claims a fresh ID
/// there rather than reusing the one it holds in another runtime's block.
#[cfg(not(tokio_unstable))]
fn claim_thread_worker_id(ctx: &RuntimeContext) -> Option<u64> {
    CLAIMED_WORKER_IDS.with(|claimed| {
        let mut claimed = claimed.borrow_mut();
        if let Some((_, id)) = claimed.iter().find(|(owner, _)| *owner == ctx.id) {
            return Some(*id);
        }
        let id = ctx.reserve_worker_ids(1);
        claimed.push((ctx.id, id));
        Some(id)
    })
}

/// Local queue depth for the current worker.
///
/// Reports `0` when tokio does not expose per-worker queue depth.
/// Consumers tell the two apart via the `tokio.local_queue` segment-metadata
/// key, which is `false` exactly when this can only return `0`.
fn current_local_queue_depth() -> usize {
    #[cfg(tokio_unstable)]
    {
        match tokio::runtime::worker_index() {
            Some(idx) => worker_metrics(|m| m.worker_local_queue_depth(idx)),
            None => 0,
        }
    }
    #[cfg(not(tokio_unstable))]
    {
        0
    }
}

crate::primitives::thread_local! {
    /// schedstat wait_time_ns captured at park time, used to compute delta on unpark.
    static PARKED_SCHED_WAIT: Cell<u64> = const { Cell::new(0) };
    /// Per-thread park counter used to sample `SchedStat::read_current`. See
    /// [`sched_wait_sample_rate`]: schedstat is only read on 1-in-N parks to
    /// bound the CPU cost of reading `/proc/self/task/<tid>/schedstat`.
    static PARK_COUNTER: Cell<u64> = const { Cell::new(0) };
    /// Whether the current park cycle successfully read schedstat at park time.
    /// Unpark only computes a wait-time delta when this is `true`, guaranteeing
    /// every reported `sched_wait_ns` comes from a matched park->unpark pair.
    static SCHED_SAMPLED_THIS_PARK: Cell<bool> = const { Cell::new(false) };
}

/// How often to read `SchedStat::read_current` in the worker park/unpark path.
///
/// Reading `/proc/self/task/<tid>/schedstat` on every park is measurable CPU
/// overhead, and there is no need to catch every scheduler pause: periodic
/// sampling still surfaces scheduling latency. We therefore only read schedstat
/// on 1-in-N parks. `N` defaults to 10 and is overridable via the
/// `DIAL9_SCHED_WAIT_SAMPLE_RATE` environment variable (values are clamped to at
/// least 1; `1` restores read-on-every-park).
fn sched_wait_sample_rate() -> u64 {
    static RATE: OnceLock<u64> = OnceLock::new();
    *RATE.get_or_init(|| {
        std::env::var("DIAL9_SCHED_WAIT_SAMPLE_RATE")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(10)
            .max(1)
    })
}

/// Advance a per-thread park counter and decide whether this park should read
/// schedstat. Samples every `rate`th park (`rate` is clamped to at least 1, so
/// `rate == 1` samples every park). Returns the incremented counter and the
/// sampling decision. Pure so the 1-in-N logic can be unit-tested without the
/// process-global rate or a live schedstat read.
fn advance_park_counter(counter: u64, rate: u64) -> (u64, bool) {
    let next = counter.wrapping_add(1);
    (next, next.is_multiple_of(rate.max(1)))
}

/// Returns a strictly monotonic timestamp for this thread.
///
/// Returns the cached `PollStart` timestamp from this thread's most
/// recent `on_before_task_poll`, if any; otherwise reads the wall
/// clock via [`crate::telemetry::events::clock_monotonic_ns`]. The
/// returned value is always **strictly greater** than the previous
/// call on this thread (bumps by 1 ns on ties), which keeps event
/// ordering correct when several samples share a clock tick — e.g.
/// an in-place realloc producing free + alloc at the same address
/// within one poll, or repeated allocations inside a tight loop.
///
/// Used by:
/// - the task-dump idle/wake bookkeeping in [`crate::task_dumped`].
#[cfg(any(feature = "taskdump", test))]
pub(crate) fn poll_start_ts_monotonic() -> u64 {
    let raw = POLL_START_TS.with(|c| c.get()).map_or_else(
        crate::telemetry::events::clock_monotonic_ns,
        NonZeroU64::get,
    );
    LAST_TS.with(|last| {
        let next = last.get().wrapping_add(1).max(raw);
        last.set(next);
        next
    })
}

/// Shared list of all attached runtimes.
pub(crate) type RuntimeContextRegistry = Arc<Mutex<Vec<Arc<RuntimeContext>>>>;

/// Flush-thread [`Source`] over all tokio runtimes. Each cycle it samples the
/// summed global queue depth across runtimes and contributes each runtime's
/// runtime->worker segment metadata.
pub(crate) struct TokioRuntimesSource {
    contexts: RuntimeContextRegistry,
    /// Metrics for each attached runtime, paired with the runtime's name, handed
    /// over once the caller has built it. The flush thread has no runtime context
    /// of its own, so it cannot ask Tokio for these itself. The name is carried
    /// alongside so each runtime's sample can be tagged with its identity.
    /// Dropped with the source at recorder teardown.
    runtime_metrics: Vec<(Option<String>, RuntimeMetrics)>,
    last_sample: Instant,
    sample_interval: Duration,
    /// Fingerprint of the metadata emitted on the last `segment_metadata` call,
    /// used to skip the rebuild when nothing changed. See `segment_metadata` for
    /// what it is and why it is sufficient. `0` means "nothing emitted yet".
    last_fingerprint: usize,
    /// Whether the process-fixed metadata entries have been emitted yet. They
    /// never change, so they are emitted exactly once (the writer keeps them in
    /// its merged cache and re-emits them on every rotation).
    fixed_metadata_emitted: bool,
    /// Next unclaimed global worker ID, shared with every [`RuntimeContext`] on
    /// this recorder. Handing each context a clone at attach keeps the claim
    /// path off the source lock.
    next_worker_id: WorkerIdCounter,
}

/// Hands out the global worker IDs for one recorder.
///
/// Every runtime attached to a recorder shares one counter, which is what keeps
/// their worker-ID blocks from overlapping.
pub(crate) type WorkerIdCounter = Arc<AtomicU64>;

impl TokioRuntimesSource {
    pub(crate) fn new(contexts: RuntimeContextRegistry) -> Self {
        Self {
            contexts,
            runtime_metrics: Vec::new(),
            last_sample: time_source().instant(),
            sample_interval: Duration::from_millis(10),
            last_fingerprint: 0,
            fixed_metadata_emitted: false,
            next_worker_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The registry of attached runtimes. `attach_tokio_runtime` shares it so every
    /// runtime on a recorder lands in the same source.
    pub(crate) fn registry(&self) -> &RuntimeContextRegistry {
        &self.contexts
    }

    /// This recorder's worker-ID counter, for a context being attached.
    pub(crate) fn worker_id_counter(&self) -> WorkerIdCounter {
        self.next_worker_id.clone()
    }
}

/// Give the source the metrics of a freshly built runtime (paired with its
/// name), so the flush thread can sample this runtime's scheduler metrics and
/// tag the sample with the runtime's identity.
///
/// Called once per attach, after the caller's runtime exists.
pub(crate) fn register_runtime_metrics(
    handle: &Dial9Handle,
    runtime_name: Option<String>,
    metrics: RuntimeMetrics,
) {
    let registered = handle.with_source(|source: &mut TokioRuntimesSource| {
        source.runtime_metrics.push((runtime_name, metrics));
    });
    if registered.is_none() {
        tracing::warn!("Tokio source missing; queue depth will not be sampled");
    }
}

impl Source for TokioRuntimesSource {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        if self.last_sample.elapsed() < self.sample_interval {
            return;
        }
        self.last_sample = time_source().instant();
        if self.runtime_metrics.is_empty() {
            return;
        }
        // One sample per runtime, tagged with its identity, so a consumer can
        // attribute a backlog to a specific runtime rather than a summed total.
        // Share one timestamp across the cycle so consumers can group a cycle's
        // per-runtime samples (e.g. to sum them back into a process-wide total).
        // The name is interned lazily in `RuntimeMetricsSample::encode`, so each
        // event costs one `u32` handle rather than a fresh `String`.
        let timestamp_ns = clock_monotonic_ns();
        for (runtime_name, metrics) in &self.runtime_metrics {
            ctx.record_event(&RuntimeMetricsSample {
                timestamp_ns,
                runtime_name: runtime_name.as_deref().unwrap_or(""),
                global_queue_depth: metrics.global_queue_depth() as u32,
                alive_tasks: metrics.num_alive_tasks() as u32,
            });
        }
    }

    fn name(&self) -> &'static str {
        "tokio_runtimes"
    }

    fn segment_metadata(&mut self, out: &mut Vec<(String, String)>) {
        // Record the schedstat sampling rate once so a consumer reading
        // `WorkerUnparkEvent::sched_wait_ns` knows the measurement represents
        // roughly 1-in-N parks, not every park. Fixed for the process lifetime
        // (the rate is read once via `OnceLock`), so emit it a single time; the
        // writer keeps it in its merged cache and re-emits it on every rotation.
        if !self.fixed_metadata_emitted {
            out.push((
                "sched.wait_sample_rate".to_string(),
                sched_wait_sample_rate().to_string(),
            ));
            // `dial9-spawns-only` means poll events cover just the tasks spawned through dial9's own helpers,
            // not every task on the runtime.
            out.push((
                "tokio.poll_coverage".to_string(),
                if cfg!(tokio_unstable) {
                    "all".to_string()
                } else {
                    "dial9-spawns-only".to_string()
                },
            ));
            // `false` means `local_queue` on every event is a sentinel 0, not a
            // measurement.
            out.push((
                "tokio.local_queue".to_string(),
                cfg!(tokio_unstable).to_string(),
            ));
            // How the trace was built. Kept for diagnostics: the fix for thin
            // poll data is a build flag, not a trace setting.
            out.push((
                "tokio.unstable".to_string(),
                cfg!(tokio_unstable).to_string(),
            ));
            self.fixed_metadata_emitted = true;
        }

        // Self-detected change: there is no external signal to keep in sync, so
        // a new caller that mutates runtime/worker metadata cannot forget to
        // announce it. The fingerprint is the runtime count plus the total
        // number of registered workers across all runtimes. Both only ever grow
        // (runtimes and workers are added, never removed) and each worker's
        // global id is fixed once assigned, so an unchanged fingerprint means
        // unchanged metadata. Cheap — a few uncontended read locks and no
        // allocation — so it runs every flush cycle.
        let contexts = self.contexts.lock().unwrap();
        let fingerprint = contexts.len()
            + contexts
                .iter()
                .map(|c| c.worker_ids.lock().unwrap().len())
                .sum::<usize>();
        if fingerprint == self.last_fingerprint {
            return;
        }
        self.last_fingerprint = fingerprint;
        // The writer's merge is additive, so emitting the full current snapshot
        // on each change is correct. A fingerprint bump from an unnamed runtime
        out.extend(contexts.iter().filter_map(|c| c.metadata_entry()));
    }
}

impl RuntimeContext {
    pub(crate) fn new(
        runtime_name: Option<String>,
        recorder_handle: Dial9Handle,
        worker_id_counter: WorkerIdCounter,
    ) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            runtime_id: OnceLock::new(),
            runtime_name,
            recorder_handle,
            worker_id_counter,
            #[cfg(tokio_unstable)]
            worker_id_base: OnceLock::new(),
            worker_ids: Mutex::new(BTreeSet::new()),
        }
    }

    /// Record an event for this runtime, if recording is on.
    ///
    /// Lazy on purpose: building one of these events can resolve the worker and
    /// read the clock, which a paused recorder must not pay for.
    fn record<E: Encodable>(&self, make: impl FnOnce() -> E) {
        self.recorder_handle.record_event_with(make);
    }

    pub(crate) fn record_worker_park(&self) {
        self.record(|| make_worker_park(self));
    }

    pub(crate) fn record_worker_unpark(&self) {
        self.record(|| make_worker_unpark(self));
    }

    pub(crate) fn record_poll_start(
        &self,
        location: &'static std::panic::Location<'static>,
        task_id: TaskId,
    ) {
        self.record(|| make_poll_start(self, location, task_id));
    }

    pub(crate) fn record_poll_end(&self) {
        self.record(|| make_poll_end(self));
    }

    #[cfg(tokio_unstable)]
    pub(crate) fn record_task_spawn(
        &self,
        location: &'static std::panic::Location<'static>,
        task_id: TaskId,
        instrumented: bool,
    ) {
        self.record(|| TaskSpawn {
            timestamp_ns: clock_monotonic_ns(),
            task_id,
            location,
            instrumented,
        });
    }

    #[cfg(tokio_unstable)]
    pub(crate) fn record_task_terminate(&self, task_id: TaskId) {
        self.record(|| TaskTerminateEvent {
            timestamp_ns: clock_monotonic_ns(),
            task_id,
        });
    }

    /// Bind this context to the runtime it instruments, once that runtime is
    /// built. Idempotent: the first binding wins.
    pub(crate) fn bind_runtime(&self, id: tokio::runtime::Id) {
        let _ = self.runtime_id.set(id);
    }

    /// Whether this context instruments the runtime with `id`.
    #[cfg(not(tokio_unstable))]
    pub(crate) fn is_runtime(&self, id: tokio::runtime::Id) -> bool {
        self.runtime_id.get() == Some(&id)
    }

    /// Build segment metadata entries for this runtime, e.g. `("runtime.main", "0,1,2,3")`.
    /// Returns `None` if unnamed or no workers resolved yet.
    pub(crate) fn metadata_entry(&self) -> Option<(String, String)> {
        let name = self.runtime_name.as_deref()?;
        let ids = self.worker_ids.lock().unwrap();
        if ids.is_empty() {
            return None;
        }
        let csv = ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        Some((format!("runtime.{name}"), csv))
    }

    /// Resolve the current thread's global worker ID.
    ///
    /// Called from the runtime hooks, where a scheduler context is current.
    /// `None` means no worker context (shouldn't happen from a hook); skip
    /// rather than misattribute to worker 0.
    fn resolve_worker(&self) -> Option<WorkerId> {
        #[cfg(tokio_unstable)]
        let global_id = self.claim_worker_id()?;
        #[cfg(not(tokio_unstable))]
        let global_id = claim_thread_worker_id(self)?;

        // Always update TLS so current_worker_id() returns the global ID.
        GLOBAL_WORKER_ID.with(|cell| cell.set(Some(global_id)));

        enroll_thread(self, global_id);

        Some(WorkerId::from(global_id as usize))
    }

    /// Reserve a block of `count` global worker IDs, returning the first.
    fn reserve_worker_ids(&self, count: u64) -> u64 {
        self.worker_id_counter.fetch_add(count, Ordering::Relaxed)
    }

    /// This thread's global worker ID within this runtime.
    ///
    /// Tokio's `worker_index()` gives the runtime-local slot (0 for a
    /// `current_thread` runtime's driver thread), so the whole runtime's IDs
    /// can be reserved as one contiguous block on the first resolve.
    #[cfg(tokio_unstable)]
    fn claim_worker_id(&self) -> Option<u64> {
        let local_index = tokio::runtime::worker_index()?;
        // `get_or_init` runs its closure exactly once, so the block is reserved
        // once however many workers resolve at the same moment.
        let base = self.worker_id_base.get_or_init(|| {
            let num_workers = worker_metrics(|m| m.num_workers()) as u64;
            self.reserve_worker_ids(num_workers)
        });
        Some(base + local_index as u64)
    }
}

/// Everything a thread does the first time it proves it belongs to a runtime.
fn enroll_thread(ctx: &RuntimeContext, global_id: u64) {
    register_worker_if_needed(ctx, global_id);
    #[cfg(feature = "cpu-profiling")]
    start_sched_sampling_if_needed(&ctx.recorder_handle);
}

/// Record global_id in the context's set (once per thread).
///
/// No need to announce the metadata change: `TokioRuntimesSource` detects the
/// new worker from the worker count on its next flush.
fn register_worker_if_needed(ctx: &RuntimeContext, global_id: u64) {
    let key = (ctx.id, global_id);
    WORKER_REGISTERED.with(|cell| {
        if cell.get() != Some(key) {
            ctx.worker_ids.lock().unwrap().insert(global_id);
            // Install the recorder handle on this thread. `on_thread_start` also
            // does this for pool threads, but a `current_thread` runtime's driver
            // thread gets no `on_thread_start`, so set it here on first poll.
            set_tl_handle(ctx.recorder_handle.clone());
            cell.set(Some(key));
        }
    });
}

/// Enroll this worker thread with the per-thread sources (once per thread).
///
/// Deferred from `on_thread_start` so that only worker threads, not blocking
/// pool threads, open perf fds.
#[cfg(feature = "cpu-profiling")]
fn start_sched_sampling_if_needed(handle: &Dial9Handle) {
    if TRACKING_ATTEMPTED.with(|attempted| attempted.replace(true)) {
        return;
    }
    match handle.track_current_thread() {
        Ok(guard) => THREAD_TRACKING.with(|cell| *cell.borrow_mut() = Some(guard)),
        Err(e) => tracing::warn!("failed to profile worker thread: {e}"),
    }
}

/// Stop tracking this thread. Called from the runtime's `on_thread_stop` hook.
#[cfg(feature = "cpu-profiling")]
pub(crate) fn stop_sched_sampling() {
    // `try_with` only fails once TLS teardown has started, and teardown drops
    // the guard itself.
    let _ = THREAD_TRACKING.try_with(|cell| cell.borrow_mut().take());
}

/// Get the current thread's global worker ID.
///
/// Returns [`WorkerId::UNKNOWN`] if called from a thread that has not yet
/// been claimed by a dial9-traced runtime (e.g., before the first poll or
/// from a non-runtime thread).
///
/// This is a thread-local read with no synchronization overhead.
pub fn current_worker_id() -> WorkerId {
    GLOBAL_WORKER_ID.with(|cell| cell.get().map_or(WorkerId::UNKNOWN, WorkerId))
}

// ── Event construction helpers ───────────────────────────────────────────────

/// Tokio-side intermediate for a `RuntimeMetricsEvent`. Holds the runtime name
/// as a borrowed `&str` so interning happens lazily inside
/// [`Encodable::encode`], against the thread-local encoder's string pool. This
/// lets the flush loop emit each cycle without cloning the name.
pub(super) struct RuntimeMetricsSample<'a> {
    pub timestamp_ns: u64,
    pub runtime_name: &'a str,
    pub global_queue_depth: u32,
    pub alive_tasks: u32,
}

impl Encodable for RuntimeMetricsSample<'_> {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let runtime_name = enc.intern_string(self.runtime_name);
        enc.encode(&RuntimeMetricsEvent {
            timestamp_ns: self.timestamp_ns,
            runtime_name,
            global_queue_depth: self.global_queue_depth,
            alive_tasks: self.alive_tasks,
        });
    }
}

/// Tokio-side intermediate for a `PollStartEvent`. Holds the raw
/// `&'static Location` so that interning happens lazily inside
/// [`Encodable::encode`], against the thread-local encoder's string pool.
///
/// Going through [`Encodable`] lets the hook closure use the public
/// [`record_event`](crate::telemetry::record_event) API uniformly for all
/// event kinds.
pub(crate) struct PollStart {
    pub timestamp_ns: u64,
    pub worker_id: WorkerId,
    pub local_queue: u8,
    pub task_id: TaskId,
    pub location: &'static std::panic::Location<'static>,
}

impl Encodable for PollStart {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let spawn_loc = enc.intern_location(self.location);
        enc.encode(&PollStartEvent {
            timestamp_ns: self.timestamp_ns,
            worker_id: self.worker_id,
            local_queue: self.local_queue,
            task_id: self.task_id,
            spawn_loc,
        });
    }
}

/// Tokio-side intermediate for a `TaskSpawnEvent`. See [`PollStart`] for
/// rationale.
#[cfg(tokio_unstable)]
pub(super) struct TaskSpawn {
    pub timestamp_ns: u64,
    pub task_id: TaskId,
    pub location: &'static std::panic::Location<'static>,
    pub instrumented: bool,
}

#[cfg(tokio_unstable)]
impl Encodable for TaskSpawn {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let spawn_loc = enc.intern_location(self.location);
        enc.encode(&TaskSpawnEvent {
            timestamp_ns: self.timestamp_ns,
            task_id: self.task_id,
            spawn_loc,
            instrumented: self.instrumented,
        });
    }
}

/// The worker id to stamp on an event this thread is about to record.
///
/// `UNKNOWN` when the thread has no worker context, rather than misattributing
/// the event to worker 0.
fn event_worker_id(ctx: &RuntimeContext) -> WorkerId {
    ctx.resolve_worker().unwrap_or(WorkerId::UNKNOWN)
}

/// Poll-start event, from tokio's `on_before_task_poll` hook when it exists and
/// from the `TracedFuture` wrapper when it does not.
fn make_poll_start(
    ctx: &RuntimeContext,
    location: &'static std::panic::Location<'static>,
    task_id: TaskId,
) -> PollStart {
    let worker_id = event_worker_id(ctx);
    let worker_local_queue_depth = current_local_queue_depth();
    let timestamp_ns = clock_monotonic_ns();
    POLL_START_TS.with(|c| c.set(NonZeroU64::new(timestamp_ns)));
    PollStart {
        timestamp_ns,
        worker_id,
        local_queue: worker_local_queue_depth as u8,
        task_id,
        location,
    }
}

/// Poll-end counterpart to [`make_poll_start`].
fn make_poll_end(ctx: &RuntimeContext) -> PollEndEvent {
    POLL_START_TS.with(|c| c.set(None));
    PollEndEvent {
        timestamp_ns: clock_monotonic_ns(),
        worker_id: event_worker_id(ctx),
    }
}

/// This thread's cached context for `runtime`, if it resolved one before and
/// that runtime is still alive.
#[cfg(not(tokio_unstable))]
pub(crate) fn cached_runtime_ctx(runtime: tokio::runtime::Id) -> Option<Arc<RuntimeContext>> {
    RUNTIME_CTX_CACHE
        .try_with(|cell| {
            let cached = cell.borrow();
            let (id, weak) = cached.as_ref()?;
            (*id == runtime).then(|| weak.upgrade())?
        })
        .ok()
        .flatten()
}

/// Remember `ctx` as this thread's context for `runtime`.
#[cfg(not(tokio_unstable))]
pub(crate) fn cache_runtime_ctx(runtime: tokio::runtime::Id, ctx: &Arc<RuntimeContext>) {
    let _ = RUNTIME_CTX_CACHE
        .try_with(|cell| *cell.borrow_mut() = Some((runtime, Arc::downgrade(ctx))));
}

/// Whether a recorded poll span is open on this thread: set by
/// [`make_poll_start`], cleared by [`make_poll_end`]. The `WakeTraced` wrapper
/// reads it to leave polls that tokio's hooks (or an outer wrapper) already
/// record to their recorder.
pub(crate) fn poll_span_open() -> bool {
    POLL_START_TS.with(|c| c.get().is_some())
}

/// Close this thread's poll-span marker unconditionally, so a buffer disabled
/// at poll end cannot leave it set and mute every later wrapper poll.
pub(crate) fn clear_poll_span() {
    POLL_START_TS.with(|c| c.set(None));
}

fn make_worker_park(ctx: &RuntimeContext) -> WorkerParkEvent {
    let worker_id = event_worker_id(ctx);
    let worker_local_queue_depth = current_local_queue_depth();
    let cpu_time_nanos = crate::telemetry::events::thread_cpu_time_nanos();
    // Only read schedstat on 1-in-N parks. The counter and the "sampled this
    // park" flag are thread-local, so the matching unpark on the same worker
    // thread reads schedstat iff park did, keeping the wait-time delta a valid
    // park->unpark pair.
    let sample = PARK_COUNTER.with(|c| {
        let (next, sample) = advance_park_counter(c.get(), sched_wait_sample_rate());
        c.set(next);
        sample
    });
    let sampled = sample
        && match SchedStat::read_current() {
            Ok(ss) => {
                PARKED_SCHED_WAIT.with(|c| c.set(ss.wait_time_ns));
                true
            }
            Err(_) => false,
        };
    SCHED_SAMPLED_THIS_PARK.with(|c| c.set(sampled));
    WorkerParkEvent {
        timestamp_ns: crate::telemetry::events::clock_monotonic_ns(),
        worker_id,
        local_queue: worker_local_queue_depth as u8,
        cpu_time_ns: cpu_time_nanos,
        tid: crate::telemetry::events::current_tid(),
    }
}

fn make_worker_unpark(ctx: &RuntimeContext) -> WorkerUnparkEvent {
    let worker_id = event_worker_id(ctx);
    let worker_local_queue_depth = current_local_queue_depth();
    let cpu_time_nanos = crate::telemetry::events::thread_cpu_time_nanos();
    // Only read schedstat on unpark if the matching park sampled it, so the
    // delta below always pairs with a park-time reading. Reset the flag either
    // way so a subsequent unsampled park can't reuse a stale pair. Report `None`
    // when unsampled so consumers can distinguish "not measured" from a genuine
    // zero-wait unpark rather than diluting their averages with false zeros.
    let sampled = SCHED_SAMPLED_THIS_PARK.with(|c| c.replace(false));
    let sched_wait_delta_nanos = if sampled {
        SchedStat::read_current().ok().map(|ss| {
            let prev = PARKED_SCHED_WAIT.with(|c| c.get());
            ss.wait_time_ns.saturating_sub(prev)
        })
    } else {
        None
    };
    WorkerUnparkEvent {
        timestamp_ns: crate::telemetry::events::clock_monotonic_ns(),
        worker_id,
        local_queue: worker_local_queue_depth as u8,
        cpu_time_ns: cpu_time_nanos,
        sched_wait_ns: sched_wait_delta_nanos,
        tid: crate::telemetry::events::current_tid(),
    }
}

#[cfg(all(test, not(shuttle)))]
mod tests {
    use super::*;

    /// Push a named runtime context with a single resolved worker into `contexts`.
    fn push_named_runtime(contexts: &RuntimeContextRegistry, name: &str, worker_id: u64) {
        let ctx = Arc::new(RuntimeContext::new(
            Some(name.to_string()),
            Dial9Handle::disabled(),
            Arc::new(AtomicU64::new(0)),
        ));
        ctx.worker_ids.lock().unwrap().insert(worker_id);
        contexts.lock().unwrap().push(ctx);
    }

    #[test]
    fn segment_metadata_only_rebuilds_after_a_change() {
        // The source detects change from the runtime / worker counts itself —
        // there is no external signal for a caller to forget to bump.
        let contexts: RuntimeContextRegistry = Arc::new(Mutex::new(Vec::new()));
        let mut source = TokioRuntimesSource::new(contexts.clone());

        // Empty registry: no runtime metadata yet, but the fixed entries are
        // emitted once on the first call.
        let mut out = Vec::new();
        source.segment_metadata(&mut out);
        assert_eq!(
            out.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec![
                "sched.wait_sample_rate",
                "tokio.poll_coverage",
                "tokio.local_queue",
                "tokio.unstable"
            ],
            "first call emits only the fixed entries"
        );

        // Register a runtime: the count grows, so the source rebuilds. The
        // fixed entries are not re-emitted (they are emitted exactly once).
        push_named_runtime(&contexts, "main", 0);

        out.clear();
        source.segment_metadata(&mut out);
        assert_eq!(out, vec![("runtime.main".to_string(), "0".to_string())]);

        // No further change: the source must not rebuild or append.
        out.clear();
        source.segment_metadata(&mut out);
        assert!(out.is_empty());

        // A second runtime grows the count again and is picked up.
        push_named_runtime(&contexts, "io", 1);

        out.clear();
        source.segment_metadata(&mut out);
        assert!(out.contains(&("runtime.main".to_string(), "0".to_string())));
        assert!(out.contains(&("runtime.io".to_string(), "1".to_string())));
        // The fixed entries are emitted exactly once, so later change cycles
        // never re-emit them.
        assert!(!out.iter().any(|(k, _)| k == "sched.wait_sample_rate"
            || k == "tokio.poll_coverage"
            || k == "tokio.local_queue"
            || k == "tokio.unstable"));
    }

    #[test]
    fn segment_metadata_reports_sched_wait_sample_rate_once() {
        // The schedstat sampling rate is recorded in segment metadata so a
        // consumer knows `sched_wait_ns` is 1-in-N sampled and knows N. It is
        // fixed for the process lifetime, so it must be emitted exactly once.
        let contexts: RuntimeContextRegistry = Arc::new(Mutex::new(Vec::new()));
        let mut source = TokioRuntimesSource::new(contexts);

        let mut out = Vec::new();
        source.segment_metadata(&mut out);
        let (_, value) = out
            .iter()
            .find(|(k, _)| k == "sched.wait_sample_rate")
            .expect("first call emits the sched-wait sample-rate entry");
        // The value is the process-global rate rendered as a positive integer.
        assert_eq!(value, &sched_wait_sample_rate().to_string());
        assert!(value.parse::<u64>().is_ok_and(|n| n >= 1));

        // Emitted exactly once: a second call (no runtime change) appends nothing.
        out.clear();
        source.segment_metadata(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn segment_metadata_reports_capabilities() {
        let contexts: RuntimeContextRegistry = Arc::new(Mutex::new(Vec::new()));
        let mut source = TokioRuntimesSource::new(contexts);

        let mut out = Vec::new();
        source.segment_metadata(&mut out);
        let value = |key: &str| {
            out.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("first call emits {key}"))
        };

        let (coverage, local_queue, unstable) = if cfg!(tokio_unstable) {
            ("all", "true", "true")
        } else {
            ("dial9-spawns-only", "false", "false")
        };
        assert_eq!(value("tokio.poll_coverage"), coverage);
        assert_eq!(value("tokio.local_queue"), local_queue);
        assert_eq!(value("tokio.unstable"), unstable);
    }

    #[test]
    fn park_counter_samples_one_in_n() {
        // rate == 1: every park samples.
        let mut counter = 0u64;
        for _ in 0..5 {
            let (next, sample) = advance_park_counter(counter, 1);
            counter = next;
            assert!(sample);
        }

        // rate == 10 (the default): exactly one park in ten samples, and the
        // sampled park is the 10th, not the 1st, so a burst of short parks is
        // not over-counted.
        counter = 0;
        let mut sampled = 0;
        let mut sampled_indices = Vec::new();
        for i in 1..=30 {
            let (next, sample) = advance_park_counter(counter, 10);
            counter = next;
            if sample {
                sampled += 1;
                sampled_indices.push(i);
            }
        }
        assert_eq!(sampled, 3);
        assert_eq!(sampled_indices, vec![10, 20, 30]);

        // rate == 0 is clamped to 1 (defensive: the env parser already clamps,
        // but the pure helper must not divide by zero).
        let (_, sample) = advance_park_counter(0, 0);
        assert!(sample);
    }

    mod steady_state_alloc {
        use super::*;
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;
        use std::sync::atomic::{AtomicUsize, Ordering};

        thread_local! {
            /// Only the measuring thread tallies, so the rest of the parallel
            /// unit-test suite running under this allocator is unaffected.
            static ARMED: Cell<bool> = const { Cell::new(false) };
        }
        static ALLOCS: AtomicUsize = AtomicUsize::new(0);

        /// Passthrough allocator that counts allocations made by the current
        /// thread while armed. Compiled only into the lib unit-test binary
        /// (`#[cfg(all(test, not(shuttle)))]`), and inert (pure System
        /// passthrough) for every test that does not arm it.
        struct CountingAllocator;
        unsafe impl GlobalAlloc for CountingAllocator {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                if ARMED.with(Cell::get) {
                    ALLOCS.fetch_add(1, Ordering::Relaxed);
                }
                unsafe { System.alloc(layout) }
            }
            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                unsafe { System.dealloc(ptr, layout) }
            }
            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                if ARMED.with(Cell::get) {
                    ALLOCS.fetch_add(1, Ordering::Relaxed);
                }
                unsafe { System.realloc(ptr, layout, new_size) }
            }
            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                if ARMED.with(Cell::get) {
                    ALLOCS.fetch_add(1, Ordering::Relaxed);
                }
                unsafe { System.alloc_zeroed(layout) }
            }
        }

        #[global_allocator]
        static GLOBAL: CountingAllocator = CountingAllocator;

        /// Count allocations made on this thread while running `f`.
        fn count_allocs(f: impl FnOnce()) -> usize {
            ALLOCS.store(0, Ordering::Relaxed);
            ARMED.with(|a| a.set(true));
            f();
            ARMED.with(|a| a.set(false));
            ALLOCS.load(Ordering::Relaxed)
        }

        /// The exact per-cycle metadata block from `flush_loop::run_flush_loop`:
        /// clear the reused buffer, poll every source, and (when non-empty)
        /// drain it into the writer. Steady-state cycles leave it empty.
        fn flush_cycle(
            sources: &Mutex<Vec<Box<dyn Source>>>,
            source_entries: &mut Vec<(String, String)>,
        ) {
            source_entries.clear();
            {
                let mut sources = sources.lock().unwrap();
                for source in sources.iter_mut() {
                    source.segment_metadata(source_entries);
                }
            }
            if !source_entries.is_empty() {
                // Stand-in for `writer.update_segment_metadata(source_entries.drain(..))`:
                // drains so the buffer keeps its capacity, like the flush loop.
                source_entries.drain(..).for_each(drop);
            }
        }

        /// Regression guard for the zero-alloc invariant the flush loop relies
        /// on: once every source has emitted its (unchanged) metadata, repeated
        /// flush cycles must allocate nothing. Breaks if a source starts
        /// rebuilding its metadata every cycle, the change-detection is dropped,
        /// or the reused buffer is moved (losing capacity) instead of drained.
        #[test]
        fn steady_state_metadata_cycles_do_not_allocate() {
            let contexts: RuntimeContextRegistry = Arc::new(Mutex::new(Vec::new()));
            push_named_runtime(&contexts, "main", 0);
            push_named_runtime(&contexts, "io", 1);
            let sources: Mutex<Vec<Box<dyn Source>>> =
                Mutex::new(vec![Box::new(TokioRuntimesSource::new(contexts))]);
            let mut source_entries: Vec<(String, String)> = Vec::new();

            // Prime: the first cycle emits and sizes the buffer (this allocates).
            flush_cycle(&sources, &mut source_entries);

            // Steady state: nothing changed, so further cycles must not allocate.
            let allocs = count_allocs(|| {
                for _ in 0..1000 {
                    flush_cycle(&sources, &mut source_entries);
                }
            });
            assert_eq!(
                allocs, 0,
                "steady-state flush cycles must not allocate; a source is \
                 rebuilding metadata or the reused buffer lost its capacity"
            );
        }
    }
}

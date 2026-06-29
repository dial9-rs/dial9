use super::SharedState;
use super::source::{FlushContext, Source};
use crate::primitives::sync::{Arc, Mutex};
use crate::telemetry::buffer::{Encodable, ThreadLocalEncoder};
use crate::telemetry::events::{SchedStat, clock_monotonic_ns};
use crate::telemetry::format::{
    PollEndEvent, PollStartEvent, QueueSampleEvent, TaskSpawnEvent, WorkerId, WorkerParkEvent,
    WorkerUnparkEvent,
};
use crate::telemetry::task_metadata::TaskId;
use metrique_timesource::{Instant, time_source};
use std::cell::Cell;
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::time::Duration;
use tokio::runtime::RuntimeMetrics;

/// Per-runtime state captured at hook registration time.
///
/// All tokio-specific concepts live here rather than in `SharedState`.
/// Each `RuntimeContext` belongs to exactly one tokio runtime.
pub(crate) struct RuntimeContext {
    /// Optional human-readable name, set via `with_runtime_name`.
    pub runtime_name: Option<String>,
    /// Set once after `builder.build()`. Contains the runtime metrics and the
    /// pre-reserved base worker ID for this runtime (`global_id = base + local_index`).
    pub metrics_and_base: OnceLock<(RuntimeMetrics, u64)>,
    /// Maps worker_index → global worker_id within this runtime.
    /// Populated lazily the first time each worker thread resolves its identity.
    pub worker_ids: RwLock<HashMap<usize, u64>>,
}

thread_local! {
    /// Global worker ID for this thread, set on every `resolve_worker` call.
    /// Read by `current_worker_id()` for wake events.
    static GLOBAL_WORKER_ID: Cell<Option<u64>> = const { Cell::new(None) };
    /// Whether we've registered this thread's worker_id mapping.
    static WORKER_REGISTERED: Cell<bool> = const { Cell::new(false) };
    /// Whether we've registered this thread's OS tid for CPU profiling.
    #[cfg(feature = "cpu-profiling")]
    static TID_REGISTERED: Cell<bool> = const { Cell::new(false) };
    /// Monotonic timestamp captured in `on_before_task_poll`, cleared in
    /// `on_after_task_poll`. Allows code running inside a poll (e.g.
    /// `TaskDumped`, memory profiler) to reuse the timestamp without an extra
    /// clock read.
    static POLL_START_TS: Cell<Option<NonZeroU64>> = const { Cell::new(None) };
    /// Last timestamp returned by `poll_start_ts_monotonic`. Ensures strictly
    /// increasing values within a thread by bumping +1ns on ties.
    static LAST_TS: Cell<u64> = const { Cell::new(0) };
}

crate::primitives::thread_local! {
    /// schedstat wait_time_ns captured at park time, used to compute delta on unpark.
    static PARKED_SCHED_WAIT: Cell<u64> = const { Cell::new(0) };
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
    last_sample: Instant,
    sample_interval: Duration,
    /// Fingerprint of the metadata emitted on the last `segment_metadata` call,
    /// used to skip the rebuild when nothing changed. See `segment_metadata` for
    /// what it is and why it is sufficient. `0` means "nothing emitted yet".
    last_fingerprint: usize,
}

impl TokioRuntimesSource {
    pub(crate) fn new(contexts: RuntimeContextRegistry) -> Self {
        Self {
            contexts,
            last_sample: time_source().instant(),
            sample_interval: Duration::from_millis(10),
            last_fingerprint: 0,
        }
    }
}

impl Source for TokioRuntimesSource {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        if self.last_sample.elapsed() < self.sample_interval {
            return;
        }
        self.last_sample = time_source().instant();
        let total_global_queue: usize = {
            let contexts = self.contexts.lock().unwrap();
            if contexts.is_empty() {
                return;
            }
            contexts.iter().map(|c| c.global_queue_depth()).sum()
        };
        ctx.record_event(&QueueSampleEvent {
            timestamp_ns: clock_monotonic_ns(),
            global_queue: total_global_queue as u8,
        });
    }

    fn name(&self) -> &'static str {
        "tokio_runtimes"
    }

    fn segment_metadata(&mut self, out: &mut Vec<(String, String)>) {
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
                .map(|c| c.worker_ids.read().unwrap().len())
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
    pub(crate) fn new(runtime_name: Option<String>) -> Self {
        Self {
            runtime_name,
            metrics_and_base: OnceLock::new(),
            worker_ids: RwLock::new(HashMap::new()),
        }
    }

    /// Build segment metadata entries for this runtime, e.g. `("runtime.main", "0,1,2,3")`.
    /// Returns `None` if unnamed or no workers resolved yet.
    pub(crate) fn metadata_entry(&self) -> Option<(String, String)> {
        let name = self.runtime_name.as_deref()?;
        let ids = self.worker_ids.read().unwrap();
        if ids.is_empty() {
            return None;
        }
        let mut sorted: Vec<u64> = ids.values().copied().collect();
        sorted.sort_unstable();
        let csv = sorted
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        Some((format!("runtime.{name}"), csv))
    }

    /// Sum of global queue depth for this runtime (0 if metrics not yet set).
    pub(crate) fn global_queue_depth(&self) -> usize {
        self.metrics_and_base
            .get()
            .map(|(m, _)| m.global_queue_depth())
            .unwrap_or(0)
    }

    /// Local queue depth for a worker in this runtime.
    fn local_queue_depth(&self, worker_index: usize) -> usize {
        self.metrics_and_base
            .get()
            .map(|(m, _)| m.worker_local_queue_depth(worker_index))
            .unwrap_or(0)
    }

    /// Resolve the current thread's global worker ID using `tokio::runtime::worker_index()`.
    fn resolve_worker(&self, shared: &SharedState) -> Option<(WorkerId, usize)> {
        let local_index = tokio::runtime::worker_index()?;
        let (_, base) = self.metrics_and_base.get()?;
        let global_id = base + local_index as u64;

        // Always update TLS so current_worker_id() returns the global ID.
        GLOBAL_WORKER_ID.with(|cell| cell.set(Some(global_id)));

        register_worker_if_needed(self, local_index, global_id);
        #[cfg(feature = "cpu-profiling")]
        start_sched_sampling_if_needed(shared);
        #[cfg(not(feature = "cpu-profiling"))]
        let _ = shared;

        Some((WorkerId::from(global_id as usize), local_index))
    }
}

/// Record worker_index → global_id in the context's map (once per thread).
///
/// No need to announce the metadata change: `TokioRuntimesSource` detects the
/// new worker from the worker count on its next flush.
fn register_worker_if_needed(ctx: &RuntimeContext, local_index: usize, global_id: u64) {
    WORKER_REGISTERED.with(|cell| {
        if !cell.get() {
            ctx.worker_ids
                .write()
                .unwrap()
                .insert(local_index, global_id);
            cell.set(true);
        }
    });
}

/// Start sched event sampling for this worker thread (once per thread).
#[cfg(feature = "cpu-profiling")]
fn start_sched_sampling_if_needed(shared: &SharedState) {
    TID_REGISTERED.with(|cell| {
        if !cell.get() {
            // Start sched event sampling for this worker thread. Deferred from
            // on_thread_start so that only worker threads (not blocking pool
            // threads) open perf fds.
            shared.with_sources_mut(|sources| {
                for source in sources.iter_mut() {
                    if let Err(e) = source.on_worker_thread_start() {
                        tracing::warn!(
                            "failed to start source {} for worker thread: {e}",
                            source.name()
                        );
                    }
                }
            });
            cell.set(true);
        }
    });
}

/// Get the current thread's global worker ID.
///
/// Returns [`WorkerId::UNKNOWN`] if called from a thread that has not yet
/// been claimed by a dial9-traced runtime (e.g., before the first poll or
/// from a non-runtime thread).
///
/// This is a thread-local read with no synchronization overhead.
pub fn current_worker_id() -> WorkerId {
    GLOBAL_WORKER_ID.with(|cell| cell.get().map(WorkerId).unwrap_or(WorkerId::UNKNOWN))
}

// ── Event construction helpers ───────────────────────────────────────────────

/// Tokio-side intermediate for a `PollStartEvent`. Holds the raw
/// `&'static Location` so that interning happens lazily inside
/// [`Encodable::encode`], against the thread-local encoder's string pool.
///
/// Going through [`Encodable`] lets the hook closure use the public
/// [`record_event`](crate::telemetry::record_event) API uniformly for all
/// event kinds.
pub(super) struct PollStart {
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
pub(super) struct TaskSpawn {
    pub timestamp_ns: u64,
    pub task_id: TaskId,
    pub location: &'static std::panic::Location<'static>,
    pub instrumented: bool,
}

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

pub(super) fn make_poll_start(
    ctx: &RuntimeContext,
    shared: &SharedState,
    location: &'static std::panic::Location<'static>,
    task_id: TaskId,
) -> PollStart {
    let resolved = ctx.resolve_worker(shared);
    let worker_local_queue_depth = resolved
        .map(|(_, idx)| ctx.local_queue_depth(idx))
        .unwrap_or(0);
    let timestamp_ns = crate::telemetry::events::clock_monotonic_ns();
    POLL_START_TS.with(|c| c.set(NonZeroU64::new(timestamp_ns)));
    PollStart {
        timestamp_ns,
        worker_id: resolved.map(|(id, _)| id).unwrap_or(WorkerId::UNKNOWN),
        local_queue: worker_local_queue_depth as u8,
        task_id,
        location,
    }
}

pub(super) fn make_poll_end(ctx: &RuntimeContext, shared: &SharedState) -> PollEndEvent {
    POLL_START_TS.with(|c| c.set(None));
    let resolved = ctx.resolve_worker(shared);
    PollEndEvent {
        timestamp_ns: crate::telemetry::events::clock_monotonic_ns(),
        worker_id: resolved.map(|(id, _)| id).unwrap_or(WorkerId::UNKNOWN),
    }
}

pub(super) fn make_worker_park(ctx: &RuntimeContext, shared: &SharedState) -> WorkerParkEvent {
    let resolved = ctx.resolve_worker(shared);
    let worker_local_queue_depth = resolved
        .map(|(_, idx)| ctx.local_queue_depth(idx))
        .unwrap_or(0);
    let cpu_time_nanos = crate::telemetry::events::thread_cpu_time_nanos();
    if let Ok(ss) = SchedStat::read_current() {
        PARKED_SCHED_WAIT.with(|c| c.set(ss.wait_time_ns));
    }
    WorkerParkEvent {
        timestamp_ns: crate::telemetry::events::clock_monotonic_ns(),
        worker_id: resolved.map(|(id, _)| id).unwrap_or(WorkerId::UNKNOWN),
        local_queue: worker_local_queue_depth as u8,
        cpu_time_ns: cpu_time_nanos,
        tid: crate::telemetry::events::current_tid(),
    }
}

pub(super) fn make_worker_unpark(ctx: &RuntimeContext, shared: &SharedState) -> WorkerUnparkEvent {
    let resolved = ctx.resolve_worker(shared);
    let worker_local_queue_depth = resolved
        .map(|(_, idx)| ctx.local_queue_depth(idx))
        .unwrap_or(0);
    let cpu_time_nanos = crate::telemetry::events::thread_cpu_time_nanos();
    let sched_wait_delta_nanos = if let Ok(ss) = SchedStat::read_current() {
        let prev = PARKED_SCHED_WAIT.with(|c| c.get());
        ss.wait_time_ns.saturating_sub(prev)
    } else {
        0
    };
    WorkerUnparkEvent {
        timestamp_ns: crate::telemetry::events::clock_monotonic_ns(),
        worker_id: resolved.map(|(id, _)| id).unwrap_or(WorkerId::UNKNOWN),
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
        let ctx = Arc::new(RuntimeContext::new(Some(name.to_string())));
        ctx.worker_ids.write().unwrap().insert(0, worker_id);
        contexts.lock().unwrap().push(ctx);
    }

    #[test]
    fn segment_metadata_only_rebuilds_after_a_change() {
        // The source detects change from the runtime / worker counts itself —
        // there is no external signal for a caller to forget to bump.
        let contexts: RuntimeContextRegistry = Arc::new(Mutex::new(Vec::new()));
        let mut source = TokioRuntimesSource::new(contexts.clone());

        // Empty registry: nothing to append.
        let mut out = Vec::new();
        source.segment_metadata(&mut out);
        assert!(out.is_empty());

        // Register a runtime: the count grows, so the source rebuilds.
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

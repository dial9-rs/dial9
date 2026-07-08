use super::shared_state::{PARK_COUNTER, PARKED_SCHED_WAIT, SCHED_SAMPLED_THIS_PARK, SharedState};
use crate::telemetry::buffer::{Encodable, ThreadLocalEncoder};
use crate::telemetry::events::SchedStat;
use crate::telemetry::format::{
    PollEndEvent, PollStartEvent, TaskSpawnEvent, WorkerId, WorkerParkEvent, WorkerUnparkEvent,
};
use crate::telemetry::task_metadata::TaskId;
use std::cell::Cell;
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::OnceLock;
use std::sync::RwLock;
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
/// - the memory profiler hook ([`TimestampMode::ReusePollStart`]).
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
        register_tid_if_needed(global_id, shared);
        #[cfg(not(feature = "cpu-profiling"))]
        let _ = shared;

        Some((WorkerId::from(global_id as usize), local_index))
    }
}

/// Record worker_index → global_id in the context's map (once per thread).
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

/// Register the current thread's OS tid for CPU profiling (once per thread).
/// Also starts sched event sampling for this worker thread.
#[cfg(feature = "cpu-profiling")]
fn register_tid_if_needed(global_id: u64, shared: &SharedState) {
    TID_REGISTERED.with(|cell| {
        if !cell.get() {
            let os_tid = crate::telemetry::events::current_tid();
            shared.thread_roles.lock().unwrap().insert(
                os_tid,
                crate::telemetry::events::ThreadRole::Worker(global_id as usize),
            );
            // Start sched event sampling for this worker thread. Deferred from
            // on_thread_start so that only worker threads (not blocking pool
            // threads) open perf fds.
            if let Ok(mut sources) = shared.sources.lock() {
                for source in sources.iter_mut() {
                    if let Err(e) = source.on_worker_thread_start() {
                        tracing::warn!(
                            "failed to start source {} for worker thread: {e}",
                            source.name()
                        );
                    }
                }
            }
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

pub(super) fn make_worker_park(ctx: &RuntimeContext, shared: &SharedState) -> WorkerParkEvent {
    let resolved = ctx.resolve_worker(shared);
    let worker_local_queue_depth = resolved
        .map(|(_, idx)| ctx.local_queue_depth(idx))
        .unwrap_or(0);
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
    // Only read schedstat on unpark if the matching park sampled it, so the
    // delta below always pairs with a park-time reading. Reset the flag either
    // way so a subsequent unsampled park can't reuse a stale pair. Unsampled
    // unparks report a wait delta of 0.
    let sampled = SCHED_SAMPLED_THIS_PARK.with(|c| c.replace(false));
    let sched_wait_delta_nanos = if sampled {
        if let Ok(ss) = SchedStat::read_current() {
            let prev = PARKED_SCHED_WAIT.with(|c| c.get());
            ss.wait_time_ns.saturating_sub(prev)
        } else {
            0
        }
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
}

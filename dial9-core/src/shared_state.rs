use crate::collector::CentralCollector;
use crate::encoder;
use crate::encoder::TlBufferHandle;
use crate::metrics::TlDrainStats;
use crate::primitives::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::primitives::sync::{Arc, Mutex};
use std::time::Duration;

/// Runtime-agnostic core recording state.
#[doc(hidden)]
pub struct SharedState {
    pub(crate) enabled: AtomicBool,
    pub(crate) collector: Arc<CentralCollector>,
    /// Absolute `CLOCK_MONOTONIC` nanosecond timestamp captured at trace start.
    pub(crate) start_time_ns: u64,
    /// Global worker ID counter. Each runtime reserves a contiguous block
    /// via `fetch_add(num_workers)` so worker IDs don't collide.
    pub(crate) next_worker_id: AtomicU64,
    /// Epoch counter bumped by the flush thread every ~30s. Thread-local
    /// buffers stamp this value on each self-flush so the flush thread can
    /// skip busy workers when draining.
    pub(crate) drain_epoch: AtomicU64,
    /// Weak handles to all registered thread-local buffers. The flush thread
    /// uses these to intrusively drain idle/silent buffers.
    tl_buffers: Mutex<Vec<TlBufferHandle>>,
    /// Data sources (CPU profiler, sched profiler, etc.) that the flush thread drains.
    pub(crate) sources: Mutex<Vec<Box<dyn crate::source::Source>>>,
    /// On-demand dump trigger, set once at build time when the runtime is
    /// built with `with_dump_trigger`. Reached by application code through
    /// [`Dial9Handle::dump_trigger`](super::handle::Dial9Handle::dump_trigger).
    #[cfg(feature = "pipeline")]
    dump_trigger: std::sync::OnceLock<crate::dump::DumpTrigger>,
}

impl SharedState {
    crate::test_util_pub! {
        fn new(start_time_ns: u64) -> Self {
            Self {
                enabled: AtomicBool::new(false),
                collector: Arc::new(CentralCollector::new()),
                start_time_ns,
                next_worker_id: AtomicU64::new(0),
                drain_epoch: AtomicU64::new(0),
                tl_buffers: Mutex::new(Vec::new()),
                sources: Mutex::new(Vec::new()),
                #[cfg(feature = "pipeline")]
                dump_trigger: std::sync::OnceLock::new(),
            }
        }
    }

    /// Register a data source to be drained by the flush thread each cycle.
    pub fn push_source(&self, source: Box<dyn crate::source::Source>) {
        self.sources.lock().unwrap().push(source);
    }

    /// Run `f` against the registered sources. Returns `None` if the lock is
    /// poisoned. Used to drive the per-thread source lifecycle hooks.
    pub fn with_sources_mut<R>(
        &self,
        f: impl FnOnce(&mut [Box<dyn crate::source::Source>]) -> R,
    ) -> Option<R> {
        self.sources.lock().ok().map(|mut sources| f(&mut sources))
    }

    /// Trace-start `CLOCK_MONOTONIC` timestamp.
    pub fn start_time_ns(&self) -> u64 {
        self.start_time_ns
    }

    /// Reserve a contiguous block of `count` worker IDs, returning the first.
    pub fn reserve_worker_ids(&self, count: u64) -> u64 {
        self.next_worker_id.fetch_add(count, Ordering::Relaxed)
    }

    /// Turn recording on.
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Relaxed);
    }

    /// Turn recording off.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Relaxed);
    }

    /// Install the on-demand dump trigger. Set once at build time by the
    /// facade builder; later calls are ignored. `pub` so the facade (a
    /// sibling crate) can wire the trigger in.
    #[cfg(feature = "pipeline")]
    pub fn set_dump_trigger(&self, trigger: crate::dump::DumpTrigger) {
        let _ = self.dump_trigger.set(trigger);
    }

    /// The on-demand dump trigger, or `None` when the runtime was built
    /// without `with_dump_trigger`.
    #[cfg(feature = "pipeline")]
    pub(crate) fn dump_trigger(&self) -> Option<&crate::dump::DumpTrigger> {
        self.dump_trigger.get()
    }

    /// Check whether recording is currently enabled.
    ///
    /// Prefer [`if_enabled`](Self::if_enabled) for event-recording paths — it
    /// provides an [`EventBuffer`] that makes it structurally impossible to
    /// record without checking first. Use `is_enabled()` only for
    /// control-flow decisions that don't directly record events (e.g.
    /// deciding whether to wrap a waker in wake-tracking polls).
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Run `f` only when recording is enabled, passing an [`EventBuffer`]
    /// that provides `record_event` / `record_encodable_event`. Returns
    /// `None` when disabled (no work is done).
    pub fn if_enabled<R>(&self, f: impl FnOnce(&EventBuffer<'_>) -> R) -> Option<R> {
        if !self.enabled.load(Ordering::Relaxed) {
            return None;
        }
        Some(f(&EventBuffer(self)))
    }

    /// Test-only shortcut to record an event directly. Production code records
    /// through [`EventBuffer`] via [`if_enabled`](Self::if_enabled).
    #[cfg(test)]
    fn record_encodable_event(&self, event: &dyn encoder::Encodable) {
        if let Some(handle) =
            encoder::record_encodable_event(event, &self.collector, &self.drain_epoch)
        {
            self.tl_buffers.lock().unwrap().push(handle);
        }
    }

    /// Bump the drain epoch and flush all idle/silent thread-local buffers.
    ///
    /// Buffers whose `FlushEpoch` matches the current epoch are skipped
    /// (the owning thread flushed recently, so locking would just add
    /// contention). Dead `Weak` handles are pruned.
    ///
    /// [`bump_drain_epoch`] is called one flush-loop tick
    /// before calling this method. That gives busy worker threads a ~5 ms
    /// grace period to self-flush on their next `record_event`, so the
    /// intrusive drain only needs to lock truly idle/silent buffers.
    ///
    /// Returns per-cycle counters so the flush thread can emit metrics.
    pub(crate) fn drain_all_tl_buffers(&self) -> TlDrainStats {
        let mut stats = TlDrainStats::default();
        let epoch = self.drain_epoch.load(Ordering::Relaxed);

        let handles: Vec<TlBufferHandle> = {
            let guard = self.tl_buffers.lock().unwrap();
            guard
                .iter()
                .map(|h| TlBufferHandle {
                    buffer: h.buffer.clone(),
                    flush_epoch: h.flush_epoch.clone(),
                })
                .collect()
        };

        for handle in &handles {
            // Skip buffers that self-flushed during the current epoch.
            if handle.flush_epoch.load() >= epoch {
                stats.buffers_skipped_busy += 1;
                continue;
            }
            if let Some(arc) = handle.buffer.upgrade() {
                let mut buf = match arc.lock() {
                    Ok(guard) => guard,
                    // Buffer is poisoned (encoder panic); skip rather than
                    // flushing potentially corrupt data.
                    Err(_) => {
                        crate::rate_limit::rate_limited!(Duration::from_secs(60), {
                            tracing::error!(
                                "dial9: thread-local buffer mutex poisoned in drain_all_tl_buffers; skipping flush"
                            );
                        });
                        continue;
                    }
                };
                stats.buffers_locked += 1;
                if buf.has_pending_events() {
                    let batch = buf.flush();
                    stats.events_flushed += batch.event_count();
                    stats.buffers_flushed += 1;
                    self.collector.accept_flush(batch);
                }
                // Stamp so we skip this buffer next cycle if it stays idle.
                handle.flush_epoch.store(epoch);
            }
        }

        // Prune dead handles (Weak refs to threads that have exited).
        let mut guard = self.tl_buffers.lock().unwrap();
        let before = guard.len();
        guard.retain(|h| h.buffer.strong_count() > 0);
        stats.dead_pruned = (before - guard.len()) as u64;

        stats
    }

    /// Advance the global drain epoch so that busy worker threads
    /// self-flush on their next `record_event` call. Call this one
    /// flush-loop tick (~5 ms) before [`drain_all_tl_buffers`] to give
    /// workers a grace period, minimising contention on the intrusive
    /// drain path.
    pub(crate) fn bump_drain_epoch(&self) {
        self.drain_epoch.fetch_add(1, Ordering::Relaxed);
    }

    /// Drain data sources and write their events into the collector.
    pub fn flush_sources(&self) {
        let ctx = self.flush_context();
        let mut sources = self.sources.lock().unwrap();
        for source in sources.iter_mut() {
            source.flush(&ctx);
        }
    }

    /// Build a [`FlushContext`] for this state.
    ///
    /// Used by `flush_sources` and by tests that construct a ctx directly.
    ///
    /// [`FlushContext`]: crate::source::FlushContext
    pub fn flush_context(&self) -> crate::source::FlushContext<'_> {
        crate::source::FlushContext::new(&self.collector, &self.drain_epoch)
    }
}

/// Handle provided by [`SharedState::if_enabled`] that proves recording is
/// active. All event-recording calls should go through this type so that
/// callers cannot accidentally emit events without an enabled check.
#[doc(hidden)]
pub struct EventBuffer<'a>(&'a SharedState);

impl EventBuffer<'_> {
    pub fn record_encodable_event(&self, event: &dyn encoder::Encodable) {
        if let Some(handle) =
            encoder::record_encodable_event(event, &self.0.collector, &self.0.drain_epoch)
        {
            self.0.tl_buffers.lock().unwrap().push(handle);
        }
    }

    pub fn with_encoder(&self, f: impl FnOnce(&mut encoder::ThreadLocalEncoder<'_>)) {
        if let Some(handle) = encoder::with_encoder(f, &self.0.collector, &self.0.drain_epoch) {
            self.0.tl_buffers.lock().unwrap().push(handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event() -> crate::format::ClockSyncEvent {
        crate::format::ClockSyncEvent {
            timestamp_ns: 1000,
            realtime_ns: 2000,
        }
    }

    /// Helper: create a SharedState with recording enabled.
    fn enabled_shared_state() -> SharedState {
        let ss = SharedState::new(0);
        ss.enabled.store(true, Ordering::Relaxed);
        ss
    }

    #[test]
    fn record_event_registers_tl_buffer_handle() {
        let ss = enabled_shared_state();
        // First event on this thread should register a handle.
        ss.record_encodable_event(&sample_event());
        let handles = ss.tl_buffers.lock().unwrap();
        assert_eq!(handles.len(), 1);
        assert!(handles[0].buffer.upgrade().is_some());
    }

    #[test]
    fn second_record_event_does_not_re_register() {
        let ss = enabled_shared_state();
        ss.record_encodable_event(&sample_event());
        ss.record_encodable_event(&sample_event());
        let handles = ss.tl_buffers.lock().unwrap();
        assert_eq!(handles.len(), 1);
    }

    #[test]
    fn drain_all_tl_buffers_flushes_idle_buffer() {
        let ss = enabled_shared_state();
        // Write an event (won't self-flush — buffer is 1MB).
        ss.record_encodable_event(&sample_event());
        // Nothing in the collector yet (buffer not full).
        assert!(ss.collector.next().is_none());
        // Bump epoch so the idle buffer (epoch 0) is stale, then drain.
        ss.bump_drain_epoch();
        ss.drain_all_tl_buffers();
        let batch = ss.collector.next().expect("expected a batch after drain");
        assert!(batch.event_count() > 0);
    }

    #[test]
    fn drain_all_tl_buffers_from_another_thread() {
        let ss = Arc::new(enabled_shared_state());
        let ss2 = ss.clone();
        // Write events from a spawned thread.
        let handle = std::thread::spawn(move || {
            ss2.record_encodable_event(&sample_event());
            ss2.record_encodable_event(&sample_event());
        });
        handle.join().unwrap();
        // Bump epoch so the buffer is stale, then drain from the main thread.
        ss.bump_drain_epoch();
        ss.drain_all_tl_buffers();
        let batch = ss.collector.next().expect("expected a batch after drain");
        assert_eq!(batch.event_count(), 2);
    }

    #[test]
    fn drain_skips_busy_buffer() {
        let ss = enabled_shared_state();
        ss.record_encodable_event(&sample_event());
        // Bump epoch to 1 (simulates the tick before the drain).
        ss.bump_drain_epoch();
        // Simulate a self-flush by stamping the current epoch.
        {
            let handles = ss.tl_buffers.lock().unwrap();
            handles[0].flush_epoch.store(1);
        }
        ss.drain_all_tl_buffers();
        // Buffer should NOT have been flushed — collector is empty.
        assert!(ss.collector.next().is_none());
    }

    #[test]
    fn drain_prunes_dead_handles() {
        let ss = Arc::new(enabled_shared_state());
        let ss2 = ss.clone();
        let handle = std::thread::spawn(move || {
            ss2.record_encodable_event(&sample_event());
        });
        handle.join().unwrap();
        // Thread exited — its Arc<Mutex<TLB>> was dropped, Weak is dead.
        // But the TLB's Drop impl flushed remaining events, so the handle
        // is dead. Drain should prune it.
        ss.drain_all_tl_buffers();
        let handles = ss.tl_buffers.lock().unwrap();
        assert_eq!(handles.len(), 0, "dead handle should have been pruned");
    }

    /// Intrusive-drain path with a *live* worker thread. Unlike
    /// `drain_all_tl_buffers_from_another_thread`, which joins the worker
    /// before draining (so events reach the collector via the TLB `Drop`
    /// impl, not via the intrusive path), here the worker is parked on a
    /// channel while the main thread bumps+drains, proving that
    /// `drain_all_tl_buffers` upgrades the live `Weak`, locks the mutex
    /// cross-thread, and flushes the pending event.
    #[test]
    fn drain_flushes_live_worker_buffer() {
        let ss = Arc::new(enabled_shared_state());
        let ss2 = ss.clone();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();

        let worker = std::thread::spawn(move || {
            // drain_epoch is 0, so no self-flush happens — the event
            // stays in the buffer.
            ss2.record_encodable_event(&sample_event());
            ready_tx.send(()).unwrap();
            // Park until main thread has drained. The TLB `Drop` impl must
            // not run before the intrusive drain, otherwise we're not
            // testing the intrusive path.
            release_rx.recv().unwrap();
        });

        ready_rx.recv().unwrap();
        // Worker is parked with one event in its TLB and a live handle.
        // Nothing in the collector yet — no self-flush was triggered.
        assert!(ss.collector.next().is_none());

        ss.bump_drain_epoch();
        ss.drain_all_tl_buffers();

        let batch = ss
            .collector
            .next()
            .expect("intrusive drain should have flushed the live worker's event");
        assert_eq!(batch.event_count(), 1);

        release_tx.send(()).unwrap();
        worker.join().unwrap();
    }

    // Concurrent-stress proptest: the core invariant of the TL buffer
    // drain feature is that no events are lost and none are duplicated,
    // regardless of how `record_encodable_event`, `bump_drain_epoch`, and
    // `drain_all_tl_buffers` interleave across threads. Spawn N writer
    // threads, each recording M events, while a drainer thread
    // concurrently bumps+drains. After joining, a final bump+drain should
    // leave exactly N*M events in the collector.
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

        #[test]
        fn concurrent_record_and_drain_preserves_event_count(
            num_threads in 1usize..=6,
            events_per_thread in 1u64..=200,
            drain_ticks in 0usize..=10,
        ) {
            let ss = Arc::new(enabled_shared_state());
            let start = Arc::new(std::sync::Barrier::new(num_threads + 1));
            let stop_drainer = Arc::new(AtomicBool::new(false));

            let writers: Vec<_> = (0..num_threads)
                .map(|_| {
                    let ss = ss.clone();
                    let start = start.clone();
                    std::thread::spawn(move || {
                        start.wait();
                        for _ in 0..events_per_thread {
                            ss.record_encodable_event(&sample_event());
                        }
                    })
                })
                .collect();

            let drainer = {
                let ss = ss.clone();
                let stop = stop_drainer.clone();
                std::thread::spawn(move || {
                    let mut ticks = 0;
                    while ticks < drain_ticks && !stop.load(Ordering::Relaxed) {
                        ss.bump_drain_epoch();
                        // Short grace period so any in-flight writer has a
                        // chance to self-flush before the intrusive drain.
                        std::thread::sleep(std::time::Duration::from_micros(50));
                        ss.drain_all_tl_buffers();
                        ticks += 1;
                    }
                })
            };

            start.wait();
            for w in writers {
                w.join().unwrap();
            }
            stop_drainer.store(true, Ordering::Relaxed);
            drainer.join().unwrap();

            // Writer threads have exited, so their TLB `Drop` impls have
            // flushed any remaining events. Do one final bump+drain to
            // prune dead handles (no-op for event capture at this point).
            ss.bump_drain_epoch();
            ss.drain_all_tl_buffers();

            let mut total: u64 = 0;
            while let Some(batch) = ss.collector.next() {
                total += batch.event_count();
            }
            // Sanity: the collector never evicted a batch under these
            // workloads. If it did, the invariant check below would be
            // meaningless.
            proptest::prop_assert_eq!(ss.collector.take_dropped_batches(), 0);
            proptest::prop_assert_eq!(
                total,
                num_threads as u64 * events_per_thread,
                "every recorded event must reach the collector exactly once"
            );
        }
    }
}

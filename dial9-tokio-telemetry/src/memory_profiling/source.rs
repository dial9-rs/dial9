#![deny(clippy::arithmetic_side_effects)]
//! `Source` impl that drains the alloc and free queues each flush cycle.

use crate::memory_profiling::profiler::Liveset;
use crate::memory_profiling::ring::{RawAlloc, RawFree, RingBuffers};
use crate::primitives::sync::Arc;
use crate::telemetry::events::clock_monotonic_ns;
use crate::telemetry::format::{AllocEvent, FreeEvent, MemoryProfileOverflowEvent};
use crate::telemetry::recorder::source::{FlushContext, Source};
use std::sync::atomic::Ordering;

/// Drains the alloc and free queues into the trace each flush cycle.
///
/// With the producer-side liveset, every non-shutdown `RawFree` that arrives
/// in the queue is guaranteed to correspond to a previously-sampled
/// allocation: the producer already did the peek/remove and denormalized
/// `size` and `alloc_ts_ns` onto the record. The consolidator emits
/// `FreeEvent` directly with no lookup.
///
/// **Shutdown-flagged frees** (`RawFree::shutdown == true`) are different:
/// the producer was in TLS teardown and couldn't safely peek the liveset,
/// so it pushed an addr-only record. The consolidator does the peek/remove
/// here against the shared `liveset` Arc. See `handle_free` for the race
/// check that prevents emitting wrong events when an address is reused
/// before the consolidator drains.
pub(crate) struct MemoryProfileSource {
    rings: Arc<RingBuffers>,
    /// The producer-side liveset, shared with the allocator hook. `None`
    /// when liveset tracking is off — in that mode the producer filters
    /// frees and `handle_free` has nothing to emit. Held here so the
    /// consolidator can also service shutdown-flagged `RawFree`s.
    liveset: Option<Arc<Liveset>>,
    /// Previous snapshot of `RingBuffers::dropped_allocs` for delta computation.
    prev_dropped_allocs: u64,
    /// Previous snapshot of `RingBuffers::dropped_frees` for delta computation.
    prev_dropped_frees: u64,
    /// Precomputed segment metadata. Fixed at construction and never changes,
    /// so it is appended on the first flush and otherwise left in the writer's
    /// merged cache, which re-emits it on every rotation (the writer's merge
    /// preserves this key on later tokio-only metadata updates).
    metadata: Vec<(String, String)>,
    /// Whether `metadata` has been appended yet. The fixed metadata is emitted
    /// once on the first flush; later flushes report no change.
    emitted: bool,
}

impl MemoryProfileSource {
    /// Create a new source that drains the supplied ring buffers.
    ///
    /// Pass `liveset = Some(_)` when liveset tracking is on; `None`
    /// disables `FreeEvent` emission entirely. The Arc is shared with the
    /// allocator hook so the consolidator can service shutdown-flagged
    /// frees.
    pub(crate) fn new(
        rings: Arc<RingBuffers>,
        liveset: Option<Arc<Liveset>>,
        sample_rate_bytes: u64,
    ) -> Self {
        Self {
            rings,
            liveset,
            prev_dropped_allocs: 0,
            prev_dropped_frees: 0,
            metadata: vec![(
                "memory.sample_rate_bytes".to_string(),
                sample_rate_bytes.to_string(),
            )],
            emitted: false,
        }
    }

    fn handle_alloc(&mut self, a: RawAlloc, ctx: &FlushContext<'_>) {
        let frame_count = a.frame_count as usize;
        let RawAlloc {
            tid,
            size,
            addr,
            ts_ns,
            frames,
            ..
        } = a;
        ctx.with_encoder(|enc| {
            let callchain = enc.intern_stack_frames(&frames[..frame_count]);
            enc.encode(&AllocEvent {
                timestamp_ns: ts_ns,
                tid,
                size,
                addr,
                callchain,
            });
        });
    }

    fn handle_free(&mut self, f: RawFree, ctx: &FlushContext<'_>) {
        // No liveset means the producer drops every free; nothing to emit.
        let Some(liveset) = &self.liveset else {
            return;
        };

        // Resolve `(size, alloc_ts_ns)`:
        // - **Normal frees** carry denormalized data from the producer's
        //   peek/remove (see `hook::on_dealloc`); use it directly.
        // - **Shutdown-flagged frees** were pushed by a thread in TLS
        //   teardown that couldn't safely touch `scc`. Do the peek/remove
        //   here, with a race check: if the entry's `alloc_ts_ns` is
        //   greater than or equal to `f.ts_ns`, an address-reuse race (or
        //   timestamp tie on nearby cores) means the entry now belongs to a
        //   *later* allocation. Leave it; the new alloc's eventual
        //   non-shutdown free will emit the correct event.
        let (size, alloc_ts_ns) = if f.shutdown {
            let Some((size, alloc_ts_ns)) = liveset.peek_with(&f.addr, |_, v| *v) else {
                return; // already cleaned, or never sampled
            };
            if alloc_ts_ns >= f.ts_ns {
                return; // address-reuse race or timestamp tie — see comment above
            }
            liveset.remove(&f.addr);
            (size, alloc_ts_ns)
        } else {
            (f.size, f.alloc_ts_ns)
        };

        ctx.with_encoder(|enc| {
            enc.encode(&FreeEvent {
                timestamp_ns: f.ts_ns,
                tid: f.tid,
                addr: f.addr,
                size,
                alloc_timestamp_ns: alloc_ts_ns,
            });
        });
    }
}

impl Source for MemoryProfileSource {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        // Merge-sort drain by timestamp. This produces a best-effort
        // timestamp-ordered stream. Ordering is not guaranteed to be perfect:
        // multiple producers push concurrently, so queue order may not match
        // timestamp order. For profiling purposes, approximate ordering is
        // sufficient.
        //
        // Hold one peeked element from each queue and emit the older one.
        // `crossbeam_queue::ArrayQueue` has no peek API, so we pop into
        // local slots and only refill after we emit. The producer can race
        // in between; that's fine — anything it pushes during this loop has
        // a timestamp later than anything we've already emitted, and we
        // either pick it up this cycle (if our last pop sees it) or next
        // cycle.
        let mut next_alloc: Option<RawAlloc> = self.rings.alloc_queue.pop();
        let mut next_free: Option<RawFree> = self.rings.free_queue.pop();
        loop {
            match (&next_alloc, &next_free) {
                (None, None) => break,
                (Some(_), None) => {
                    let a = next_alloc.take().expect("checked Some above");
                    self.handle_alloc(a, ctx);
                    next_alloc = self.rings.alloc_queue.pop();
                }
                (None, Some(_)) => {
                    let f = next_free.take().expect("checked Some above");
                    self.handle_free(f, ctx);
                    next_free = self.rings.free_queue.pop();
                }
                (Some(a), Some(f)) => {
                    if a.ts_ns <= f.ts_ns {
                        let a = next_alloc.take().expect("checked Some above");
                        self.handle_alloc(a, ctx);
                        next_alloc = self.rings.alloc_queue.pop();
                    } else {
                        let f = next_free.take().expect("checked Some above");
                        self.handle_free(f, ctx);
                        next_free = self.rings.free_queue.pop();
                    }
                }
            }
        }

        // Emit overflow event if any samples were dropped since last flush.
        // Relaxed ordering is sufficient: the flush thread is the sole reader,
        // and we only need eventual visibility of producer increments. The two
        // counters are independent so we don't need ordering between the loads.
        let current_dropped_allocs = self.rings.dropped_allocs.load(Ordering::Relaxed);
        let current_dropped_frees = self.rings.dropped_frees.load(Ordering::Relaxed);
        let delta_allocs = current_dropped_allocs.saturating_sub(self.prev_dropped_allocs);
        let delta_frees = current_dropped_frees.saturating_sub(self.prev_dropped_frees);
        if delta_allocs > 0 || delta_frees > 0 {
            ctx.with_encoder(|enc| {
                enc.encode(&MemoryProfileOverflowEvent {
                    timestamp_ns: clock_monotonic_ns(),
                    dropped_allocs: delta_allocs,
                    dropped_frees: delta_frees,
                });
            });
            self.prev_dropped_allocs = current_dropped_allocs;
            self.prev_dropped_frees = current_dropped_frees;
        }
    }

    fn name(&self) -> &'static str {
        "memory"
    }

    fn segment_metadata(&mut self, out: &mut Vec<(String, String)>) {
        // Metadata is fixed at construction, so it only needs to be emitted
        // once: the writer keeps it in its merged cache and re-emits it on
        // every rotation. No need to observe the shared metadata-change counter
        // (unlike `TokioRuntimesSource`, whose entries grow over time).
        if self.emitted {
            return;
        }
        out.extend(self.metadata.iter().cloned());
        self.emitted = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_profiling::ring::{DEFAULT_MAX_FRAMES, RawAlloc, RawFree, RingBuffers};
    use crate::primitives::sync::Arc;
    use crate::telemetry::analysis_events::Dial9Event;
    use crate::telemetry::format::decode_events;
    use crate::telemetry::recorder::SharedState;
    use dial9_core::test_util;

    fn make_raw_alloc(addr: u64, size: u64, ts_ns: u64) -> RawAlloc {
        let mut frames = [0u64; DEFAULT_MAX_FRAMES];
        frames[0] = 0xAAAA;
        frames[1] = 0xBBBB;
        frames[2] = 0xCCCC;
        RawAlloc {
            tid: 1,
            size,
            addr,
            ts_ns,
            frames,
            frame_count: 3,
        }
    }

    fn make_raw_free(addr: u64, ts_ns: u64, size: u64, alloc_ts_ns: u64) -> RawFree {
        RawFree {
            tid: 2,
            addr,
            ts_ns,
            size,
            alloc_ts_ns,
            shutdown: false,
        }
    }

    /// Build a shutdown-flagged `RawFree`. The producer pushes these during
    /// TLS teardown when it can't safely peek the liveset; the consolidator
    /// fills in size / alloc_ts_ns from its own peek.
    fn make_shutdown_free(addr: u64, ts_ns: u64, tid: u32) -> RawFree {
        RawFree {
            tid,
            addr,
            ts_ns,
            size: 0,
            alloc_ts_ns: 0,
            shutdown: true,
        }
    }

    fn rings(alloc_cap: usize, free_cap: usize) -> Arc<RingBuffers> {
        Arc::new(RingBuffers::new(alloc_cap, free_cap))
    }

    /// Build a fresh empty liveset with the same hasher we use in production.
    fn fresh_liveset() -> Arc<Liveset> {
        Arc::new(scc::HashIndex::with_capacity_and_hasher(
            0,
            dial9_trace_format::encoder::FxBuildHasher::default(),
        ))
    }

    /// Convenience: build a `MemoryProfileSource` for tests where we don't
    /// need to retain a separate handle on the liveset. Pass
    /// `track_liveset = true` to enable `FreeEvent` emission (the helper
    /// builds a fresh liveset internally); `false` disables it.
    fn make_source(
        rings: Arc<RingBuffers>,
        track_liveset: bool,
        sample_rate_bytes: u64,
    ) -> MemoryProfileSource {
        let liveset = if track_liveset {
            Some(fresh_liveset())
        } else {
            None
        };
        MemoryProfileSource::new(rings, liveset, sample_rate_bytes)
    }

    fn new_shared() -> SharedState {
        let shared = SharedState::new(0);
        shared.enable();
        shared
    }

    fn flush_and_collect(shared: &SharedState) -> Vec<Dial9Event> {
        shared.flush_sources();
        let mut events = Vec::new();
        for bytes in test_util::drain_encoded_batches(shared) {
            if let Ok(decoded) = decode_events(&bytes) {
                events.extend(decoded);
            }
        }
        events
    }

    #[test]
    fn source_emits_alloc_event() {
        let rings = rings(16, 16);
        rings
            .alloc_queue
            .push(make_raw_alloc(0x1000, 4096, 100))
            .ok();

        let shared = new_shared();
        shared.push_source(Box::new(make_source(Arc::clone(&rings), false, 512 * 1024)));

        let events = flush_and_collect(&shared);
        let allocs: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::AllocEvent(..)))
            .collect();
        assert_eq!(allocs.len(), 1);
        match &allocs[0] {
            Dial9Event::AllocEvent(e) => {
                assert_eq!(e.timestamp_ns, 100);
                assert_eq!(e.tid, 1);
                assert_eq!(e.size, 4096);
                assert_eq!(e.addr, 0x1000);
                assert_eq!(e.callchain, &[0xAAAA, 0xBBBB, 0xCCCC]);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn source_emits_free_event_for_matching_alloc() {
        let rings = rings(16, 16);
        rings
            .alloc_queue
            .push(make_raw_alloc(0x2000, 512, 200))
            .ok();
        // Producer-side liveset denormalizes size and alloc_ts onto RawFree.
        rings
            .free_queue
            .push(make_raw_free(0x2000, 300, 512, 200))
            .ok();

        let shared = new_shared();
        shared.push_source(Box::new(make_source(Arc::clone(&rings), true, 512 * 1024)));

        let events = flush_and_collect(&shared);
        let allocs: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::AllocEvent(..)))
            .collect();
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
            .collect();
        assert_eq!(allocs.len(), 1);
        assert_eq!(frees.len(), 1);
        match &frees[0] {
            Dial9Event::FreeEvent(e) => {
                assert_eq!(e.timestamp_ns, 300);
                assert_eq!(e.tid, 2);
                assert_eq!(e.addr, 0x2000);
                assert_eq!(e.size, 512);
                assert_eq!(e.alloc_timestamp_ns, 200);
            }
            _ => unreachable!(),
        }
    }

    /// With the producer-side liveset, a RawFree only reaches the queue if the
    /// address was in the liveset. The consolidator emits every RawFree it sees.
    /// This test verifies that behavior: a RawFree in the queue produces a FreeEvent.
    #[test]
    fn free_in_queue_is_always_emitted() {
        let rings = rings(16, 16);
        // Simulate a RawFree that passed producer-side filtering.
        rings
            .free_queue
            .push(make_raw_free(0x9999, 400, 128, 100))
            .ok();

        let shared = new_shared();
        shared.push_source(Box::new(make_source(Arc::clone(&rings), true, 512 * 1024)));

        let events = flush_and_collect(&shared);
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
            .collect();
        assert_eq!(frees.len(), 1);
        match &frees[0] {
            Dial9Event::FreeEvent(e) => {
                assert_eq!(e.addr, 0x9999);
                assert_eq!(e.size, 128);
                assert_eq!(e.alloc_timestamp_ns, 100);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn liveset_off_drops_all_frees() {
        let rings = rings(16, 16);
        rings
            .alloc_queue
            .push(make_raw_alloc(0x3000, 128, 500))
            .ok();
        rings
            .free_queue
            .push(make_raw_free(0x3000, 600, 128, 500))
            .ok();

        let shared = new_shared();
        shared.push_source(Box::new(make_source(Arc::clone(&rings), false, 512 * 1024)));

        let events = flush_and_collect(&shared);
        let allocs: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::AllocEvent(..)))
            .collect();
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
            .collect();
        assert_eq!(allocs.len(), 1);
        assert_eq!(frees.len(), 0);
    }

    #[test]
    fn alloc_then_free_in_separate_flush_cycles() {
        let rings = rings(16, 16);

        let shared = new_shared();
        shared.push_source(Box::new(make_source(Arc::clone(&rings), true, 512 * 1024)));

        // First flush: only the alloc
        rings
            .alloc_queue
            .push(make_raw_alloc(0x4000, 256, 700))
            .ok();
        let events = flush_and_collect(&shared);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Dial9Event::AllocEvent(..)))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
                .count(),
            0
        );

        // Second flush: the free arrives (denormalized from producer-side liveset)
        rings
            .free_queue
            .push(make_raw_free(0x4000, 800, 256, 700))
            .ok();
        let events = flush_and_collect(&shared);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Dial9Event::AllocEvent(..)))
                .count(),
            0
        );
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
            .collect();
        assert_eq!(frees.len(), 1);
        match &frees[0] {
            Dial9Event::FreeEvent(e) => {
                assert_eq!(e.size, 256);
                assert_eq!(e.alloc_timestamp_ns, 700);
            }
            _ => unreachable!(),
        }
    }

    /// With the producer-side liveset, address reuse is handled atomically
    /// by the scc::HashIndex (insert/remove are serialized per-key). The
    /// consolidator simply emits every RawFree it receives. This test verifies
    /// that two allocs and one free at the same address produce the expected
    /// events when processed by the source.
    #[test]
    fn address_reuse_within_flush_cycle_emits_correct_events() {
        let rings = rings(16, 16);
        rings
            .alloc_queue
            .push(make_raw_alloc(0x5000, 256, 100))
            .ok();
        // Free carries denormalized data from the first alloc.
        rings
            .free_queue
            .push(make_raw_free(0x5000, 200, 256, 100))
            .ok();
        rings
            .alloc_queue
            .push(make_raw_alloc(0x5000, 512, 300))
            .ok();

        let shared = new_shared();
        shared.push_source(Box::new(make_source(Arc::clone(&rings), true, 512 * 1024)));

        let events = flush_and_collect(&shared);
        let allocs: Vec<&Dial9Event> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::AllocEvent(..)))
            .collect();
        let frees: Vec<&Dial9Event> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
            .collect();
        assert_eq!(allocs.len(), 2, "both allocs should be emitted");
        assert_eq!(
            frees.len(),
            1,
            "the matching free should be emitted exactly once"
        );

        // The free carries denormalized data from the first alloc.
        match frees[0] {
            Dial9Event::FreeEvent(e) => {
                assert_eq!(e.addr, 0x5000);
                assert_eq!(e.size, 256, "free should report size from first alloc");
                assert_eq!(
                    e.alloc_timestamp_ns, 100,
                    "free should reference timestamp of first alloc"
                );
            }
            _ => unreachable!(),
        }

        // Verify a second free for the second alloc also works.
        rings
            .free_queue
            .push(make_raw_free(0x5000, 400, 512, 300))
            .ok();
        let events2 = flush_and_collect(&shared);
        let frees2: Vec<_> = events2
            .iter()
            .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
            .collect();
        assert_eq!(frees2.len(), 1, "second flush should emit one free");
        match frees2[0] {
            Dial9Event::FreeEvent(e) => {
                assert_eq!(e.size, 512, "free should match second alloc size");
                assert_eq!(
                    e.alloc_timestamp_ns, 300,
                    "free should reference timestamp of second alloc"
                );
            }
            _ => unreachable!(),
        }
    }

    /// Demonstrates that `poll_start_ts_monotonic` produces strictly ordered
    /// timestamps even for events that would otherwise share a clock tick —
    /// the scenario that occurs during a realloc (free old + alloc new at
    /// same address). The producer-side liveset resolves address reuse
    /// atomically, but timestamp ordering is still useful for event ordering
    /// in the trace viewer.
    #[test]
    fn monotonic_ts_solves_realloc_ordering() {
        use crate::telemetry::recorder::poll_start_ts_monotonic;

        // Simulate a realloc: alloc, free, alloc — all at the "same instant".
        // poll_start_ts_monotonic guarantees each gets a distinct, increasing
        // timestamp.
        let t1 = poll_start_ts_monotonic();
        let t2 = poll_start_ts_monotonic();
        let t3 = poll_start_ts_monotonic();
        assert!(t1 < t2 && t2 < t3, "timestamps must be strictly ordered");

        let rings = rings(16, 16);
        rings.alloc_queue.push(make_raw_alloc(0x6000, 256, t1)).ok();
        // Free carries denormalized data from first alloc.
        rings
            .free_queue
            .push(make_raw_free(0x6000, t2, 256, t1))
            .ok();
        rings.alloc_queue.push(make_raw_alloc(0x6000, 512, t3)).ok();

        let shared = new_shared();
        shared.push_source(Box::new(make_source(Arc::clone(&rings), true, 512 * 1024)));
        let events = flush_and_collect(&shared);

        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
            .collect();
        assert_eq!(frees.len(), 1);
        match frees[0] {
            Dial9Event::FreeEvent(e) => {
                assert_eq!(e.size, 256, "free should match the first alloc");
                assert_eq!(e.alloc_timestamp_ns, t1);
            }
            _ => unreachable!(),
        }

        // Second alloc still exists — push a free for it.
        let t4 = poll_start_ts_monotonic();
        rings
            .free_queue
            .push(make_raw_free(0x6000, t4, 512, t3))
            .ok();
        let events2 = flush_and_collect(&shared);
        let frees2: Vec<_> = events2
            .iter()
            .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
            .collect();
        assert_eq!(frees2.len(), 1);
        match frees2[0] {
            Dial9Event::FreeEvent(e) => {
                assert_eq!(e.size, 512, "free should match second alloc size");
                assert_eq!(e.alloc_timestamp_ns, t3);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn segment_metadata_contains_sample_rate_bytes() {
        use crate::telemetry::recorder::source::Source;
        let rings = rings(16, 16);
        let mut source = make_source(Arc::clone(&rings), false, 1024 * 1024);
        let mut meta = Vec::new();
        source.segment_metadata(&mut meta);
        assert_eq!(
            meta,
            vec![(
                "memory.sample_rate_bytes".to_string(),
                "1048576".to_string()
            )]
        );
        // Fixed metadata: a second call appends nothing.
        let mut meta2 = Vec::new();
        source.segment_metadata(&mut meta2);
        assert!(meta2.is_empty());
    }

    /// Happy-path shutdown drain: the producer pushed an addr-only flagged
    /// `RawFree` because it was in TLS teardown. The liveset still has the
    /// original entry; the consolidator peeks it, emits a correct
    /// `FreeEvent`, and removes the entry.
    #[test]
    fn shutdown_drain_emits_free_event_and_cleans_liveset() {
        let rings = rings(16, 16);
        let liveset = fresh_liveset();
        // Producer-side state: the dying thread sampled this allocation
        // earlier with timestamp 100 and size 4096.
        liveset.insert(0xAABB, (4096, 100)).expect("liveset insert");

        // Producer pushed a shutdown-flagged free at ts=200.
        rings
            .free_queue
            .push(make_shutdown_free(0xAABB, 200, 7))
            .ok();

        let shared = new_shared();
        shared.push_source(Box::new(MemoryProfileSource::new(
            Arc::clone(&rings),
            Some(Arc::clone(&liveset)),
            512 * 1024,
        )));

        let events = flush_and_collect(&shared);
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
            .collect();
        assert_eq!(frees.len(), 1, "shutdown drain must emit one FreeEvent");
        match frees[0] {
            Dial9Event::FreeEvent(e) => {
                assert_eq!(e.timestamp_ns, 200, "uses producer-push ts");
                assert_eq!(e.tid, 7, "uses producer-push tid");
                assert_eq!(e.addr, 0xAABB);
                assert_eq!(e.size, 4096, "from consolidator-side liveset peek");
                assert_eq!(e.alloc_timestamp_ns, 100, "from liveset peek");
            }
            _ => unreachable!(),
        }

        assert!(
            liveset.peek_with(&0xAABB, |_, _| ()).is_none(),
            "consolidator must have removed the entry"
        );
    }

    /// Race detection: between the producer's shutdown push (ts=100) and
    /// the consolidator drain, the address was reused for a NEW sampled
    /// allocation (ts=300, after the shutdown push). The consolidator must
    /// detect this via `alloc_ts_ns > free.ts_ns` and *not* remove the
    /// entry — otherwise it would emit a wrong `FreeEvent` and lose the
    /// new alloc's eventual free.
    #[test]
    fn shutdown_drain_detects_address_reuse_race() {
        let rings = rings(16, 16);
        let liveset = fresh_liveset();
        // Liveset contains the NEW allocation's data — the dying thread's
        // entry was already overwritten by `on_alloc` (via the
        // remove-then-reinsert path) before the consolidator ran.
        liveset.insert(0xAABB, (8192, 300)).expect("liveset insert");

        // Producer's shutdown push has ts=100 (older than the new alloc).
        rings
            .free_queue
            .push(make_shutdown_free(0xAABB, 100, 9))
            .ok();

        let shared = new_shared();
        shared.push_source(Box::new(MemoryProfileSource::new(
            Arc::clone(&rings),
            Some(Arc::clone(&liveset)),
            512 * 1024,
        )));

        let events = flush_and_collect(&shared);
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
            .collect();
        assert_eq!(
            frees.len(),
            0,
            "race must be detected and no FreeEvent emitted"
        );

        // The new alloc's entry must remain so its eventual free still
        // emits a correct FreeEvent.
        assert_eq!(
            liveset.peek_with(&0xAABB, |_, v| *v),
            Some((8192, 300)),
            "new alloc's entry must survive the race-aware drain"
        );
    }

    /// If a shutdown-flagged free arrives for an address that's not in the
    /// liveset (already cleaned by a prior dealloc, or never sampled), the
    /// consolidator must silently drop it — no `FreeEvent`, no panic.
    #[test]
    fn shutdown_drain_ignores_misses() {
        let rings = rings(16, 16);
        let liveset = fresh_liveset();
        rings
            .free_queue
            .push(make_shutdown_free(0xDEAD, 100, 1))
            .ok();

        let shared = new_shared();
        shared.push_source(Box::new(MemoryProfileSource::new(
            Arc::clone(&rings),
            Some(Arc::clone(&liveset)),
            512 * 1024,
        )));

        let events = flush_and_collect(&shared);
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
            .collect();
        assert_eq!(frees.len(), 0);
    }

    /// Timestamp tie: the shutdown-flagged free has the SAME timestamp as the
    /// liveset entry's `alloc_ts_ns`. This can happen when `clock_monotonic_ns()`
    /// returns the same value on nearby cores. The consolidator must treat ties
    /// conservatively: skip the drain rather than risk removing a live entry that
    /// belongs to a concurrent new allocation with the same timestamp.
    #[test]
    fn shutdown_drain_skips_on_timestamp_tie() {
        let rings = rings(16, 16);
        let liveset = fresh_liveset();
        // Liveset entry has alloc_ts_ns = 100.
        liveset.insert(0xBBCC, (2048, 100)).expect("liveset insert");

        // Shutdown free also has ts_ns = 100 (tie).
        rings
            .free_queue
            .push(make_shutdown_free(0xBBCC, 100, 3))
            .ok();

        let shared = new_shared();
        shared.push_source(Box::new(MemoryProfileSource::new(
            Arc::clone(&rings),
            Some(Arc::clone(&liveset)),
            512 * 1024,
        )));

        let events = flush_and_collect(&shared);
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
            .collect();
        assert_eq!(
            frees.len(),
            0,
            "timestamp tie must be treated as a race — no FreeEvent emitted"
        );

        // Entry must remain intact.
        assert_eq!(
            liveset.peek_with(&0xBBCC, |_, v| *v),
            Some((2048, 100)),
            "liveset entry must survive when timestamps tie"
        );
    }
}

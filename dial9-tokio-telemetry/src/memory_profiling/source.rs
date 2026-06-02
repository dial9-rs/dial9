#![deny(clippy::arithmetic_side_effects)]
//! `Source` impl that drains the alloc and free queues each flush cycle.

use crate::memory_profiling::ring::{RawAlloc, RawFree, RingBuffers};
use crate::primitives::sync::Arc;
use crate::telemetry::buffer::with_encoder;
use crate::telemetry::events::clock_monotonic_ns;
use crate::telemetry::format::{AllocEvent, FreeEvent, MemoryProfileOverflowEvent};
use crate::telemetry::recorder::source::{FlushContext, Source};
use std::collections::HashMap;
use std::sync::atomic::Ordering;

/// Liveset entry tracking a live sampled allocation, kept by the consolidator
/// (flush thread). Only `size` and `timestamp_ns` are needed: both are
/// denormalized onto `FreeEvent` so leak analysis stays useful when the
/// matching `AllocEvent` has been evicted by trace rotation. Storing the stack
/// here would bloat the liveset (see design §8).
#[derive(Debug, Clone, Copy)]
struct LivesetEntry {
    size: u64,
    timestamp_ns: u64,
}

/// Drains the alloc and free queues into the trace each flush cycle.
///
/// The drain is timestamp-ordered: at each step we look at the head of each
/// queue and process the older one first. This matters for liveset
/// correctness when the producer reuses an address within a single flush
/// cycle (alloc → free → alloc-with-same-addr); naive "drain all allocs,
/// then all frees" would race and corrupt the liveset.
pub(crate) struct MemoryProfileSource {
    rings: Arc<RingBuffers>,
    liveset: Option<HashMap<u64, LivesetEntry>>,
    /// Previous snapshot of `RingBuffers::dropped_allocs` for delta computation.
    prev_dropped_allocs: u64,
    /// Previous snapshot of `RingBuffers::dropped_frees` for delta computation.
    prev_dropped_frees: u64,
    /// Precomputed segment metadata, returned (cloned) on every flush
    /// cycle. Cached in `new()` so the `segment_metadata()` hot path —
    /// called from the flush loop every ~5 ms — does not allocate fresh
    /// `String`s per cycle.
    metadata: Vec<(String, String)>,
}

impl MemoryProfileSource {
    /// Create a new source that drains the supplied ring buffers.
    ///
    /// `track_liveset = true` enables `FreeEvent` emission (matched against
    /// previously-sampled allocations); `false` means frees are silently
    /// dropped on the consumer side.
    pub(crate) fn new(
        rings: Arc<RingBuffers>,
        track_liveset: bool,
        sample_rate_bytes: u64,
    ) -> Self {
        Self {
            rings,
            liveset: track_liveset.then(HashMap::new),
            prev_dropped_allocs: 0,
            prev_dropped_frees: 0,
            metadata: vec![(
                "memory.sample_rate_bytes".to_string(),
                sample_rate_bytes.to_string(),
            )],
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
        with_encoder(
            |enc| {
                let callchain = enc.intern_stack_frames(&frames[..frame_count]);
                enc.encode(&AllocEvent {
                    timestamp_ns: ts_ns,
                    tid,
                    size,
                    addr,
                    callchain,
                });
            },
            ctx.collector,
            ctx.drain_epoch,
        );
        if let Some(liveset) = self.liveset.as_mut() {
            liveset.insert(
                addr,
                LivesetEntry {
                    size,
                    timestamp_ns: ts_ns,
                },
            );
        }
    }

    fn handle_free(&mut self, f: RawFree, ctx: &FlushContext<'_>) {
        let Some(liveset) = self.liveset.as_mut() else {
            return;
        };
        let Some(entry) = liveset.remove(&f.addr) else {
            return;
        };
        with_encoder(
            |enc| {
                enc.encode(&FreeEvent {
                    timestamp_ns: f.ts_ns,
                    tid: f.tid,
                    addr: f.addr,
                    size: entry.size,
                    alloc_timestamp_ns: entry.timestamp_ns,
                });
            },
            ctx.collector,
            ctx.drain_epoch,
        );
    }
}

impl Source for MemoryProfileSource {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        // Merge-sort drain by timestamp. This produces a best-effort
        // timestamp-ordered stream. Ordering is not guaranteed to be perfect:
        // - Multiple producers push concurrently, so queue order may not
        //   match timestamp order.
        // - TimestampMode::ReusePollStart can produce stale timestamps.
        // For profiling purposes, approximate ordering is sufficient.
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
            with_encoder(
                |enc| {
                    enc.encode(&MemoryProfileOverflowEvent {
                        timestamp_ns: clock_monotonic_ns(),
                        dropped_allocs: delta_allocs,
                        dropped_frees: delta_frees,
                    });
                },
                ctx.collector,
                ctx.drain_epoch,
            );
            self.prev_dropped_allocs = current_dropped_allocs;
            self.prev_dropped_frees = current_dropped_frees;
        }
    }

    fn name(&self) -> &'static str {
        "memory"
    }

    fn segment_metadata(&self) -> Vec<(String, String)> {
        // Cached in `new()`. Cloned on every flush cycle (a tiny vec —
        // typically one entry — so the clone cost is dwarfed by the
        // surrounding lock acquisitions in the flush loop). Was previously
        // rebuilding two `String`s per cycle; see PR #442 review.
        self.metadata.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_profiling::ring::{DEFAULT_MAX_FRAMES, RawAlloc, RawFree, RingBuffers};
    use crate::primitives::sync::Arc;
    use crate::primitives::sync::atomic::Ordering;
    use crate::telemetry::analysis_events::Dial9Event;
    use crate::telemetry::buffer;
    use crate::telemetry::format::decode_events;
    use crate::telemetry::recorder::SharedState;

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

    fn make_raw_free(addr: u64, ts_ns: u64) -> RawFree {
        RawFree {
            tid: 2,
            addr,
            ts_ns,
        }
    }

    fn rings(alloc_cap: usize, free_cap: usize) -> Arc<RingBuffers> {
        Arc::new(RingBuffers::new(alloc_cap, free_cap))
    }

    fn new_shared() -> SharedState {
        let shared = SharedState::new(0, None);
        shared.enabled.store(true, Ordering::Relaxed);
        shared
    }

    fn flush_and_collect(shared: &SharedState) -> Vec<Dial9Event> {
        shared.flush_sources();
        buffer::drain_to_collector(&shared.collector);
        let mut events = Vec::new();
        while let Some(batch) = shared.collector.next() {
            if let Ok(decoded) = decode_events(&batch.encoded_bytes) {
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
        shared.push_source(Box::new(MemoryProfileSource::new(
            Arc::clone(&rings),
            false,
            512 * 1024,
        )));

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
        rings.free_queue.push(make_raw_free(0x2000, 300)).ok();

        let shared = new_shared();
        shared.push_source(Box::new(MemoryProfileSource::new(
            Arc::clone(&rings),
            true,
            512 * 1024,
        )));

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

    #[test]
    fn free_without_alloc_is_silently_dropped() {
        let rings = rings(16, 16);
        rings.free_queue.push(make_raw_free(0x9999, 400)).ok();

        let shared = new_shared();
        shared.push_source(Box::new(MemoryProfileSource::new(
            Arc::clone(&rings),
            true,
            512 * 1024,
        )));

        let events = flush_and_collect(&shared);
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::FreeEvent(..)))
            .collect();
        assert_eq!(frees.len(), 0);
    }

    #[test]
    fn liveset_off_drops_all_frees() {
        let rings = rings(16, 16);
        rings
            .alloc_queue
            .push(make_raw_alloc(0x3000, 128, 500))
            .ok();
        rings.free_queue.push(make_raw_free(0x3000, 600)).ok();

        let shared = new_shared();
        shared.push_source(Box::new(MemoryProfileSource::new(
            Arc::clone(&rings),
            false,
            512 * 1024,
        )));

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
        shared.push_source(Box::new(MemoryProfileSource::new(
            Arc::clone(&rings),
            true,
            512 * 1024,
        )));

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

        // Second flush: the free arrives
        rings.free_queue.push(make_raw_free(0x4000, 800)).ok();
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

    /// Regression test for the address-reuse race during a single flush cycle.
    ///
    /// Sequence at the producer (timestamps strictly increasing):
    ///   t=100  alloc 0x5000 (size 256)   → alloc_queue
    ///   t=200  free  0x5000              → free_queue
    ///   t=300  alloc 0x5000 (size 512)   → alloc_queue
    ///
    /// Naïve "drain all allocs, then all frees" emits:
    ///   alloc(t=100, size=256), alloc(t=300, size=512), free(t=200)
    /// and the free incorrectly evicts the *second* alloc (size=512, t=300)
    /// from the liveset — the second allocation looks freed even though it's
    /// still live.
    ///
    /// Timestamp-ordered drain emits alloc, free, alloc and the second
    /// allocation correctly remains in the liveset.
    #[test]
    fn address_reuse_within_flush_cycle_preserves_liveset() {
        let rings = rings(16, 16);
        rings
            .alloc_queue
            .push(make_raw_alloc(0x5000, 256, 100))
            .ok();
        rings.free_queue.push(make_raw_free(0x5000, 200)).ok();
        rings
            .alloc_queue
            .push(make_raw_alloc(0x5000, 512, 300))
            .ok();

        let shared = new_shared();
        shared.push_source(Box::new(MemoryProfileSource::new(
            Arc::clone(&rings),
            true,
            512 * 1024,
        )));

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

        // The single free must match the *first* allocation (size=256, t=100).
        // If drain order is wrong, the free would match alloc2 (size=512).
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

        // The second allocation must remain live in the liveset.
        // Prove it by freeing the address in a second flush cycle and checking
        // the emitted FreeEvent carries the second alloc's size and timestamp.
        rings.free_queue.push(make_raw_free(0x5000, 400)).ok();
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
    /// same address).
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
        rings.free_queue.push(make_raw_free(0x6000, t2)).ok();
        rings.alloc_queue.push(make_raw_alloc(0x6000, 512, t3)).ok();

        let shared = new_shared();
        shared.push_source(Box::new(MemoryProfileSource::new(
            Arc::clone(&rings),
            true,
            512 * 1024,
        )));
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

        // Second alloc remains live.
        // Prove it by freeing the address in a second flush cycle.
        rings.free_queue.push(make_raw_free(0x6000, t3 + 1)).ok();
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
        let source = MemoryProfileSource::new(Arc::clone(&rings), false, 1024 * 1024);
        let meta = source.segment_metadata();
        assert_eq!(
            meta,
            vec![(
                "memory.sample_rate_bytes".to_string(),
                "1048576".to_string()
            )]
        );
    }
}

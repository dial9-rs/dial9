//! `Source` impl that drains the alloc and free queues each flush cycle.

use crate::memory_profiling::ring::{DEFAULT_MAX_FRAMES, RawAlloc, RawFree, RingBuffers};
use crate::primitives::sync::Arc;
use crate::telemetry::buffer::with_encoder;
use crate::telemetry::format::{AllocEvent, FreeEvent};
use crate::telemetry::recorder::source::{FlushContext, Source};
use std::collections::HashMap;

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
pub(crate) struct MemoryProfileSource<const MAX_FRAMES: usize = DEFAULT_MAX_FRAMES> {
    rings: Arc<RingBuffers<MAX_FRAMES>>,
    liveset: Option<HashMap<u64, LivesetEntry>>,
}

impl<const MAX_FRAMES: usize> MemoryProfileSource<MAX_FRAMES> {
    /// Create a new source that drains the supplied ring buffers.
    ///
    /// `track_liveset = true` enables `FreeEvent` emission (matched against
    /// previously-sampled allocations); `false` means frees are silently
    /// dropped on the consumer side.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wired up by allocator hook in a later commit")
    )]
    pub(crate) fn new(rings: Arc<RingBuffers<MAX_FRAMES>>, track_liveset: bool) -> Self {
        Self {
            rings,
            liveset: track_liveset.then(HashMap::new),
        }
    }

    fn handle_alloc(&mut self, a: RawAlloc<MAX_FRAMES>, ctx: &FlushContext<'_>) {
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

impl<const MAX_FRAMES: usize> Source for MemoryProfileSource<MAX_FRAMES> {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        // Merge-sort drain by timestamp. Hold one peeked element from each
        // queue and emit the older one. `crossbeam_queue::ArrayQueue` has no
        // peek API, so we pop into local slots and only refill after we
        // emit. The producer can race in between; that's fine — anything it
        // pushes during this loop has a timestamp later than anything we've
        // already emitted, and we either pick it up this cycle (if our last
        // pop sees it) or next cycle.
        let mut next_alloc: Option<RawAlloc<MAX_FRAMES>> = self.rings.alloc_queue.pop();
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
    }

    fn name(&self) -> &'static str {
        "memory"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_profiling::ring::{DEFAULT_MAX_FRAMES, RawAlloc, RawFree, RingBuffers};
    use crate::primitives::sync::Arc;
    use crate::primitives::sync::atomic::AtomicU64;
    use crate::telemetry::buffer::drain_to_collector;
    use crate::telemetry::collector::CentralCollector;
    use crate::telemetry::events::{TelemetryEvent, ThreadRole};
    use crate::telemetry::format::decode_events;
    use crate::telemetry::recorder::source::FlushContext;
    use std::collections::HashMap;

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
            size: 0, // size on the free side is informational; consolidator uses liveset
            ts_ns,
        }
    }

    fn rings(alloc_cap: usize, free_cap: usize) -> Arc<RingBuffers> {
        Arc::new(RingBuffers::new(alloc_cap, free_cap))
    }

    fn flush_and_collect(source: &mut MemoryProfileSource) -> Vec<TelemetryEvent> {
        let collector = Arc::new(CentralCollector::new());
        let drain_epoch = AtomicU64::new(0);
        let thread_roles: HashMap<u32, ThreadRole> = HashMap::new();
        let ctx = FlushContext {
            collector: &collector,
            drain_epoch: &drain_epoch,
            thread_roles: &thread_roles,
        };
        source.flush(&ctx);
        drain_to_collector(&collector);
        let mut events = Vec::new();
        while let Some(batch) = collector.next() {
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

        let mut source = MemoryProfileSource::new(Arc::clone(&rings), false);

        let events = flush_and_collect(&mut source);
        let allocs: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
            .collect();
        assert_eq!(allocs.len(), 1);
        match &allocs[0] {
            TelemetryEvent::Alloc {
                timestamp_nanos,
                tid,
                size,
                addr,
                callchain,
            } => {
                assert_eq!(*timestamp_nanos, 100);
                assert_eq!(*tid, 1);
                assert_eq!(*size, 4096);
                assert_eq!(*addr, 0x1000);
                assert_eq!(callchain, &[0xAAAA, 0xBBBB, 0xCCCC]);
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

        let mut source = MemoryProfileSource::new(Arc::clone(&rings), true);

        let events = flush_and_collect(&mut source);
        let allocs: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
            .collect();
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
            .collect();
        assert_eq!(allocs.len(), 1);
        assert_eq!(frees.len(), 1);
        match &frees[0] {
            TelemetryEvent::Free {
                timestamp_nanos,
                tid,
                addr,
                size,
                alloc_timestamp_nanos,
            } => {
                assert_eq!(*timestamp_nanos, 300);
                assert_eq!(*tid, 2);
                assert_eq!(*addr, 0x2000);
                assert_eq!(*size, 512);
                assert_eq!(*alloc_timestamp_nanos, 200);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn free_without_alloc_is_silently_dropped() {
        let rings = rings(16, 16);
        rings.free_queue.push(make_raw_free(0x9999, 400)).ok();

        let mut source = MemoryProfileSource::new(Arc::clone(&rings), true);

        let events = flush_and_collect(&mut source);
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
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

        let mut source = MemoryProfileSource::new(Arc::clone(&rings), false);

        let events = flush_and_collect(&mut source);
        let allocs: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
            .collect();
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
            .collect();
        assert_eq!(allocs.len(), 1);
        assert_eq!(frees.len(), 0);
    }

    #[test]
    fn alloc_then_free_in_separate_flush_cycles() {
        let rings = rings(16, 16);
        let mut source = MemoryProfileSource::new(Arc::clone(&rings), true);

        // First flush: only the alloc
        rings
            .alloc_queue
            .push(make_raw_alloc(0x4000, 256, 700))
            .ok();
        let events = flush_and_collect(&mut source);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
                .count(),
            0
        );

        // Second flush: the free arrives
        rings.free_queue.push(make_raw_free(0x4000, 800)).ok();
        let events = flush_and_collect(&mut source);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
                .count(),
            0
        );
        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
            .collect();
        assert_eq!(frees.len(), 1);
        match &frees[0] {
            TelemetryEvent::Free {
                size,
                alloc_timestamp_nanos,
                ..
            } => {
                assert_eq!(*size, 256);
                assert_eq!(*alloc_timestamp_nanos, 700);
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

        let mut source = MemoryProfileSource::new(Arc::clone(&rings), true);

        let events = flush_and_collect(&mut source);
        let allocs: Vec<&TelemetryEvent> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
            .collect();
        let frees: Vec<&TelemetryEvent> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
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
            TelemetryEvent::Free {
                size,
                alloc_timestamp_nanos,
                addr,
                ..
            } => {
                assert_eq!(*addr, 0x5000);
                assert_eq!(*size, 256, "free should report size from first alloc");
                assert_eq!(
                    *alloc_timestamp_nanos, 100,
                    "free should reference timestamp of first alloc"
                );
            }
            _ => unreachable!(),
        }

        // The second allocation must remain live in the liveset.
        let liveset = source.liveset.as_ref().expect("liveset is on");
        assert_eq!(liveset.len(), 1, "second alloc should still be live");
        let entry = liveset
            .get(&0x5000)
            .expect("addr 0x5000 should be in liveset");
        assert_eq!(entry.size, 512);
        assert_eq!(entry.timestamp_ns, 300);
    }

    /// Demonstrates that `poll_start_ts_or_now` produces strictly ordered
    /// timestamps even for events that would otherwise share a clock tick —
    /// the scenario that occurs during a realloc (free old + alloc new at
    /// same address).
    #[test]
    fn monotonic_ts_solves_realloc_ordering() {
        use crate::telemetry::recorder::poll_start_ts_monotonic;

        // Simulate a realloc: alloc, free, alloc — all at the "same instant".
        // poll_start_ts_or_now guarantees each gets a distinct, increasing timestamp.
        let t1 = poll_start_ts_monotonic();
        let t2 = poll_start_ts_monotonic();
        let t3 = poll_start_ts_monotonic();
        assert!(t1 < t2 && t2 < t3, "timestamps must be strictly ordered");

        let rings = rings(16, 16);
        rings.alloc_queue.push(make_raw_alloc(0x6000, 256, t1)).ok();
        rings.free_queue.push(make_raw_free(0x6000, t2)).ok();
        rings.alloc_queue.push(make_raw_alloc(0x6000, 512, t3)).ok();

        let mut source = MemoryProfileSource::new(Arc::clone(&rings), true);
        let events = flush_and_collect(&mut source);

        let frees: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
            .collect();
        assert_eq!(frees.len(), 1);
        match frees[0] {
            TelemetryEvent::Free {
                size,
                alloc_timestamp_nanos,
                ..
            } => {
                assert_eq!(*size, 256, "free should match the first alloc");
                assert_eq!(*alloc_timestamp_nanos, t1);
            }
            _ => unreachable!(),
        }

        // Second alloc remains live.
        let liveset = source.liveset.as_ref().unwrap();
        assert_eq!(liveset.len(), 1);
        let entry = liveset.get(&0x6000).unwrap();
        assert_eq!(entry.size, 512);
        assert_eq!(entry.timestamp_ns, t3);
    }
}

//! `Source` impl that drains the memory profiler into the trace each flush.

use crate::memory_profiling::clock::clock_monotonic_ns;
use crate::memory_profiling::events::{AllocEvent, FreeEvent, MemoryProfileOverflowEvent};
use crate::memory_profiling::profiler::{MemorySample, MemorySampler};
use dial9_core::source::{FlushContext, Source};

/// Wraps an installed [`MemorySampler`] and emits each drained allocation and
/// free as a trace event. The ordering and shutdown-free resolution live on
/// the sampler, this stage only encodes.
pub struct MemoryProfileSource {
    sampler: MemorySampler,
    /// Precomputed segment metadata. Fixed at construction and never changes,
    /// so it is appended on the first flush and otherwise left in the writer's
    /// merged cache, which re-emits it on every rotation.
    metadata: Vec<(String, String)>,
    /// Whether `metadata` has been appended yet. The fixed metadata is emitted
    /// once on the first flush; later flushes report no change.
    emitted: bool,
}

impl MemoryProfileSource {
    /// Wrap an installed [`MemorySampler`].
    pub fn new(sampler: MemorySampler) -> Self {
        let metadata = vec![(
            "memory.sample_rate_bytes".to_string(),
            sampler.sample_rate_bytes().to_string(),
        )];
        Self {
            sampler,
            metadata,
            emitted: false,
        }
    }
}

impl Source for MemoryProfileSource {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        self.sampler.drain(|sample| match sample {
            MemorySample::Alloc(a) => ctx.with_encoder(|enc| {
                let callchain = enc.intern_stack_frames(a.callchain);
                enc.encode(&AllocEvent {
                    timestamp_ns: a.timestamp_ns,
                    tid: a.tid,
                    size: a.size,
                    addr: a.addr,
                    callchain,
                });
            }),
            MemorySample::Free(fr) => ctx.with_encoder(|enc| {
                enc.encode(&FreeEvent {
                    timestamp_ns: fr.timestamp_ns,
                    tid: fr.tid,
                    addr: fr.addr,
                    size: fr.size,
                    alloc_timestamp_ns: fr.alloc_timestamp_ns,
                });
            }),
        });

        // Emit an overflow event if any samples were dropped since last flush.
        let dropped = self.sampler.take_dropped();
        if dropped.allocs > 0 || dropped.frees > 0 {
            ctx.with_encoder(|enc| {
                enc.encode(&MemoryProfileOverflowEvent {
                    timestamp_ns: clock_monotonic_ns(),
                    dropped_allocs: dropped.allocs,
                    dropped_frees: dropped.frees,
                });
            });
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
    use dial9_core::shared_state::SharedState;
    use dial9_core::test_util;
    use dial9_trace_format::decoder::Decoder;
    use std::sync::Arc;

    fn rings(alloc_cap: usize, free_cap: usize) -> Arc<RingBuffers> {
        Arc::new(RingBuffers::new(alloc_cap, free_cap))
    }

    fn make_raw_alloc(addr: u64, size: u64, ts_ns: u64) -> RawAlloc {
        let mut frames = [0u64; DEFAULT_MAX_FRAMES];
        frames[0] = 0xAAAA;
        frames[1] = 0xBBBB;
        RawAlloc {
            tid: 1,
            size,
            addr,
            ts_ns,
            frames,
            frame_count: 2,
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

    /// A source wrapping a sampler over `rings`, with liveset tracking on so
    /// frees are emitted.
    fn source_over(rings: Arc<RingBuffers>) -> MemoryProfileSource {
        let sampler = MemorySampler::new_for_test(rings, true, 512 * 1024);
        MemoryProfileSource::new(sampler)
    }

    fn new_shared() -> SharedState {
        let shared = SharedState::new(0);
        shared.enable();
        shared
    }

    /// Flush all sources and decode every emitted event into a JSON value.
    fn flush_and_collect(shared: &SharedState) -> Vec<serde_json::Value> {
        shared.flush_sources();
        let mut events = Vec::new();
        for bytes in test_util::drain_encoded_batches(shared) {
            let Some(mut dec) = Decoder::new(&bytes) else {
                continue;
            };
            dec.for_each_event(|raw| {
                if let Ok(v) = raw.deserialize::<serde_json::Value>() {
                    events.push(v);
                }
            })
            .ok();
        }
        events
    }

    fn of_kind<'a>(events: &'a [serde_json::Value], k: &str) -> Vec<&'a serde_json::Value> {
        events
            .iter()
            .filter(|v| v["event"].as_str() == Some(k))
            .collect()
    }

    fn field(v: &serde_json::Value, name: &str) -> u64 {
        v[name].as_u64().unwrap_or_else(|| panic!("missing {name}"))
    }

    /// End-to-end through the dial9 `Source`: a queued alloc + matching free
    /// encode into `AllocEvent`/`FreeEvent` with the right fields.
    #[test]
    fn source_encodes_alloc_and_free() {
        let rings = rings(16, 16);
        rings
            .alloc_queue
            .push(make_raw_alloc(0x2000, 512, 200))
            .ok();
        rings
            .free_queue
            .push(make_raw_free(0x2000, 300, 512, 200))
            .ok();

        let shared = new_shared();
        shared.push_source(Box::new(source_over(Arc::clone(&rings))));

        let events = flush_and_collect(&shared);
        let allocs = of_kind(&events, "AllocEvent");
        let frees = of_kind(&events, "FreeEvent");
        assert_eq!(allocs.len(), 1);
        assert_eq!(field(allocs[0], "size"), 512);
        assert_eq!(field(allocs[0], "addr"), 0x2000);
        assert_eq!(frees.len(), 1);
        assert_eq!(field(frees[0], "size"), 512);
        assert_eq!(field(frees[0], "alloc_timestamp_ns"), 200);
    }

    /// A ring overflow surfaces as a `MemoryProfileOverflowEvent`.
    #[test]
    fn source_emits_overflow_event() {
        // alloc queue holds 1; pushing 3 drops 2.
        let rings = rings(1, 1);
        for i in 0..3 {
            rings.push_alloc(make_raw_alloc(0x3000 + i, 64, 100 + i));
        }

        let shared = new_shared();
        shared.push_source(Box::new(source_over(Arc::clone(&rings))));

        let events = flush_and_collect(&shared);
        let overflow = of_kind(&events, "MemoryProfileOverflowEvent");
        assert_eq!(overflow.len(), 1);
        assert_eq!(field(overflow[0], "dropped_allocs"), 2);
    }

    #[test]
    fn segment_metadata_contains_sample_rate_bytes() {
        let sampler = MemorySampler::new_for_test(rings(16, 16), false, 1024 * 1024);
        let mut source = MemoryProfileSource::new(sampler);
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
}

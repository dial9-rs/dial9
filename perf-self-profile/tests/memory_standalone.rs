//! Standalone memory profiling without dial9 integration.
#![cfg(all(target_os = "linux", feature = "memory-profiling"))]

use dial9_perf_self_profile::memory_profiling::{
    MemoryProfiler, MemoryProfilingConfig, MemorySample, SamplingAllocator,
};

#[global_allocator]
static ALLOC: SamplingAllocator = SamplingAllocator::system();

#[test]
fn standalone_drain_yields_real_allocations() {
    let mut sampler = MemoryProfiler::from_config(
        MemoryProfilingConfig::builder()
            .sample_rate_bytes(512)
            .track_liveset(true)
            .rng_seed(7)
            .build(),
    )
    .install()
    .expect("standalone install should succeed");

    // Generate sampled allocations.
    let mut sink: Vec<Vec<u8>> = Vec::new();
    for i in 0..2000 {
        sink.push(Vec::with_capacity(256 + (i % 256)));
    }
    std::hint::black_box(&sink);
    drop(sink);

    let mut allocs = 0usize;
    let mut frees = 0usize;
    let mut max_frames = 0usize;
    sampler.drain(|s| match s {
        MemorySample::Alloc(a) => {
            allocs += 1;
            max_frames = max_frames.max(a.callchain.len());
            assert!(a.size > 0, "alloc sample size should be non-zero");
        }
        MemorySample::Free(_) => frees += 1,
        _ => {}
    });

    assert!(allocs > 0, "standalone drain captured no allocations");
    assert!(
        max_frames > 0,
        "expected at least one captured stack frame across the samples"
    );
    eprintln!("standalone: {allocs} allocs, {frees} frees, deepest stack {max_frames} frames");
}

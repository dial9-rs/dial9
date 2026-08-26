#![cfg(all(
    feature = "memory-profiling",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use dial9_core::buffer::MemoryBuffer;
use dial9_core::recorder::recorder;
use dial9_perf_self_profile::memory_profiling::{
    Dial9Allocator, MemoryProfiler, MemoryProfilingConfig,
};
use std::time::Duration;

#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

#[test]
fn allocation_hook_survives_best_effort_stack_capture() {
    let recorder = recorder(MemoryBuffer::new(1 << 20).expect("memory buffer")).build();
    let config = MemoryProfilingConfig::builder()
        .sample_rate_bytes(1)
        .ring_capacity(4096)
        .build();
    let _guard = MemoryProfiler::from_config(config)
        .install(recorder.handle().clone())
        .expect("memory profiler install");

    for size in 1..=2048 {
        let allocation = vec![0u8; size];
        std::hint::black_box(allocation);
    }

    recorder.graceful_shutdown(Duration::from_secs(1));
}

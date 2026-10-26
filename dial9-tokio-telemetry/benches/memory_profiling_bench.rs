//! Integration benchmark for memory profiling.
//!
//! Measures the overhead of the Dial9Allocator hook path by calling
//! GlobalAlloc methods directly on plain and Dial9-wrapped System and jemalloc.
//! The process-global allocator is unchanged; application allocation workloads
//! live in allocator_workload_{plain,dial9}.
//!
//! Usage: `cargo bench -p dial9-tokio-telemetry --features memory-profiling \
//!     --bench memory_profiling_bench`
//!
//! Four groups:
//! - tight_alloc: high-frequency small allocs, measures fast-path cost.
//! - zeroed_alloc: exercises the alloc_zeroed path.
//! - realloc_growth: Vec::push pattern, exercises the realloc path.
//! - mixed_sizes: varying sizes across the sample-rate boundary.
//!
//! NOTE: The profiler is installed once at process start via BENCH_CONFIG
//! env var. Without it, we measure the "no profiler" baseline (just the
//! OnceLock::get() check). With BENCH_CONFIG=sampling_only or
//! BENCH_CONFIG=sampling_with_liveset, the full hook path is exercised.
//!
//! Run all three configs:
//!   cargo bench --bench memory_profiling_bench --features memory-profiling
//!   BENCH_CONFIG=sampling_only cargo bench --bench memory_profiling_bench --features memory-profiling
//!   BENCH_CONFIG=sampling_with_liveset cargo bench --bench memory_profiling_bench --features memory-profiling

use criterion::{Criterion, Throughput};
use dial9_tokio_telemetry::memory_profiling::Dial9Allocator;
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;

fn bench_tight_alloc(c: &mut Criterion, allocator: &impl GlobalAlloc, label: &str) {
    let layout = Layout::from_size_align(64, 8).unwrap();

    let mut group = c.benchmark_group("tight_alloc");
    group.throughput(Throughput::Bytes(64));
    group.bench_function(label, |b| {
        b.iter(|| {
            // SAFETY: layout is valid, we dealloc immediately.
            let ptr = unsafe { allocator.alloc(black_box(layout)) };
            assert!(!ptr.is_null());
            unsafe { allocator.dealloc(black_box(ptr), layout) };
        });
    });
    group.finish();
}

fn bench_zeroed_alloc(c: &mut Criterion, allocator: &impl GlobalAlloc, label: &str) {
    let layout = Layout::from_size_align(64, 8).unwrap();

    let mut group = c.benchmark_group("zeroed_alloc");
    group.throughput(Throughput::Bytes(64));
    group.bench_function(label, |b| {
        b.iter(|| {
            // SAFETY: layout is valid, we dealloc immediately.
            let ptr = unsafe { allocator.alloc_zeroed(black_box(layout)) };
            assert!(!ptr.is_null());
            unsafe { allocator.dealloc(black_box(ptr), layout) };
        });
    });
    group.finish();
}

fn bench_realloc_growth(c: &mut Criterion, allocator: &impl GlobalAlloc, label: &str) {
    let mut group = c.benchmark_group("realloc_growth");
    // Total bytes touched: 64+128+256+512+1024+2048+4096 = 8128
    group.throughput(Throughput::Bytes(8128));
    group.bench_function(label, |b| {
        b.iter(|| {
            let layout = Layout::from_size_align(64, 8).unwrap();
            // SAFETY: layout is valid.
            let mut ptr = unsafe { allocator.alloc(black_box(layout)) };
            assert!(!ptr.is_null());
            let mut current_layout = layout;
            let mut size = 128usize;
            while size <= 4096 {
                // SAFETY: ptr was allocated with current_layout, new_size > 0.
                let new_ptr =
                    unsafe { allocator.realloc(black_box(ptr), current_layout, black_box(size)) };
                assert!(!new_ptr.is_null());
                ptr = new_ptr;
                current_layout = Layout::from_size_align(size, 8).unwrap();
                size *= 2;
            }
            // SAFETY: ptr was allocated with current_layout.
            unsafe { allocator.dealloc(black_box(ptr), current_layout) };
        });
    });
    group.finish();
}

fn bench_mixed_sizes(c: &mut Criterion, allocator: &impl GlobalAlloc, label: &str) {
    let sizes: &[usize] = &[8, 64, 256, 1024, 64, 16, 8192, 32, 512, 4096];
    let total_bytes: u64 = sizes.iter().map(|&s| s as u64).sum();

    let mut group = c.benchmark_group("mixed_sizes");
    group.throughput(Throughput::Bytes(total_bytes));
    group.bench_function(label, |b| {
        b.iter(|| {
            for &size in sizes {
                let layout = Layout::from_size_align(size, 8).unwrap();
                // SAFETY: layout is valid.
                let ptr = unsafe { allocator.alloc(black_box(layout)) };
                assert!(!ptr.is_null());
                // SAFETY: ptr was allocated with this layout.
                unsafe { allocator.dealloc(black_box(ptr), layout) };
            }
        });
    });
    group.finish();
}

fn install_profiler() {
    use dial9_tokio_telemetry::memory_profiling::{MemoryProfiler, MemoryProfilingConfig};
    use dial9_tokio_telemetry::telemetry::{
        Dial9HandleTokioExt, MemoryBuffer, TokioAttachOptions, recorder,
    };

    // We leak the runtime and recorder so they live for the process lifetime.
    // This is intentional — the profiler is process-permanent anyway.
    let recorder = recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap()).build();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(1);
    let runtime = recorder
        .handle()
        .attach_tokio_runtime(builder, TokioAttachOptions::default())
        .unwrap();
    let handle = recorder.handle().clone();

    let track_liveset = matches!(
        std::env::var("BENCH_CONFIG").as_deref(),
        Ok("sampling_with_liveset")
    );

    let cfg = MemoryProfilingConfig::builder()
        .sample_rate_bytes(512 * 1024)
        .track_liveset(track_liveset)
        .rng_seed(42)
        .build();

    let _mem_guard = MemoryProfiler::from_config(cfg)
        .install(handle)
        .expect("profiler install should succeed in bench");

    // Leak everything to keep the profiler alive for the process.
    std::mem::forget(runtime);
    std::mem::forget(recorder);
    // _mem_guard doesn't implement Drop — dropping is fine.
}

fn main() {
    match std::env::var("BENCH_CONFIG").as_deref() {
        Ok("sampling_only") | Ok("sampling_with_liveset") => install_profiler(),
        Ok("none") | Err(std::env::VarError::NotPresent) => {}
        other => panic!("unknown BENCH_CONFIG: {other:?}"),
    }

    let mut criterion = Criterion::default().configure_from_args();
    let system = Dial9Allocator::system();
    // Keep the existing wrapped-System benchmark names for history.
    bench_tight_alloc(&mut criterion, &system, "alloc_dealloc_64b");
    bench_zeroed_alloc(&mut criterion, &system, "alloc_zeroed_dealloc_64b");
    bench_realloc_growth(&mut criterion, &system, "64_to_4096");
    bench_mixed_sizes(&mut criterion, &system, "10_mixed_allocs");
    benchmark_allocator(&mut criterion, &System, "system_plain");
    #[cfg(unix)]
    {
        benchmark_allocator(
            &mut criterion,
            &tikv_jemallocator::Jemalloc,
            "jemalloc_plain",
        );
        benchmark_allocator(
            &mut criterion,
            &Dial9Allocator::new(tikv_jemallocator::Jemalloc),
            "jemalloc_dial9",
        );
    }
    criterion.final_summary();
}

fn benchmark_allocator(c: &mut Criterion, allocator: &impl GlobalAlloc, label: &str) {
    bench_tight_alloc(c, allocator, label);
    bench_zeroed_alloc(c, allocator, label);
    bench_realloc_growth(c, allocator, label);
    bench_mixed_sizes(c, allocator, label);
}

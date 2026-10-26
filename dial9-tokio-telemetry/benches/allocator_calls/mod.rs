//! Time individual global allocator operations; setup and cleanup stay outside timing.

use criterion::{Criterion, Throughput};
use std::alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc};
use std::hint::black_box;
use std::time::{Duration, Instant};

// Amortize timer reads while bounding the live allocation set to 64–128 KiB.
const BATCH: usize = 1024;

#[derive(Clone, Copy)]
enum Operation {
    Alloc,
    AllocZeroed,
    Dealloc,
    Realloc,
}

fn measure(operation: Operation, iterations: u64) -> Duration {
    let old_layout = Layout::from_size_align(64, 8).unwrap();
    let new_layout = Layout::from_size_align(128, 8).unwrap();
    let mut pointers = vec![std::ptr::null_mut(); BATCH];
    let mut remaining = iterations;
    let mut elapsed = Duration::ZERO;

    while remaining > 0 {
        let count = usize::try_from(remaining.min(BATCH as u64)).unwrap();
        let slots = &mut pointers[..count];

        if matches!(operation, Operation::Dealloc | Operation::Realloc) {
            for slot in slots.iter_mut() {
                // SAFETY: valid layout; the matching operation below owns this block.
                *slot = unsafe { alloc(black_box(old_layout)) };
                assert!(!slot.is_null());
                if matches!(operation, Operation::Realloc) {
                    // SAFETY: initialize the live block before checking preserved contents.
                    unsafe { (*slot).write_bytes(0xa5, old_layout.size()) };
                }
            }
        }

        let start = Instant::now();
        match operation {
            Operation::Alloc => {
                for slot in slots.iter_mut() {
                    // SAFETY: valid layout; cleanup below releases the block.
                    *slot = unsafe { alloc(black_box(old_layout)) };
                }
            }
            Operation::AllocZeroed => {
                for slot in slots.iter_mut() {
                    // SAFETY: valid layout; cleanup below releases the block.
                    *slot = unsafe { alloc_zeroed(black_box(old_layout)) };
                }
            }
            Operation::Dealloc => {
                for slot in slots.iter() {
                    // SAFETY: every pointer was allocated with old_layout above.
                    unsafe { dealloc(black_box(*slot), black_box(old_layout)) };
                }
            }
            Operation::Realloc => {
                for slot in slots.iter_mut() {
                    // SAFETY: this pointer is live with old_layout; new_size is nonzero.
                    *slot =
                        unsafe { realloc(black_box(*slot), black_box(old_layout), black_box(128)) };
                }
            }
        }
        elapsed += start.elapsed();

        if !matches!(operation, Operation::Dealloc) {
            for slot in slots.iter() {
                assert!(!slot.is_null(), "allocation failed");
                let layout = if matches!(operation, Operation::Realloc) {
                    // SAFETY: successful realloc preserves the old 64-byte contents.
                    assert_eq!(unsafe { (*slot).read() }, 0xa5);
                    assert_eq!(unsafe { (*slot).add(63).read() }, 0xa5);
                    new_layout
                } else {
                    if matches!(operation, Operation::AllocZeroed) {
                        // SAFETY: the entire successful allocation is initialized to zero.
                        assert_eq!(unsafe { (*slot).read() }, 0);
                        assert_eq!(unsafe { (*slot).add(63).read() }, 0);
                    }
                    old_layout
                };
                // SAFETY: the operation produced this live block with this layout.
                unsafe { dealloc(*slot, layout) };
            }
        }
        remaining -= count as u64;
    }
    // Each iteration accounts for one operation, rather than an alloc/free pair.
    elapsed
}

fn install_profiler() {
    use dial9_tokio_telemetry::memory_profiling::{MemoryProfiler, MemoryProfilingConfig};
    use dial9_tokio_telemetry::telemetry::{MemoryBuffer, recorder};

    let recorder = recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap()).build();
    let config = MemoryProfilingConfig::builder()
        .sample_rate_bytes(512 * 1024)
        .track_liveset(std::env::var("BENCH_CONFIG").as_deref() == Ok("sampling_with_liveset"))
        .rng_seed(42)
        .build();
    let _guard = MemoryProfiler::from_config(config)
        .install(recorder.handle().clone())
        .expect("profiler install");
    // Keep installation lifetime identical in plain and wrapped executables.
    // Plain allocators do not invoke the installed hooks.
    std::mem::forget(recorder);
}

pub fn run() {
    match std::env::var("BENCH_CONFIG").as_deref() {
        Ok("none") | Err(std::env::VarError::NotPresent) => {}
        Ok("sampling_only") | Ok("sampling_with_liveset") => install_profiler(),
        other => panic!("unknown BENCH_CONFIG: {other:?}"),
    }

    let mut criterion = Criterion::default().configure_from_args();
    let mut group = criterion.benchmark_group("allocator_call");
    group.throughput(Throughput::Elements(1));
    for (name, operation) in [
        ("alloc_64b", Operation::Alloc),
        ("alloc_zeroed_64b", Operation::AllocZeroed),
        ("dealloc_64b", Operation::Dealloc),
        ("realloc_64_to_128b", Operation::Realloc),
    ] {
        group.bench_function(name, |b| {
            b.iter_custom(|iterations| measure(operation, iterations));
        });
    }
    group.finish();
    criterion.final_summary();
}

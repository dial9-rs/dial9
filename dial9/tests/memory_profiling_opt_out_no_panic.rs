#![cfg(feature = "memory-profiling")]
#![cfg(target_os = "linux")]
//! Verifies the OPT_OUT TLS sentinel prevents panics during thread teardown.
//!
//! Forces TLS destruction order such that a `LateGuard::drop` (registered
//! BEFORE scc/sdd's TLS) runs AFTER sdd's destructor has already cleared
//! its slot. Without the OPT_OUT pattern, `scc` operations in `LateGuard::drop`
//! would panic. With OPT_OUT, they bail out cleanly.

use dial9::memory::{Dial9Allocator, MemoryProfiler, MemoryProfilingConfig};
use dial9::{MemoryBuffer, RecorderBuilderTokioExt, recorder};
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

static PANICS: AtomicU64 = AtomicU64::new(0);

/// A TLS guard that performs allocations/deallocations in its destructor,
/// simulating real-world code that frees memory during thread teardown.
struct LateGuard;

impl Drop for LateGuard {
    fn drop(&mut self) {
        // Perform allocations and deallocations that will hit the memory
        // profiler hook. If the OPT_OUT sentinel is working, these will
        // bail out without panicking even though sdd's TLS may be destroyed.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Allocate and free some memory — this exercises the hook path.
            let v: Vec<u8> = Vec::with_capacity(1024);
            std::hint::black_box(&v);
            drop(v);
            let v2: Vec<u8> = Vec::with_capacity(2048);
            std::hint::black_box(&v2);
            drop(v2);
        }));
        if result.is_err() {
            PANICS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

thread_local! {
    /// Initialized BEFORE the profiler's scc TLS, so it drops AFTER.
    static LATE: RefCell<Option<LateGuard>> = const { RefCell::new(None) };
}

#[test]
fn opt_out_prevents_tls_teardown_panic() {
    // Install a custom panic hook that suppresses output (if the test were
    // to fail, we don't want 32 threads worth of panic backtraces).
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    PANICS.store(0, Ordering::Relaxed);

    let traced = recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap())
        .with_tokio(|t| {
            t.worker_threads(1);
        })
        .build()
        .unwrap();

    let handle = traced.record_handle();
    let _mem_guard = MemoryProfiler::from_config(
        MemoryProfilingConfig::builder()
            .sample_rate_bytes(64) // sample aggressively
            .track_liveset(true)
            .rng_seed(42)
            .build(),
    )
    .install(handle)
    .expect("install should succeed");

    const N_THREADS: usize = 16;

    traced.runtime().block_on(async {
        // Give time for the profiler to fully initialize.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut handles = Vec::new();
        for _ in 0..N_THREADS {
            handles.push(tokio::task::spawn_blocking(|| {
                // Step 1: Initialize our LATE guard TLS first.
                LATE.with(|cell| {
                    *cell.borrow_mut() = Some(LateGuard);
                });

                // Step 2: Do some allocations that hit the profiler hook,
                // which initializes scc/sdd's TLS.
                for _ in 0..50 {
                    let v: Vec<u8> = Vec::with_capacity(256);
                    std::hint::black_box(&v);
                    drop(v);
                }

                // Thread exits → TLS destructors run in reverse-init order:
                // sdd drops first → our LateGuard drops second → exercises
                // the hook path after sdd is gone.
            }));
        }

        for h in handles {
            h.await.expect("spawn_blocking panicked");
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    drop(traced);

    // Restore the original panic hook.
    std::panic::set_hook(prev_hook);

    let panics = PANICS.load(Ordering::Relaxed);
    assert_eq!(
        panics, 0,
        "expected 0 panics with OPT_OUT sentinel, got {panics}"
    );
}

//! End-to-end test: processed `.bin.gz` files must not leak on disk.
//!
//! The background worker writes compressed segments as `.bin.gz` and deletes
//! the original `.bin`.  When the writer evicts old segments, it must also
//! clean up the renamed `.bin.gz` variants.  A leak here means unbounded disk
//! growth in production.
#![cfg(all(feature = "cpu-profiling", target_os = "linux"))]

use dial9_tokio_telemetry::telemetry::CpuProfilingConfig;
use dial9_tokio_telemetry::telemetry::{
    DiskBuffer, RecorderBuilderTokioExt, RecorderPerfExt, recorder,
};
use std::time::Duration;

/// Produce enough trace data to trigger multiple rotations and evictions,
/// then verify that:
/// 1. No unprocessed `.bin` files remain (worker processed everything).
/// 2. The number of `.bin.gz` files on disk respects the eviction budget
///    (no leaked processed segments).
#[test]
fn eviction_cleans_up_processed_gz_segments() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let trace_dir = tempfile::tempdir().unwrap();

    // Small file/total budget to force frequent rotation and eviction.
    // Each segment holds roughly one flush cycle worth of events.
    let max_file_size = 4 * 1024; // 4 KiB per segment
    let max_number_files = 4;
    let max_total_size = max_number_files * max_file_size; // 16 KiB total ⇒ ~4 segments before eviction

    let writer = DiskBuffer::builder()
        .base_path(trace_dir.path())
        .max_file_size(max_file_size)
        .max_total_size(max_total_size)
        .build()
        .unwrap();

    let traced = recorder(writer)
        .with_cpu_profiling(CpuProfilingConfig::default())
        .worker_poll_interval(Duration::from_millis(50))
        .with_tokio(|t| {
            t.worker_threads(2);
        })
        .build()
        .unwrap();

    // Generate enough work to produce many sealed segments, exceeding the
    // total budget so eviction must kick in.
    traced.runtime().block_on(async {
        for _ in 0..30 {
            let mut handles = Vec::new();
            for _ in 0..20 {
                handles.push(tokio::spawn(async {
                    for _ in 0..50 {
                        tokio::task::yield_now().await;
                    }
                }));
            }
            for h in handles {
                let _ = h.await;
            }
            // Give the worker time to process sealed segments between bursts.
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    });

    traced.graceful_shutdown(Duration::from_secs(10));

    // Collect all trace-related files in the directory.
    let mut bin_files = Vec::new();
    let mut gz_files = Vec::new();
    for entry in std::fs::read_dir(trace_dir.path()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name.ends_with(".bin") {
            bin_files.push(path);
        } else if name.ends_with(".bin.gz") {
            gz_files.push(path);
        }
    }

    // After graceful_shutdown the worker has processed all sealed segments,
    // so no raw `.bin` files should remain.
    assert!(
        bin_files.is_empty(),
        "expected no unprocessed .bin files after graceful shutdown, found: {bin_files:?}"
    );

    // Coarse leak detection. This e2e test's unique job is to prove the *real*
    // pipeline doesn't leak: the background worker gzip-renames each sealed
    // `.bin` segment to `.bin.gz`, and eviction must then delete those renamed
    // files.
    //
    // We deliberately do NOT assert an exact on-disk byte budget here. Real
    // CPU-profile segment sizes and gzip ratios are nondeterministic, and
    // eviction always retains the most-recent segment — which can itself exceed
    // the entire budget (a single logical unit can be larger than
    // `max_file_size`, and gzip framing overhead on incompressible data can
    // push a segment above its uncompressed size). Asserting compressed bytes
    // `<= max_total_size` flaked in CI for exactly these reasons. The precise
    // eviction/budget contract — including the keep-most-recent floor and the
    // `.bin` -> `.bin.gz` cleanup — is covered deterministically by the writer
    // unit tests: `test_rotating_writer_eviction`,
    // `test_eviction_removes_gz_variant`, and
    // `test_eviction_keeps_most_recent_segment_when_over_budget`.
    //
    // The qualitative invariant a leak *would* violate is that disk usage stays
    // bounded and does not grow with the amount produced. The workload above
    // generated far more than `max_total_size` of trace data across many
    // rotations; if eviction failed to remove renamed `.bin.gz` files they
    // would accumulate one per rotation (dozens), rather than the handful the
    // budget allows.
    assert!(
        gz_files.len() <= max_number_files as usize,
        "retained {} .bin.gz files (eviction budget is ~{max_number_files} \
         segments) — processed segments are leaking. Files: {gz_files:?}",
        gz_files.len()
    );
}

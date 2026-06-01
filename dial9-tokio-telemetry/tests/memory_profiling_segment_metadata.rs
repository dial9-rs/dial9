#![cfg(feature = "memory-profiling")]
#![cfg(feature = "analysis")]
#![cfg(target_os = "linux")]
//! End-to-end test: memory.sample_rate_bytes appears in segment metadata.

use dial9_tokio_telemetry::memory_profiling::{
    Dial9Allocator, MemoryProfiler, MemoryProfilingConfig,
};
use dial9_tokio_telemetry::telemetry::{DiskWriter, TelemetryEvent, TracedRuntime};
use std::time::Duration;

#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

#[test]
fn memory_sample_rate_appears_in_segment_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let writer = DiskWriter::single_file(&trace_path).unwrap();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(trace_path.to_str().unwrap())
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();
    let _mem_guard = MemoryProfiler::from_config(
        MemoryProfilingConfig::builder()
            .sample_rate_bytes(2048)
            .rng_seed(42)
            .build(),
    )
    .install(handle)
    .expect("install should succeed");

    runtime.block_on(async {
        // Give the flush thread time to merge source metadata.
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    drop(runtime);
    let _ = guard.graceful_shutdown(Duration::from_secs(5));

    // Read all trace files and find SegmentMetadata events.
    let mut found = false;
    let files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
        .collect();

    for file in &files {
        let data = std::fs::read(file).unwrap();
        let events =
            dial9_tokio_telemetry::analysis_unstable::decode_events(&data).unwrap_or_default();
        for event in &events {
            if let TelemetryEvent::SegmentMetadata { entries, .. } = event {
                found |= entries
                    .iter()
                    .any(|(k, v)| k == "memory.sample_rate_bytes" && v == "2048");
            }
        }
    }

    assert!(
        found,
        "expected memory.sample_rate_bytes=2048 in segment metadata, files: {files:?}"
    );
}

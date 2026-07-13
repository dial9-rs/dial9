#![cfg(feature = "memory-profiling")]
#![cfg(feature = "analysis")]
#![cfg(target_os = "linux")]
//! End-to-end test: memory.sample_rate_bytes appears in segment metadata.

mod common;

use common::decode_file;
use dial9_tokio_telemetry::memory_profiling::{
    Dial9Allocator, MemoryProfiler, MemoryProfilingConfig,
};
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
use dial9_tokio_telemetry::telemetry::{DiskWriter, RecorderBuilderTokioExt, recorder};
use std::time::Duration;

#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

#[test]
fn memory_sample_rate_appears_in_segment_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let writer = DiskWriter::single_file(&trace_path).unwrap();

    let traced = recorder(writer)
        .with_tokio(|t| {
            t.worker_threads(1);
        })
        .graceful_shutdown(Duration::from_secs(5))
        .build()
        .unwrap();

    let handle = traced.guard().handle();
    let _mem_guard = MemoryProfiler::from_config(
        MemoryProfilingConfig::builder()
            .sample_rate_bytes(2048)
            .rng_seed(42)
            .build(),
    )
    .install(handle)
    .expect("install should succeed");

    traced.runtime().block_on(async {
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    traced.graceful_shutdown();

    let mut found = false;
    let files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
        .collect();

    for file in &files {
        let events: Vec<Dial9Event> = decode_file(file);
        for event in &events {
            if let Dial9Event::SegmentMetadataEvent(m) = event {
                found |= m
                    .entries
                    .get("memory.sample_rate_bytes")
                    .map(String::as_str)
                    == Some("2048");
            }
        }
    }

    assert!(
        found,
        "expected memory.sample_rate_bytes=2048 in segment metadata, files: {files:?}"
    );
}

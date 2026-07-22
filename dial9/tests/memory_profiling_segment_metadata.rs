#![cfg(feature = "memory-profiling")]
#![cfg(feature = "analysis")]
#![cfg(target_os = "linux")]
//! End-to-end test: memory.sample_rate_bytes appears in segment metadata.

mod common;

use common::decode_file;
use dial9::analysis::analysis_events::Dial9Event;
use dial9::memory::{Dial9Allocator, MemoryProfiler, MemoryProfilingConfig};
use dial9::{DiskBuffer, RecorderTokioExt, recorder};
use std::time::Duration;

#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

#[test]
fn memory_sample_rate_appears_in_segment_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let writer = DiskBuffer::single_file(&trace_path).unwrap();

    let recorder = recorder(writer).build();
    let (recorder, rt) = recorder
        .attach_tokio_runtime(|t| {
            t.enable_all();
            t.worker_threads(1);
        })
        .expect("attach tokio");

    let handle = recorder.handle().clone();
    let _mem_guard = MemoryProfiler::from_config(
        MemoryProfilingConfig::builder()
            .sample_rate_bytes(2048)
            .rng_seed(42)
            .build(),
    )
    .install(handle)
    .expect("install should succeed");

    rt.block_on(async {
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

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

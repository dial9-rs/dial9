#![cfg(feature = "memory-profiling")]
#![cfg(feature = "analysis")]
#![cfg(target_os = "linux")]
//! Test that install() registers the MemoryProfileSource with the recorder,
//! so synthetic allocs pushed into the queue appear in the trace.

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::memory_profiling::{MemoryProfiler, push_test_alloc};
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
use dial9_tokio_telemetry::telemetry::{InMemoryWriter, TracedRuntime};
use std::time::Duration;

#[test]
fn install_registers_source_with_recorder() {
    let (capture, batches) = capture_processor();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();

    let handle = guard.handle();
    let _mem_guard = MemoryProfiler::with_defaults()
        .install(handle)
        .expect("install should succeed");

    // Push a synthetic alloc into the queue.
    assert!(
        push_test_alloc(0xCAFE_0000, 4096, 12345),
        "push should succeed"
    );

    // Give the flush thread time to drain.
    runtime.block_on(async {
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(1))
        .expect("clean shutdown");

    let b = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&b);
    let allocs: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::AllocEvent(_)))
        .collect();

    assert!(
        !allocs.is_empty(),
        "expected at least one AllocEvent from the synthetic push"
    );

    if let Dial9Event::AllocEvent(e) = &allocs[0] {
        assert_eq!(e.addr, 0xCAFE_0000);
        assert_eq!(e.size, 4096);
        assert_eq!(e.callchain, &[0xDEAD, 0xBEEF]);
    }
}

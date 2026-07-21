#![cfg(all(unix, feature = "process-resource"))]

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
use dial9_tokio_telemetry::telemetry::{
    MemoryBuffer, ProcessResourceUsageConfig, RecorderBuilderTokioExt, RecorderPerfExt, recorder,
};
use std::time::Duration;

#[test]
fn traced_runtime_records_process_resource_usage() {
    let (capture, batches) = capture_processor();

    let traced = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_process_resource_usage(ProcessResourceUsageConfig::default())
        .with_tokio(|t| {
            t.worker_threads(1);
        })
        .with_custom_pipeline(|p| p.pipe(capture))
        .build()
        .unwrap();

    traced.graceful_shutdown(Duration::from_secs(1));

    let batches = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&batches);
    let metrics: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Dial9Event::ProcessResourceUsageEvent(event) => Some(event),
            _ => None,
        })
        .collect();

    assert!(
        !metrics.is_empty(),
        "expected at least one process resource usage event"
    );
    assert!(metrics[0].timestamp_ns > 0);
    assert!(metrics[0].max_rss_bytes > 0);
}

#[test]
fn traced_runtime_does_not_record_process_resource_usage_by_default() {
    let (capture, batches) = capture_processor();

    let traced = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_tokio(|t| {
            t.worker_threads(1);
        })
        .with_custom_pipeline(|p| p.pipe(capture))
        .build()
        .unwrap();

    traced.graceful_shutdown(Duration::from_secs(1));

    let batches = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&batches);

    assert!(
        events
            .iter()
            .all(|event| !matches!(event, Dial9Event::ProcessResourceUsageEvent(_))),
        "process resource usage should be opt-in"
    );
}

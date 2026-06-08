mod common;

#[cfg(unix)]
use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
#[cfg(unix)]
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
#[cfg(unix)]
use dial9_tokio_telemetry::telemetry::{InMemoryWriter, ProcessResourceUsageConfig, TracedRuntime};

#[cfg(unix)]
#[test]
fn traced_runtime_records_process_resource_usage() {
    let (capture, batches) = capture_processor();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_process_resource_usage(ProcessResourceUsageConfig::default())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(1))
        .expect("clean shutdown");

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

#[cfg(unix)]
#[test]
fn traced_runtime_does_not_record_process_resource_usage_by_default() {
    let (capture, batches) = capture_processor();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(1))
        .expect("clean shutdown");

    let batches = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&batches);

    assert!(
        events
            .iter()
            .all(|event| !matches!(event, Dial9Event::ProcessResourceUsageEvent(_))),
        "process resource usage should be opt-in"
    );
}

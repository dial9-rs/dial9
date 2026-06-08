mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor};
use dial9_tokio_telemetry::telemetry::{
    CustomEventsConfig, InMemoryWriter, TelemetryCore, TracedRuntime,
};
use dial9_trace_format::TraceEvent;
use dial9_trace_format::decoder::Decoder;

#[derive(Debug, serde::Deserialize, TraceEvent)]
struct QueuedEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    value: u64,
}

fn decode_queued_events(batches: &[Vec<u8>]) -> Vec<QueuedEvent> {
    let mut events = Vec::new();
    for bytes in batches {
        let mut decoder = Decoder::new(bytes).expect("captured batch should be a valid trace");
        decoder
            .for_each_event(|raw| {
                if raw.name == "QueuedEvent" {
                    events.push(raw.deserialize().expect("queued event should decode"));
                }
            })
            .expect("decode batch");
    }
    events
}

#[test]
fn traced_runtime_records_custom_events_callback_events() {
    let (capture, batches) = capture_processor();
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(QueuedEvent {
        timestamp_ns: 1,
        value: 7,
    })
    .unwrap();
    drop(tx);

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_custom_events(CustomEventsConfig::default(), move |ctx| {
            while let Ok(event) = rx.try_recv() {
                ctx.record_event(event);
            }
        })
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(1))
        .expect("clean shutdown");

    let batches = batches.lock().unwrap();
    let events = decode_queued_events(&batches);

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].timestamp_ns, 1);
    assert_eq!(events[0].value, 7);
}

#[test]
fn telemetry_core_attach_runtime_records_custom_events_callback_events() {
    let (capture, batches) = capture_processor();
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(QueuedEvent {
        timestamp_ns: 2,
        value: 11,
    })
    .unwrap();
    drop(tx);

    let guard = TelemetryCore::builder()
        .writer(InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .processors(vec![Box::new(capture)])
        .build()
        .unwrap();
    guard.enable();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let (runtime, _handle) = guard
        .trace_runtime("main")
        .with_custom_events(CustomEventsConfig::default(), move |ctx| {
            while let Ok(event) = rx.try_recv() {
                ctx.record_event(event);
            }
        })
        .build(builder)
        .unwrap();

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(1))
        .expect("clean shutdown");

    let batches = batches.lock().unwrap();
    let events = decode_queued_events(&batches);

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].timestamp_ns, 2);
    assert_eq!(events[0].value, 11);
}

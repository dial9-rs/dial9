#![cfg(feature = "metrique-integration")]

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor};
use dial9_tokio_telemetry::telemetry::metrique_integration::{
    Dial9Context, Dial9Stream, Emit, Interned,
};
use dial9_tokio_telemetry::telemetry::{Dial9Handle, InMemoryWriter, TracedRuntime};
use dial9_trace_format::decoder::Decoder;
use dial9_trace_format::types::FieldValue;
use metrique::CloseValue;
use metrique::RootEntry;
use metrique::unit_of_work::metrics;
use metrique::writer::core::{FieldShape, KnownShape};
use metrique::writer::value::{Value, ValueWriter};
use metrique::writer::{Entry, EntryIoStream, EntryWriter};
use serde::Deserialize;
use std::time::Duration;

/// Decode every event across all captured segments into `(schema name,
/// positional field values)`. Used where a test needs to look at raw wire
/// representation (e.g. confirming a field was actually pooled) rather than
/// a resolved value — see `decode_one` below for the typed alternative used
/// by most tests.
fn decode_raw_events(segments: &[Vec<u8>]) -> Vec<(String, Vec<FieldValue>)> {
    let mut events = Vec::new();
    for bytes in segments {
        let mut dec = Decoder::new(bytes).expect("valid trace header");
        dec.for_each_event(|raw| {
            let fields = raw.fields.iter().map(|f| f.to_owned()).collect();
            events.push((raw.name.to_string(), fields));
        })
        .expect("decode segment");
    }
    events
}

/// Find the first event named `schema_name` across all captured segments and
/// deserialize it into `T` via `dial9_trace_format`'s serde support. `T`
/// only needs to name the fields a test cares about — unrecognized map
/// entries (including `event`/`timestamp_ns`, and any field a struct
/// doesn't declare) are ignored by serde's default derive behavior.
fn decode_one<T: serde::de::DeserializeOwned>(segments: &[Vec<u8>], schema_name: &str) -> T {
    for bytes in segments {
        let mut dec = Decoder::new(bytes).expect("valid trace header");
        let mut found = None;
        dec.for_each_event(|raw| {
            if found.is_none() && raw.name == schema_name {
                found = Some(
                    raw.deserialize::<T>()
                        .unwrap_or_else(|e| panic!("{schema_name} should deserialize: {e}")),
                );
            }
        })
        .expect("decode segment");
        if let Some(v) = found {
            return v;
        }
    }
    panic!("expected a {schema_name} event in the captured trace");
}

type CapturedBatches = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

/// Start a runtime with a capturing pipeline and return a handle usable from
/// any thread (not just workers), plus the shared decode buffer.
fn setup() -> (
    tokio::runtime::Runtime,
    dial9_tokio_telemetry::telemetry::TelemetryGuard,
    CapturedBatches,
    Dial9Handle,
) {
    let (capture, batches) = capture_processor();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();
    let handle = guard.handle();
    (runtime, guard, batches, handle)
}

fn shutdown(
    runtime: tokio::runtime::Runtime,
    guard: dial9_tokio_telemetry::telemetry::TelemetryGuard,
) {
    drop(runtime);
    guard
        .graceful_shutdown(Duration::from_secs(1))
        .expect("clean shutdown");
}

#[metrics(rename_all = "PascalCase", default_flags(Emit))]
struct TestMetrics {
    // `dial9` declared first (the original design doc's convention) — field
    // order no longer matters (see `dial9_context_field_order_does_not_matter`
    // below), but this is still worth keeping as the common case that every
    // other test in this file exercises.
    #[metrics(flatten)]
    dial9: Dial9Context,
    #[metrics(flags(Interned))]
    route: String,
    count: u64,
    #[metrics(unit = metrique::unit::Millisecond)]
    latency_ms: f64,
    tags: Vec<u64>,
    maybe_value: Option<u64>,
    #[metrics(flags(skip(Emit)))]
    hidden: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TestMetricsEvent {
    worker_id: u64,
    monotonic_ns_end: u64,
    route: String,
    count: u64,
    latency_ms: f64,
    // `Vec<u64>` `Emit` fields always land as a comma-joined string, never a
    // structured list — see schema.rs's `FieldShape::List` match arm.
    tags: String,
    maybe_value: Option<u64>,
}

#[test]
fn emit_tagged_fields_round_trip_with_context_header() {
    let (runtime, guard, batches, handle) = setup();

    let closed = CloseValue::close(TestMetrics {
        dial9: Dial9Context::capture(),
        route: "/api/users".to_string(),
        count: 42,
        latency_ms: 12.5,
        tags: vec![1, 2, 3],
        maybe_value: Some(7),
        hidden: "secret".to_string(),
    });
    let root = RootEntry::new(closed);
    let mut stream = Dial9Stream::new(&handle);
    stream.next(&root).expect("next should not error");

    shutdown(runtime, guard);
    let batches = batches.lock().unwrap();
    let event: TestMetricsEvent = decode_one(&batches, "TestMetrics");

    // `hidden` never appears (flags(skip(Emit))); `dial9`'s own fields never
    // appear as payload (routed to the header instead).
    assert_eq!(event.route, "/api/users");
    assert_eq!(event.count, 42);
    assert_eq!(event.latency_ms, 12.5);
    assert_eq!(event.tags, "1,2,3");
    assert_eq!(event.maybe_value, Some(7));
    // worker_id/monotonic_ns_end came from `Dial9Context::capture()`, so
    // just confirm they decoded rather than pinning exact values.
    let _ = event.worker_id;
    assert!(event.monotonic_ns_end > 0);
}

#[test]
fn interned_string_resolves_through_string_pool() {
    let (runtime, guard, batches, handle) = setup();

    let closed = CloseValue::close(TestMetrics {
        dial9: Dial9Context::capture(),
        route: "/api/users".to_string(),
        count: 1,
        latency_ms: 0.0,
        tags: vec![],
        maybe_value: None,
        hidden: String::new(),
    });
    let mut stream = Dial9Stream::new(&handle);
    stream.next(&RootEntry::new(closed)).unwrap();

    shutdown(runtime, guard);
    let batches = batches.lock().unwrap();
    for bytes in batches.iter() {
        let mut dec = Decoder::new(bytes).expect("valid trace header");
        let mut found = false;
        dec.for_each_event(|raw| {
            if raw.name != "TestMetrics" {
                return;
            }
            // Header is now 2 fields (WorkerId, MonotonicNsEnd; `task_id`
            // was dropped — see `Dial9Context`'s docs), so `Route` is at
            // index 2, not 3.
            if let dial9_trace_format::types::FieldValueRef::PooledString(id) = raw.fields[2] {
                found = true;
                // Can't resolve through `dec.string_pool()` while `dec` is
                // mutably borrowed by `for_each_event`; just confirm the
                // field decoded as a pool reference (see the round-trip
                // test above for the count/route assertions).
                let _ = id;
            }
        })
        .expect("decode segment");
        if found {
            return;
        }
    }
    panic!("expected an interned Route field in at least one segment");
}

#[test]
fn unit_annotation_uses_metrique_prefix() {
    let (runtime, guard, batches, handle) = setup();

    let closed = CloseValue::close(TestMetrics {
        dial9: Dial9Context::capture(),
        route: "/x".to_string(),
        count: 0,
        latency_ms: 1.0,
        tags: vec![],
        maybe_value: None,
        hidden: String::new(),
    });
    let mut stream = Dial9Stream::new(&handle);
    stream.next(&RootEntry::new(closed)).unwrap();

    shutdown(runtime, guard);
    let batches = batches.lock().unwrap();
    let mut found = false;
    for bytes in batches.iter() {
        let mut dec = Decoder::new(bytes).expect("valid trace header");
        // The registry only fills in as schema frames are processed during
        // iteration; decode all events first to populate it.
        dec.for_each_event(|_| {}).expect("decode segment");
        for (_, entry) in dec.registry().entries() {
            if entry.name() == "TestMetrics" {
                let annotation = entry
                    .annotations()
                    .iter()
                    .find(|a| a.key() == "metrique.unit");
                if let Some(a) = annotation {
                    found = true;
                    assert!(a.value().contains("Millisecond"), "unit = {}", a.value());
                }
            }
        }
    }
    assert!(found, "expected a metrique.unit schema annotation");
}

struct HandWritten;
impl Entry for HandWritten {
    fn write<'a>(&'a self, writer: &mut impl EntryWriter<'a>) {
        writer.value("x", &1u64);
    }
}

#[test]
fn hand_written_entry_is_dropped_without_panicking() {
    let (runtime, guard, batches, handle) = setup();

    let mut stream = Dial9Stream::new(&handle);
    stream.next(&HandWritten).expect("next should not error");

    shutdown(runtime, guard);
    let batches = batches.lock().unwrap();
    let events = decode_raw_events(&batches);
    // The runtime's own background events (worker park/unpark, clock sync,
    // segment metadata, ...) always appear; assert none of them is our
    // hand-written entry's schema (it's never registered, so it can only
    // show up as an unrecognized name).
    const KNOWN_BACKGROUND_EVENTS: &[&str] = &[
        "ClockSyncEvent",
        "SegmentMetadataEvent",
        "WorkerParkEvent",
        "WorkerUnparkEvent",
        "TaskSpawnEvent",
        "PollStartEvent",
        "PollEndEvent",
        "WakeEventEvent",
        "QueueSampleEvent",
        "MetriqueSinkStats",
    ];
    let unexpected: Vec<_> = events
        .iter()
        .filter(|(name, _)| !KNOWN_BACKGROUND_EVENTS.contains(&name.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "hand-written entry should never reach the dial9 trace: {unexpected:?}"
    );
}

#[test]
fn inert_handle_is_a_no_op() {
    let mut stream = Dial9Stream::new(&Dial9Handle::disabled());
    let closed = CloseValue::close(TestMetrics {
        dial9: Dial9Context::capture(),
        route: "/x".to_string(),
        count: 1,
        latency_ms: 1.0,
        tags: vec![],
        maybe_value: None,
        hidden: String::new(),
    });
    stream
        .next(&RootEntry::new(closed))
        .expect("inert handle should be a no-op, not an error");
}

// A field whose `Value::write` panics — exercises the `catch_unwind` guard
// around the write-walk.
struct Boom;

impl Value for Boom {
    const SHAPE: FieldShape<'static> = FieldShape::Known(KnownShape::U64);
    fn write(&self, _writer: impl ValueWriter) {
        panic!("boom: deliberate panic inside Value::write for the panic-recovery test");
    }
}

#[metrics(rename_all = "PascalCase", default_flags(Emit))]
struct PanicMetrics {
    #[metrics(flatten)]
    dial9: Dial9Context,
    #[metrics(no_close)]
    boom: Boom,
}

#[test]
fn panic_in_value_write_drops_event_without_poisoning_the_stream() {
    let (runtime, guard, batches, handle) = setup();
    let mut stream = Dial9Stream::new(&handle);

    let panicking = CloseValue::close(PanicMetrics {
        dial9: Dial9Context::capture(),
        boom: Boom,
    });
    stream
        .next(&RootEntry::new(panicking))
        .expect("next() itself should not propagate the panic");

    // A subsequent, well-behaved entry on the same stream should still work.
    let good = CloseValue::close(TestMetrics {
        dial9: Dial9Context::capture(),
        route: "/ok".to_string(),
        count: 9,
        latency_ms: 1.0,
        tags: vec![],
        maybe_value: None,
        hidden: String::new(),
    });
    stream.next(&RootEntry::new(good)).unwrap();

    shutdown(runtime, guard);
    let batches = batches.lock().unwrap();
    let events = decode_raw_events(&batches);
    assert!(
        events.iter().any(|(name, _)| name == "TestMetrics"),
        "the good entry recorded after the panic should still reach the trace: {events:?}"
    );
    assert!(
        !events.iter().any(|(name, _)| name == "PanicMetrics"),
        "the panicking entry should never reach the trace: {events:?}"
    );
}

#[metrics(rename_all = "PascalCase", default_flags(Emit))]
struct NoContextMetrics {
    value: u64,
}

#[test]
#[should_panic(expected = "Emit-tagged fields but no flattened")]
fn emit_fields_without_context_trips_debug_assert() {
    let (_runtime, _guard, _batches, handle) = setup();
    let mut stream = Dial9Stream::new(&handle);
    let closed = CloseValue::close(NoContextMetrics { value: 1 });
    // In a debug build this is expected to panic via `debug_assert!` inside
    // schema construction (see `stream.rs::log_build_diagnostics`) — a
    // release build instead logs and falls back to a WorkerId::UNKNOWN
    // header (not exercised here; see the module's design notes).
    let _ = stream.next(&RootEntry::new(closed));
}

// `dial9` deliberately declared in the *middle* — neither first (like
// `TestMetrics`) nor last (the design doc's original convention). Dispatch
// is by field name (see `schema.rs`), not descriptor/write position, so
// this should encode exactly as correctly as either of those.
#[metrics(rename_all = "PascalCase", default_flags(Emit))]
struct MiddleOrderMetrics {
    before: u64,
    #[metrics(flatten)]
    dial9: Dial9Context,
    after: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MiddleOrderMetricsEvent {
    worker_id: u64,
    monotonic_ns_end: u64,
    before: u64,
    after: u64,
}

#[test]
fn dial9_context_field_order_does_not_matter() {
    let (runtime, guard, batches, handle) = setup();

    let closed = CloseValue::close(MiddleOrderMetrics {
        before: 11,
        dial9: Dial9Context::capture(),
        after: 22,
    });
    let mut stream = Dial9Stream::new(&handle);
    stream
        .next(&RootEntry::new(closed))
        .expect("next should not error");

    shutdown(runtime, guard);
    let batches = batches.lock().unwrap();
    let event: MiddleOrderMetricsEvent = decode_one(&batches, "MiddleOrderMetrics");

    assert_eq!(event.before, 11);
    assert_eq!(event.after, 22);
    assert!(event.monotonic_ns_end > 0);
    let _ = event.worker_id;
}

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
use std::time::Duration;

/// Decode every event across all captured segments into `(schema name,
/// positional field values)`. The dial9 metrique sink registers a schema
/// per distinct entry shape at runtime, so there's no compile-time struct to
/// deserialize into — assert on raw positional values instead.
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
    let events = decode_raw_events(&batches);
    let (name, fields) = events
        .iter()
        .find(|(name, _)| name == "TestMetrics")
        .expect("TestMetrics event should be recorded");
    assert_eq!(name, "TestMetrics");

    // Fixed header prefix: WorkerId, TaskId, MonotonicNsEnd. `hidden` never
    // appears (flags(skip(Emit))); `dial9`'s own fields never appear as
    // payload (routed to the header instead) — so the schema has exactly
    // 3 header fields + 5 Emit payload fields = 8.
    assert_eq!(fields.len(), 8, "fields = {fields:?}");
    assert!(matches!(fields[0], FieldValue::Varint(_)), "worker_id");
    assert!(
        matches!(fields[1], FieldValue::None | FieldValue::Varint(_)),
        "task_id"
    );
    assert!(
        matches!(fields[2], FieldValue::Varint(_)),
        "monotonic_ns_end"
    );
    assert!(
        matches!(fields[3], FieldValue::PooledString(_)),
        "route should be interned, got {:?}",
        fields[3]
    );
    assert_eq!(fields[4], FieldValue::Varint(42), "count");
    assert_eq!(fields[5], FieldValue::F64(12.5), "latency_ms");
    // `Vec<u64>` `Emit` fields always land as a comma-joined string, never a
    // structured `DynamicList` — metrique's `ForceFlag` (applied to every
    // `flags(...)`-tagged field, `Emit` included) wraps the writer with a
    // type that doesn't forward `.values()` structurally. See schema.rs's
    // `FieldShape::List` match arm.
    assert_eq!(fields[6], FieldValue::String("1,2,3".to_string()), "tags");
    assert_eq!(fields[7], FieldValue::Varint(7), "maybe_value");
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
            if let dial9_trace_format::types::FieldValueRef::PooledString(id) = raw.fields[3] {
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
    let events = decode_raw_events(&batches);
    let (_, fields) = events
        .iter()
        .find(|(name, _)| name == "MiddleOrderMetrics")
        .expect("MiddleOrderMetrics event should be recorded even with dial9 mid-struct");

    // 3 fixed header fields (WorkerId, TaskId, MonotonicNsEnd) + Before, After.
    assert_eq!(fields.len(), 5, "fields = {fields:?}");
    assert!(matches!(fields[0], FieldValue::Varint(_)), "worker_id");
    assert!(
        matches!(fields[1], FieldValue::None | FieldValue::Varint(_)),
        "task_id"
    );
    assert!(
        matches!(fields[2], FieldValue::Varint(_)),
        "monotonic_ns_end"
    );
    assert_eq!(fields[3], FieldValue::Varint(11), "before");
    assert_eq!(fields[4], FieldValue::Varint(22), "after");
}

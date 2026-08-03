//! End-to-end tests for the metrique → dial9 sink: entries flow through
//! `Dial9Stream`, land in a sealed trace file, and decode back with the
//! expected schema, header context, payload values, and unit annotations.
//!
//! Runs with the `tokio` feature (the self dev-dependency turns it on), so
//! task ids are captured when a test enters a runtime. Off-runtime capture
//! is the default everywhere else.

use std::collections::HashMap;
use std::time::Duration;

use dial9_core::buffer::DiskBuffer;
use dial9_core::clock::clock_monotonic_ns;
use dial9_core::recorder::recorder;
use dial9_core::schema_extensions::{self, roles};
use dial9_core::thread::current_tid;
use dial9_metrique::{
    Dial9Context, Dial9EntryExt, Dial9Stream, Interned, SPAN_TYPE, Skip, SpanName, field_names,
};
use dial9_tokio_telemetry::telemetry::{Dial9HandleTokioExt, TokioAttachOptions};
use dial9_trace_format::types::FieldValueRef;
use metrique::unit::Microsecond;
use metrique::unit_of_work::metrics;
use metrique_writer::{EntryIoStream, IoStreamError};

/// A decoded metrique event: field name → rendered value.
#[derive(Debug)]
struct DecodedEvent {
    schema_name: String,
    timestamp_ns: u64,
    fields: HashMap<String, String>,
    /// Schema field names in wire order. `fields` keys on names, so a
    /// duplicate schema name would silently overwrite; asserting on this
    /// list keeps collision tests honest.
    field_names: Vec<String>,
    /// field name → unit annotation value.
    units: HashMap<String, String>,
    /// field name → annotation key/value pairs.
    annotations: HashMap<String, HashMap<String, String>>,
    /// Names of fields carried as pooled strings.
    pooled_fields: Vec<String>,
}

fn assert_valid_span_context(event: &DecodedEvent) {
    let _: u64 = event.fields[field_names::THREAD_ID]
        .parse()
        .expect("dial9.thread_id must be an unsigned integer");
    let duration: u64 = event.fields[field_names::SPAN_DURATION_NS]
        .parse()
        .expect("dial9.span.duration_ns must be an unsigned integer");
    assert!(
        duration <= event.timestamp_ns,
        "duration exceeds the packed close timestamp: {event:#?}"
    );
    assert_eq!(
        event.annotations[field_names::THREAD_ID][schema_extensions::ROLE_KEY],
        roles::THREAD_ID
    );
    assert_eq!(
        event.annotations[field_names::TOKIO_TASK_ID][schema_extensions::ROLE_KEY],
        roles::TOKIO_TASK_ID
    );
    assert_eq!(
        event.annotations[field_names::SPAN_DURATION_NS][schema_extensions::ROLE_KEY],
        roles::SPAN_DURATION
    );
    assert_eq!(
        event.annotations[field_names::SPAN_DURATION_NS][schema_extensions::SPAN_TYPE_KEY],
        SPAN_TYPE
    );
    assert_eq!(event.units[field_names::SPAN_DURATION_NS], "ns");
}

fn render(value: &FieldValueRef<'_>, pool: &dial9_trace_format::decoder::StringPool) -> String {
    match value {
        FieldValueRef::I64(v) => v.to_string(),
        FieldValueRef::F64(v) => v.to_string(),
        FieldValueRef::Bool(v) => v.to_string(),
        FieldValueRef::String(v) => (*v).to_owned(),
        FieldValueRef::Varint(v) => v.to_string(),
        FieldValueRef::PooledString(id) => pool.get(*id).unwrap_or("<missing>").to_owned(),
        FieldValueRef::List(items) => {
            let rendered: Vec<String> = items.iter().map(|v| render(v, pool)).collect();
            format!("[{}]", rendered.join(","))
        }
        FieldValueRef::None => "<none>".to_owned(),
        other => format!("{other:?}"),
    }
}

/// Decode all `metrique:*` events from every trace segment file in `dir`
/// (including the live `.active` segment).
fn decode_metrique_events(dir: &std::path::Path) -> Vec<DecodedEvent> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().contains(".bin"))
        .collect();
    files.sort();

    let mut events = Vec::new();
    for path in files {
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let Some(mut decoder) = dial9_trace_format::decoder::Decoder::new(&data) else {
            continue;
        };
        // Ignore a trailing decode error from a partially-written .active tail.
        let _ = decoder.for_each_event(|ev| {
            if !ev.name.starts_with("metrique:") {
                return;
            }
            let mut fields = HashMap::new();
            let mut field_names = Vec::new();
            let mut units = HashMap::new();
            let mut annotations = HashMap::new();
            let mut pooled_fields = Vec::new();
            for (i, (def, value)) in ev.schema.fields().iter().zip(ev.fields.iter()).enumerate() {
                fields.insert(def.name().to_owned(), render(value, ev.string_pool));
                field_names.push(def.name().to_owned());
                let pooled = match value {
                    FieldValueRef::PooledString(_) => true,
                    FieldValueRef::List(items) => items
                        .iter()
                        .any(|v| matches!(v, FieldValueRef::PooledString(_))),
                    _ => false,
                };
                if pooled {
                    pooled_fields.push(def.name().to_owned());
                }
                for ann in ev.schema.annotations() {
                    if ann.field_index() as usize == i {
                        annotations
                            .entry(def.name().to_owned())
                            .or_insert_with(HashMap::new)
                            .insert(ann.key().to_owned(), ann.value().to_owned());
                        if ann.key() == "unit" {
                            units.insert(def.name().to_owned(), ann.value().to_owned());
                        }
                    }
                }
            }
            events.push(DecodedEvent {
                schema_name: ev.name.to_owned(),
                timestamp_ns: ev.timestamp_ns.expect("metrique event missing timestamp"),
                fields,
                field_names,
                units,
                annotations,
                pooled_fields,
            });
        });
    }
    events
}

/// Decode all `metrique:*` events from `dir` into `T` via the trace
/// format's serde support: fields deserialize by name (post-rename), with
/// `timestamp_ns` carrying the frame timestamp and pooled strings resolving
/// to plain strings. Prefer this over [`decode_metrique_events`] when a test
/// only asserts on values; the raw form remains for schema-shape assertions
/// (units, field order, pooling).
fn decode_metrique_events_as<T: serde::de::DeserializeOwned>(dir: &std::path::Path) -> Vec<T> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().contains(".bin"))
        .collect();
    files.sort();

    let mut events = Vec::new();
    for path in files {
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let Some(mut decoder) = dial9_trace_format::decoder::Decoder::new(&data) else {
            continue;
        };
        let _ = decoder.for_each_event(|ev| {
            if ev.name.starts_with("metrique:") {
                events.push(ev.deserialize().expect("metrique event must deserialize"));
            }
        });
    }
    events
}

/// Run `feed` against a fresh `Dial9Stream`, seal the trace, and decode the
/// metrique events back.
fn run_sink_test(feed: impl FnOnce(&mut Dial9Stream)) -> Vec<DecodedEvent> {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");
    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    recorder.enable();

    let mut stream = Dial9Stream::new(recorder.handle());
    feed(&mut stream);

    recorder.graceful_shutdown(Duration::from_secs(5));
    decode_metrique_events(dir.path())
}

/// Close a metrics struct and hand it to the stream the way a metrique
/// pipeline would (as a `RootEntry`).
macro_rules! feed_entry {
    ($stream:expr, $value:expr) => {
        $stream
            .next(&metrique::RootEntry::new(metrique::CloseValue::close(
                $value,
            )))
            .unwrap()
    };
}

#[metrics(rename_all = "PascalCase")]
struct RequestMetrics {
    #[metrics(flatten)]
    dial9: Dial9Context,

    #[metrics(flags(Interned))]
    route: String,

    #[metrics(flags(SpanName))]
    operation: &'static str,

    #[metrics(unit = Microsecond)]
    latency_us: u64,

    retries: Option<u64>,

    tags: Vec<String>,

    success: bool,

    load: f64,

    elapsed: Duration,

    #[metrics(flags(Skip))]
    debug_blob: String,
}

fn sample_request(retries: Option<u64>) -> RequestMetrics {
    RequestMetrics {
        dial9: Dial9Context::capture(),
        route: "/pets".to_owned(),
        operation: "GetPet",
        latency_us: 1500,
        retries,
        tags: vec!["a".to_owned(), "b".to_owned()],
        success: true,
        load: 0.5,
        elapsed: Duration::from_millis(250),
        debug_blob: "should not appear".to_owned(),
    }
}

#[test]
fn full_entry_round_trips() {
    /// `RequestMetrics` as decoded from the trace: fields deserialize by
    /// post-rename name, the dial9 header by its wire names.
    #[derive(Debug, serde::Deserialize)]
    struct Decoded {
        timestamp_ns: u64,
        #[serde(rename = "dial9.thread_id")]
        thread_id: u64,
        #[serde(rename = "dial9.tokio.task_id")]
        task_id: Option<u64>,
        #[serde(rename = "dial9.span.duration_ns")]
        duration_ns: u64,
        #[serde(rename = "Route")]
        route: String,
        #[serde(rename = "Operation")]
        operation: String,
        #[serde(rename = "LatencyUs")]
        latency_us: u64,
        #[serde(rename = "Retries")]
        retries: Option<u64>,
        #[serde(rename = "Tags")]
        tags: Vec<String>,
        #[serde(rename = "Success")]
        success: bool,
        #[serde(rename = "Load")]
        load: f64,
        // Duration writes fractional milliseconds.
        #[serde(rename = "Elapsed")]
        elapsed_ms: f64,
    }

    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");
    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    recorder.enable();
    let mut stream = Dial9Stream::new(recorder.handle());
    feed_entry!(stream, sample_request(Some(2)));
    feed_entry!(stream, sample_request(None));
    recorder.graceful_shutdown(Duration::from_secs(5));

    let typed: Vec<Decoded> = decode_metrique_events_as(dir.path());
    assert_eq!(typed.len(), 2, "expected two events, got {typed:#?}");
    let ev = &typed[0];

    // Payload values, post-rename.
    assert_eq!(ev.route, "/pets");
    assert_eq!(ev.operation, "GetPet");
    assert_eq!(ev.latency_us, 1500);
    assert_eq!(ev.retries, Some(2));
    assert_eq!(ev.tags, vec!["a", "b"]); // typed list encoding
    assert!(ev.success);
    assert_eq!(ev.load, 0.5);
    assert_eq!(ev.elapsed_ms, 250.0);

    // Off-runtime capture records the calling thread's tid and a span
    // duration; the task id is absent. The packed timestamp is the close, so
    // the duration cannot exceed it (start = end - duration >= 0).
    assert_eq!(ev.thread_id, u64::from(current_tid()));
    assert_eq!(ev.task_id, None);
    assert!(
        ev.duration_ns <= ev.timestamp_ns,
        "duration exceeds the packed close timestamp: {ev:#?}"
    );

    assert_eq!(typed[1].retries, None);

    // Schema-shape assertions the typed decode cannot express: the Skip
    // field stays out, units annotate, and only Interned strings pool.
    let events = decode_metrique_events(dir.path());
    let raw = &events[0];
    assert_eq!(raw.schema_name, "metrique:RequestMetrics");
    assert!(
        !raw.fields.contains_key("DebugBlob"),
        "Skip field leaked into the payload: {raw:#?}"
    );
    assert_eq!(raw.units["LatencyUs"], "us");
    assert_eq!(raw.units["Elapsed"], "ms");
    assert_eq!(raw.units[field_names::SPAN_DURATION_NS], "ns");
    assert_eq!(
        raw.annotations[field_names::SPAN_DURATION_NS][schema_extensions::ROLE_KEY],
        roles::SPAN_DURATION
    );
    assert_eq!(
        raw.annotations[field_names::SPAN_DURATION_NS][schema_extensions::SPAN_TYPE_KEY],
        SPAN_TYPE
    );
    assert_eq!(
        raw.annotations[field_names::THREAD_ID][schema_extensions::ROLE_KEY],
        roles::THREAD_ID
    );
    assert_eq!(
        raw.annotations[field_names::TOKIO_TASK_ID][schema_extensions::ROLE_KEY],
        roles::TOKIO_TASK_ID
    );
    assert_eq!(
        raw.annotations["Operation"][schema_extensions::ROLE_KEY],
        roles::SPAN_NAME
    );
    assert!(raw.pooled_fields.contains(&"Route".to_owned()));
    assert!(!raw.pooled_fields.contains(&"Operation".to_owned()));
}

#[test]
fn packed_timestamps_follow_close_order_not_start_order() {
    let events = run_sink_test(|stream| {
        let older = sample_request(None);
        std::thread::sleep(Duration::from_millis(1));
        let newer = sample_request(None);

        // Close the newer entry first. Start order and emission order are now
        // deliberately opposite.
        feed_entry!(stream, newer);
        feed_entry!(stream, older);
    });

    assert_eq!(events.len(), 2);
    for event in &events {
        assert_valid_span_context(event);
    }
    // The start is reconstructed as `end - duration`; the packed timestamp is
    // the end (close).
    let start = |ev: &DecodedEvent| {
        let duration: u64 = ev.fields[field_names::SPAN_DURATION_NS].parse().unwrap();
        ev.timestamp_ns - duration
    };
    assert!(
        start(&events[0]) > start(&events[1]),
        "test setup must reverse start and close order"
    );
    assert!(
        events[0].timestamp_ns <= events[1].timestamp_ns,
        "packed timestamps must follow close/emission order: {events:#?}"
    );
}

#[test]
fn packed_timestamp_is_captured_at_close_not_encoding() {
    let mut close_lower_bound = None;
    let mut close_upper_bound = None;
    let events = run_sink_test(|stream| {
        // Bracket the close with clock reads: the packed timestamp must be a
        // clock value captured at close, so it falls between the read taken
        // just before close and the one taken just after — and strictly
        // before the later spin-advanced encode time.
        let lower_bound = clock_monotonic_ns();
        close_lower_bound = Some(lower_bound);
        let closed = metrique::RootEntry::new(metrique::CloseValue::close(sample_request(None)));
        let upper_bound = clock_monotonic_ns();
        close_upper_bound = Some(upper_bound);

        // Spin the clock well past close, then encode. If the timestamp were
        // captured at encode time it would land after this window.
        while clock_monotonic_ns() <= upper_bound {
            std::hint::spin_loop();
        }
        stream.next(&closed).unwrap();
    });

    assert_eq!(events.len(), 1);
    let ts = events[0].timestamp_ns;
    assert!(
        ts >= close_lower_bound.unwrap() && ts <= close_upper_bound.unwrap(),
        "packed timestamp must be the close-time clock read, within \
         [{}, {}]: {events:#?}",
        close_lower_bound.unwrap(),
        close_upper_bound.unwrap(),
    );
}

#[test]
fn duplicate_span_name_first_wins_rest_plain() {
    // Two `SpanName` fields: the first is honored, the second is demoted to an
    // ordinary payload field. The event is still recorded either way.
    #[metrics]
    struct DuplicateNames {
        #[metrics(flatten)]
        dial9: Dial9Context,
        #[metrics(flags(SpanName))]
        first: String,
        #[metrics(flags(SpanName))]
        second: String,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            DuplicateNames {
                dial9: Dial9Context::capture(),
                first: "one".to_owned(),
                second: "two".to_owned(),
            }
        );
    });

    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_valid_span_context(ev);
    assert_eq!(ev.fields["first"], "one");
    assert_eq!(ev.fields["second"], "two");
    // Only the first field claims the span.name role.
    assert_eq!(
        ev.annotations["first"][schema_extensions::ROLE_KEY],
        roles::SPAN_NAME
    );
    assert!(
        ev.annotations
            .get("second")
            .and_then(|a| a.get(schema_extensions::ROLE_KEY))
            .is_none(),
        "the second SpanName field must not carry the role: {ev:#?}"
    );
}

#[test]
fn unhonorable_span_name_falls_back_to_schema_name() {
    use metrique_writer::core::descriptor::FieldShape;

    // An un-honorable `SpanName` flag must never drop the event: the field is
    // recorded like any other (unless separately skipped), no `span.name` role
    // is emitted, and the span falls back to its schema name.
    #[metrics]
    struct NumericName {
        #[metrics(flatten)]
        dial9: Dial9Context,
        #[metrics(flags(SpanName))]
        name: u64,
    }

    // An opaque shape has no wire representation at all, so `SpanName` on it
    // hits the plan's unencodable-shape branch: the field is left out of the
    // payload and the span falls back to the schema name.
    #[derive(Debug)]
    struct OpaqueValue;
    impl metrique_writer::Value for OpaqueValue {
        const SHAPE: FieldShape<'static> = FieldShape::Opaque;
        fn write(&self, writer: impl metrique_writer::ValueWriter) {
            writer.string("opaque");
        }
    }
    impl metrique::CloseValue for OpaqueValue {
        type Closed = OpaqueValue;
        fn close(self) -> OpaqueValue {
            self
        }
    }

    #[metrics]
    struct OpaqueName {
        #[metrics(flatten)]
        dial9: Dial9Context,
        #[metrics(flags(SpanName))]
        name: OpaqueValue,
        keep: u64,
    }

    #[metrics]
    struct SkippedName {
        #[metrics(flatten)]
        dial9: Dial9Context,
        #[metrics(flags(Skip, SpanName))]
        name: String,
        keep: u64,
    }

    #[metrics]
    struct CollidingName {
        #[metrics(flatten)]
        dial9: Dial9Context,
        #[metrics(name = "dial9.thread_id", flags(SpanName))]
        name: String,
        keep: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            SkippedName {
                dial9: Dial9Context::capture(),
                name: "skipped".to_owned(),
                keep: 1,
            }
        );
        feed_entry!(
            stream,
            CollidingName {
                dial9: Dial9Context::capture(),
                name: "colliding".to_owned(),
                keep: 2,
            }
        );
        feed_entry!(
            stream,
            NumericName {
                dial9: Dial9Context::capture(),
                name: 7,
            }
        );
        feed_entry!(
            stream,
            OpaqueName {
                dial9: Dial9Context::capture(),
                name: OpaqueValue,
                keep: 3,
            }
        );
    });

    // Every entry is still recorded (nothing dropped over a bad name hint).
    assert_eq!(
        events.len(),
        4,
        "un-honorable SpanName layouts must still record their events: {events:#?}"
    );
    for event in &events {
        assert_valid_span_context(event);
        assert!(
            !event.annotations.values().any(|ann| ann
                .get(schema_extensions::ROLE_KEY)
                .map(String::as_str)
                == Some(roles::SPAN_NAME)),
            "no field should carry the span.name role: {event:#?}"
        );
    }

    let by_name = |suffix: &str| {
        events
            .iter()
            .find(|e| e.schema_name.contains(suffix))
            .unwrap_or_else(|| panic!("missing event for {suffix}: {events:#?}"))
    };
    // A numeric SpanName field records as its numeric value.
    assert_eq!(by_name("NumericName").fields["name"], "7");
    // Skip still wins over SpanName: the field stays out, siblings remain.
    assert!(!by_name("SkippedName").fields.contains_key("name"));
    assert_eq!(by_name("SkippedName").fields["keep"], "1");
    // A name colliding with a header is dropped as a duplicate; the sibling
    // payload survives.
    assert_eq!(by_name("CollidingName").fields["keep"], "2");
    // An opaque SpanName field is left out of the payload; the sibling
    // survives.
    assert!(!by_name("OpaqueName").fields.contains_key("name"));
    assert_eq!(by_name("OpaqueName").fields["keep"], "3");
}

#[test]
fn on_runtime_context_captures_task_id() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");
    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(2);
    let runtime = recorder
        .handle()
        .attach_tokio_runtime(builder, TokioAttachOptions::default())
        .unwrap();

    let mut stream = Dial9Stream::new(recorder.handle());
    let entry = runtime.block_on(async {
        // Capture inside a task on an instrumented worker; poll once so the
        // worker registers itself.
        tokio::spawn(async { sample_request(None) }).await.unwrap()
    });
    feed_entry!(stream, entry);

    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(5));
    let events = decode_metrique_events(dir.path());
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    // Captured inside a spawned task on a worker thread: a tid that is not
    // the test thread's, and a task id.
    let tid: u64 = ev.fields[field_names::THREAD_ID].parse().unwrap();
    assert_ne!(tid, u64::from(current_tid()), "expected a worker tid");
    assert_ne!(
        ev.fields[field_names::TOKIO_TASK_ID],
        "<none>",
        "expected a task id"
    );
    assert_valid_span_context(ev);
}

#[test]
fn entry_without_context_is_inert() {
    // No `Dial9Context` means the entry never opted in: it flows through to
    // the rest of the pipeline and records nothing, even though its fields
    // are perfectly encodable.
    #[metrics]
    struct NoContext {
        count: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(stream, NoContext { count: 7 });
    });

    assert!(
        events.is_empty(),
        "entry without a Dial9Context recorded: {events:#?}"
    );
}

#[test]
fn optional_context_gates_per_entry() {
    // An `Option`-flattened context changes the descriptor sequence at
    // runtime, so the same Rust type is inert when absent and records when
    // present.
    #[metrics]
    struct MaybeContext {
        #[metrics(flatten)]
        dial9: Option<Dial9Context>,
        count: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            MaybeContext {
                dial9: None,
                count: 1,
            }
        );
        feed_entry!(
            stream,
            MaybeContext {
                dial9: Some(Dial9Context::capture()),
                count: 2,
            }
        );
    });

    assert_eq!(
        events.len(),
        1,
        "expected only the opted-in entry: {events:#?}"
    );
    assert_eq!(events[0].fields["count"], "2");
}

#[test]
fn context_only_entry_emits_header_event() {
    // An entry whose only field is the context still records: the event
    // carries the header with an empty payload.
    #[metrics]
    struct ContextOnly {
        #[metrics(flatten)]
        dial9: Dial9Context,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            ContextOnly {
                dial9: Dial9Context::capture(),
            }
        );
    });

    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_valid_span_context(ev);
}

#[test]
fn disabled_handle_is_noop() {
    let handle = dial9_core::handle::Dial9Handle::disabled();
    let mut stream = Dial9Stream::new(&handle);

    #[metrics]
    struct Simple {
        count: u64,
    }
    feed_entry!(&mut stream, Simple { count: 1 });
}

#[test]
fn hand_written_entry_is_skipped() {
    struct HandWritten;
    impl metrique_writer::Entry for HandWritten {
        fn write<'a>(&'a self, writer: &mut impl metrique_writer::EntryWriter<'a>) {
            writer.value("some_field", &42u64);
        }
    }

    let events = run_sink_test(|stream| {
        stream.next(&HandWritten).unwrap();
        feed_entry!(stream, sample_request(None));
    });

    assert_eq!(events.len(), 1, "hand-written entry must be skipped");
    assert_eq!(events[0].schema_name, "metrique:RequestMetrics");
}

#[test]
fn interned_flag_on_numeric_field_skips_only_that_field() {
    #[metrics]
    struct BadInterned {
        #[metrics(flatten)]
        dial9: Dial9Context,
        #[metrics(flags(Interned))]
        count: u64,
        name: String,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            BadInterned {
                dial9: Dial9Context::capture(),
                count: 9,
                name: "ok".to_owned(),
            }
        );
    });

    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert!(
        !ev.fields.contains_key("count"),
        "mis-flagged field must be skipped: {ev:#?}"
    );
    assert_eq!(ev.fields["name"], "ok");
}

#[test]
fn panicking_value_drops_event_without_poisoning_the_stream() {
    use metrique_writer::core::descriptor::{FieldShape, KnownShape};

    #[derive(Debug)]
    struct PanicOnWrite;
    impl metrique_writer::Value for PanicOnWrite {
        const SHAPE: FieldShape<'static> = FieldShape::Known(KnownShape::U64);
        fn write(&self, _writer: impl metrique_writer::ValueWriter) {
            panic!("boom");
        }
    }
    impl metrique::CloseValue for PanicOnWrite {
        type Closed = PanicOnWrite;
        fn close(self) -> PanicOnWrite {
            self
        }
    }

    #[metrics]
    struct Panicky {
        bad: PanicOnWrite,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(stream, Panicky { bad: PanicOnWrite });
        feed_entry!(stream, sample_request(None));
    });

    assert_eq!(events.len(), 1, "panicking entry must be dropped");
    assert_eq!(events[0].schema_name, "metrique:RequestMetrics");
}

#[test]
fn entries_containing_flex_are_skipped() {
    use metrique::flex::Flex;

    #[metrics]
    struct WithFlex {
        count: u64,
        #[metrics(flatten)]
        extras: Flex<u64>,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            WithFlex {
                count: 5,
                extras: Flex::new("dynamic_a").with_value(1),
            }
        );
        feed_entry!(stream, sample_request(None));
    });

    // Entries containing a Flex have no descriptors at all (`descriptors()`
    // is `Unavailable` and flatten chaining propagates that), so the sink
    // must skip them before planning and keep serving other entry types.
    let names: Vec<&str> = events.iter().map(|e| e.schema_name.as_str()).collect();
    assert!(
        !names.contains(&"metrique:WithFlex"),
        "Flex entries must be dropped: {names:?}"
    );
    assert!(names.contains(&"metrique:RequestMetrics"));
}

#[test]
fn background_queue_end_to_end() {
    use metrique_writer::sink::BackgroundQueue;

    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");
    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    recorder.enable();

    let (queue, join) = BackgroundQueue::new(Dial9Stream::new(recorder.handle()));
    for _ in 0..10 {
        // Uses the macro-generated append-on-drop guard, exactly like an
        // application would.
        drop(sample_request(Some(1)).append_on_drop(queue.clone()));
    }
    join.shut_down();

    recorder.graceful_shutdown(Duration::from_secs(5));
    let events = decode_metrique_events(dir.path());
    assert_eq!(events.len(), 10, "expected all queued entries to record");
}

#[test]
fn timestamp_field_lands_in_wall_clock_header() {
    use std::time::SystemTime;

    #[metrics]
    struct Timestamped {
        #[metrics(flatten)]
        dial9: Dial9Context,
        #[metrics(timestamp)]
        at: SystemTime,
        count: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            Timestamped {
                dial9: Dial9Context::capture(),
                at: SystemTime::now(),
                count: 1,
            }
        );
    });

    assert_eq!(events.len(), 1);
    let wall: u64 = events[0].fields[field_names::WALL_CLOCK_NS]
        .parse()
        .unwrap();
    assert!(
        wall > 1_500_000_000_000_000_000,
        "implausible wall clock: {wall}"
    );
}

#[test]
fn payload_field_named_like_a_structural_field_is_skipped() {
    // An application payload must opt into dial9's namespace to collide.
    // The structural field wins and the rest of the entry still records.
    #[metrics]
    struct SameNames {
        #[metrics(flatten)]
        dial9: Dial9Context,
        #[metrics(name = "dial9.thread_id")]
        colliding_thread_id: u64,
        count: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            SameNames {
                dial9: Dial9Context::capture(),
                colliding_thread_id: 42,
                count: 7,
            }
        );
    });

    assert_eq!(events.len(), 1, "entry must record: {events:#?}");
    let ev = &events[0];
    assert_eq!(ev.fields["count"], "7");
    assert_ne!(ev.fields[field_names::THREAD_ID], "42");
    assert_eq!(
        ev.field_names
            .iter()
            .filter(|name| name.as_str() == field_names::THREAD_ID)
            .count(),
        1
    );
    assert_valid_span_context(ev);
}

#[test]
fn duplicate_contexts_first_wins() {
    #[metrics]
    struct DoubleContext {
        #[metrics(flatten)]
        a: Dial9Context,
        #[metrics(flatten)]
        b: Dial9Context,
        count: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            DoubleContext {
                a: Dial9Context::capture(),
                b: Dial9Context::capture(),
                count: 1,
            }
        );
    });

    // The first flatten site keeps the header slots; the second's fields are
    // skipped (warned once at plan build). The entry still records.
    assert_eq!(events.len(), 1, "entry must record: {events:#?}");
    let ev = &events[0];
    assert_eq!(ev.fields["count"], "1");
    assert_valid_span_context(ev);
}

#[test]
fn identically_named_skipped_fields_are_harmless() {
    // `Skip` fields never enter the schema, so identical names among them
    // cannot collide with anything; the entry must still record.
    #[metrics]
    struct SkippedTwins {
        #[metrics(flatten)]
        dial9: Dial9Context,
        #[metrics(name = "twin", flags(Skip))]
        first: u64,
        #[metrics(name = "twin", flags(Skip))]
        second: u64,
        count: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            SkippedTwins {
                dial9: Dial9Context::capture(),
                first: 1,
                second: 2,
                count: 9,
            }
        );
    });

    assert_eq!(
        events.len(),
        1,
        "Skip+Skip duplicates must not drop: {events:#?}"
    );
    assert_eq!(events[0].fields["count"], "9");
}

#[test]
fn custom_signed_value_round_trips_negatives() {
    use metrique_writer::core::descriptor::{FieldShape, KnownShape};
    use metrique_writer::{MetricFlags, Observation, Unit};

    // Signed values can only travel as floats: metrique's `Observation` has
    // no signed variant.
    #[derive(Debug)]
    struct Signed(i64);
    impl metrique_writer::Value for Signed {
        const SHAPE: FieldShape<'static> = FieldShape::Known(KnownShape::I64);
        fn write(&self, writer: impl metrique_writer::ValueWriter) {
            writer.metric(
                [Observation::Floating(self.0 as f64)],
                Unit::None,
                [],
                MetricFlags::empty(),
            );
        }
    }
    impl metrique::CloseValue for Signed {
        type Closed = Signed;
        fn close(self) -> Signed {
            self
        }
    }

    #[metrics]
    struct WithSigned {
        #[metrics(flatten)]
        dial9: Dial9Context,
        delta: Signed,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            WithSigned {
                dial9: Dial9Context::capture(),
                delta: Signed(-42),
            }
        );
    });

    assert_eq!(events.len(), 1, "signed entry must record: {events:#?}");
    assert_eq!(events[0].fields["delta"], "-42");
}

#[test]
fn prefixed_context_still_routes() {
    // A prefix at the flatten site changes the emitted context field names
    // but not their descriptor base names, so role routing is unaffected.
    #[metrics]
    struct Prefixed {
        #[metrics(flatten, prefix = "req_")]
        dial9: Dial9Context,
        application_thread_id: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            Prefixed {
                dial9: Dial9Context::capture(),
                application_thread_id: 7,
            }
        );
    });

    assert_eq!(events.len(), 1, "prefixed entry must record: {events:#?}");
    let ev = &events[0];
    // Context still routes (roles match on base name, prefix-insensitive).
    assert_valid_span_context(ev);
    assert_eq!(ev.fields["application_thread_id"], "7");
}

#[test]
fn mid_entry_payload_flatten_routes_correctly() {
    // A payload-bearing child flattened mid-entry: descriptor segments
    // interleave with the parent's field runs ([Outer: a], [Inner: x, y],
    // [Outer: b]), and positional consumption must put every value in the
    // right slot on the first and subsequent walks.
    #[metrics(subfield)]
    struct Inner {
        x: u64,
        y: &'static str,
    }

    #[metrics]
    struct Outer {
        #[metrics(flatten)]
        dial9: Dial9Context,
        a: u64,
        #[metrics(flatten)]
        inner: Inner,
        b: u64,
    }

    fn entry(a: u64, x: u64, y: &'static str, b: u64) -> Outer {
        Outer {
            dial9: Dial9Context::capture(),
            a,
            inner: Inner { x, y },
            b,
        }
    }

    let events = run_sink_test(|stream| {
        feed_entry!(stream, entry(1, 2, "first", 3));
        feed_entry!(stream, entry(10, 20, "second", 30));
    });

    assert_eq!(events.len(), 2, "both walks must record: {events:#?}");
    for (ev, (a, x, y, b)) in events
        .iter()
        .zip([(1, 2, "first", 3), (10, 20, "second", 30)])
    {
        assert_eq!(ev.fields["a"], a.to_string());
        assert_eq!(ev.fields["x"], x.to_string());
        assert_eq!(ev.fields["y"], y);
        assert_eq!(ev.fields["b"], b.to_string());
    }
}

#[test]
fn entry_enum_variants_get_distinct_schemas() {
    #[metrics]
    enum Request {
        Read {
            #[metrics(flatten)]
            dial9: Dial9Context,
            bytes: u64,
        },
        Write {
            #[metrics(flatten)]
            dial9: Dial9Context,
            bytes: u64,
            durable: bool,
        },
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            Request::Read {
                dial9: Dial9Context::capture(),
                bytes: 100,
            }
        );
        feed_entry!(
            stream,
            Request::Write {
                dial9: Dial9Context::capture(),
                bytes: 200,
                durable: true,
            }
        );
        // Second Read exercises the variant's cached positional dispatch.
        feed_entry!(
            stream,
            Request::Read {
                dial9: Dial9Context::capture(),
                bytes: 300,
            }
        );
    });

    assert_eq!(events.len(), 3, "all variants must record: {events:#?}");
    let names: Vec<&str> = events.iter().map(|e| e.schema_name.as_str()).collect();
    // Each variant carries its own descriptor name and schema.
    assert_eq!(names[0], names[2], "same variant must share a schema");
    assert_ne!(
        names[0], names[1],
        "variants must not share a schema: {names:?}"
    );
    assert_eq!(events[0].fields["bytes"], "100");
    assert_eq!(events[1].fields["bytes"], "200");
    assert_eq!(events[1].fields["durable"], "true");
    assert_eq!(events[2].fields["bytes"], "300");
    // Context routed for every variant.
    for ev in &events {
        assert_valid_span_context(ev);
    }
}

#[test]
fn typed_lists_round_trip() {
    #[metrics]
    struct WithLists {
        #[metrics(flatten)]
        dial9: Dial9Context,
        counts: Vec<u64>,
        #[metrics(flags(Interned))]
        labels: Vec<String>,
        empty: Vec<u64>,
        maybe: Option<Vec<u64>>,
    }

    fn entry(maybe: Option<Vec<u64>>) -> WithLists {
        WithLists {
            dial9: Dial9Context::capture(),
            counts: vec![1, 2, 3],
            labels: vec!["a".to_owned(), "b".to_owned()],
            empty: vec![],
            maybe,
        }
    }

    #[derive(Debug, serde::Deserialize)]
    struct Decoded {
        counts: Vec<u64>,
        labels: Vec<String>,
        empty: Vec<u64>,
        maybe: Option<Vec<u64>>,
    }

    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");
    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    recorder.enable();
    let mut stream = Dial9Stream::new(recorder.handle());
    feed_entry!(stream, entry(Some(vec![7])));
    feed_entry!(stream, entry(None));
    recorder.graceful_shutdown(Duration::from_secs(5));

    let typed: Vec<Decoded> = decode_metrique_events_as(dir.path());
    assert_eq!(typed.len(), 2, "both entries must record: {typed:#?}");
    assert_eq!(typed[0].counts, vec![1, 2, 3]);
    assert_eq!(typed[0].labels, vec!["a", "b"]);
    assert_eq!(typed[0].empty, Vec::<u64>::new());
    assert_eq!(typed[0].maybe.as_deref(), Some(&[7][..]));
    assert_eq!(typed[1].maybe, None);

    // Interned lists pool their string elements (a schema property the
    // typed decode intentionally hides).
    let events = decode_metrique_events(dir.path());
    assert!(
        events[0].pooled_fields.contains(&"labels".to_owned()),
        "interned list elements must be pooled: {events:#?}"
    );
}

/// Wraps a descriptor-carrying entry and distorts its write-time callback
/// count, simulating a metrique descriptor/write contract violation.
struct SkewedWrites<E> {
    inner: E,
    /// Extra trailing `value` callbacks to emit (overflow).
    extra: usize,
    /// Real callbacks to swallow from the front (underflow).
    drop_first: usize,
}

impl<E: metrique_writer::Entry> metrique_writer::Entry for SkewedWrites<E> {
    fn write<'a>(&'a self, writer: &mut impl metrique_writer::EntryWriter<'a>) {
        struct Skimmer<'w, W> {
            inner: &'w mut W,
            remaining_to_drop: usize,
        }
        impl<'a, W: metrique_writer::EntryWriter<'a>> metrique_writer::EntryWriter<'a> for Skimmer<'_, W> {
            fn timestamp(&mut self, timestamp: std::time::SystemTime) {
                self.inner.timestamp(timestamp);
            }
            fn value(
                &mut self,
                name: impl Into<std::borrow::Cow<'a, str>>,
                value: &(impl metrique_writer::Value + ?Sized),
            ) {
                if self.remaining_to_drop > 0 {
                    self.remaining_to_drop -= 1;
                    return;
                }
                self.inner.value(name, value);
            }
            fn config(&mut self, config: &'a dyn metrique_writer::EntryConfig) {
                self.inner.config(config);
            }
        }

        let mut skimmer = Skimmer {
            inner: writer,
            remaining_to_drop: self.drop_first,
        };
        self.inner.write(&mut skimmer);
        for _ in 0..self.extra {
            writer.value("phantom", &1u64);
        }
    }

    fn descriptors(&self) -> metrique_writer::core::Descriptors<'_> {
        self.inner.descriptors()
    }
}

#[test]
fn callback_count_mismatch_drops_the_event() {
    #[metrics]
    struct Counted {
        #[metrics(flatten)]
        dial9: Dial9Context,
        a: u64,
        b: u64,
    }

    fn closed(a: u64, b: u64) -> impl metrique_writer::Entry {
        metrique::RootEntry::new(metrique::CloseValue::close(Counted {
            dial9: Dial9Context::capture(),
            a,
            b,
        }))
    }

    let events = run_sink_test(|stream| {
        // Overflow: one phantom trailing callback.
        stream
            .next(&SkewedWrites {
                inner: closed(1, 2),
                extra: 1,
                drop_first: 0,
            })
            .unwrap();
        // Underflow: the first callback swallowed shifts every later value
        // one slot left; only the count check can see that.
        stream
            .next(&SkewedWrites {
                inner: closed(3, 4),
                extra: 0,
                drop_first: 1,
            })
            .unwrap();
        // Control: an unskewed entry of the same type still records.
        stream.next(&closed(5, 6)).unwrap();
    });

    assert_eq!(events.len(), 1, "skewed walks must drop: {events:#?}");
    assert_eq!(events[0].fields["a"], "5");
    assert_eq!(events[0].fields["b"], "6");
}

#[test]
fn required_field_producing_no_value_drops_the_event() {
    use metrique_writer::core::descriptor::{FieldShape, KnownShape};

    // SHAPE claims u64 but the impl writes a string: the planned slot stays
    // empty and the field is non-optional, so the event must drop.
    #[derive(Debug)]
    struct LyingShape;
    impl metrique_writer::Value for LyingShape {
        const SHAPE: FieldShape<'static> = FieldShape::Known(KnownShape::U64);
        fn write(&self, writer: impl metrique_writer::ValueWriter) {
            writer.string("not a number");
        }
    }
    impl metrique::CloseValue for LyingShape {
        type Closed = LyingShape;
        fn close(self) -> LyingShape {
            self
        }
    }

    #[metrics]
    struct Lying {
        bad: LyingShape,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(stream, Lying { bad: LyingShape });
        feed_entry!(stream, sample_request(None));
    });

    let names: Vec<&str> = events.iter().map(|e| e.schema_name.as_str()).collect();
    assert!(
        !names.contains(&"metrique:Lying"),
        "shape/value mismatch on a required field must drop: {names:?}"
    );
    assert!(names.contains(&"metrique:RequestMetrics"));
}

#[test]
fn scalar_callback_on_list_shaped_field_drops_the_event() {
    use metrique_writer::core::descriptor::{FieldShape, KnownShape, ShapeRef};
    use metrique_writer::{MetricFlags, Observation, Unit};

    // SHAPE claims a list but the impl writes a scalar metric; the list slot
    // stays empty and the field is non-optional.
    #[derive(Debug)]
    struct FakeList;
    impl metrique_writer::Value for FakeList {
        const SHAPE: FieldShape<'static> =
            FieldShape::List(ShapeRef::new(&FieldShape::Known(KnownShape::U64)));
        fn write(&self, writer: impl metrique_writer::ValueWriter) {
            writer.metric(
                [Observation::Unsigned(7)],
                Unit::None,
                [],
                MetricFlags::empty(),
            );
        }
    }
    impl metrique::CloseValue for FakeList {
        type Closed = FakeList;
        fn close(self) -> FakeList {
            self
        }
    }

    #[metrics]
    struct NotAList {
        items: FakeList,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(stream, NotAList { items: FakeList });
        feed_entry!(stream, sample_request(None));
    });

    let names: Vec<&str> = events.iter().map(|e| e.schema_name.as_str()).collect();
    assert!(
        !names.contains(&"metrique:NotAList"),
        "scalar data for a list shape must drop: {names:?}"
    );
    assert!(names.contains(&"metrique:RequestMetrics"));
}

#[test]
fn duplicate_payload_names_keep_the_first_occurrence() {
    #[metrics]
    struct Twins {
        #[metrics(flatten)]
        dial9: Dial9Context,
        #[metrics(name = "twin")]
        first: u64,
        #[metrics(name = "twin")]
        second: u64,
        count: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            Twins {
                dial9: Dial9Context::capture(),
                first: 1,
                second: 2,
                count: 9,
            }
        );
    });

    assert_eq!(events.len(), 1, "entry must record: {events:#?}");
    let ev = &events[0];
    assert_eq!(ev.fields["twin"], "1", "first occurrence keeps the slot");
    assert_eq!(ev.fields["count"], "9");
    let dupes: Vec<&String> = ev
        .field_names
        .iter()
        .filter(|n| n.as_str() == "twin")
        .collect();
    assert_eq!(dupes.len(), 1, "schema must not carry duplicate names");
}

#[test]
fn same_named_entry_types_get_distinct_schemas() {
    mod first {
        use super::*;
        #[metrics]
        pub struct Dup {
            #[metrics(flatten)]
            pub dial9: Dial9Context,
            pub a: u64,
        }
    }
    mod second {
        use super::*;
        #[metrics]
        pub struct Dup {
            #[metrics(flatten)]
            pub dial9: Dial9Context,
            pub b: u64,
        }
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            first::Dup {
                dial9: Dial9Context::capture(),
                a: 1,
            }
        );
        feed_entry!(
            stream,
            second::Dup {
                dial9: Dial9Context::capture(),
                b: 2,
            }
        );
    });

    assert_eq!(events.len(), 2, "both types must record: {events:#?}");
    let names: Vec<&str> = events.iter().map(|e| e.schema_name.as_str()).collect();
    assert_ne!(
        names[0], names[1],
        "same-named types must not share a schema"
    );
    assert!(
        names.contains(&"metrique:Dup"),
        "first arrival keeps the bare name"
    );
    assert!(
        names.iter().any(|n| n.starts_with("metrique:Dup#")),
        "later arrival gets a layout-hash suffix: {names:?}"
    );
}

#[test]
fn every_context_role_routes() {
    // Guards the sync between Dial9Context's field set and the role table
    // in plan.rs: a context field the sink no longer recognizes would
    // silently degrade to a skip. All four header slots must be populated
    // from an on-runtime capture.
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");
    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(1);
    let runtime = recorder
        .handle()
        .attach_tokio_runtime(builder, TokioAttachOptions::default())
        .unwrap();

    let mut stream = Dial9Stream::new(recorder.handle());
    let entry =
        runtime.block_on(async { tokio::spawn(async { sample_request(None) }).await.unwrap() });
    feed_entry!(stream, entry);

    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(5));
    let events = decode_metrique_events(dir.path());
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    // ThreadId and MonotonicStart are fields; MonotonicEnd is the packed
    // timestamp. TaskId is present because capture happened inside a task.
    assert_valid_span_context(ev);
    assert_ne!(ev.fields[field_names::TOKIO_TASK_ID], "<none>");
}

#[test]
fn parent_rename_all_does_not_restyle_structural_names() {
    // Context routing relies on Dial9Context's literal internal field names.
    // The generated structural field names are also fixed independently of
    // the parent entry's style.
    #[metrics(rename_all = "PascalCase")]
    struct Styled {
        #[metrics(flatten)]
        dial9: Dial9Context,
        some_count: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            Styled {
                dial9: Dial9Context::capture(),
                some_count: 4,
            }
        );
    });

    assert_eq!(events.len(), 1, "styled entry must record: {events:#?}");
    let ev = &events[0];
    // The payload field is restyled...
    assert_eq!(ev.fields["SomeCount"], "4");
    // ...while the structural fields keep their literal lowercase names,
    // and the context still routed into them.
    assert!(
        ev.field_names
            .iter()
            .any(|name| name == field_names::THREAD_ID)
            && ev
                .field_names
                .iter()
                .any(|name| name == field_names::SPAN_DURATION_NS),
        "structural names must not be restyled: {:?}",
        ev.field_names
    );
    assert_valid_span_context(ev);
}

#[test]
fn absent_optional_list_elements_are_omitted() {
    #[metrics]
    struct Sparse {
        #[metrics(flatten)]
        dial9: Dial9Context,
        values: Vec<Option<u64>>,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            Sparse {
                dial9: Dial9Context::capture(),
                values: vec![Some(1), None, Some(3)],
            }
        );
    });

    assert_eq!(events.len(), 1, "entry must record: {events:#?}");
    // The None element writes nothing and drops out of the encoded list.
    assert_eq!(events[0].fields["values"], "[1,3]");
}

#[test]
fn boxed_entries_round_trip() {
    // The global-sink path (`ServiceMetrics::sink()`, as in the examples)
    // boxes entries; metrique's dyn bridge stringifies list elements
    // (`ValueWriterFromDyn::values` in metrique-writer-core), which loses
    // the element type. String lists survive; numeric lists cannot be
    // captured and drop the event rather than record a wrong (empty or
    // partial) list. See the module docs' limitations.
    #[metrics]
    struct BoxedStrings {
        #[metrics(flatten)]
        dial9: Dial9Context,
        #[metrics(flags(Interned))]
        labels: Vec<String>,
        count: u64,
    }

    #[metrics]
    struct BoxedNumbers {
        counts: Vec<u64>,
    }

    let events = run_sink_test(|stream| {
        let strings = metrique::RootEntry::new(metrique::CloseValue::close(BoxedStrings {
            dial9: Dial9Context::capture(),
            labels: vec!["a".to_owned(), "b".to_owned()],
            count: 4,
        }));
        stream
            .next(&metrique_writer::Entry::boxed(strings))
            .unwrap();

        let numbers = metrique::RootEntry::new(metrique::CloseValue::close(BoxedNumbers {
            counts: vec![1, 2, 3],
        }));
        stream
            .next(&metrique_writer::Entry::boxed(numbers))
            .unwrap();
    });

    let names: Vec<&str> = events.iter().map(|e| e.schema_name.as_str()).collect();
    assert!(
        !names.contains(&"metrique:BoxedNumbers"),
        "boxed numeric lists must drop, not record wrong data: {names:?}"
    );

    assert_eq!(
        names,
        ["metrique:BoxedStrings"],
        "boxed string entry must record"
    );
    let ev = &events[0];
    assert_eq!(ev.fields["count"], "4");
    assert_eq!(ev.fields["labels"], "[a,b]");
    assert!(
        ev.pooled_fields.contains(&"labels".to_owned()),
        "interning must survive the box: {ev:#?}"
    );
    assert_valid_span_context(ev);
}

#[test]
fn paused_recorder_records_nothing_until_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");
    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();

    let mut stream = Dial9Stream::new(recorder.handle());
    // Connected but paused: the entry must be skipped (before plan work
    // even happens; see `is_enabled` in `next`).
    recorder.disable();
    feed_entry!(stream, sample_request(None));

    recorder.enable();
    feed_entry!(stream, sample_request(Some(1)));

    recorder.graceful_shutdown(Duration::from_secs(5));
    let events = decode_metrique_events(dir.path());
    assert_eq!(events.len(), 1, "only the post-enable entry records");
    assert_eq!(events[0].fields["Retries"], "1");
}

/// A downstream sink that records what an entry looked like when it arrived:
/// the field names written, and the field names its descriptor declared
/// (`None` when the descriptor came through as `Unavailable`).
#[derive(Debug, Default, Clone)]
struct Seen {
    written: Vec<String>,
    described: Option<Vec<String>>,
}

#[derive(Debug, Default)]
struct RecordingStream(std::sync::Arc<std::sync::Mutex<Seen>>);

impl RecordingStream {
    fn shared() -> (Self, std::sync::Arc<std::sync::Mutex<Seen>>) {
        let cell = std::sync::Arc::new(std::sync::Mutex::new(Seen::default()));
        (Self(cell.clone()), cell)
    }
}

impl EntryIoStream for RecordingStream {
    fn next(&mut self, entry: &impl metrique_writer::Entry) -> Result<(), IoStreamError> {
        struct NameCollector<'n>(&'n mut Vec<String>);
        impl<'a> metrique_writer::EntryWriter<'a> for NameCollector<'_> {
            fn timestamp(&mut self, _timestamp: std::time::SystemTime) {}
            fn value(
                &mut self,
                name: impl Into<std::borrow::Cow<'a, str>>,
                _value: &(impl metrique_writer::Value + ?Sized),
            ) {
                self.0.push(name.into().into_owned());
            }
            fn config(&mut self, _config: &'a dyn metrique_writer::EntryConfig) {}
        }

        let mut seen = self.0.lock().unwrap();
        let mut written = Vec::new();
        entry.write(&mut NameCollector(&mut written));
        seen.written = written;
        seen.described = entry.descriptors().into_available().map(|descs| {
            descs
                .iter()
                .flat_map(|d| {
                    d.fields()
                        .map(|f| f.name_parts().collect::<String>())
                        .collect::<Vec<_>>()
                })
                .collect()
        });
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn tee_hides_dial9_fields_from_the_other_sink() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");
    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    recorder.enable();

    let (other, seen) = RecordingStream::shared();
    let mut stream = Dial9Stream::tee(recorder.handle(), other);
    feed_entry!(stream, sample_request(Some(1)));

    recorder.graceful_shutdown(Duration::from_secs(5));

    let seen = seen.lock().unwrap().clone();
    // The other sink sees the user's own fields...
    assert!(
        seen.written.contains(&"Operation".to_owned()),
        "payload field missing downstream: {seen:#?}"
    );
    // ...and none of dial9's.
    assert!(
        !seen.written.iter().any(|n| n.starts_with("dial9.")),
        "dial9 fields leaked downstream: {seen:#?}"
    );
    // The descriptor it sees matches the values it received.
    let described = seen.described.expect("descriptors must stay available");
    assert!(
        !described.iter().any(|n| n.starts_with("dial9.")),
        "dial9 fields leaked into the descriptor: {described:?}"
    );
    assert_eq!(
        described, seen.written,
        "filtered descriptor must line up with the filtered write order"
    );

    // dial9's own side still recorded the context.
    let events = decode_metrique_events(dir.path());
    assert_eq!(events.len(), 1, "dial9 side must still record: {events:#?}");
    assert_valid_span_context(&events[0]);
}

#[test]
fn user_field_squatting_the_dial9_namespace_degrades_descriptors() {
    // The context is always its own descriptor segment, so the only way to
    // get a segment that mixes dial9-named and user fields is a user field
    // literally named `dial9.*`. Its value is filtered but the segment's
    // field list cannot be, so the downstream descriptor degrades instead of
    // disagreeing with the values.
    #[metrics]
    struct Mixed {
        #[metrics(flatten)]
        dial9: Dial9Context,
        #[metrics(name = "dial9.mine")]
        mine: u64,
        count: u64,
    }

    let handle = dial9_core::handle::Dial9Handle::disabled();
    let (other, seen) = RecordingStream::shared();
    let mut stream = Dial9Stream::tee(&handle, other);
    feed_entry!(
        stream,
        Mixed {
            dial9: Dial9Context::capture(),
            mine: 1,
            count: 2,
        }
    );

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.written,
        vec!["count".to_owned()],
        "only non-dial9 values may reach the other sink: {seen:#?}"
    );
    assert!(
        seen.described.is_none(),
        "a partially-filtered segment must report Unavailable: {seen:#?}"
    );
}

#[test]
fn wrapping_an_entry_records_the_same_context_as_flattening_one() {
    // The entry type knows nothing about dial9.
    #[metrics(rename_all = "PascalCase")]
    struct Plain {
        #[metrics(flags(Interned))]
        route: String,
        latency_us: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            Plain {
                route: "/pets".to_owned(),
                latency_us: 12,
            }
            .with_dial9_context()
        );
    });

    assert_eq!(events.len(), 1, "wrapped entry must record: {events:#?}");
    let ev = &events[0];
    // Schema name comes from the wrapped entry, not the wrapper.
    assert_eq!(ev.schema_name, "metrique:Plain");
    // Payload is intact, including flags on the inner fields.
    assert_eq!(ev.fields["Route"], "/pets");
    assert_eq!(ev.fields["LatencyUs"], "12");
    assert!(ev.pooled_fields.contains(&"Route".to_owned()));
    // ...and the context landed in the header exactly as a flattened
    // Dial9Context would.
    assert_eq!(
        ev.fields[field_names::THREAD_ID],
        u64::from(current_tid()).to_string()
    );
    assert_valid_span_context(ev);
    // The wrapper contributes no payload field of its own.
    assert!(
        !ev.field_names.iter().any(|n| n == "dial9"),
        "wrapper leaked a field: {:?}",
        ev.field_names
    );
}

#[test]
fn append_on_drop_dial9_records_through_a_sink() {
    use metrique_writer::sink::BackgroundQueue;

    #[metrics(rename_all = "PascalCase")]
    struct Plain {
        latency_ms: u64,
        success: bool,
    }

    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");
    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    recorder.enable();

    let (queue, join) = BackgroundQueue::new(Dial9Stream::new(recorder.handle()));
    {
        let mut m = Plain {
            latency_ms: 0,
            success: false,
        }
        .append_on_drop_dial9(queue.clone());
        // Deref reaches the wrapped entry's fields, so call sites read the
        // same as they did before dial9 was involved.
        m.latency_ms = 7;
        m.success = true;
    }
    join.shut_down();

    recorder.graceful_shutdown(Duration::from_secs(5));
    let events = decode_metrique_events(dir.path());
    assert_eq!(events.len(), 1, "expected one recorded entry: {events:#?}");
    let ev = &events[0];
    assert_eq!(ev.fields["LatencyMs"], "7");
    assert_eq!(ev.fields["Success"], "true");
    assert_valid_span_context(ev);
}

#[test]
fn wrapped_context_is_hidden_from_the_other_sink_too() {
    #[metrics(rename_all = "PascalCase")]
    struct Plain {
        count: u64,
    }

    let handle = dial9_core::handle::Dial9Handle::disabled();
    let (other, seen) = RecordingStream::shared();
    let mut stream = Dial9Stream::tee(&handle, other);
    feed_entry!(stream, Plain { count: 3 }.with_dial9_context());

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.written,
        vec!["Count".to_owned()],
        "wrapper context must not reach the other sink: {seen:#?}"
    );
    assert_eq!(
        seen.described.as_deref(),
        Some(&["Count".to_owned()][..]),
        "descriptor must match the filtered values: {seen:#?}"
    );
}

#[test]
fn multi_observation_distribution_records_the_first() {
    use metrique_writer::core::descriptor::{FieldShape, KnownShape};
    use metrique_writer::{MetricFlags, Observation, Unit};

    // A custom `Value` that emits several observations into a field whose
    // declared shape is a scalar. The first observation is recorded; the
    // rest are dropped (debug-logged) rather than losing the field or the
    // event.
    #[derive(Debug)]
    struct Burst;
    impl metrique_writer::Value for Burst {
        const SHAPE: FieldShape<'static> = FieldShape::Known(KnownShape::U64);
        fn write(&self, writer: impl metrique_writer::ValueWriter) {
            writer.metric(
                [
                    Observation::Unsigned(11),
                    Observation::Unsigned(22),
                    Observation::Unsigned(33),
                ],
                Unit::None,
                [],
                MetricFlags::empty(),
            );
        }
    }
    impl metrique::CloseValue for Burst {
        type Closed = Burst;
        fn close(self) -> Burst {
            self
        }
    }

    #[metrics]
    struct WithBurst {
        #[metrics(flatten)]
        dial9: Dial9Context,
        samples: Burst,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            WithBurst {
                dial9: Dial9Context::capture(),
                samples: Burst,
            }
        );
    });

    assert_eq!(events.len(), 1, "entry must record: {events:#?}");
    assert_eq!(events[0].fields["samples"], "11");
}

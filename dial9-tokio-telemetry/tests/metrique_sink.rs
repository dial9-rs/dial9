//! End-to-end tests for the metrique → dial9 sink: entries flow through
//! `Dial9Stream`, land in a sealed trace file, and decode back with the
//! expected schema, header context, payload values, and unit annotations.
#![cfg(feature = "metrique-sink")]

use std::collections::HashMap;
use std::time::Duration;

use dial9_tokio_telemetry::metrique_sink::{Dial9Context, Dial9Stream, Interned, Skip};
use dial9_tokio_telemetry::telemetry::{DiskBuffer, RecorderTokioExt, WorkerId, recorder};
use dial9_trace_format::types::FieldValueRef;
use metrique::unit::Microsecond;
use metrique::unit_of_work::metrics;
use metrique_writer::EntryIoStream;

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
    /// Names of fields carried as pooled strings.
    pooled_fields: Vec<String>,
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
                    if ann.field_index() as usize == i && ann.key() == "unit" {
                        units.insert(def.name().to_owned(), ann.value().to_owned());
                    }
                }
            }
            events.push(DecodedEvent {
                schema_name: ev.name.to_owned(),
                timestamp_ns: ev.timestamp_ns.expect("metrique event missing timestamp"),
                fields,
                field_names,
                units,
                pooled_fields,
            });
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
    let events = run_sink_test(|stream| {
        feed_entry!(stream, sample_request(Some(2)));
        feed_entry!(stream, sample_request(None));
    });

    assert_eq!(events.len(), 2, "expected two events, got {events:#?}");
    let ev = &events[0];
    assert_eq!(ev.schema_name, "metrique:RequestMetrics");

    // Payload fields, post-rename.
    assert_eq!(ev.fields["Route"], "/pets");
    assert_eq!(ev.fields["Operation"], "GetPet");
    assert_eq!(ev.fields["LatencyUs"], "1500");
    assert_eq!(ev.fields["Retries"], "2");
    assert_eq!(ev.fields["Tags"], "[a,b]"); // typed list encoding
    assert_eq!(ev.fields["Success"], "true");
    assert_eq!(ev.fields["Load"], "0.5");
    // Duration writes fractional milliseconds.
    assert_eq!(ev.fields["Elapsed"], "250");
    assert!(
        !ev.fields.contains_key("DebugBlob"),
        "Skip field leaked into the payload: {ev:#?}"
    );

    assert_eq!(ev.units["LatencyUs"], "us");
    assert_eq!(ev.units["Elapsed"], "ms");

    assert!(ev.pooled_fields.contains(&"Route".to_owned()));
    assert!(!ev.pooled_fields.contains(&"Operation".to_owned()));

    // Off-runtime capture still records timestamps; worker is unknown and
    // the task id is absent.
    assert_eq!(
        ev.fields["dial9.worker_id"],
        WorkerId::UNKNOWN.as_u64().to_string()
    );
    assert_eq!(ev.fields["dial9.task_id"], "<none>");
    assert!(ev.timestamp_ns > 0, "start timestamp missing");
    // Close-time duration: present and numeric (may be arbitrarily small,
    // since the entry closes right after capture).
    let _duration: u64 = ev.fields["dial9.duration_ns"].parse().unwrap();

    assert_eq!(events[1].fields["Retries"], "<none>");
}

#[test]
fn on_runtime_context_captures_worker_and_task() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");
    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    let (recorder, runtime) = recorder
        .attach_tokio_runtime(|t| {
            t.worker_threads(2);
        })
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
    assert_ne!(
        ev.fields["dial9.worker_id"],
        WorkerId::UNKNOWN.as_u64().to_string(),
        "expected a real worker id"
    );
    assert_ne!(ev.fields["dial9.task_id"], "<none>", "expected a task id");
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
    assert_ne!(ev.fields["dial9.duration_ns"], "<none>");
}

#[test]
fn disabled_handle_is_noop() {
    let handle = dial9_tokio_telemetry::telemetry::Dial9Handle::disabled();
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
    let wall: u64 = events[0].fields["dial9.wall_clock_ns"].parse().unwrap();
    assert!(
        wall > 1_500_000_000_000_000_000,
        "implausible wall clock: {wall}"
    );
}

#[test]
fn payload_field_named_like_a_header_field_coexists_with_it() {
    // The `dial9.` prefix on header fields means a user field named
    // `worker_id` is not a collision: both land, side by side.
    #[metrics]
    struct SameNames {
        #[metrics(flatten)]
        dial9: Dial9Context,
        worker_id: u64,
        count: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            SameNames {
                dial9: Dial9Context::capture(),
                worker_id: 42,
                count: 7,
            }
        );
    });

    assert_eq!(events.len(), 1, "entry must record: {events:#?}");
    let ev = &events[0];
    assert_eq!(ev.fields["count"], "7");
    // The payload keeps its own value...
    assert_eq!(ev.fields["worker_id"], "42");
    // ...and the header carries the captured context, unaffected.
    assert_ne!(ev.fields["dial9.worker_id"], "42");
    let _duration: u64 = ev.fields["dial9.duration_ns"].parse().unwrap();
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
    let _duration: u64 = ev.fields["dial9.duration_ns"].parse().unwrap();
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
        worker_id: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            Prefixed {
                dial9: Dial9Context::capture(),
                worker_id: 7,
            }
        );
    });

    assert_eq!(events.len(), 1, "prefixed entry must record: {events:#?}");
    let ev = &events[0];
    // Context still routes (roles match on base name, prefix-insensitive).
    let _duration: u64 = ev.fields["dial9.duration_ns"].parse().unwrap();
    assert_ne!(ev.fields["dial9.worker_id"], "7");
    assert_eq!(ev.fields["worker_id"], "7");
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
        let _duration: u64 = ev.fields["dial9.duration_ns"].parse().unwrap();
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

    let events = run_sink_test(|stream| {
        feed_entry!(stream, entry(Some(vec![7])));
        feed_entry!(stream, entry(None));
    });

    assert_eq!(events.len(), 2, "both entries must record: {events:#?}");
    let ev = &events[0];
    assert_eq!(ev.fields["counts"], "[1,2,3]");
    assert_eq!(ev.fields["labels"], "[a,b]");
    assert_eq!(ev.fields["empty"], "[]");
    assert_eq!(ev.fields["maybe"], "[7]");
    // Interned lists pool their string elements.
    assert!(
        ev.pooled_fields.contains(&"labels".to_owned()),
        "interned list elements must be pooled: {ev:#?}"
    );
    assert_eq!(events[1].fields["maybe"], "<none>");
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
    let (recorder, runtime) = recorder
        .attach_tokio_runtime(|t| {
            t.worker_threads(1);
        })
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
    // WorkerId, TaskId, MonotonicEnd routed; MonotonicStart is the event
    // timestamp.
    assert_ne!(
        ev.fields["dial9.worker_id"],
        WorkerId::UNKNOWN.as_u64().to_string()
    );
    assert_ne!(ev.fields["dial9.task_id"], "<none>");
    assert_ne!(ev.fields["dial9.duration_ns"], "<none>");
    assert!(ev.timestamp_ns > 0);
}

#[test]
fn parent_rename_all_does_not_restyle_context_names() {
    // Context routing and the `dial9.` header names both rely on
    // Dial9Context's literal field names being style-invariant. A parent
    // asking for a different style must not reach them.
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
    // ...while the header keeps its literal lowercase names, and the
    // context still routed into it.
    assert!(
        ev.field_names.contains(&"dial9.worker_id".to_owned()),
        "header names must not be restyled: {:?}",
        ev.field_names
    );
    let _duration: u64 = ev.fields["dial9.duration_ns"].parse().unwrap();
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
    let _duration: u64 = ev.fields["dial9.duration_ns"]
        .parse()
        .expect("context must route through the box");
}

#[test]
fn paused_recorder_records_nothing_until_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");
    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();

    let mut stream = Dial9Stream::new(recorder.handle());
    // Connected but paused: the entry must be skipped (before plan work
    // even happens; see `is_recording` in `next`).
    recorder.disable();
    feed_entry!(stream, sample_request(None));

    recorder.enable();
    feed_entry!(stream, sample_request(Some(1)));

    recorder.graceful_shutdown(Duration::from_secs(5));
    let events = decode_metrique_events(dir.path());
    assert_eq!(events.len(), 1, "only the post-enable entry records");
    assert_eq!(events[0].fields["Retries"], "1");
}

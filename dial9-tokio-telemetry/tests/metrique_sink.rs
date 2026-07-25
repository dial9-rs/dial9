//! End-to-end tests for the metrique → dial9 sink: entries flow through
//! `Dial9Stream`, land in a sealed trace file, and decode back with the
//! expected schema, header context, payload values, and unit annotations.

use std::collections::HashMap;
use std::time::Duration;

use dial9_tokio_telemetry::metrique_sink::{Dial9Context, Dial9Stream, Emit, Interned};
use dial9_tokio_telemetry::telemetry::{DiskBuffer, RecorderTokioExt, recorder};
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
            let mut units = HashMap::new();
            let mut pooled_fields = Vec::new();
            for (i, (def, value)) in ev.schema.fields().iter().zip(ev.fields.iter()).enumerate() {
                fields.insert(def.name().to_owned(), render(value, ev.string_pool));
                if matches!(value, FieldValueRef::PooledString(_)) {
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
                timestamp_ns: ev.timestamp_ns.unwrap_or(0),
                fields,
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

#[metrics(rename_all = "PascalCase", default_flags(Emit))]
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

    #[metrics(flags(skip(Emit)))]
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
    // Lists arrive comma-joined (metrique's ForceFlag does not forward the
    // structured `values()` path; see the module docs).
    assert_eq!(ev.fields["Tags"], "a,b");
    assert_eq!(ev.fields["Success"], "true");
    assert_eq!(ev.fields["Load"], "0.5");
    // Duration writes fractional milliseconds.
    assert_eq!(ev.fields["Elapsed"], "250");
    assert!(
        !ev.fields.contains_key("DebugBlob"),
        "skip(Emit) field leaked into the payload: {ev:#?}"
    );

    // Units arrive as schema annotations.
    assert_eq!(ev.units["LatencyUs"], "Microseconds");
    assert_eq!(ev.units["Elapsed"], "Milliseconds");

    // Interned strings go through the pool; plain strings do not.
    assert!(ev.pooled_fields.contains(&"Route".to_owned()));
    assert!(!ev.pooled_fields.contains(&"Operation".to_owned()));

    // Header context: off-runtime capture still records timestamps; worker
    // is unknown (255) and the task id is absent.
    assert_eq!(ev.fields["worker_id"], "255");
    assert_eq!(ev.fields["task_id"], "<none>");
    assert!(ev.timestamp_ns > 0, "start timestamp missing");
    let end: u64 = ev.fields["monotonic_ns_end"].parse().unwrap();
    assert!(
        end >= ev.timestamp_ns,
        "close-time end timestamp must be >= start"
    );

    // Absent optional in the second event.
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
    assert_ne!(ev.fields["worker_id"], "255", "expected a real worker id");
    assert_ne!(ev.fields["task_id"], "<none>", "expected a task id");
}

#[test]
fn entry_without_context_falls_back() {
    #[metrics(default_flags(Emit))]
    struct NoContext {
        count: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(stream, NoContext { count: 7 });
    });

    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.fields["count"], "7");
    assert_eq!(ev.fields["worker_id"], "255");
    assert_eq!(ev.fields["monotonic_ns_end"], "<none>");
    assert!(
        ev.timestamp_ns > 0,
        "flush-thread fallback timestamp missing"
    );
}

#[test]
fn context_only_entry_emits_header_event() {
    #[metrics]
    struct ContextOnly {
        #[metrics(flatten)]
        dial9: Dial9Context,
        // Not flagged: EMF-only field.
        count: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            ContextOnly {
                dial9: Dial9Context::capture(),
                count: 3,
            }
        );
    });

    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert!(
        !ev.fields.contains_key("count"),
        "unflagged field leaked: {ev:#?}"
    );
    assert!(ev.fields["monotonic_ns_end"] != "<none>");
}

#[test]
fn unflagged_entry_is_inert() {
    #[metrics]
    struct Plain {
        count: u64,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(stream, Plain { count: 1 });
    });
    assert!(
        events.is_empty(),
        "inert entry produced events: {events:#?}"
    );
}

#[test]
fn disabled_handle_is_noop() {
    let handle = dial9_tokio_telemetry::telemetry::Dial9Handle::disabled();
    let mut stream = Dial9Stream::new(&handle);

    #[metrics(default_flags(Emit))]
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
        // A descriptor-bearing entry after it still records fine.
        feed_entry!(stream, sample_request(None));
    });

    assert_eq!(events.len(), 1, "hand-written entry must be skipped");
    assert_eq!(events[0].schema_name, "metrique:RequestMetrics");
}

#[test]
fn interned_on_numeric_field_is_skipped_but_entry_survives() {
    #[metrics(default_flags(Emit))]
    struct BadInterned {
        #[metrics(flags(Interned))]
        count: u64,
        name: String,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(
            stream,
            BadInterned {
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

    #[metrics(default_flags(Emit))]
    struct Panicky {
        bad: PanicOnWrite,
    }

    let events = run_sink_test(|stream| {
        feed_entry!(stream, Panicky { bad: PanicOnWrite });
        // The stream keeps working afterwards.
        feed_entry!(stream, sample_request(None));
    });

    assert_eq!(events.len(), 1, "panicking entry must be dropped");
    assert_eq!(events[0].schema_name, "metrique:RequestMetrics");
}

#[test]
fn flex_entry_is_dropped_but_stream_survives() {
    use metrique::flex::Flex;

    #[metrics(default_flags(Emit))]
    struct WithFlex {
        count: u64,
        // Dynamic key name, only known at runtime: the dial9 sink cannot
        // align it with a static descriptor.
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

    // The Flex entry either carries no descriptor for its dynamic segment or
    // emits value callbacks that cannot be aligned with the static
    // descriptor; the sink must drop it rather than record garbage, and keep
    // serving other entry types.
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

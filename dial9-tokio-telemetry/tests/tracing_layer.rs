// NOTE: `span_events_appear_in_trace` uses `set_global_default` because it
// spawns tasks across worker threads and needs the subscriber visible globally.
// Only one test per process can do this. All other tests must use `set_default`
// (thread-local) instead.

use dial9_tokio_telemetry::telemetry::{DiskWriter, TracedRuntime};
use dial9_tokio_telemetry::tracing_layer::Dial9TracingLayer;
use dial9_trace_format::types::FieldValueRef;
use std::collections::HashSet;
use std::sync::{Arc, Barrier};
use std::time::Duration;
use tracing_subscriber::prelude::*;

/// Helper: decode span events from a sealed trace file.
struct SpanEvents {
    enter_count: u32,
    exit_count: u32,
    close_count: u32,
    enter_names: Vec<String>,
    /// All (field_key, field_value) pairs seen on enter events.
    enter_fields: Vec<(String, String)>,
    /// All (field_key, field_value) pairs seen on exit events.
    exit_fields: Vec<(String, String)>,
    /// Whether any enter event had a non-zero parent_span_id.
    saw_parent_span_id: bool,
    /// Worker IDs seen on enter events.
    worker_ids: HashSet<u64>,
    /// Unique schema names seen for enter events.
    enter_schema_names: HashSet<String>,
    /// Unique span IDs seen on enter events.
    entered_span_ids: HashSet<u64>,
    /// Span IDs seen on close events.
    closed_span_ids: HashSet<u64>,
    /// span_instance_ids seen on enter events.
    enter_instance_ids: HashSet<u64>,
    /// span_instance_ids seen on close events.
    close_instance_ids: HashSet<u64>,
    /// tids seen on enter events (non-zero).
    enter_tids: HashSet<u64>,
    /// tids seen on exit events (non-zero).
    exit_tids: HashSet<u64>,
    /// span_instance_ids seen on exit events.
    exit_instance_ids: HashSet<u64>,
    /// Close event summaries for detailed assertions.
    close_summaries: Vec<CloseSummary>,
}

/// Decoded fields from a SpanCloseEvent.
#[derive(Debug)]
struct CloseSummary {
    span_id: u64,
    span_instance_id: u64,
    start_timestamp_ns: u64,
    first_enter_timestamp_ns: Option<u64>,
    active_ns: u64,
    span_name: String,
    target: String,
    file: Option<String>,
    line: Option<u32>,
    parent_span_instance_id: Option<u64>,
    attributes: Vec<(String, String)>,
    unbalanced_enters: u32,
    concurrent: u32,
    saturated: u32,
    loss_observable: u32,
}

fn decode_span_events(path: &std::path::Path) -> SpanEvents {
    let data = std::fs::read(path).unwrap();
    let mut decoder = dial9_trace_format::decoder::Decoder::new(&data).unwrap();

    let mut result = SpanEvents {
        enter_count: 0,
        exit_count: 0,
        close_count: 0,
        enter_names: Vec::new(),
        enter_fields: Vec::new(),
        exit_fields: Vec::new(),
        saw_parent_span_id: false,
        worker_ids: HashSet::new(),
        enter_schema_names: HashSet::new(),
        entered_span_ids: HashSet::new(),
        closed_span_ids: HashSet::new(),
        enter_instance_ids: HashSet::new(),
        close_instance_ids: HashSet::new(),
        enter_tids: HashSet::new(),
        exit_tids: HashSet::new(),
        exit_instance_ids: HashSet::new(),
        close_summaries: Vec::new(),
    };

    decoder
        .for_each_event(|ev| {
            if ev.name.starts_with("SpanEnter:") {
                result.enter_count += 1;
                result.enter_schema_names.insert(ev.name.to_owned());
                for (field_def, field_val) in ev.schema.fields().iter().zip(ev.fields.iter()) {
                    if field_def.name() == "span_name"
                        && let FieldValueRef::PooledString(id) = field_val
                        && let Some(name) = ev.string_pool.get(*id)
                    {
                        result.enter_names.push(name.to_owned());
                    }
                    if field_def.name() == "span_id"
                        && let FieldValueRef::Varint(v) = field_val
                    {
                        result.entered_span_ids.insert(*v);
                    }
                    if field_def.name() == "span_instance_id"
                        && let FieldValueRef::Varint(v) = field_val
                    {
                        result.enter_instance_ids.insert(*v);
                    }
                    if field_def.name() == "tid"
                        && let FieldValueRef::Varint(v) = field_val
                        && *v > 0
                    {
                        result.enter_tids.insert(*v);
                    }
                    if field_def.name() == "parent_span_id"
                        && let FieldValueRef::Varint(v) = field_val
                        && *v > 0
                    {
                        result.saw_parent_span_id = true;
                    }
                    if field_def.name() == "worker_id"
                        && let FieldValueRef::Varint(v) = field_val
                    {
                        result.worker_ids.insert(*v);
                    }
                    // User-defined fields are optional pooled strings
                    if ![
                        "worker_id",
                        "span_id",
                        "span_instance_id",
                        "tid",
                        "parent_span_id",
                        "span_name",
                    ]
                    .contains(&field_def.name())
                        && let FieldValueRef::PooledString(id) = field_val
                        && let Some(v) = ev.string_pool.get(*id)
                    {
                        result
                            .enter_fields
                            .push((field_def.name().to_owned(), v.to_owned()));
                    }
                }
            } else if ev.name.starts_with("SpanExit:") {
                result.exit_count += 1;
                for (field_def, field_val) in ev.schema.fields().iter().zip(ev.fields.iter()) {
                    if field_def.name() == "span_instance_id"
                        && let FieldValueRef::Varint(v) = field_val
                    {
                        result.exit_instance_ids.insert(*v);
                    }
                    if field_def.name() == "tid"
                        && let FieldValueRef::Varint(v) = field_val
                        && *v > 0
                    {
                        result.exit_tids.insert(*v);
                    }
                    if ![
                        "worker_id",
                        "span_id",
                        "span_instance_id",
                        "tid",
                        "span_name",
                    ]
                    .contains(&field_def.name())
                        && let FieldValueRef::PooledString(id) = field_val
                        && let Some(v) = ev.string_pool.get(*id)
                    {
                        result
                            .exit_fields
                            .push((field_def.name().to_owned(), v.to_owned()));
                    }
                }
            } else if ev.name == "SpanCloseEvent" {
                result.close_count += 1;
                let mut summary = CloseSummary {
                    span_id: 0,
                    span_instance_id: 0,
                    start_timestamp_ns: 0,
                    first_enter_timestamp_ns: None,
                    active_ns: 0,
                    span_name: String::new(),
                    target: String::new(),
                    file: None,
                    line: None,
                    parent_span_instance_id: None,
                    attributes: Vec::new(),
                    unbalanced_enters: 0,
                    concurrent: 0,
                    saturated: 0,
                    loss_observable: 0,
                };
                for (field_def, field_val) in ev.schema.fields().iter().zip(ev.fields.iter()) {
                    match field_def.name() {
                        "span_id" => {
                            if let FieldValueRef::Varint(v) = field_val {
                                summary.span_id = *v;
                                result.closed_span_ids.insert(*v);
                            }
                        }
                        "span_instance_id" => {
                            if let FieldValueRef::Varint(v) = field_val {
                                summary.span_instance_id = *v;
                                result.close_instance_ids.insert(*v);
                            }
                        }
                        "start_timestamp_ns" => {
                            if let FieldValueRef::Varint(v) = field_val {
                                summary.start_timestamp_ns = *v;
                            }
                        }
                        "first_enter_timestamp_ns" => {
                            if let FieldValueRef::Varint(v) = field_val {
                                summary.first_enter_timestamp_ns = Some(*v);
                            }
                        }
                        "active_ns" => {
                            if let FieldValueRef::Varint(v) = field_val {
                                summary.active_ns = *v;
                            }
                        }
                        "span_name" => {
                            if let FieldValueRef::String(s) = field_val {
                                summary.span_name = s.to_string();
                            }
                        }
                        "target" => {
                            if let FieldValueRef::String(s) = field_val {
                                summary.target = s.to_string();
                            }
                        }
                        "file" => {
                            if let FieldValueRef::String(s) = field_val {
                                summary.file = Some(s.to_string());
                            }
                        }
                        "line" => {
                            if let FieldValueRef::Varint(v) = field_val {
                                summary.line = Some(*v as u32);
                            }
                        }
                        "parent_span_instance_id" => {
                            if let FieldValueRef::Varint(v) = field_val {
                                summary.parent_span_instance_id = Some(*v);
                            }
                        }
                        "unbalanced_enters" => {
                            if let FieldValueRef::Varint(v) = field_val {
                                summary.unbalanced_enters = *v as u32;
                            }
                        }
                        "concurrent" => {
                            if let FieldValueRef::Varint(v) = field_val {
                                summary.concurrent = *v as u32;
                            }
                        }
                        "saturated" => {
                            if let FieldValueRef::Varint(v) = field_val {
                                summary.saturated = *v as u32;
                            }
                        }
                        "loss_observable" => {
                            if let FieldValueRef::Varint(v) = field_val {
                                summary.loss_observable = *v as u32;
                            }
                        }
                        "attributes" => {
                            if let FieldValueRef::StringMap(map) = field_val {
                                for (k, v) in map.iter() {
                                    summary.attributes.push((k.to_string(), v.to_string()));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                result.close_summaries.push(summary);
            }
        })
        .unwrap();

    result
}

/// Verify that span enter/exit events appear in the trace with correct names,
/// fields, parent span IDs, and that on_record captures late fields.
#[test]
fn span_events_appear_in_trace() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let writer = DiskWriter::single_file(&trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::build_and_start(builder, writer).unwrap();

    let subscriber = tracing_subscriber::registry().with(Dial9TracingLayer::new());
    tracing::subscriber::set_global_default(subscriber).expect("failed to set global subscriber");

    runtime.block_on(async {
        // Test on_record: span with an empty field filled in later
        async fn late_record_span() {
            let span = tracing::info_span!("late_fields", answer = tracing::field::Empty);
            span.record("answer", 42);
            let _enter = span.enter();
        }

        #[tracing::instrument(fields(user_id = 42), skip(barrier))]
        async fn handle_request(barrier: Option<Arc<Barrier>>) {
            if let Some(b) = barrier {
                b.wait();
            }
            inner_op("redis").await;
            inner_op("redis").await;
        }

        #[tracing::instrument]
        async fn inner_op(backend: &str) {
            tokio::task::yield_now().await;
        }

        // Spawn across multiple workers for concurrency coverage. Two of the
        // tasks share a blocking Barrier so they are guaranteed to run on two
        // distinct workers (see handle_request), making the multi-worker
        // assertion deterministic rather than dependent on scheduler placement.
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for i in 0..10 {
            let b = (i < 2).then(|| Arc::clone(&barrier));
            handles.push(tokio::spawn(handle_request(b)));
        }
        for h in handles {
            h.await.unwrap();
        }

        // Test on_record
        tokio::spawn(late_record_span()).await.unwrap();

        // Test explicit parent via span!(parent: ...)
        tokio::spawn(async {
            let parent = tracing::info_span!("explicit_parent");
            let _guard = parent.enter();
            let _child = tracing::info_span!(parent: &parent, "explicit_child").entered();
        })
        .await
        .unwrap();

        // Test explicit parent via .instrument()
        tokio::spawn(async {
            use tracing::Instrument;
            let parent = tracing::info_span!("instrument_parent");
            async {
                tokio::task::yield_now().await;
            }
            .instrument(parent)
            .await;
        })
        .await
        .unwrap();

        // Wait for flush cycle
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    drop(guard);

    let sealed_path = dir.path().join("trace.0.bin");
    let events = decode_span_events(&sealed_path);

    // Basic enter/exit pairing
    assert!(
        events.enter_count >= 30,
        "expected at least 30 span enters (10 x 3 spans), got {}",
        events.enter_count
    );
    assert_eq!(
        events.enter_count, events.exit_count,
        "enter/exit count mismatch"
    );

    // Close events: one per unique span instance, and every entered span must be closed
    assert_eq!(
        events.entered_span_ids, events.closed_span_ids,
        "every entered span should have a matching close event"
    );
    assert_eq!(
        events.close_count,
        events.entered_span_ids.len() as u32,
        "each span should close exactly once"
    );

    // Span names
    assert!(
        events.enter_names.contains(&"handle_request".to_string()),
        "missing handle_request span"
    );
    assert!(
        events.enter_names.contains(&"inner_op".to_string()),
        "missing inner_op span"
    );

    // Fields from on_new_span
    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "user_id" && v == "42"),
        "missing user_id=42 field on enter"
    );
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "user_id" && v == "42"),
        "missing user_id=42 field on exit"
    );

    // Fields from on_record (late recording)
    assert!(
        events.enter_names.contains(&"late_fields".to_string()),
        "missing late_fields span"
    );
    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "answer" && v == "42")
            || events
                .exit_fields
                .iter()
                .any(|(k, v)| k == "answer" && v == "42"),
        "missing late-recorded answer=42 field"
    );

    // Parent span ID: explicit parents (span!(parent:) and .instrument()) should
    // produce parent_span_id, but #[instrument] spans should not (contextual
    // parenting is unreliable in multi-task runtimes).
    assert!(
        events.saw_parent_span_id,
        "expected parent_span_id from explicit parent spans (explicit_child or instrument_parent)"
    );
    assert!(
        events.enter_names.contains(&"explicit_child".to_string()),
        "missing explicit_child span"
    );
    assert!(
        events
            .enter_names
            .contains(&"instrument_parent".to_string()),
        "missing instrument_parent span"
    );

    // Multi-worker: span events should come from more than one worker
    assert!(
        events.worker_ids.len() > 1,
        "expected span events from multiple workers, got {:?}",
        events.worker_ids
    );

    // Callsite schema dedup: multiple calls to the same #[instrument] function
    // should share a single schema. We have handle_request, inner_op,
    // late_fields, explicit_parent, explicit_child, instrument_parent = 6 callsites.
    // Each gets one SpanEnter schema.
    assert!(
        events.enter_schema_names.len() <= 6,
        "expected at most 6 unique enter schemas (one per callsite), got {}: {:?}",
        events.enter_schema_names.len(),
        events.enter_schema_names
    );
    // handle_request is called 10 times but should produce only 1 schema
    let hr_schemas: Vec<_> = events
        .enter_schema_names
        .iter()
        .filter(|n| n.contains("handle_request"))
        .collect();
    assert_eq!(
        hr_schemas.len(),
        1,
        "expected 1 schema for handle_request, got {hr_schemas:?}"
    );

    // ── Stage 1: Stable span instance identity ──────────────────────────────

    // Every enter event must carry a non-zero span_instance_id
    assert!(
        !events.enter_instance_ids.is_empty(),
        "expected span_instance_ids on enter events"
    );
    assert!(
        !events.enter_instance_ids.contains(&0),
        "span_instance_id must be non-zero (0 is reserved for disabled)"
    );
    // Instance IDs must be unique per span instance
    assert_eq!(
        events.enter_instance_ids.len(),
        events.entered_span_ids.len(),
        "every unique tracing span ID should have a unique instance ID"
    );

    // Exit events must carry the same set of instance IDs
    assert_eq!(
        events.enter_instance_ids, events.exit_instance_ids,
        "exit events must carry the same span_instance_ids as enter events"
    );

    // Close events must carry the same set of instance IDs
    assert_eq!(
        events.enter_instance_ids, events.close_instance_ids,
        "close events must carry the same span_instance_ids as enter events"
    );

    // ── Stage 1: tid on enter/exit ──────────────────────────────────────────

    // Every enter event must carry a non-zero tid
    assert!(
        !events.enter_tids.is_empty(),
        "expected non-zero tids on enter events"
    );
    // Every exit event must carry a non-zero tid
    assert!(
        !events.exit_tids.is_empty(),
        "expected non-zero tids on exit events"
    );

    // ── Stage 1: Self-contained close summary ───────────────────────────────

    // Find a handle_request close summary and verify its fields
    let hr_close = events
        .close_summaries
        .iter()
        .find(|s| s.span_name == "handle_request")
        .expect("expected a close summary for handle_request");
    assert!(
        hr_close.span_instance_id > 0,
        "close summary must have non-zero span_instance_id"
    );
    assert!(
        hr_close.start_timestamp_ns > 0,
        "close summary must have non-zero start_timestamp_ns"
    );
    assert!(
        hr_close.first_enter_timestamp_ns.is_some(),
        "handle_request was entered, so first_enter_timestamp_ns must be Some"
    );
    assert!(
        hr_close.first_enter_timestamp_ns.unwrap() >= hr_close.start_timestamp_ns,
        "first_enter must be >= start"
    );
    assert!(
        hr_close.active_ns > 0,
        "handle_request was entered and exited, so active_ns must be > 0"
    );
    assert!(
        !hr_close.target.is_empty(),
        "close summary must carry the tracing target"
    );
    assert!(
        hr_close.file.is_some(),
        "close summary must carry file metadata"
    );
    assert!(
        hr_close.line.is_some(),
        "close summary must carry line metadata"
    );
    // Check attributes from handle_request(fields(user_id = 42))
    assert!(
        hr_close
            .attributes
            .iter()
            .any(|(k, v)| k == "user_id" && v == "42"),
        "close summary must carry final field values as attributes, got: {:?}",
        hr_close.attributes
    );
    assert_eq!(
        hr_close.unbalanced_enters, 0,
        "handle_request should have balanced enters/exits"
    );

    // ── Stage 1: Saturation and loss observability quality flags ─────────────

    // A normal short-lived span must not have saturated
    assert_eq!(
        hr_close.saturated, 0,
        "handle_request active_ns should not have saturated"
    );

    // Loss is unobservable in the current implementation: the producer cannot
    // detect dropped enter/exit events in the thread-local buffer path.
    assert_eq!(
        hr_close.loss_observable, 0,
        "loss_observable must be 0 (unobservable) in the current implementation"
    );

    // Verify explicit parent instance ID is recorded on explicit_child
    let child_close = events
        .close_summaries
        .iter()
        .find(|s| s.span_name == "explicit_child")
        .expect("expected a close summary for explicit_child");
    assert!(
        child_close.parent_span_instance_id.is_some(),
        "explicit_child must have parent_span_instance_id set"
    );
    let parent_close = events
        .close_summaries
        .iter()
        .find(|s| s.span_name == "explicit_parent")
        .expect("expected a close summary for explicit_parent");
    assert_eq!(
        child_close.parent_span_instance_id.unwrap(),
        parent_close.span_instance_id,
        "explicit_child's parent_span_instance_id must match explicit_parent's instance_id"
    );
}

/// Verify the layer silently skips when no Dial9Handle is present.
#[test]
fn no_telemetry_handle_does_not_panic() {
    let subscriber = tracing_subscriber::registry().with(Dial9TracingLayer::new());
    let _guard = tracing::subscriber::set_default(subscriber);

    // This runs on a plain thread with no dial9 runtime, so no Dial9Handle.
    // The layer should silently skip without panicking.
    let span = tracing::info_span!("orphan_span", key = "value");
    let _enter = span.enter();
    // If we get here without panicking, the test passes.
}

/// Verify that span events are emitted on current_thread runtimes.
/// Regression test: the calling thread (which IS the worker for current_thread)
/// must have CURRENT_HANDLE installed, otherwise all span events are silently dropped.
#[test]
fn span_events_on_current_thread_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();

    let writer = DiskWriter::single_file(&trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::build_and_start(builder, writer).unwrap();

    let subscriber = tracing_subscriber::registry().with(Dial9TracingLayer::new());
    let _sub_guard = tracing::subscriber::set_default(subscriber);

    runtime.block_on(async {
        #[tracing::instrument]
        async fn do_work() {
            tokio::task::yield_now().await;
        }

        do_work().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    drop(guard);

    let sealed_path = dir.path().join("trace.0.bin");
    let events = decode_span_events(&sealed_path);

    assert!(
        events.enter_count >= 1,
        "expected at least 1 span enter on current_thread runtime, got {}",
        events.enter_count
    );
    assert_eq!(
        events.enter_count, events.exit_count,
        "enter/exit count mismatch on current_thread runtime"
    );
    assert!(
        events.enter_names.contains(&"do_work".to_string()),
        "missing do_work span on current_thread runtime"
    );
    assert_eq!(
        events.entered_span_ids, events.closed_span_ids,
        "every entered span should have a matching close event on current_thread runtime"
    );

    // Stage 1: instance IDs and close summaries on current_thread
    assert_eq!(
        events.enter_instance_ids, events.close_instance_ids,
        "close events must carry the same span_instance_ids on current_thread runtime"
    );
    assert!(
        !events.enter_tids.is_empty(),
        "expected non-zero tids on enter events for current_thread runtime"
    );
    let do_work_close = events
        .close_summaries
        .iter()
        .find(|s| s.span_name == "do_work")
        .expect("expected a close summary for do_work");
    assert!(
        do_work_close.start_timestamp_ns > 0,
        "do_work close summary must have non-zero start_timestamp_ns"
    );
    assert!(
        do_work_close.active_ns > 0,
        "do_work was entered, so active_ns must be > 0"
    );
}

/// Verify correct active_ns accumulation under same-thread re-entry.
/// A span entered twice on the same thread (re-entrant) before exiting should
/// correctly track each enter→exit interval independently via the per-tid LIFO stack.
#[test]
fn same_thread_reentry_produces_correct_active_ns() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();

    let writer = DiskWriter::single_file(&trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::build_and_start(builder, writer).unwrap();

    let subscriber = tracing_subscriber::registry().with(Dial9TracingLayer::new());
    let _sub_guard = tracing::subscriber::set_default(subscriber);

    runtime.block_on(async {
        // Create a span and enter it twice on the same thread (re-entry)
        let span = tracing::info_span!("reentrant_span");
        {
            let _guard1 = span.enter();
            // Sleep a bit to ensure measurable active_ns
            std::thread::sleep(Duration::from_millis(5));
            {
                let _guard2 = span.enter();
                std::thread::sleep(Duration::from_millis(5));
                // _guard2 drops here (inner exit)
            }
            // _guard1 drops here (outer exit)
        }
        // span drops here (close)
        drop(span);

        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    drop(guard);

    let sealed_path = dir.path().join("trace.0.bin");
    let events = decode_span_events(&sealed_path);

    let close = events
        .close_summaries
        .iter()
        .find(|s| s.span_name == "reentrant_span")
        .expect("expected a close summary for reentrant_span");

    // active_ns should reflect the sum of both enter→exit intervals.
    // Each interval is at least 5ms = 5_000_000 ns, so total >= 10ms.
    assert!(
        close.active_ns >= 8_000_000, // allow some slack for scheduling
        "re-entrant span active_ns should be >= ~10ms (sum of both intervals), got {} ns",
        close.active_ns
    );

    // Unbalanced enters should be 0 (both entries were properly exited)
    assert_eq!(
        close.unbalanced_enters, 0,
        "re-entrant span should have balanced enters/exits"
    );

    // Re-entry on the same thread does NOT set the concurrent flag
    // (concurrent means multiple distinct threads, not same-thread re-entry)
    assert_eq!(
        close.concurrent, 0,
        "same-thread re-entry should not set concurrent flag"
    );
}

/// Verify correct behavior when a span is entered concurrently from two threads.
/// The concurrent flag must be set, and active_ns must reflect both threads'
/// contributions.
#[test]
fn two_thread_concurrent_entry_sets_concurrent_flag() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let writer = DiskWriter::single_file(&trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::build_and_start(builder, writer).unwrap();

    let subscriber = tracing_subscriber::registry().with(Dial9TracingLayer::new());
    let _sub_guard = tracing::subscriber::set_default(subscriber);

    runtime.block_on(async {
        // Create a span that will be entered from two different OS threads.
        let span = tracing::info_span!("concurrent_span");

        let barrier = Arc::new(Barrier::new(2));

        // Spawn two blocking tasks that each enter the span concurrently
        let span_clone1 = span.clone();
        let barrier_clone1 = Arc::clone(&barrier);
        let h1 = tokio::task::spawn_blocking(move || {
            let _guard = span_clone1.enter();
            barrier_clone1.wait(); // ensure both threads are inside simultaneously
            std::thread::sleep(Duration::from_millis(10));
        });

        let span_clone2 = span.clone();
        let barrier_clone2 = Arc::clone(&barrier);
        let h2 = tokio::task::spawn_blocking(move || {
            let _guard = span_clone2.enter();
            barrier_clone2.wait(); // ensure both threads are inside simultaneously
            std::thread::sleep(Duration::from_millis(10));
        });

        h1.await.unwrap();
        h2.await.unwrap();
        drop(span);

        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    drop(guard);

    let sealed_path = dir.path().join("trace.0.bin");
    let events = decode_span_events(&sealed_path);

    let close = events
        .close_summaries
        .iter()
        .find(|s| s.span_name == "concurrent_span")
        .expect("expected a close summary for concurrent_span");

    // The concurrent flag must be set because two distinct tids entered.
    assert_eq!(
        close.concurrent, 1,
        "concurrent flag must be set for multi-thread entry"
    );

    // active_ns should reflect both threads' intervals (sum of both).
    // Each thread slept ~10ms, so total should be ~20ms. Allow slack.
    assert!(
        close.active_ns >= 15_000_000,
        "concurrent span active_ns should reflect both threads' intervals (~20ms), got {} ns",
        close.active_ns
    );

    // No unbalanced enters (both entries were properly exited)
    assert_eq!(
        close.unbalanced_enters, 0,
        "concurrent span should have balanced enters/exits"
    );
}

/// Regression test: enter, exit, and close events must be emitted even when a
/// span created on a dial9 runtime is used and finally dropped on a plain
/// std::thread with no thread-local Dial9Handle installed.
#[test]
fn span_close_emitted_when_dropped_on_plain_thread() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();

    let writer = DiskWriter::single_file(&trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::build_and_start(builder, writer).unwrap();

    let subscriber = tracing_subscriber::registry().with(Dial9TracingLayer::new());
    let _sub_guard = tracing::subscriber::set_default(subscriber);

    runtime.block_on(async {
        // Create the span on a dial9 runtime thread, then transfer the last
        // handle to a plain thread. Before the captured-handle fix, on_enter
        // and on_exit silently returned there; only the close summary survived.
        let span = tracing::info_span!("cross_thread_span", tag = "hello");
        let thread = std::thread::spawn(move || {
            let enter = span.enter();
            std::thread::sleep(Duration::from_millis(5));
            drop(enter);
            drop(span);
        });
        thread.join().unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    drop(guard);

    let sealed_path = dir.path().join("trace.0.bin");
    let events = decode_span_events(&sealed_path);

    assert_eq!(events.enter_count, 1, "plain-thread enter must be emitted");
    assert_eq!(events.exit_count, 1, "plain-thread exit must be emitted");

    // There must be exactly one close summary for our span.
    let close_summaries: Vec<_> = events
        .close_summaries
        .iter()
        .filter(|s| s.span_name == "cross_thread_span")
        .collect();
    assert_eq!(
        close_summaries.len(),
        1,
        "expected exactly 1 close event for cross_thread_span, got {}",
        close_summaries.len()
    );

    let close = close_summaries[0];
    assert!(
        close.span_instance_id > 0,
        "close summary must have non-zero span_instance_id"
    );
    assert!(
        events.enter_instance_ids.contains(&close.span_instance_id),
        "enter event must carry the close summary's stable instance id"
    );
    assert!(
        events.exit_instance_ids.contains(&close.span_instance_id),
        "exit event must carry the close summary's stable instance id"
    );
    assert!(
        close.start_timestamp_ns > 0,
        "close summary must have non-zero start_timestamp_ns"
    );
    assert!(
        close.first_enter_timestamp_ns.is_some(),
        "span was entered, so first_enter_timestamp_ns must be Some"
    );
    assert!(
        close.active_ns > 0,
        "span was entered and exited, so active_ns must be > 0"
    );
    assert_eq!(
        close.unbalanced_enters, 0,
        "span should have balanced enters/exits"
    );
    // Verify attributes were preserved through the cross-thread close
    assert!(
        close
            .attributes
            .iter()
            .any(|(k, v)| k == "tag" && v == "hello"),
        "close summary must carry final field values, got: {:?}",
        close.attributes
    );
}

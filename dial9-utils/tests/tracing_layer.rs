// NOTE: `span_events_appear_in_trace` uses `set_global_default` because it
// spawns tasks across worker threads and needs the subscriber visible globally.
// Only one test per process can do this. All other tests must use `set_default`
// (thread-local) instead.

use dial9_core::buffer::DiskBuffer;
use dial9_core::recorder::recorder;
use dial9_core::recording::Recorder;
use dial9_tokio_telemetry::telemetry::{Dial9HandleTokioExt, TokioAttachOptions};
use dial9_trace_format::types::FieldValueRef;
use dial9_utils::tracing_layer::Dial9TracingLayer;
use std::collections::HashSet;
use std::sync::{Arc, Barrier};
use std::time::Duration;
use tracing_subscriber::prelude::*;

fn attach(
    recorder: &Recorder,
    workers: usize,
    options: TokioAttachOptions,
) -> tokio::runtime::Runtime {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(workers);
    recorder
        .handle()
        .attach_tokio_runtime(builder, options)
        .expect("build tokio runtime")
}

fn attach_current_thread(
    recorder: &Recorder,
    options: TokioAttachOptions,
) -> tokio::runtime::Runtime {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    recorder
        .handle()
        .attach_tokio_runtime(builder, options)
        .expect("build tokio runtime")
}

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
    /// Tokio task IDs seen on span enter and exit events.
    enter_task_ids: HashSet<u64>,
    exit_task_ids: HashSet<u64>,
    /// Span events recorded while outside a Tokio task.
    enters_without_task_id: u32,
    exits_without_task_id: u32,
    /// Tokio task IDs recorded by the runtime's poll events.
    poll_task_ids: HashSet<u64>,
    /// Unique schema names seen for enter events.
    enter_schema_names: HashSet<String>,
    /// Unique span IDs seen on enter events.
    entered_span_ids: HashSet<u64>,
    /// Span IDs seen on close events.
    closed_span_ids: HashSet<u64>,
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
        enter_task_ids: HashSet::new(),
        exit_task_ids: HashSet::new(),
        enters_without_task_id: 0,
        exits_without_task_id: 0,
        poll_task_ids: HashSet::new(),
        enter_schema_names: HashSet::new(),
        entered_span_ids: HashSet::new(),
        closed_span_ids: HashSet::new(),
    };

    decoder
        .for_each_event(|ev| {
            if ev.name == "PollStartEvent" {
                for (field_def, field_val) in ev.schema.fields().iter().zip(ev.fields.iter()) {
                    if field_def.name() == "task_id"
                        && let FieldValueRef::Varint(task_id) = field_val
                        && *task_id != 0
                    {
                        result.poll_task_ids.insert(*task_id);
                    }
                }
            } else if ev.name.starts_with("SpanEnter:") {
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
                    if field_def.name() == "parent_span_id"
                        && let FieldValueRef::Varint(v) = field_val
                        && *v > 0
                    {
                        result.saw_parent_span_id = true;
                    }
                    if field_def.name() == "task_id" {
                        match field_val {
                            FieldValueRef::Varint(task_id) if *task_id != 0 => {
                                result.enter_task_ids.insert(*task_id);
                            }
                            FieldValueRef::None => result.enters_without_task_id += 1,
                            _ => {}
                        }
                    }
                    // User-defined fields are optional pooled strings
                    if !["task_id", "span_id", "parent_span_id", "span_name"]
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
                    if field_def.name() == "task_id" {
                        match field_val {
                            FieldValueRef::Varint(task_id) if *task_id != 0 => {
                                result.exit_task_ids.insert(*task_id);
                            }
                            FieldValueRef::None => result.exits_without_task_id += 1,
                            _ => {}
                        }
                    }
                    if !["task_id", "span_id", "span_name"].contains(&field_def.name())
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
                for (field_def, field_val) in ev.schema.fields().iter().zip(ev.fields.iter()) {
                    if field_def.name() == "span_id"
                        && let FieldValueRef::Varint(v) = field_val
                    {
                        result.closed_span_ids.insert(*v);
                    }
                }
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

    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    let runtime = attach(&recorder, 4, TokioAttachOptions::default());

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
        // distinct workers (see handle_request), exercising the shared layer
        // concurrently rather than depending on scheduler placement.
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
    drop(recorder);

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

    // Every span task ID must use the same representation as the runtime's
    // PollStart events, so readers can join the two directly.
    assert!(
        events.enter_task_ids.len() >= 10,
        "expected task IDs from the spawned request tasks, got {:?}",
        events.enter_task_ids
    );
    assert_eq!(events.enter_task_ids, events.exit_task_ids);
    assert!(
        events.enter_task_ids.is_subset(&events.poll_task_ids),
        "span task IDs {:?} were not all present in PollStart events {:?}",
        events.enter_task_ids,
        events.poll_task_ids
    );
    assert_eq!(events.enters_without_task_id, 0);
    assert_eq!(events.exits_without_task_id, 0);

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

    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    let runtime = attach_current_thread(&recorder, TokioAttachOptions::default());

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
    drop(recorder);

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
    // A future driven directly by Runtime::block_on is not a Tokio task. The
    // layer still emits its span and represents the missing task explicitly.
    assert!(events.enter_task_ids.is_empty());
    assert!(events.exit_task_ids.is_empty());
    assert!(events.enters_without_task_id >= 1);
    assert!(events.exits_without_task_id >= 1);
}

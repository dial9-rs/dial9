//! Tests for the ad-hoc span wrappers (`dial9_utils::span`).
//!
//! These decode the sealed trace and assert on the emitted `SpanEnter:*` /
//! `SpanExit:*` / `SpanCloseEvent` wire events, the same format the tracing
//! layer produces.
#![cfg(feature = "span")]

use dial9_core::buffer::DiskBuffer;
use dial9_core::handle::set_tl_handle;
use dial9_core::recorder::recorder;
use dial9_trace_format::types::FieldValueRef;
use dial9_utils::dial9_span;
use dial9_utils::span::{Dial9Span, Instrument as _, Span as _};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

const ADHOC_ID_BIT: u64 = 1 << 63;

#[derive(Default)]
struct SpanEvents {
    enter_count: u32,
    exit_count: u32,
    close_count: u32,
    enter_names: Vec<String>,
    enter_fields: Vec<(String, String)>,
    exit_fields: Vec<(String, String)>,
    /// One entry per `SpanEnter:*`: the task id it carried, or `None` when the
    /// span was opened outside a Tokio task.
    enter_task_ids: Vec<Option<u64>>,
    parent_span_ids: HashSet<u64>,
    entered_span_ids: HashSet<u64>,
    closed_span_ids: HashSet<u64>,
    enter_schema_names: HashSet<String>,
}

/// Render a field value as a string for assertions, whether it rode the wire as
/// a pooled/inline string or a typed scalar (numerics are now `Varint`/etc.,
/// not stringified).
fn field_string(
    pool: &dial9_trace_format::decoder::StringPool,
    fv: &FieldValueRef,
) -> Option<String> {
    match fv {
        FieldValueRef::PooledString(id) => pool.get(*id).map(|s| s.to_owned()),
        FieldValueRef::String(s) => Some(s.to_string()),
        FieldValueRef::Varint(v) => Some(v.to_string()),
        FieldValueRef::I64(v) => Some(v.to_string()),
        FieldValueRef::F64(v) => Some(v.to_string()),
        FieldValueRef::Bool(v) => Some(v.to_string()),
        _ => None,
    }
}

fn decode(path: &std::path::Path) -> SpanEvents {
    let data = std::fs::read(path).unwrap();
    let mut decoder = dial9_trace_format::decoder::Decoder::new(&data).unwrap();
    let mut r = SpanEvents::default();

    decoder
        .for_each_event(|ev| {
            if ev.name.starts_with("SpanEnter:") {
                r.enter_count += 1;
                r.enter_schema_names.insert(ev.name.to_owned());
                for (fd, fv) in ev.schema.fields().iter().zip(ev.fields.iter()) {
                    match (fd.name(), fv) {
                        ("span_name", _) => {
                            if let Some(n) = field_string(ev.string_pool, fv) {
                                r.enter_names.push(n);
                            }
                        }
                        ("dial9.tokio.task_id", FieldValueRef::Varint(v)) => {
                            r.enter_task_ids.push(Some(*v));
                        }
                        ("dial9.tokio.task_id", FieldValueRef::None) => {
                            r.enter_task_ids.push(None);
                        }
                        ("span_id", FieldValueRef::Varint(v)) => {
                            r.entered_span_ids.insert(*v);
                        }
                        ("parent_span_id", FieldValueRef::Varint(v)) => {
                            r.parent_span_ids.insert(*v);
                        }
                        (name, _)
                            if ![
                                "dial9.tokio.task_id",
                                "span_id",
                                "parent_span_id",
                                "span_name",
                            ]
                            .contains(&name) =>
                        {
                            if let Some(v) = field_string(ev.string_pool, fv) {
                                r.enter_fields.push((name.to_owned(), v));
                            }
                        }
                        _ => {}
                    }
                }
            } else if ev.name.starts_with("SpanExit:") {
                r.exit_count += 1;
                for (fd, fv) in ev.schema.fields().iter().zip(ev.fields.iter()) {
                    if !["dial9.tokio.task_id", "span_id", "span_name"].contains(&fd.name())
                        && let Some(v) = field_string(ev.string_pool, fv)
                    {
                        r.exit_fields.push((fd.name().to_owned(), v));
                    }
                }
            } else if ev.name == "SpanCloseEvent" {
                r.close_count += 1;
                for (fd, fv) in ev.schema.fields().iter().zip(ev.fields.iter()) {
                    if fd.name() == "span_id"
                        && let FieldValueRef::Varint(v) = fv
                    {
                        r.closed_span_ids.insert(*v);
                    }
                }
            }
        })
        .unwrap();
    r
}

/// Run `body` on a freshly built traced multi-thread runtime, flush, and
/// decode the sealed trace.
fn run_traced<F, Fut>(worker_threads: usize, body: F) -> SpanEvents
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();

    // Install the recorder handle on the block_on thread and every worker, so
    // spans emit no matter which thread polls them. Done by hand rather than
    // through `attach_tokio_runtime`, which keeps these tests on dial9-core
    // alone — all the spans themselves need.
    set_tl_handle(recorder.handle().clone());
    let worker_handle = recorder.handle().clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .on_thread_start(move || set_tl_handle(worker_handle.clone()))
        .build()
        .unwrap();

    runtime.block_on(async {
        body().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    // Drop the runtime first so worker threads exit and flush their
    // thread-local buffers, then finalize the segment.
    drop(runtime);
    recorder.graceful_shutdown(Duration::ZERO);

    decode(&dir.path().join("trace.0.bin"))
}

/// Both span flavors record the source location they were created at, so a span
/// in the viewer points back at the code that opened it.
#[test]
fn spans_record_their_call_site() {
    let events = run_traced(1, || async {
        async {}.instrument(dial9_span!("macro.located")).await;
        async {}.instrument(Dial9Span::new("runtime.located")).await;
    });

    let locations: Vec<_> = events
        .enter_fields
        .iter()
        .filter(|(k, _)| k == "location")
        .map(|(_, v)| v.clone())
        .collect();
    assert_eq!(locations.len(), 2, "one location per enter: {locations:?}");
    assert!(
        locations
            .iter()
            .all(|l| l.starts_with("dial9-utils/tests/span_wrappers.rs:")),
        "locations should point at this file: {locations:?}"
    );
}

/// A span opened outside any Tokio task carries no task id: the field is
/// optional, so it is absent on the wire rather than holding a sentinel.
#[test]
fn span_off_runtime_omits_task_id() {
    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();
    let recorder = recorder(writer).build();
    set_tl_handle(recorder.handle().clone());

    {
        let span = dial9_span!("offline.work");
        let _entered = span.enter();
    }

    recorder.graceful_shutdown(Duration::ZERO);
    let events = decode(&dir.path().join("trace.0.bin"));

    assert_eq!(events.enter_count, 1, "one enter");
    assert_eq!(
        events.enter_task_ids,
        vec![None],
        "no task id off a runtime"
    );
}

/// Spans created while polling a spawned Tokio task carry that task's stable id,
/// allowing the viewer to reconstruct active intervals from its poll timeline.
#[test]
fn span_inside_spawn_records_task_id() {
    let expected_task_id = Arc::new(Mutex::new(None));
    let task_id_from_task = expected_task_id.clone();
    let events = run_traced(1, || async move {
        tokio::spawn(async move {
            *task_id_from_task.lock().unwrap() = tokio::task::try_id()
                .map(dial9_tokio_telemetry::telemetry::TaskId::from)
                .map(|id| id.to_u64());
            async {}.instrument(dial9_span!("spawned.work")).await;
        })
        .await
        .unwrap();
    });

    assert_eq!(
        events.enter_task_ids,
        vec![*expected_task_id.lock().unwrap()],
        "span task id must match the id used by Tokio poll telemetry"
    );
}

/// The sync guard emits exactly one enter/exit pair plus one close, with the
/// macro's construction-time fields on both enter and exit.
#[test]
fn sync_guard_emits_one_pair_and_closes() {
    let events = run_traced(1, || async {
        let span = dial9_span!("pricing.compute", order_id: u64 = 7u64, total_cents: u64 = 4950u64);
        let entered = span.enter();
        // pretend CPU work
        let _total: u64 = (0..100).sum();
        drop(entered); // exit here
        // span drops at end of scope → close
    });

    assert_eq!(events.enter_count, 1, "one enter");
    assert_eq!(events.exit_count, 1, "one exit");
    assert_eq!(events.close_count, 1, "one close");
    assert!(events.enter_names.contains(&"pricing.compute".to_string()));

    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "order_id" && v == "7"),
        "enter fields: {:?}",
        events.enter_fields
    );
    // Macro fields ride every segment, including the exit.
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "total_cents" && v == "4950"),
        "exit fields: {:?}",
        events.exit_fields
    );
    // A sync scope reports one poll and completion, with no idle.
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "poll_count" && v == "1"),
        "sync guard poll_count should be 1: {:?}",
        events.exit_fields
    );
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "completed" && v == "true"),
        "sync guard should report completed: {:?}",
        events.exit_fields
    );
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "idle_ns" && v == "0"),
        "sync guard has no idle time: {:?}",
        events.exit_fields
    );

    // Every entered span is closed; ids carry the ad-hoc top bit.
    assert_eq!(events.entered_span_ids, events.closed_span_ids);
    assert!(
        events
            .entered_span_ids
            .iter()
            .all(|id| id & ADHOC_ID_BIT != 0),
        "ad-hoc span ids must have the top bit set: {:?}",
        events.entered_span_ids
    );
}

/// The instrumented future emits exactly one enter and one completion exit
/// (not one per poll), and the exit carries the aggregate timing: `poll_count`
/// (>= 2 for a future that yields), a `completed` flag, and `active_ns`.
#[test]
fn instrumented_future_emits_enter_and_completion() {
    let events = run_traced(2, || async {
        async fn two_polls() {
            tokio::task::yield_now().await; // forces a second poll
        }
        two_polls()
            .instrument(dial9_span!("db.query", table: &'static str = "orders", rows: u64 = 3u64))
            .await;
    });

    // One enter, one exit, one close — regardless of poll count.
    assert_eq!(events.enter_count, 1, "exactly one enter");
    assert_eq!(events.exit_count, 1, "exactly one completion exit");
    assert_eq!(events.close_count, 1, "exactly one close");
    assert!(events.enter_names.contains(&"db.query".to_string()));

    // User fields on both enter and the completion exit.
    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "table" && v == "orders")
    );
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "rows" && v == "3")
    );

    // Aggregate timing rides the completion exit.
    let poll_count = events
        .exit_fields
        .iter()
        .find(|(k, _)| k == "poll_count")
        .map(|(_, v)| v.parse::<u64>().unwrap());
    assert!(
        poll_count.is_some_and(|n| n >= 2),
        "poll_count should be >= 2 for a yielding future: {:?}",
        events.exit_fields
    );
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "completed" && v == "true"),
        "completed future must report completed=true: {:?}",
        events.exit_fields
    );
    assert!(
        events.exit_fields.iter().any(|(k, _)| k == "active_ns"),
        "completion exit must carry active_ns: {:?}",
        events.exit_fields
    );
}

/// A name-only `Dial9Span::new` span shares the runtime schema, while a
/// `dial9_span!` span gets a distinct call-site schema.
#[test]
fn name_only_and_macro_schemas() {
    let events = run_traced(1, || async {
        async {}.instrument(Dial9Span::new("tax.fetch_rate")).await;
        async {}.instrument(dial9_span!("tax.compute")).await;
    });
    assert!(events.enter_names.contains(&"tax.fetch_rate".to_string()));
    assert!(events.enter_names.contains(&"tax.compute".to_string()));
    assert_eq!(events.close_count, 2);

    // Name-only spans share one runtime schema.
    assert!(
        events
            .enter_schema_names
            .iter()
            .any(|n| n == "SpanEnter:dial9_utils::runtime:runtime:0"),
        "schema names: {:?}",
        events.enter_schema_names
    );
    // Macro spans get a call-site-based schema id (this file).
    assert!(
        events
            .enter_schema_names
            .iter()
            .any(|n| n.contains("span_wrappers.rs")),
        "schema names: {:?}",
        events.enter_schema_names
    );
}

/// Two call sites with different field sets register distinct, non-colliding
/// schemas (the schema id embeds `file:line:col`).
#[test]
fn distinct_callsites_get_distinct_schemas() {
    let events = run_traced(1, || async {
        async {}
            .instrument(dial9_span!("op.a", base: u64 = 1u64))
            .await;
        async {}
            .instrument(dial9_span!("op.b", base: u64 = 1u64, extra: u64 = 2u64))
            .await;
    });

    let adhoc: Vec<_> = events
        .enter_schema_names
        .iter()
        .filter(|n| n.contains("span_wrappers.rs"))
        .collect();
    assert!(
        adhoc.len() >= 2,
        "expected >= 2 distinct call-site schemas, got {:?}",
        events.enter_schema_names
    );
    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "extra" && v == "2")
    );
}

/// Explicit parenting via `span.id()` across a spawn boundary produces a
/// parent_span_id on the child that matches the parent's id.
#[test]
fn explicit_parent_across_spawn() {
    let events = run_traced(2, || async {
        let parent = dial9_span!("payment.charge", order_id: u64 = 1u64);
        let parent_id = parent.id();
        // `id()`/`with_parent_id()` are `Span`-trait methods, so parenting works
        // on a macro span exactly as on `Dial9Span::new`.
        let child = dial9_span!("audit.emit").with_parent_id(parent_id);

        tokio::spawn(async {}.instrument(child)).await.unwrap();
        // Drive the parent too so it appears in the trace.
        async {}.instrument(parent).await;

        assert!(parent_id.as_u64() & ADHOC_ID_BIT != 0);
    });

    assert!(events.enter_names.contains(&"audit.emit".to_string()));
    assert!(events.enter_names.contains(&"payment.charge".to_string()));
    // The child recorded a non-None parent, and it is an ad-hoc id.
    assert!(
        events
            .parent_span_ids
            .iter()
            .any(|id| id & ADHOC_ID_BIT != 0),
        "expected an ad-hoc parent_span_id, got {:?}",
        events.parent_span_ids
    );
    // The parent id the child points at must be one we actually entered.
    assert!(
        events.parent_span_ids.is_subset(&events.entered_span_ids),
        "parent ids {:?} not all among entered ids {:?}",
        events.parent_span_ids,
        events.entered_span_ids
    );
}

/// The wrappers are silent no-ops off a dial9 runtime (no panic, nothing to
/// decode). Runs on a plain, untraced tokio runtime.
#[test]
fn off_runtime_is_silent_noop() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Sync guard off-runtime.
        let span = dial9_span!("x", k: &'static str = "v");
        let entered = span.enter();
        drop(entered);
        drop(span);

        // Instrumented futures off-runtime, polled to completion.
        async {}.instrument(dial9_span!("y")).await;
        async { tokio::task::yield_now().await }
            .instrument(Dial9Span::new("z"))
            .await;
    });
    // Reaching here without panicking is the assertion.
}

/// A cancelled (dropped) future still emits a completion exit — marked
/// `completed=false` — and a close, so the viewer can finalize the span.
#[test]
fn cancelled_future_emits_incomplete_exit_and_closes() {
    let events = run_traced(1, || async {
        // A future that never completes; poll it once then drop it.
        let mut fut = Box::pin(std::future::pending::<()>().instrument(dial9_span!("stuck")));
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let poll = fut.as_mut().poll(&mut cx);
        assert!(matches!(poll, Poll::Pending));
        drop(fut); // cancellation → completion exit (not completed) + close on drop
    });

    assert!(events.enter_names.contains(&"stuck".to_string()));
    assert_eq!(events.enter_count, 1, "one enter");
    assert_eq!(events.exit_count, 1, "cancellation still emits the exit");
    assert_eq!(events.close_count, 1, "cancelled span must still close");
    assert_eq!(events.entered_span_ids, events.closed_span_ids);
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "completed" && v == "false"),
        "cancelled future must report completed=false: {:?}",
        events.exit_fields
    );
}

/// A `%`-formatted string field (owned `String` on the wire) round-trips, and a
/// bare typed `u64` field decodes back to its value — confirming numeric fields
/// keep their type rather than being stringified.
#[test]
fn typed_and_display_fields_roundtrip() {
    let events = run_traced(1, || async {
        let id = "req-abc123";
        async {}
            .instrument(dial9_span!("request", request_id = %id, attempt: u64 = 2u64))
            .await;
    });
    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "request_id" && v == "req-abc123"),
        "display field should roundtrip: {:?}",
        events.enter_fields
    );
    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "attempt" && v == "2"),
        "typed numeric field should roundtrip: {:?}",
        events.enter_fields
    );
}

/// The macro stores its name through the generic carrier, so call sites may use
/// an owned runtime name without changing the concrete generated event schema.
#[test]
fn owned_span_name_roundtrips() {
    let events = run_traced(1, || async {
        let name = format!("request.{}", 42);
        async {}
            .instrument(dial9_span!(name, attempt: u64 = 2u64))
            .await;
    });

    assert!(
        events.enter_names.contains(&"request.42".to_string()),
        "owned span name should roundtrip: {:?}",
        events.enter_names
    );
}

/// The tower layer creates one span per request; a `make_span` closure names
/// it, and the response future emits one enter + one completion exit + close.
#[cfg(feature = "tower")]
#[test]
fn tower_layer_wraps_request() {
    use dial9_utils::tower::Dial9SpanLayer;
    use std::convert::Infallible;
    use std::pin::Pin;
    use tower_service::Service;

    // Doubles the request, but yields first so the response future is polled
    // more than once — the realistic case.
    struct Doubler;
    impl Service<u32> for Doubler {
        type Response = u32;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<u32, Infallible>> + Send>>;
        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: u32) -> Self::Future {
            Box::pin(async move {
                tokio::task::yield_now().await;
                Ok(req * 2)
            })
        }
    }

    let events = run_traced(1, || async {
        use tower_layer::Layer;
        // `make_span` receives the request, so the span can carry request fields.
        let layer = Dial9SpanLayer::new(|n: &u32| dial9_span!("request", n: u32 = *n));
        let mut svc = layer.layer(Doubler);
        let out = svc.call(21).await.unwrap();
        assert_eq!(out, 42);
    });

    assert!(events.enter_names.contains(&"request".to_string()));
    assert_eq!(events.enter_count, 1, "one enter per request");
    assert_eq!(events.exit_count, 1, "one completion exit per request");
    assert_eq!(events.close_count, 1, "one span per request");
    // The yielding response future was polled more than once.
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "poll_count" && v.parse::<u64>().is_ok_and(|n| n >= 2)),
        "poll_count should reflect the yield: {:?}",
        events.exit_fields
    );
    // The field was read from the request (`n = 21`), proving `make_span(&Req)`.
    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "n" && v == "21"),
        "request-derived field on enter: {:?}",
        events.enter_fields
    );
}

/// `Dial9SpanLayerWithResponse` records a late field from the response: the
/// `finish` callback sets a `Slot` after the inner service resolves, and the
/// value rides the completion exit — absent from enter.
#[cfg(feature = "tower")]
#[test]
fn tower_response_layer_records_late_field() {
    use dial9_utils::tower::Dial9SpanLayerWithResponse;
    use std::convert::Infallible;
    use std::pin::Pin;
    use tower_service::Service;

    struct Doubler;
    impl Service<u32> for Doubler {
        type Response = u32;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<u32, Infallible>> + Send>>;
        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: u32) -> Self::Future {
            Box::pin(async move {
                tokio::task::yield_now().await;
                Ok(req * 2)
            })
        }
    }

    let events = run_traced(1, || async {
        use tower_layer::Layer;
        // `n` is request-derived (eager); `doubled` is response-derived (late),
        // set by the finish callback that captures the span's slots.
        let layer = Dial9SpanLayerWithResponse::new(|n: &u32| {
            let (span, slots) = dial9_span!("request", n: u32 = *n, doubled: u64);
            (span, move |resp: &u32| slots.doubled.set(*resp as u64))
        });
        let mut svc = layer.layer(Doubler);
        let out = svc.call(21).await.unwrap();
        assert_eq!(out, 42);
    });

    assert_eq!(events.enter_count, 1, "one enter per request");
    assert_eq!(events.exit_count, 1, "one completion exit per request");
    // Eager request field on enter; the late field must not be on enter.
    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "n" && v == "21"),
        "request-derived field on enter: {:?}",
        events.enter_fields
    );
    assert!(
        !events.enter_fields.iter().any(|(k, _)| k == "doubled"),
        "late field must not appear on enter: {:?}",
        events.enter_fields
    );
    // The response-derived late field lands on the completion exit.
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "doubled" && v == "42"),
        "response-derived late field on exit: {:?}",
        events.exit_fields
    );
}

/// A `?`-formatted field renders its `Debug` representation on the wire.
#[test]
fn debug_field_roundtrips() {
    #[derive(Debug)]
    #[allow(dead_code)] // read only via the Debug derive
    struct Cfg {
        retries: u32,
    }
    let events = run_traced(1, || async {
        let cfg = Cfg { retries: 3 };
        async {}
            .instrument(dial9_span!("validate", config = ?cfg))
            .await;
    });
    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "config" && v == "Cfg { retries: 3 }"),
        "debug field should roundtrip its Debug repr: {:?}",
        events.enter_fields
    );
}

/// A future that suspends between polls reports non-zero `idle_ns` on its
/// completion event, and `active_ns + idle_ns` never exceeds the wall clock.
#[test]
fn completion_reports_idle_for_awaiting_future() {
    let events = run_traced(2, || async {
        // Sleep forces a real suspension between polls → measurable idle time.
        async {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        .instrument(dial9_span!("io.wait"))
        .await;
    });

    let field = |k: &str| -> Option<u64> {
        events
            .exit_fields
            .iter()
            .find(|(fk, _)| fk == k)
            .and_then(|(_, v)| v.parse::<u64>().ok())
    };
    let idle = field("idle_ns").expect("idle_ns present");
    let active = field("active_ns").expect("active_ns present");
    let polls = field("poll_count").expect("poll_count present");

    assert!(
        idle > 0,
        "awaiting future must report idle time, got {idle}"
    );
    assert!(polls >= 2, "sleep forces a second poll, got {polls}");
    // Active time is the time spent inside poll(); it should be far less than
    // the ~20ms the future spent suspended.
    assert!(
        active < idle,
        "active ({active}) should be well under idle ({idle}) for an io-bound future"
    );
}

/// A late field (`name: Type`) set during the future's body lands on the
/// completion (exit) event and is absent from the enter event. Eager fields
/// still ride both.
#[test]
fn late_field_lands_on_exit_only() {
    let events = run_traced(2, || async {
        let (span, slots) =
            dial9_span!("request", route: &'static str = "/checkout", status: u16, bytes: u64);
        async move {
            tokio::time::sleep(Duration::from_millis(2)).await;
            slots.status.set(200u16);
            slots.bytes.set(4096u64);
        }
        .instrument(span)
        .await;
    });

    // Eager field on enter; late fields must NOT be on enter (not set yet).
    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "route" && v == "/checkout"),
        "eager route on enter: {:?}",
        events.enter_fields
    );
    assert!(
        !events
            .enter_fields
            .iter()
            .any(|(k, _)| k == "status" || k == "bytes"),
        "late fields must not appear on enter: {:?}",
        events.enter_fields
    );
    // Late values land on exit, keeping their numeric type on the wire.
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "status" && v == "200"),
        "late status on exit: {:?}",
        events.exit_fields
    );
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "bytes" && v == "4096"),
        "late bytes on exit: {:?}",
        events.exit_fields
    );
}

/// A late field that is never set is recorded as `None`, so it carries no
/// readable value on exit.
#[test]
fn late_field_unset_is_none_on_exit() {
    let events = run_traced(1, || async {
        let (span, _slots) = dial9_span!("request", status: u16);
        // Never call `_slots.status.set(..)`.
        async {}.instrument(span).await;
    });

    assert_eq!(events.exit_count, 1, "one exit");
    assert!(
        !events.exit_fields.iter().any(|(k, _)| k == "status"),
        "an unset late field should decode as None (no value): {:?}",
        events.exit_fields
    );
}

/// Late fields also work with the sync guard: set during the guarded scope,
/// read when the guard drops (exit).
#[test]
fn late_field_with_sync_guard() {
    let events = run_traced(1, || async {
        let (span, slots) = dial9_span!("pricing", order_id: u64 = 7u64, tax_cents: u64);
        let entered = span.enter();
        slots.tax_cents.set(123u64);
        drop(entered); // exit reads the slot
    });

    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "order_id" && v == "7"),
        "eager field on exit: {:?}",
        events.exit_fields
    );
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "tax_cents" && v == "123"),
        "late field set in a sync scope on exit: {:?}",
        events.exit_fields
    );
}

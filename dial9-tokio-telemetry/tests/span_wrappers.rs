//! Tests for the ad-hoc span wrappers (`dial9_tokio_telemetry::span`).
//!
//! These decode the sealed trace and assert on the emitted `SpanEnter:*` /
//! `SpanExit:*` / `SpanCloseEvent` wire events, the same format the tracing
//! layer produces.

use dial9_tokio_telemetry::dial9_span;
use dial9_tokio_telemetry::span::{Dial9Span, Dial9SpanLayer, Instrument as _, Span as _};
use dial9_tokio_telemetry::telemetry::{DiskBuffer, RecorderTokioExt, recorder};
use dial9_trace_format::types::FieldValueRef;
use std::collections::HashSet;
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
                        ("span_id", FieldValueRef::Varint(v)) => {
                            r.entered_span_ids.insert(*v);
                        }
                        ("parent_span_id", FieldValueRef::Varint(v)) => {
                            r.parent_span_ids.insert(*v);
                        }
                        (name, _)
                            if !["worker_id", "span_id", "parent_span_id", "span_name"]
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
                    if !["worker_id", "span_id", "span_name"].contains(&fd.name())
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
    let (recorder, runtime) = recorder
        .attach_tokio_runtime(|t| {
            t.worker_threads(worker_threads);
            t.enable_all();
        })
        .unwrap();

    runtime.block_on(async {
        body().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(recorder);
    drop(runtime);

    decode(&dir.path().join("trace.0.bin"))
}

/// The sync guard emits exactly one enter/exit pair plus one close, with the
/// macro's construction-time fields on both enter and exit.
#[test]
fn sync_guard_emits_one_pair_and_closes() {
    let events = run_traced(1, || async {
        let span = dial9_span!("pricing.compute", order_id = 7u64, total_cents = 4950u64);
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
            .instrument(dial9_span!("db.query", table = "orders", rows = 3u64))
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
            .any(|n| n.contains("adhoc::runtime")),
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
        async {}.instrument(dial9_span!("op.a", base = 1u64)).await;
        async {}
            .instrument(dial9_span!("op.b", base = 1u64, extra = 2u64))
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
        let parent = dial9_span!("payment.charge", order_id = 1u64);
        let parent_id = parent.id();
        let child = Dial9Span::new("audit.emit").with_parent_id(parent_id);

        tokio::spawn(async {}.instrument(child)).await.unwrap();
        // Drive the parent too so it appears in the trace.
        async {}.instrument(parent).await;

        assert!(parent_id & ADHOC_ID_BIT != 0);
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
        let span = dial9_span!("x", k = "v");
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
            .instrument(dial9_span!("request", request_id = %id, attempt = 2u64))
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

/// The tower layer creates one span per request; a `make_span` closure names
/// it, and the response future emits one enter + one completion exit + close.
#[test]
fn tower_layer_wraps_request() {
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
        let layer = Dial9SpanLayer::new(|| dial9_span!("request", service = "checkout"));
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
    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "service" && v == "checkout"),
        "request field on enter: {:?}",
        events.enter_fields
    );
}

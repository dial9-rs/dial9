//! Tests for the ad-hoc span wrappers (`dial9_tokio_telemetry::span`).
//!
//! These decode the sealed trace and assert on the emitted `SpanEnter:*` /
//! `SpanExit:*` / `SpanCloseEvent` wire events, the same format the tracing
//! layer produces.

use dial9_tokio_telemetry::span::{Dial9SpanLayer, SpanFutureExt as _, span};
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
                        ("span_name", FieldValueRef::PooledString(id)) => {
                            if let Some(n) = ev.string_pool.get(*id) {
                                r.enter_names.push(n.to_owned());
                            }
                        }
                        ("span_id", FieldValueRef::Varint(v)) => {
                            r.entered_span_ids.insert(*v);
                        }
                        ("parent_span_id", FieldValueRef::Varint(v)) => {
                            r.parent_span_ids.insert(*v);
                        }
                        (name, FieldValueRef::PooledString(id))
                            if !["worker_id", "span_id", "parent_span_id", "span_name"]
                                .contains(&name) =>
                        {
                            if let Some(v) = ev.string_pool.get(*id) {
                                r.enter_fields.push((name.to_owned(), v.to_owned()));
                            }
                        }
                        _ => {}
                    }
                }
            } else if ev.name.starts_with("SpanExit:") {
                r.exit_count += 1;
                for (fd, fv) in ev.schema.fields().iter().zip(ev.fields.iter()) {
                    if !["worker_id", "span_id", "span_name"].contains(&fd.name())
                        && let FieldValueRef::PooledString(id) = fv
                        && let Some(v) = ev.string_pool.get(*id)
                    {
                        r.exit_fields.push((fd.name().to_owned(), v.to_owned()));
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

/// The sync guard emits exactly one enter/exit pair plus one close, with
/// construction-time fields on enter and late `record`ed fields on exit.
#[test]
fn sync_guard_emits_one_pair_and_closes() {
    let events = run_traced(1, || async {
        let mut guard = span("pricing.compute").field("order_id", 7u64).entered();
        // pretend CPU work
        let total: u64 = (0..100).sum();
        guard.record("total_cents", total);
        guard.exit();
    });

    assert_eq!(events.enter_count, 1, "one enter");
    assert_eq!(events.exit_count, 1, "one exit");
    assert_eq!(events.close_count, 1, "one close");
    assert!(events.enter_names.contains(&"pricing.compute".to_string()));

    // Construction-time field is on enter.
    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "order_id" && v == "7"),
        "enter fields: {:?}",
        events.enter_fields
    );
    // Late-recorded field rides the exit.
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

/// The future wrapper emits one enter/exit pair per poll and a single close,
/// and late fields recorded via the handle ride an exit.
#[test]
fn future_wrapper_emits_per_poll_and_late_fields() {
    let events = run_traced(2, || async {
        async fn two_polls() {
            tokio::task::yield_now().await; // forces a second poll
        }
        let fut = two_polls().in_span(span("db.query").field("table", "orders"));
        let handle = fut.handle();
        handle.record("rows", 3u64);
        fut.await;
    });

    // yield_now → at least two polls → at least two enter/exit pairs.
    assert!(
        events.enter_count >= 2,
        "expected >= 2 enters (one per poll), got {}",
        events.enter_count
    );
    assert_eq!(events.enter_count, events.exit_count, "enter/exit paired");
    assert_eq!(events.close_count, 1, "exactly one close for the span");
    assert!(events.enter_names.contains(&"db.query".to_string()));

    // Construction field on enter, late field (via handle) on exit.
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
            .any(|(k, v)| k == "rows" && v == "3"),
        "late field via handle should ride an exit: {:?}",
        events.exit_fields
    );
}

/// `.in_span("name")` shorthand works and produces a callsite schema.
#[test]
fn in_span_str_shorthand() {
    let events = run_traced(1, || async {
        async {}.in_span("tax.fetch_rate").await;
    });
    assert!(events.enter_names.contains(&"tax.fetch_rate".to_string()));
    assert_eq!(events.close_count, 1);
    // Schema id is callsite-based (this file), not the span name.
    assert!(
        events
            .enter_schema_names
            .iter()
            .any(|n| n.contains("adhoc::") && n.contains("span_wrappers.rs")),
        "schema names: {:?}",
        events.enter_schema_names
    );
}

/// Explicit parenting via `span.id()` across a spawn boundary produces a
/// parent_span_id on the child that matches the parent's id.
#[test]
fn explicit_parent_across_spawn() {
    let events = run_traced(2, || async {
        let parent = span("payment.charge").field("order_id", 1u64);
        let parent_id = parent.id().as_u64();
        let child = span("audit.emit").parent(parent.id());

        tokio::spawn(async {}.in_span(child)).await.unwrap();
        // Drive the parent too so it appears in the trace.
        async {}.in_span(parent).await;

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
        let mut g = span("x").field("k", "v").entered();
        g.record("late", 1u64);
        drop(g);

        // Future wrapper off-runtime, polled to completion.
        async {}.in_span(span("y")).await;
        async { tokio::task::yield_now().await }.in_span("z").await;
    });
    // Reaching here without panicking is the assertion.
}

/// A cancelled (dropped) future still emits a close so the viewer can finalize
/// the span.
#[test]
fn cancelled_future_still_closes() {
    let events = run_traced(1, || async {
        // A future that never completes; poll it once then drop it.
        let mut fut = Box::pin(std::future::pending::<()>().in_span(span("stuck")));
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let poll = fut.as_mut().poll(&mut cx);
        assert!(matches!(poll, Poll::Pending));
        drop(fut); // cancellation → close on drop
    });

    assert!(events.enter_names.contains(&"stuck".to_string()));
    assert_eq!(events.close_count, 1, "cancelled span must still close");
    assert_eq!(events.entered_span_ids, events.closed_span_ids);
}

/// Regression: a callsite that emits more than one field set must not collide
/// on the wire.
///
/// A future that yields `Pending` before `Ready` exits twice: the first exit
/// carries only the base fields, the second also carries a field recorded via
/// the handle in between. Both come from the same callsite, so if the schema
/// name did not encode the field list the encoder would reject the second
/// definition ("schema already registered with different definition").
#[test]
fn late_field_after_pending_poll_does_not_collide() {
    let events = run_traced(1, || async {
        let fut = async { tokio::task::yield_now().await }.in_span(span("op").field("base", 1u64));
        let handle = fut.handle();
        let driver = tokio::spawn(fut);
        // Record while the future is parked, so an earlier exit already went
        // out with the base field set only.
        tokio::task::yield_now().await;
        handle.record("late", 2u64);
        driver.await.unwrap();
    });

    assert!(events.enter_count >= 2, "expected multiple polls");
    assert_eq!(events.enter_count, events.exit_count);
    assert_eq!(events.close_count, 1);
    // Both field sets reached the wire.
    assert!(
        events.exit_fields.iter().any(|(k, _)| k == "base"),
        "exit fields: {:?}",
        events.exit_fields
    );
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "late" && v == "2"),
        "late field must land on a later exit: {:?}",
        events.exit_fields
    );
}

/// The tower layer creates one span per request and `on_response` records a
/// late field that lands on the exit.
#[test]
fn tower_layer_wraps_request() {
    use std::convert::Infallible;
    use std::pin::Pin;
    use tower_service::Service;

    // Doubles the request, but yields first so the response future is polled
    // more than once — the realistic case, and the one where the first exit
    // goes out before `on_response` has recorded anything.
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
        let layer = Dial9SpanLayer::builder()
            .make_span(|req: &u32| span("request").field("n", *req))
            .on_response(|h, res: &u32| h.record("doubled", *res))
            .build();
        let mut svc = layer.layer(Doubler);
        let out = svc.call(21).await.unwrap();
        assert_eq!(out, 42);
    });

    assert!(events.enter_names.contains(&"request".to_string()));
    assert_eq!(events.close_count, 1, "one span per request");
    assert!(
        events
            .enter_fields
            .iter()
            .any(|(k, v)| k == "n" && v == "21"),
        "request field on enter: {:?}",
        events.enter_fields
    );
    assert!(
        events
            .exit_fields
            .iter()
            .any(|(k, v)| k == "doubled" && v == "42"),
        "on_response late field should ride the exit: {:?}",
        events.exit_fields
    );
}

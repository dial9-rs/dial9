//! dial9 spans record spans directly to the trace file without using `tracing`
//! as a bridge.
//!
//! They render in the viewer's span lanes exactly like tracing-layer spans.
//!
//! Run with the `tower` feature to include the middleware section:
//!
//! ```sh
//! cargo run -p dial9-utils --example adhoc_spans --features tower
//! ```
use dial9::{Dial9HandleTokioExt, DiskBuffer, TokioAttachOptions, recorder};
use dial9_utils::dial9_span;
use dial9_utils::span::{Instrument as _, Span as _};
use std::time::Duration;

struct Order {
    total_cents: u64,
}

async fn load_order(id: u64) -> Order {
    tokio::time::sleep(Duration::from_millis(2)).await;
    Order {
        total_cents: 1200 + id,
    }
}

fn compute_tax(order: &Order) -> u64 {
    // Pure CPU work, no awaits: the sync guard is the right tool.
    (0..50_000).fold(0u64, |acc, i| acc.wrapping_add(order.total_cents ^ i)) % 997
}

async fn charge(order_id: u64) {
    tokio::time::sleep(Duration::from_millis(8)).await;
    let _ = order_id;
}

/// One checkout request, instrumented three different ways.
async fn checkout(order_id: u64) {
    // 1. Instrumented future with a *late* field. `total_cents` isn't known at
    //    the call site — it's only known once the order loads — so it's
    //    declared as `total_cents: u64` and set through `slots` inside the body.
    //    It rides the completion event, not the enter event.
    let (load_span, slots) =
        dial9_span!("db.load_order", order_id: u64 = order_id, total_cents: u64);
    let order = async {
        let order = load_order(order_id).await;
        slots.total_cents.set(order.total_cents);
        order
    }
    .instrument(load_span)
    .await;

    // 2. Sync RAII guard for a blocking/CPU section. The macro captures fields
    //    at construction; they ride every segment.
    let tax = {
        let span = dial9_span!("pricing.compute_tax", order_id: u64 = order_id);
        let _entered = span.enter();
        compute_tax(&order)
    }; // exit + close here

    // 3. Explicit parenting across a spawn. The viewer nests spans by timestamp
    //    containment, which breaks when a task outlives its parent, so link the
    //    audit span explicitly. `id()`/`with_parent_id()` are `Span` methods, so
    //    they work on a `dial9_span!` span just like `Dial9Span::new`.
    let charge_span = dial9_span!("payment.charge", order_id: u64 = order_id, tax_cents: u64 = tax);
    let audit = dial9_span!("audit.emit").with_parent_id(charge_span.id());
    let audit_task = tokio::spawn(
        async {
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        .instrument(audit),
    );

    charge(order_id).instrument(charge_span).await;
    let _ = audit_task.await;
}

/// A background loop, nowhere near HTTP. One combinator in the loop body makes
/// every iteration a span.
async fn settlement_worker() {
    for batch in 0..3u64 {
        let span_name = format!("settlement.batch.{batch}");
        async {
            tokio::time::sleep(Duration::from_millis(6)).await;
        }
        .instrument(dial9_span!(span_name, batch: u64 = batch))
        .await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(feature = "tower")]
async fn tower_demo() {
    use dial9_utils::tower::Dial9SpanLayerWithResponse;
    use std::convert::Infallible;
    use tower_layer::Layer;
    use tower_service::Service;

    // A stand-in for a real service: takes an order id, returns a status code.
    struct Checkout;
    impl Service<u64> for Checkout {
        type Response = u16;
        type Error = Infallible;
        type Future = std::pin::Pin<Box<dyn Future<Output = Result<u16, Infallible>> + Send>>;

        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, order_id: u64) -> Self::Future {
            Box::pin(async move {
                checkout(order_id).await;
                Ok(200)
            })
        }
    }

    // `make_span` receives the request, so it can attach request-derived fields
    // (the order id) eagerly. The status code is only known once the service
    // responds, so it's a *late* field: the `finish` closure captures the
    // span's slots and sets it from the response, and it rides the completion
    // event.
    let layer = Dial9SpanLayerWithResponse::new(|order_id: &u64| {
        let (span, slots) = dial9_span!("checkout", order_id: u64 = *order_id, status: u16);
        (span, move |status: &u16| slots.status.set(*status))
    });

    let mut svc = layer.layer(Checkout);
    for order_id in 0..3u64 {
        let status = svc.call(order_id).await.unwrap();
        assert_eq!(status, 200);
    }
}

fn main() {
    let writer = DiskBuffer::single_file("adhoc_spans_trace.bin").unwrap();
    let recorder = recorder(writer).build();

    // Attaching the runtime installs the recorder handle on every worker, so
    // spans emit no matter which thread polls them.
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();
    let runtime = recorder
        .handle()
        .attach_tokio_runtime(builder, TokioAttachOptions::default())
        .unwrap();

    runtime.block_on(async {
        let settlement = tokio::spawn(settlement_worker());

        // Plain spans, no middleware involved.
        let checkouts: Vec<_> = (0..5).map(|i| tokio::spawn(checkout(i))).collect();
        for c in checkouts {
            let _ = c.await;
        }

        #[cfg(feature = "tower")]
        tower_demo().await;

        let _ = settlement.await;

        // Let the flush cycle land before we tear the runtime down.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    // Drop the runtime first so worker threads exit and flush their
    // thread-local buffers, then finalize the segment.
    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(0));

    println!("wrote adhoc_spans_trace.0.bin — open it in the dial9 viewer");
    #[cfg(not(feature = "tower"))]
    println!("(rebuild with --features tower to include the middleware section)");
}

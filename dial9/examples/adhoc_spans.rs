//! Ad-hoc span wrappers: spans in a dial9 trace with no `tracing` subscriber.
//!
//! Note what is absent: no `tracing_subscriber::registry()`, no
//! `Dial9TracingLayer`. Each wrapper emits span events directly, and they
//! render in the viewer's span lanes exactly like tracing-layer spans.
//!
//! Run with the `tower` feature to include the middleware section:
//!
//! ```sh
//! cargo run -p dial9 --example adhoc_spans --features tokio,tower
//! ```
use dial9::span::{SpanFutureExt as _, span};
use dial9::{DiskBuffer, RecorderTokioExt, recorder};
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
    // 1. Future wrapper. Enters/exits around each poll, so the awaits inside
    //    show as idle rather than as span time.
    let order = load_order(order_id)
        .in_span(span("db.load_order").field("order_id", order_id))
        .await;

    // 2. Sync RAII guard for a blocking/CPU section. `record` attaches a value
    //    discovered mid-span; it rides the exit event.
    let tax = {
        let mut guard = span("pricing.compute_tax")
            .field("order_id", order_id)
            .entered();
        let tax = compute_tax(&order);
        guard.record("tax_cents", tax);
        tax
    }; // exit + close here

    // 3. Explicit parenting across a spawn. The viewer nests spans by timestamp
    //    containment, which breaks when a task outlives its parent, so link the
    //    audit span explicitly. `id()` is the handle that crosses the boundary.
    let charge_span = span("payment.charge")
        .field("order_id", order_id)
        .field("tax_cents", tax);
    let audit = span("audit.emit").parent(charge_span.id());
    let audit_task = tokio::spawn(
        async {
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        .in_span(audit),
    );

    charge(order_id).in_span(charge_span).await;
    let _ = audit_task.await;
}

/// A background loop, nowhere near HTTP. One combinator in the loop body makes
/// every iteration a span.
async fn settlement_worker() {
    for batch in 0..3u64 {
        async {
            tokio::time::sleep(Duration::from_millis(6)).await;
        }
        .in_span(span("settlement.run").field("batch", batch))
        .await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(feature = "tower")]
async fn tower_demo() {
    use dial9::span::Dial9SpanLayer;
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

    // `make_span` runs on the request path; `on_response` records fields that
    // are only knowable once the response exists (here, the status).
    let layer = Dial9SpanLayer::builder()
        .make_span(|order_id: &u64| span("http_request").field("order_id", *order_id))
        .on_response(|handle, status: &u16| handle.record("status", *status))
        .build();

    let mut svc = layer.layer(Checkout);
    for order_id in 0..3u64 {
        let status = svc.call(order_id).await.unwrap();
        assert_eq!(status, 200);
    }
}

fn main() {
    let writer = DiskBuffer::single_file("adhoc_spans_trace.bin").unwrap();
    let recorder = recorder(writer).build();
    let (recorder, runtime) = recorder
        .attach_tokio_runtime(|t| {
            t.worker_threads(2);
            t.enable_all();
        })
        .unwrap();

    runtime.block_on(async {
        let settlement = tokio::spawn(settlement_worker());

        // Plain wrappers, no middleware involved.
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

    drop(recorder);
    drop(runtime);

    println!("wrote adhoc_spans_trace.0.bin — open it in the dial9 viewer");
    #[cfg(not(feature = "tower"))]
    println!("(rebuild with --features tokio,tower to include the middleware section)");
}

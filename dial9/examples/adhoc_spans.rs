//! Ad-hoc span wrappers: spans in a dial9 trace with no `tracing` subscriber.
//!
//! Note what is absent: no `tracing_subscriber::registry()`, no
//! `Dial9TracingLayer`. Each span emits its events directly, and they render in
//! the viewer's span lanes exactly like tracing-layer spans.
//!
//! Run with the `tower` feature to include the middleware section:
//!
//! ```sh
//! cargo run -p dial9 --example adhoc_spans --features tokio,tower
//! ```
use dial9::span::{Instrument as _, Span as _};
use dial9::{DiskBuffer, RecorderTokioExt, dial9_span, recorder};
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
    // 1. Instrumented future. Enters/exits around each poll, so the awaits
    //    inside show as idle rather than as span time.
    let order = load_order(order_id)
        .instrument(dial9_span!("db.load_order", order_id = order_id))
        .await;

    // 2. Sync RAII guard for a blocking/CPU section. The macro captures fields
    //    at construction; they ride every segment.
    let tax = {
        let span = dial9_span!("pricing.compute_tax", order_id = order_id);
        let _entered = span.enter();
        compute_tax(&order)
    }; // exit + close here

    // 3. Explicit parenting across a spawn. The viewer nests spans by timestamp
    //    containment, which breaks when a task outlives its parent, so link the
    //    audit span explicitly. `id()`/`with_parent_id()` are `Span` methods, so
    //    they work on a `dial9_span!` span just like `Dial9Span::new`.
    let charge_span = dial9_span!("payment.charge", order_id = order_id, tax_cents = tax);
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
        async {
            tokio::time::sleep(Duration::from_millis(6)).await;
        }
        .instrument(dial9_span!("settlement.run", batch = batch))
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

    // `make_span` receives the request, so it can attach request-derived fields
    // (here the order id) rather than hardcoding them.
    let layer = Dial9SpanLayer::new(|order_id: &u64| dial9_span!("checkout", order_id = *order_id));

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

    drop(recorder);
    drop(runtime);

    println!("wrote adhoc_spans_trace.0.bin — open it in the dial9 viewer");
    #[cfg(not(feature = "tower"))]
    println!("(rebuild with --features tokio,tower to include the middleware section)");
}

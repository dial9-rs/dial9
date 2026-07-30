//! Example program for issue dial9-rs/dial9#498 — ad-hoc span wrappers.
//!
//! The API written out as a complete program, so it can be read the way it
//! will be used. It is checked in next to `ad-hoc-span-wrappers.md`, which
//! explains the mechanics behind each call.
//!
//! This file is documentation, not a build target: it is not a member of any
//! crate and is not compiled by CI. The `axum` usage and the fake domain
//! plumbing below stand in for a real service. For compiled, asserted usage of
//! the same API see `dial9-tokio-telemetry/tests/span_wrappers.rs`, and for a
//! runnable version see `dial9/examples/adhoc_spans.rs`.
//!
//! The program is a small order-checkout service: an axum/tower HTTP API in
//! front of a fake database and payment client, plus a background settlement
//! worker. It is deliberately the kind of service that has *no* tracing
//! subscriber wired up — the whole point of #498. Every span below reaches
//! the dial9 trace directly and renders in the viewer's span lanes with zero
//! extra wiring.

use std::time::Duration;

use axum::{Router, extract::Path, routing::post};
// Runtime setup (buffer/recorder/attach) comes from the tokio telemetry crate;
// the spans themselves live in `dial9-utils`.
use dial9_tokio_telemetry::telemetry::{DiskBuffer, RecorderTokioExt, recorder};

// The entire span surface. The macro + core wrappers are unconditional;
// `Dial9SpanLayer` is behind the `tower` cargo feature.
use dial9_utils::dial9_span;
use dial9_utils::span::{Dial9Span, Dial9SpanLayer, Instrument as _, Span as _};

// Note what is absent here: no tracing_subscriber::registry(), no
// Dial9TracingLayer, no `tracing` dependency at all.
fn main() {
    let writer = DiskBuffer::single_file("/tmp/checkout-traces/trace.bin").unwrap();
    let recorder = recorder(writer).build();
    let (recorder, runtime) = recorder
        .attach_tokio_runtime(|t| {
            t.enable_all();
        })
        .unwrap();

    runtime.block_on(serve());
    drop(recorder);
}

async fn serve() {
    tokio::spawn(settlement_worker());

    // ── UX 1: the tower layer ────────────────────────────────────────────
    // One span per request. The `make_span` closure receives the request, so it
    // names the span and attaches request-derived fields (method, path). The
    // response future emits one enter + one completion event carrying the
    // active/idle/poll aggregate, and closes when it resolves.
    let span_layer = Dial9SpanLayer::new(|req: &axum::extract::Request| {
        dial9_span!("http_request", method = %req.method(), path = %req.uri().path())
    });

    let app = Router::new()
        .route("/orders/{id}/checkout", post(checkout))
        .layer(span_layer);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn checkout(Path(order_id): Path<u64>) -> String {
    // ── UX 2: the future combinator ──────────────────────────────────────
    // `.instrument(...)` is the async workhorse. The span enters/exits around
    // each poll (so awaits show as idle, and each on-CPU segment is attributed
    // to the worker that ran it) and closes when the future completes or is
    // cancelled. Fields are captured at the call site by the macro; u64/Display
    // values need no `.to_string()`.
    let order = load_order(order_id)
        .instrument(dial9_span!("db.load_order", order_id = order_id))
        .await;

    // A name-only span (no fields) needs no macro — `Dial9Span::new` builds one
    // whose name is chosen at runtime.
    let rate = fetch_tax_rate(&order)
        .instrument(Dial9Span::new("tax.fetch_rate"))
        .await;

    // ── UX 3: the sync guard ─────────────────────────────────────────────
    // RAII for blocking/CPU-bound sections. `Entered` is !Send and must not be
    // held across .await — for async, use `.instrument` above. The macro's
    // fields ride every segment, including the exit.
    let quote = {
        let span = dial9_span!("pricing.compute", order_id = order_id);
        let _entered = span.enter();
        compute_quote(&order, rate) // pure CPU
    }; // exit + close here

    // ── UX 4: explicit parenting across a spawn ──────────────────────────
    // The viewer nests spans by timestamp containment, which is right for
    // everything above. A spawned task can outlive this request, so
    // containment breaks — link it explicitly instead. `span.id()` is the
    // only handle that crosses the task boundary.
    let charge_span = dial9_span!("payment.charge", order_id = order_id);
    let audit = Dial9Span::new("audit.emit").with_parent_id(charge_span.id());
    tokio::spawn(emit_audit_record(order_id).instrument(audit));

    let receipt = charge(&order, &quote).instrument(charge_span).await;

    format!("charged: {}", receipt.confirmation)
}

async fn emit_audit_record(order_id: u64) {
    let _ = order_id;
    tokio::time::sleep(Duration::from_millis(3)).await;
}

/// Background loop, no HTTP anywhere near it — the wrappers don't care.
/// This is the "integrate ad hoc" case from the issue: one combinator in a
/// loop body, and each iteration is a span in the trace.
async fn settlement_worker() {
    loop {
        let batch = next_settlement_batch().await;
        settle(&batch)
            .instrument(dial9_span!("settlement.run", batch_len = batch.len() as u64))
            .await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ── Fake domain plumbing (not part of the API under review) ──────────────

struct Order;
struct Quote {
    #[allow(dead_code)]
    total_cents: u64,
}
struct Receipt {
    confirmation: String,
}

async fn load_order(_id: u64) -> Order {
    tokio::time::sleep(Duration::from_millis(2)).await;
    Order
}

async fn fetch_tax_rate(_order: &Order) -> f64 {
    tokio::time::sleep(Duration::from_millis(1)).await;
    0.08
}

fn compute_quote(_order: &Order, _rate: f64) -> Quote {
    Quote { total_cents: 12900 }
}

async fn charge(_order: &Order, _quote: &Quote) -> Receipt {
    tokio::time::sleep(Duration::from_millis(8)).await;
    Receipt {
        confirmation: "ch_42".into(),
    }
}

async fn next_settlement_batch() -> Vec<u64> {
    tokio::time::sleep(Duration::from_millis(4)).await;
    vec![1, 2, 3]
}

async fn settle(_batch: &[u64]) {
    tokio::time::sleep(Duration::from_millis(6)).await;
}

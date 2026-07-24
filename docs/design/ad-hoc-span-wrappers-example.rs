//! Example program for issue dial9-rs/dial9#498 — ad-hoc span wrappers.
//!
//! The API written out as a complete program, so it can be read the way it
//! will be used. It is checked in next to `ad-hoc-span-wrappers.md`, which
//! explains the mechanics behind each call.
//!
//! This file is documentation, not a build target: it is not a member of any
//! crate and is not compiled by CI. The `axum` usage and the fake domain
//! plumbing below stand in for a real service. For compiled, asserted usage of
//! the same API see `dial9-tokio-telemetry/tests/span_wrappers.rs`.
//!
//! The program is a small order-checkout service: an axum/tower HTTP API in
//! front of a fake database and payment client, plus a background settlement
//! worker. It is deliberately the kind of service that has *no* tracing
//! subscriber wired up — the whole point of #498. Every span below reaches
//! the dial9 trace directly and renders in the viewer's span lanes with zero
//! extra wiring.

use std::time::Duration;

use axum::{Router, extract::Path, routing::post};
use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::telemetry::Dial9TokioHandle;

// The entire new surface. Core wrappers are dep-free and unconditional;
// `Dial9SpanLayer` is behind the `tower` cargo feature.
use dial9_tokio_telemetry::span::{Dial9SpanLayer, SpanFutureExt as _, span};

fn my_config() -> Dial9Config {
    Dial9Config::builder()
        .on_disk_buffer("/tmp/checkout-traces/trace.bin")
        .max_file_size(10_000_000)
        .max_total_size(50_000_000)
        .build_or_disabled()
}

// Note what is absent here: no tracing_subscriber::registry(), no
// Dial9TracingLayer, no `tracing` dependency at all.
#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    let handle = Dial9TokioHandle::current();
    handle.spawn(settlement_worker());

    // ── UX 1: the tower layer ────────────────────────────────────────────
    // One span per request. `make_span` is the only required setter; it
    // receives the request and returns a `Span` description. `on_response`
    // records late fields (status is unknowable at request time) — they
    // surface in the viewer's span inspector like any other field.
    let span_layer = Dial9SpanLayer::builder()
        .make_span(|req: &axum::extract::Request| {
            span("http_request")
                .field("method", req.method())
                .field("path", req.uri().path())
        })
        .on_response(|span, res: &axum::response::Response| {
            span.record("status", res.status().as_u16());
        })
        .build();

    let app = Router::new()
        .route("/orders/{id}/checkout", post(checkout))
        .layer(span_layer);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn checkout(Path(order_id): Path<u64>) -> String {
    // ── UX 2: the future wrapper ─────────────────────────────────────────
    // `.in_span(...)` is the async workhorse. The span enters/exits around
    // each poll (so awaits show as idle, and each on-CPU segment is
    // attributed to the worker that ran it) and closes when the future
    // completes or is cancelled. Fields are interned; u64/Display values
    // need no `.to_string()`.
    let order = load_order(order_id)
        .in_span(span("db.load_order").field("order_id", order_id))
        .await;

    // Shorthand for the common no-fields case. Open question #2 in the
    // design doc: is this worth having next to `span("...")`?
    let rate = fetch_tax_rate(&order).in_span("tax.fetch_rate").await;

    // ── UX 3: the sync guard ─────────────────────────────────────────────
    // RAII for blocking/CPU-bound sections. `EnteredSpan` is !Send and must
    // not be held across .await — for async, use `in_span` above. `record`
    // attaches results discovered mid-span.
    let quote = {
        let mut s = span("pricing.compute")
            .field("order_id", order_id)
            .entered();
        let quote = compute_quote(&order, rate); // pure CPU
        s.record("total_cents", quote.total_cents);
        quote
    }; // exit + close here

    // ── UX 4: explicit parenting across a spawn ──────────────────────────
    // The viewer nests spans by timestamp containment, which is right for
    // everything above. A spawned task can outlive this request, so
    // containment breaks — link it explicitly instead. `span.id()` is the
    // only handle that crosses the task boundary.
    let charge_span = span("payment.charge").field("order_id", order_id);
    let audit = span("audit.emit").parent(charge_span.id());
    tokio::spawn(emit_audit_record(order_id).in_span(audit));

    let receipt = charge(&order, &quote).in_span(charge_span).await;

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
            .in_span(span("settlement.run").field("batch_len", batch.len()))
            .await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ── Fake domain plumbing (not part of the API under review) ──────────────

struct Order;
struct Quote {
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

//! Example: embedding the dial9 viewer with custom routes and middleware.
//!
//! Shows how a downstream consumer (e.g. an internal tool) can reuse the
//! viewer's built-in routes while adding its own endpoints and wrapping the
//! whole thing with middleware such as auth.
//!
//! The extension point is [`dial9_viewer::build_app`]: describe the viewer in
//! code with a [`dial9_viewer::ViewerConfig`], get back a fully-assembled
//! [`axum::Router`] (which is also a `tower::Service`), then `.merge()` /
//! `.layer()` it and bind it yourself. Binding, logging, and the per-request
//! metrics sink are all the embedder's to own — see
//! [`dial9_viewer::init_tracing`], [`dial9_viewer::attach_request_metrics`],
//! and [`dial9_viewer::shutdown_signal`].
//!
//! ```text
//! cargo run --example embedded_viewer -- my-trace-bucket
//! ```

use axum::http::{HeaderName, HeaderValue};
use dial9_viewer::{
    ViewerConfig, attach_request_metrics, build_app, init_tracing, shutdown_signal,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bucket = std::env::args()
        .nth(1)
        .expect("usage: embedded_viewer <bucket>");

    // Logging and per-request metrics are process-global concerns the embedder
    // owns; `build_app` deliberately does not touch them. `local = true` gives
    // human-readable logs + metrique's local metrics format (nice for a dev
    // example); pass `false` for JSON logs + EMF in a deployed service. Hold
    // the metrics guard for the life of the server.
    init_tracing(true);
    let _metrics = attach_request_metrics(true);

    // 1. Describe the viewer in code: serve from an S3 bucket (region detection,
    //    BYO-credentials and the assume-role path are wired up automatically),
    //    with demand-driven aggregation enabled. Everything else is defaulted.
    let config = ViewerConfig {
        bucket: Some(bucket),
        agg: true,
        ..Default::default()
    };

    // 2. Assemble the fully-configured app (built-in routes + state +
    //    aggregation). `build_app` RETURNS the router rather than binding it.
    let app = build_app(config).await?;

    // 3. Merge custom routes — e.g. a first-party feedback endpoint.
    let app =
        app.merge(axum::Router::new().route("/api/feedback", axum::routing::post(handle_feedback)));

    // 4. Wrap the WHOLE app (built-in + custom routes) with middleware — this is
    //    where you'd put auth; here it just injects a response header.
    let app = app.layer(axum::middleware::from_fn(add_server_header));

    // 5. Bind and serve — the embedder owns the listener/port/TLS.
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    tracing::info!("embedded viewer listening on http://localhost:3000");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn handle_feedback(body: String) -> &'static str {
    tracing::info!(feedback = %body, "received feedback");
    "thanks"
}

async fn add_server_header(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        HeaderName::from_static("x-served-by"),
        HeaderValue::from_static("my-internal-viewer"),
    );
    resp
}

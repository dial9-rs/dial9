//! Example: embedding the dial9 viewer with custom routes and middleware.
//!
//! Shows how a downstream consumer (e.g. an internal tool) can reuse the
//! viewer's built-in routes while adding its own endpoints and wrapping the
//! whole thing with middleware.
//!
//! The extension point is [`dial9_viewer::server::router`] — it returns an
//! [`axum::Router`] that you can `.merge()`, `.layer()`, or `.nest_service()`
//! before binding to a listener.
//!
//! ```text
//! cargo run --example embedded_viewer -- my-trace-bucket
//! ```

use axum::http::{HeaderName, HeaderValue};
use dial9_viewer::server::{AppState, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let bucket = std::env::args()
        .nth(1)
        .expect("usage: embedded_viewer <bucket>");

    // 1. Build AppState from an S3 bucket (handles region detection,
    //    BYO-credentials, and the assume-role path automatically).
    let state = AppState::from_bucket(&bucket, None).await;

    // 2. Get the base router with all built-in routes.
    let app = router(state);

    // 3. Merge custom routes — e.g. a feedback endpoint.
    let app =
        app.merge(axum::Router::new().route("/api/feedback", axum::routing::post(handle_feedback)));

    // 4. Layer middleware — e.g. inject a response header for every request.
    let app = app.layer(axum::middleware::from_fn(add_server_header));

    // 5. Bind and serve.
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    tracing::info!("embedded viewer listening on http://localhost:3000");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
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

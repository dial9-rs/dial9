//! Per-request operational metrics, published via metrique.
//!
//! One [`RequestMetrics`] entry is emitted for every HTTP request that reaches
//! the API router (see [`record_request_metrics`]). //! ## Dimensions
//!
//! Metrics are emitted with two EMF dimension sets:
//!
//! - `["operation"]` — per-route (the matched path template, e.g.
//!   `/api/browse`, `/api/object`), so you can alarm a single noisy endpoint.
//! - `[]` (the empty set) — a service-wide aggregate across all operations.

use std::time::SystemTime;

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrique::ServiceMetrics;
use metrique::emf::Emf;
use metrique::local::{LocalFormat, OutputStyle};
use metrique::timers::Timer;
use metrique::unit::{Count, Millisecond};
use metrique::unit_of_work::metrics;
use metrique::writer::sink::AttachHandle;
use metrique::writer::{AttachGlobalEntrySinkExt, FormatExt};

const CLOUDWATCH_NAMESPACE: &str = "dial9_viewer";

/// Operation label used when a request did not match any registered route
/// (e.g. a stray request that fell through to the API router). Keeps the
/// `operation` dimension's cardinality bounded — we never emit the raw URI.
const UNMATCHED_OPERATION: &str = "unmatched";

#[metrics(emf::dimension_sets = [["operation"], []])]
struct RequestMetrics {
    /// Stamp the metric at request *start*. Without this the timestamp would be
    /// taken when the entry is flushed (after the handler runs), skewing it by
    /// the request's own latency.
    #[metrics(timestamp)]
    timestamp: SystemTime,

    operation: String,

    /// HTTP status code as a string (e.g. `"200"`). Emitted as a plain value,
    /// not a metric or dimension: it's there for CloudWatch Logs Insights /
    /// Contributor Insights without fragmenting the `fault`/`error` counts.
    status_code: String,

    /// Always `1`. The request-rate denominator.
    #[metrics(unit = Count)]
    count: u32,

    /// `1` on a `5xx` response, else `0` — the availability signal.
    #[metrics(unit = Count)]
    fault: u32,

    /// `1` on a `4xx` response, else `0` — client errors, kept separate from
    /// `fault` so they don't trip an availability alarm.
    #[metrics(unit = Count)]
    error: u32,

    /// Time to produce the response (headers). Auto-stops when the entry
    /// closes; we stop it explicitly so streaming bodies don't inflate it.
    #[metrics(unit = Millisecond)]
    latency: Timer,
}

/// Axum middleware that emits one [`RequestMetrics`] entry per request to the
/// global [`ServiceMetrics`] sink.
///
/// Wired via [`axum::middleware::from_fn`], so it carries no state. The
/// [`MatchedPath`] is read from the request extensions (axum inserts it during
/// routing, so it is available to a router `layer`); requests that matched no
/// route fall back to [`UNMATCHED_OPERATION`].
pub async fn record_request_metrics(req: Request, next: Next) -> Response {
    // Read the matched-path template *before* `next.run` consumes the request.
    let operation = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| UNMATCHED_OPERATION.to_string());

    let mut metrics = RequestMetrics {
        timestamp: SystemTime::now(),
        operation,
        status_code: String::new(),
        count: 1,
        fault: 0,
        error: 0,
        latency: Timer::start_now(),
    }
    // `sink_or_discard` (not `sink`) so this never panics when no sink is
    // attached: the deployed `serve` path attaches one at startup, but tests
    // that build the router directly do not. Entries are forwarded to the
    // attached (or test) sink if present, else silently dropped.
    .append_on_drop(ServiceMetrics::sink_or_discard());

    let response = next.run(req).await;

    // Stop the timer at response-headers time (not body completion): for a
    // streamed body the bytes flow after this middleware returns.
    metrics.latency.stop();
    let status = response.status();
    metrics.status_code = status.as_u16().to_string();
    if status.is_server_error() {
        metrics.fault = 1;
    } else if status.is_client_error() {
        metrics.error = 1;
    }

    response
}

/// Attach the process-global [`ServiceMetrics`] sink for request metrics, once,
/// at startup. Hold the returned [`AttachHandle`] for the life of the process;
/// dropping it flushes and detaches the sink.
///
/// - `local == false` (deployed): EMF to **stdout**, the format CloudWatch
///   ingests from the log stream.
/// - `local == true` (`--local`): metrique's human-readable [`LocalFormat`] to
///   stdout, for local runs.
pub fn attach_request_metrics(local: bool) -> AttachHandle {
    if local {
        ServiceMetrics::attach_to_stream(
            LocalFormat::new(OutputStyle::Pretty).output_to_makewriter(|| std::io::stdout().lock()),
        )
    } else {
        // `vec![vec![]]` = a single empty global dimension set, which the
        // entry-level `dimension_sets` are cartesian-joined onto, yielding the
        // final `["operation"]` and `[]` sets.
        ServiceMetrics::attach_to_stream(
            Emf::builder(CLOUDWATCH_NAMESPACE.to_string(), vec![vec![]])
                .build()
                .output_to_makewriter(|| std::io::stdout().lock()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use metrique::test_util::{TestEntrySink, test_entry_sink};
    use tower::ServiceExt; // for `oneshot`

    /// Drive one request through the middleware against a test sink installed
    /// on the current (single-threaded) tokio runtime, and return the single
    /// captured entry. The runtime-scoped guard keeps parallel tests isolated.
    async fn run(route: &str, uri: &str, status: StatusCode) -> metrique::test_util::TestEntry {
        let TestEntrySink { inspector, sink } = test_entry_sink();
        let _guard = ServiceMetrics::set_test_sink_on_current_tokio_runtime(sink);

        let app: Router = Router::new()
            .route(route, get(move || async move { status }))
            .layer(axum::middleware::from_fn(record_request_metrics));

        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), status);

        let entries = inspector.entries();
        assert_eq!(entries.len(), 1, "exactly one metric entry per request");
        entries.into_iter().next().unwrap()
    }

    #[tokio::test]
    async fn server_error_is_a_fault() {
        let e = run(
            "/items/{id}",
            "/items/42",
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .await;
        // Operation is the matched template, not the concrete `/items/42`.
        assert_eq!(e.values["operation"], "/items/{id}");
        assert_eq!(e.values["status_code"], "500");
        assert_eq!(e.metrics["count"].as_u64(), 1);
        assert_eq!(e.metrics["fault"].as_u64(), 1);
        assert_eq!(e.metrics["error"].as_u64(), 0);
        // Latency metric is present (a single observation).
        assert_eq!(e.metrics["latency"].num_observations(), 1);
    }

    #[tokio::test]
    async fn client_error_is_an_error_not_a_fault() {
        let e = run("/x", "/x", StatusCode::BAD_REQUEST).await;
        assert_eq!(e.metrics["fault"].as_u64(), 0);
        assert_eq!(e.metrics["error"].as_u64(), 1);
        assert_eq!(e.values["status_code"], "400");
    }

    #[tokio::test]
    async fn success_is_neither_fault_nor_error() {
        let e = run("/x", "/x", StatusCode::OK).await;
        assert_eq!(e.metrics["fault"].as_u64(), 0);
        assert_eq!(e.metrics["error"].as_u64(), 0);
        assert_eq!(e.metrics["count"].as_u64(), 1);
    }
}

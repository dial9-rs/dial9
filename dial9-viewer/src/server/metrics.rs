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

    /// Operation-specific detail, set by the handler (see [`OperationMetrics`]).
    /// `None` for endpoints that emit no extra detail, so the entry carries only
    /// the generic fields above. Flattened, so when present its fields and the
    /// `op_detail` tag appear inline on the same entry.
    #[metrics(flatten)]
    op_detail: Option<OperationMetrics>,
}

/// Per-operation metric detail, attached by a handler to the response so the
/// [`record_request_metrics`] middleware can fold it into the one entry for
/// that request. Each variant carries the signals the generic `count`/`fault`/
/// `error`/`latency` fields **cannot** see — chiefly silent degradation and
/// failures that don't surface as a non-2xx status.
///
/// This is a metrique *entry enum* (`tag` + `subfield`): the `op_detail` tag
/// records the variant name, and each variant's fields are flattened onto the
/// request entry. See the metrique `enums.rs` example.
#[derive(Clone)]
#[metrics(tag(name = "op_detail"), subfield)]
pub enum OperationMetrics {
    /// `GET /api/browse` — trace-file listing.
    Browse(#[metrics(flatten)] BrowseMetrics),
    /// `GET /api/flamegraph` — on-demand fold.
    Flamegraph(#[metrics(flatten)] FlamegraphMetrics),
    /// `GET /api/tokio-stats` — long-poll aggregation.
    TokioStats(#[metrics(flatten)] TokioStatsMetrics),
}

/// Detail for `GET /api/browse`.
#[derive(Clone)]
#[metrics(subfield)]
pub struct BrowseMetrics {
    /// Trace files returned to the client.
    objects_returned: usize,
    /// Time prefixes the request fanned out to (S3 list calls).
    prefixes_fanned_out: usize,
    /// `1` when results were capped (per-prefix or range cap) — **silent data
    /// loss behind a `200`**, invisible to the status-code metrics. Alarm here
    /// to catch users seeing partial trace lists.
    #[metrics(unit = Count)]
    truncated: u32,
    /// `1` when an hour-granularity prefix overflowed and was re-listed at
    /// 10-minute granularity (extra S3 round-trips → slower, busier bucket).
    #[metrics(unit = Count)]
    refined: u32,
}

/// Detail for `GET /api/flamegraph`.
///
/// The endpoint streams (SSE): op detail is attached at response-head time, so
/// these are RESOLVE-TIME values — the scope size and how much of it was
/// already folded when the stream opened. What the stream folds afterwards is
/// not visible here (the middleware reads the extension before the body runs).
#[derive(Clone)]
#[metrics(subfield)]
pub struct FlamegraphMetrics {
    /// Source segment files in scope.
    files_matched: u32,
    /// Files already folded when the stream opened (the instantly-served
    /// snapshot).
    files_folded: u32,
    /// Resolve-time coverage percent (`files_folded / files_matched * 100`).
    /// Persistently low values mean scopes keep opening cold — folds are not
    /// sticking (e.g. output-bucket write failures) or scopes never repeat.
    #[metrics(unit = metrique::unit::Percent)]
    coverage_pct: f64,
    /// Stack samples folded into the tree. `None` (absent) on the streaming
    /// path, where samples are only known after part-files are read inside the
    /// stream body.
    samples: Option<u64>,
}

/// Detail for `GET /api/tokio-stats`.
///
/// Streaming endpoint: resolve-time values, same caveat as
/// [`FlamegraphMetrics`].
#[derive(Clone)]
#[metrics(subfield)]
pub struct TokioStatsMetrics {
    /// Source segment files in scope.
    files_matched: u32,
    /// Files already folded when the stream opened.
    files_folded: u32,
    /// Long polls above the notable-duration floor (the signal the endpoint
    /// exists to surface). `None` (absent) on the streaming path, where polls
    /// are only read inside the stream body.
    notable_polls: Option<u64>,
}

/// Standalone metric for `GET /api/object`, emitted when the response body
/// finishes streaming — **not** part of [`RequestMetrics`].
///
/// `/api/object` streams the object bytes, so the outcome that matters
/// (`truncated_mid_stream`) is only known *after* the `200` status line and
/// headers are already sent — i.e. after the request middleware has returned
/// its entry. A streamed truncation is therefore invisible both to the HTTP
/// status and to [`RequestMetrics::fault`]. This entry closes that gap: the
/// handler arms it on the byte stream so it is appended when the stream is
/// dropped (normal end or truncation), carrying the same `operation` dimension
/// as the request metric so both line up in CloudWatch.
#[metrics(emf::dimension_sets = [["operation"], []])]
pub struct ObjectStreamMetrics {
    #[metrics(timestamp)]
    timestamp: SystemTime,
    /// Matched route (`/api/object`), to align with the request metric's
    /// `operation` dimension.
    operation: String,
    /// Always `1`: the count of completed object streams (the denominator for a
    /// mid-stream-truncation rate alarm).
    #[metrics(unit = Count)]
    count: u32,
    /// `1` when the body errored after the `200` headers were sent, so the
    /// client received a truncated object — the status-code `fault` metric is
    /// structurally blind to this. Alarm on the `truncated_mid_stream` / `count`
    /// ratio for streamed-response integrity.
    ///
    /// `pub` so the `/api/object` handler can flip it through the guard's
    /// `DerefMut` from the mid-stream error closure (it lives in a sibling
    /// module). Set it to `1` via the guard, e.g. `guard.truncated_mid_stream = 1`.
    #[metrics(unit = Count)]
    pub truncated_mid_stream: u32,
}

impl ObjectStreamMetrics {
    /// Arm a stream-scoped metric guard for an `/api/object` response. Append it
    /// to the global sink on drop (stream end). The handler flips
    /// `truncated_mid_stream` on the guard if a chunk errors mid-stream.
    pub fn arm(operation: impl Into<String>) -> ObjectStreamMetricsGuard {
        ObjectStreamMetrics {
            timestamp: SystemTime::now(),
            operation: operation.into(),
            count: 1,
            truncated_mid_stream: 0,
        }
        .append_on_drop(ServiceMetrics::sink_or_discard())
    }
}

// Constructors keep the metric fields private (so new fields stay
// non-breaking) while letting handlers build the detail with plain bool/number
// args. `u32` flag fields map from `bool` here, in one place.
impl OperationMetrics {
    /// Browse detail. `truncated`: results were capped (data loss behind a
    /// 200). `refined`: an hour prefix overflowed and was re-listed finer.
    pub fn browse(
        objects_returned: usize,
        prefixes_fanned_out: usize,
        truncated: bool,
        refined: bool,
    ) -> Self {
        Self::Browse(BrowseMetrics {
            objects_returned,
            prefixes_fanned_out,
            truncated: truncated as u32,
            refined: refined as u32,
        })
    }

    /// Flamegraph detail. `coverage_pct` is computed here from the folded /
    /// matched file counts (0.0 when nothing matched, to avoid NaN).
    pub fn flamegraph(files_matched: u32, files_folded: u32, samples: Option<u64>) -> Self {
        let coverage_pct = if files_matched == 0 {
            0.0
        } else {
            (files_folded as f64 / files_matched as f64) * 100.0
        };
        Self::Flamegraph(FlamegraphMetrics {
            files_matched,
            files_folded,
            coverage_pct,
            samples,
        })
    }

    /// Tokio-stats detail.
    pub fn tokio_stats(files_matched: u32, files_folded: u32, notable_polls: Option<u64>) -> Self {
        Self::TokioStats(TokioStatsMetrics {
            files_matched,
            files_folded,
            notable_polls,
        })
    }
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
        op_detail: None,
    }
    // `sink_or_discard` (not `sink`) so this never panics when no sink is
    // attached: the deployed `serve` path attaches one at startup, but tests
    // that build the router directly do not. Entries are forwarded to the
    // attached (or test) sink if present, else silently dropped.
    .append_on_drop(ServiceMetrics::sink_or_discard());

    let mut response = next.run(req).await;

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

    // Fold in any operation-specific detail the handler stashed on the response
    // (see `OperationMetrics`). Removed so it does not linger in the response
    // extensions sent to the client.
    metrics.op_detail = response.extensions_mut().remove::<OperationMetrics>();

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

    #[tokio::test]
    async fn no_op_detail_when_handler_attaches_none() {
        // A plain handler attaches no `OperationMetrics`, so the op-specific
        // fields and the `op_detail` tag are absent.
        let e = run("/x", "/x", StatusCode::OK).await;
        assert!(!e.values.contains_key("op_detail"));
        assert!(!e.metrics.contains_key("truncated"));
    }

    /// Drive one request whose handler attaches `op` to the response, and return
    /// the folded entry.
    async fn run_with_op(route: &str, op: OperationMetrics) -> metrique::test_util::TestEntry {
        let TestEntrySink { inspector, sink } = test_entry_sink();
        let _guard = ServiceMetrics::set_test_sink_on_current_tokio_runtime(sink);

        let app: Router = Router::new()
            .route(route, get(move || async move { axum::Extension(op) }))
            .layer(axum::middleware::from_fn(record_request_metrics));

        app.oneshot(Request::builder().uri(route).body(Body::empty()).unwrap())
            .await
            .unwrap();

        let entries = inspector.entries();
        assert_eq!(entries.len(), 1, "exactly one metric entry per request");
        entries.into_iter().next().unwrap()
    }

    #[tokio::test]
    async fn browse_op_detail_folds_into_entry() {
        // 7 objects, 4 prefixes, capped (truncated), not refined.
        let e = run_with_op("/api/browse", OperationMetrics::browse(7, 4, true, false)).await;
        // The enum tag records the variant.
        assert_eq!(e.values["op_detail"], "Browse");
        assert_eq!(e.metrics["objects_returned"].as_u64(), 7);
        assert_eq!(e.metrics["prefixes_fanned_out"].as_u64(), 4);
        assert_eq!(e.metrics["truncated"].as_u64(), 1);
        assert_eq!(e.metrics["refined"].as_u64(), 0);
        // Generic fields are still present on the same entry.
        assert_eq!(e.metrics["count"].as_u64(), 1);
    }

    #[tokio::test]
    async fn flamegraph_op_detail_computes_coverage() {
        // 50 of 200 files folded → 25% coverage.
        let e = run_with_op(
            "/api/flamegraph",
            OperationMetrics::flamegraph(200, 50, Some(9000)),
        )
        .await;
        assert_eq!(e.values["op_detail"], "Flamegraph");
        assert_eq!(e.metrics["files_matched"].as_u64(), 200);
        assert_eq!(e.metrics["files_folded"].as_u64(), 50);
        assert_eq!(e.metrics["coverage_pct"].as_f64(), 25.0);
        assert_eq!(e.metrics["samples"].as_u64(), 9000);
    }

    #[tokio::test]
    async fn flamegraph_unknown_samples_are_absent_not_zero() {
        // The streaming path can't know samples at response-head time; the
        // field must be ABSENT (never a fake 0 that would drag averages down).
        let e = run_with_op(
            "/api/flamegraph",
            OperationMetrics::flamegraph(200, 50, None),
        )
        .await;
        assert!(!e.metrics.contains_key("samples"));
        assert_eq!(e.metrics["coverage_pct"].as_f64(), 25.0);
    }

    #[tokio::test]
    async fn flamegraph_coverage_is_zero_when_nothing_matched() {
        // Guard against NaN (0/0) when the scope matched no files.
        let e = run_with_op("/api/flamegraph", OperationMetrics::flamegraph(0, 0, None)).await;
        assert_eq!(e.metrics["coverage_pct"].as_f64(), 0.0);
    }

    #[tokio::test]
    async fn object_stream_metric_is_a_separate_entry() {
        let TestEntrySink { inspector, sink } = test_entry_sink();
        let _guard = ServiceMetrics::set_test_sink_on_current_tokio_runtime(sink);

        // Arm, then simulate a mid-stream truncation, then drop → one entry.
        let mut guard = ObjectStreamMetrics::arm("/api/object");
        guard.truncated_mid_stream = 1;
        drop(guard);

        let entries = inspector.entries();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.values["operation"], "/api/object");
        assert_eq!(e.metrics["count"].as_u64(), 1);
        assert_eq!(e.metrics["truncated_mid_stream"].as_u64(), 1);
    }

    #[tokio::test]
    async fn object_stream_metric_clean_when_not_truncated() {
        let TestEntrySink { inspector, sink } = test_entry_sink();
        let _guard = ServiceMetrics::set_test_sink_on_current_tokio_runtime(sink);

        drop(ObjectStreamMetrics::arm("/api/object"));

        let e = inspector.get(0);
        assert_eq!(e.metrics["truncated_mid_stream"].as_u64(), 0);
    }
}

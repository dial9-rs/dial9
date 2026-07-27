use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
};
use dial9::metrique_sink::{Dial9Context, Interned};
use metrique::ServiceMetrics;
use metrique::timers::Timer;
use metrique::unit_of_work::metrics;
use metrique::writer::GlobalEntrySink;
use serde::{Deserialize, Serialize};

use crate::AppState;

/// Per-request metrics, recorded into the dial9 trace via the
/// `Dial9Stream` attached in `main`.
#[metrics(rename_all = "PascalCase")]
struct RequestMetrics {
    #[metrics(flatten)]
    dial9: Dial9Context,

    #[metrics(flags(Interned))]
    operation: &'static str,

    #[metrics(flags(Interned))]
    metric_name: String,

    /// Stops at entry close; carries a Milliseconds unit annotation.
    latency: Timer,

    success: bool,
}

impl RequestMetrics {
    fn init(operation: &'static str, metric_name: String) -> RequestMetricsGuard {
        RequestMetrics {
            dial9: Dial9Context::capture(),
            operation,
            metric_name,
            latency: Timer::start_now(),
            success: false,
        }
        .append_on_drop(ServiceMetrics::sink())
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/metrics", post(record_metric))
        .route("/metrics/{name}", get(query_metric))
        .route("/terminate", post(terminate))
        .with_state(state)
}

#[derive(Deserialize)]
struct MetricPayload {
    name: String,
    value: f64,
}

#[tracing::instrument(skip(state, payload), fields(metric_name = %payload.name, request_id = %uuid::Uuid::new_v4()))]
async fn record_metric(
    State(state): State<AppState>,
    Json(payload): Json<MetricPayload>,
) -> StatusCode {
    let mut metrics = RequestMetrics::init("RecordMetric", payload.name.clone());
    state.buffer.record(payload.name, payload.value).await;
    metrics.success = true;
    StatusCode::ACCEPTED
}

#[derive(Serialize)]
struct AggregateRow {
    timestamp: u64,
    sum: f64,
    count: u64,
    min: f64,
    max: f64,
}

#[tracing::instrument(skip_all, fields(name = %name, request_id = %uuid::Uuid::new_v4()))]
async fn query_metric(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Vec<AggregateRow>>, StatusCode> {
    let mut metrics = RequestMetrics::init("QueryMetric", name.clone());
    let result = state
        .ddb
        .query_metric(&name)
        .await
        .map(|rows| {
            Json(
                rows.into_iter()
                    .map(|(timestamp, sum, count, min, max)| AggregateRow {
                        timestamp,
                        sum,
                        count,
                        min,
                        max,
                    })
                    .collect(),
            )
        })
        .map_err(|e| {
            eprintln!("query error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        });
    metrics.success = result.is_ok();
    result
}

async fn terminate(State(state): State<AppState>) -> StatusCode {
    println!("Received /terminate – initiating graceful shutdown.");
    state.shutdown.cancel();
    StatusCode::OK
}

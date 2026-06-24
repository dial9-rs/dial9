use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::server::AppState;

#[derive(Serialize)]
pub struct ConfigResponse {
    pub default_bucket: Option<String>,
    pub default_prefix: Option<String>,
    /// True when the server runs demand-driven aggregation, so the client's
    /// flamegraph button should drive the sampled `/api/flamegraph?api=1` loop
    /// (scope-based) instead of streaming raw traces for client-side decode.
    pub aggregation_enabled: bool,
    /// Whether the UI should offer the bring-your-own-credentials panel.
    pub supports_byo_credentials: bool,
}

pub async fn get_config(State(state): State<AppState>) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        default_bucket: state.default_bucket.clone(),
        default_prefix: state.default_prefix.clone(),
        // On-demand aggregation runs either when the server was started with a
        // server-side `AggContext`, or against any S3 source (any bucket can
        // drive the `/api/flamegraph` refinement loop).
        aggregation_enabled: state.agg.is_some() || state.allow_byo_creds,
        // The credentials panel is only meaningful for an S3 source.
        supports_byo_credentials: state.allow_byo_creds,
    })
}

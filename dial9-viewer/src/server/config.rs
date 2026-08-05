use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::server::AppState;

#[derive(Serialize)]
pub struct ConfigResponse {
    pub default_bucket: Option<String>,
    pub default_prefix: Option<String>,
    /// True when the server runs demand-driven aggregation, so the client's
    /// flamegraph button should open the sampled `/api/flamegraph?api=1` SSE
    /// stream (scope-based) instead of streaming raw traces for client-side decode.
    pub aggregation_enabled: bool,
    /// Whether the UI should offer the bring-your-own-credentials panel.
    pub supports_byo_credentials: bool,
    /// Shape of source keys used for browse and service discovery.
    ///
    /// Additive for compatibility: older clients infer this from credential
    /// support, while current clients use it directly.
    pub source_layout: &'static str,
    /// Whether the server will assume a caller-named role ARN
    /// (`x-dial9-aws-role-arn`) — i.e. an assumer is wired. Lets a client offer
    /// "read via role" as an alternative to pasting credentials.
    pub supports_assume_role: bool,
    /// Bucket-name substring (matched case-insensitively, client-side) the UI's
    /// bucket picker uses to surface likely trace buckets. Defaults to "dial9";
    /// an empty string disables the filtering. The page may override this per
    /// load with a `bucket_filter=` query param on its own URL (T15; resolves
    /// the features/01 Finding-2 bucket-filter lockout structurally).
    pub bucket_filter: String,
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
        source_layout: if state.time_partitioned_source {
            "time-partitioned"
        } else {
            "flat"
        },
        // Assume-role is available only when an assumer was wired (S3 source
        // with an ambient identity); the local-only implementation always
        // reports false.
        supports_assume_role: state.supports_assume_role(),
        bucket_filter: state.bucket_filter.clone(),
    })
}

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::server::error::storage_error_response;

#[derive(Deserialize)]
pub struct PrefixParams {
    pub bucket: Option<String>,
    pub prefix: Option<String>,
}

pub async fn list_prefixes(
    State(state): State<AppState>,
    creds: MaybeCreds,
    Query(params): Query<PrefixParams>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    let backend = state.resolve(creds)?;

    let bucket = params
        .bucket
        .or(state.default_bucket.clone())
        .ok_or((StatusCode::BAD_REQUEST, "bucket is required".to_string()))?;

    let prefix = params.prefix.unwrap_or_default();

    let prefixes = backend
        .list_prefixes(&bucket, &prefix)
        .await
        .map_err(storage_error_response)?;

    Ok(Json(prefixes))
}

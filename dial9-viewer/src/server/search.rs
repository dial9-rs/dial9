use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::server::error::storage_error_response;
use crate::storage::ObjectInfo;

#[derive(Deserialize)]
pub struct SearchParams {
    /// Search query — used as S3 prefix in MVP
    pub q: Option<String>,
    pub bucket: Option<String>,
}

pub async fn search(
    State(state): State<AppState>,
    creds: MaybeCreds,
    Query(params): Query<SearchParams>,
) -> Result<Json<Vec<ObjectInfo>>, (StatusCode, String)> {
    let backend = state.resolve(creds)?;

    let bucket = params
        .bucket
        .or(state.default_bucket.clone())
        .ok_or((StatusCode::BAD_REQUEST, "bucket is required".to_string()))?;

    let prefix = match (&state.default_prefix, &params.q) {
        (Some(pfx), Some(q)) => format!("{pfx}/{q}"),
        (Some(pfx), None) => pfx.clone(),
        (None, Some(q)) => q.clone(),
        (None, None) => String::new(),
    };

    let objects = backend
        .list_objects(&bucket, &prefix)
        .await
        .map_err(storage_error_response)?;

    Ok(Json(objects))
}

//! Bucket-listing endpoints. `GET /api/buckets` preserves the names-only API;
//! `GET /api/buckets/details` also returns each bucket's region for the picker.
//! Both double as credential checks (they fail if the credentials are bad).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::server::error::storage_error_response;

pub async fn list_buckets(
    State(state): State<AppState>,
    creds: MaybeCreds,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    let backend = state.resolve(creds).await?;
    let buckets = backend
        .list_buckets()
        .await
        .map_err(storage_error_response)?;
    Ok(Json(buckets))
}

pub async fn list_bucket_details(
    State(state): State<AppState>,
    creds: MaybeCreds,
) -> Result<Json<Vec<crate::storage::BucketInfo>>, (StatusCode, String)> {
    let backend = state.resolve(creds).await?;
    let buckets = backend
        .list_bucket_details()
        .await
        .map_err(storage_error_response)?;
    Ok(Json(buckets))
}

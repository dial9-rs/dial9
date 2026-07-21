//! `GET /api/buckets` lists each visible bucket and its region for the picker.
//! It also doubles as a credential check (it fails if the credentials are bad).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::server::error::storage_error_response;

pub async fn list_buckets(
    State(state): State<AppState>,
    creds: MaybeCreds,
) -> Result<Json<Vec<crate::storage::BucketInfo>>, (StatusCode, String)> {
    let backend = state.resolve(creds).await?;
    let buckets = backend
        .list_buckets()
        .await
        .map_err(storage_error_response)?;
    Ok(Json(buckets))
}

//! `GET /api/buckets` — list the buckets the supplied credentials can see, so
//! the viewer can offer a bucket picker instead of requiring the user to know
//! the name. Also doubles as a credential check (it fails if the creds are bad).

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

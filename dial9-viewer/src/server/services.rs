//! Service discovery for the trace browser's lightweight initial query.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::server::AppState;
use crate::server::browse::{historical_minute_prefixes, key_host, key_service, resolve_base};
use crate::server::credentials::MaybeCreds;
use crate::server::error::storage_error_response;
use crate::source_layout;

const LOCAL_OBJECT_CAP: usize = 10_000;
const LIST_CONCURRENCY: usize = 32;
/// Historical service discovery is only a recent-activity feeler. Keeping this
/// window fixed prevents a large browse range from creating one LIST per minute.
const DISCOVERY_WINDOW_SECS: i64 = 10 * 60;

#[derive(Deserialize)]
pub struct ServicesParams {
    pub bucket: Option<String>,
    /// Optional key prefix before the layout root.
    pub prefix: Option<String>,
    /// Inclusive start of the requested browse range, unix seconds. Versioned
    /// discovery scans a bounded number of days; historical discovery scans
    /// the trailing ten minutes.
    pub from: i64,
    /// Inclusive end of the discovery window, unix seconds.
    pub to: i64,
}

#[derive(Serialize)]
pub struct ServicesResponse {
    pub services: Vec<String>,
    /// Additive metadata keyed by service name. `services` remains for clients
    /// that predate metadata support.
    pub service_metadata: Vec<ServiceMetadata>,
    /// True when the local object listing or S3 discovery exceeded its bound.
    pub truncated: bool,
}

#[derive(Serialize)]
pub struct ServiceMetadata {
    pub service: String,
    /// S3 discovery omits this until the selected service has been browsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_count: Option<usize>,
    /// Opaque layout discovery state that clients may echo to `/api/browse`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout_hint: Option<String>,
}

pub async fn list_services(
    State(state): State<AppState>,
    creds: MaybeCreds,
    Query(params): Query<ServicesParams>,
) -> Result<Json<ServicesResponse>, (StatusCode, String)> {
    if params.to < params.from {
        return Err((
            StatusCode::BAD_REQUEST,
            "`to` must be greater than or equal to `from`".to_string(),
        ));
    }

    let backend = state.resolve(creds).await?;
    let bucket = params
        .bucket
        .or(state.default_bucket.clone())
        .ok_or((StatusCode::BAD_REQUEST, "bucket is required".to_string()))?;
    let key_prefix = params
        .prefix
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let base = resolve_base(state.default_prefix.as_deref(), key_prefix);

    if !state.time_partitioned_source {
        let page = backend
            .list_objects(&bucket, &base, LOCAL_OBJECT_CAP)
            .await
            .map_err(storage_error_response)?;
        let mut hosts_by_service = BTreeMap::<String, BTreeSet<String>>::new();
        for object in &page.objects {
            let Some(service) = key_service(&object.key) else {
                continue;
            };
            let hosts = hosts_by_service.entry(service).or_default();
            if let Some(host) = key_host(&object.key) {
                hosts.insert(host);
            }
        }
        let services = hosts_by_service.keys().cloned().collect();
        let service_metadata = metadata_from_hosts(hosts_by_service);
        return Ok(Json(ServicesResponse {
            services,
            service_metadata,
            truncated: page.truncated,
        }));
    }

    let version1 = source_layout::discover_version1_services(
        &*backend,
        &bucket,
        &base,
        params.from,
        params.to,
    )
    .await
    .map_err(storage_error_response)?;

    let discovery_from = params
        .to
        .saturating_sub(DISCOVERY_WINDOW_SECS)
        .max(params.from);
    let (prefixes, truncated) = historical_minute_prefixes(&base, discovery_from, params.to);
    let results = futures::stream::iter(prefixes)
        .map(|prefix| {
            let backend = backend.clone();
            let bucket = bucket.clone();
            async move {
                let children = backend
                    .list_prefixes(&bucket, &format!("{prefix}/"))
                    .await?;
                Ok::<_, crate::storage::StorageError>((prefix, children))
            }
        })
        .buffer_unordered(LIST_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    let mut services = version1.services().cloned().collect::<BTreeSet<_>>();
    for result in results {
        let (minute_prefix, children) = result.map_err(storage_error_response)?;
        let service_root = format!("{minute_prefix}/");
        for child in children {
            let Some(segment) = child
                .strip_prefix(&service_root)
                .and_then(|rest| rest.strip_suffix('/'))
                .filter(|rest| !rest.is_empty() && !rest.contains('/'))
            else {
                continue;
            };
            services.insert(segment.to_string());
        }
    }

    let service_metadata = services
        .iter()
        .cloned()
        .map(|service| ServiceMetadata {
            layout_hint: (!version1.truncated()).then(|| {
                source_layout::hint_for_service(
                    &bucket,
                    &base,
                    &service,
                    version1.scanned_days(),
                    version1.first_date_for(&service).map(String::as_str),
                )
            }),
            service,
            host_count: None,
        })
        .collect();

    Ok(Json(ServicesResponse {
        services: services.into_iter().collect(),
        service_metadata,
        truncated: truncated || version1.truncated(),
    }))
}

fn metadata_from_hosts(
    hosts_by_service: BTreeMap<String, BTreeSet<String>>,
) -> Vec<ServiceMetadata> {
    hosts_by_service
        .into_iter()
        .map(|(service, hosts)| ServiceMetadata {
            service,
            host_count: Some(hosts.len()),
            layout_hint: None,
        })
        .collect()
}

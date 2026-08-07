//! Service discovery for the trace browser's lightweight initial query.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::server::AppState;
use crate::server::browse::{key_host, key_service, minute_time_prefixes, resolve_base};
use crate::server::credentials::MaybeCreds;
use crate::server::error::storage_error_response;

const LOCAL_OBJECT_CAP: usize = 10_000;
const LIST_CONCURRENCY: usize = 32;
/// Service discovery is only a recent-activity feeler. Keeping this window
/// fixed prevents a large browse range from creating one LIST per minute.
const DISCOVERY_WINDOW_SECS: i64 = 10 * 60;

#[derive(Deserialize)]
pub struct ServicesParams {
    pub bucket: Option<String>,
    /// Optional key prefix before the date partition.
    pub prefix: Option<String>,
    /// Inclusive start of the requested browse range, unix seconds. S3
    /// discovery scans at most its trailing ten minutes.
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
    /// True when the local object listing exceeded its bound. S3 discovery
    /// uses a fixed trailing window and therefore has bounded prefix fan-out.
    pub truncated: bool,
}

#[derive(Serialize)]
pub struct ServiceMetadata {
    pub service: String,
    pub host_count: usize,
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
            let hosts = hosts_by_service.entry(service.to_string()).or_default();
            if let Some(host) = key_host(&object.key) {
                hosts.insert(host.to_string());
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

    let discovery_from = params
        .to
        .saturating_sub(DISCOVERY_WINDOW_SECS)
        .max(params.from);
    let (prefixes, truncated) = minute_time_prefixes(&base, discovery_from, params.to);
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

    let mut service_roots = Vec::new();
    let mut services = BTreeSet::new();
    for result in results {
        let (minute_prefix, children) = result.map_err(storage_error_response)?;
        let service_root = format!("{minute_prefix}/");
        for child in children {
            if let Some(service) = child
                .strip_prefix(&service_root)
                .and_then(|rest| rest.strip_suffix('/'))
                .filter(|rest| !rest.is_empty() && !rest.contains('/'))
            {
                services.insert(service.to_string());
                service_roots.push((service.to_string(), child));
            }
        }
    }

    let host_results = futures::stream::iter(service_roots)
        .map(|(service, service_root)| {
            let backend = backend.clone();
            let bucket = bucket.clone();
            async move {
                let children = backend.list_prefixes(&bucket, &service_root).await?;
                Ok::<_, crate::storage::StorageError>((service, service_root, children))
            }
        })
        .buffer_unordered(LIST_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    let mut hosts_by_service = services
        .iter()
        .cloned()
        .map(|service| (service, BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for result in host_results {
        let (service, service_root, children) = result.map_err(storage_error_response)?;
        let hosts = hosts_by_service
            .get_mut(&service)
            .expect("discovered service must have a metadata entry");
        for child in children {
            if let Some(host) = child
                .strip_prefix(&service_root)
                .and_then(|rest| rest.strip_suffix('/'))
                .filter(|rest| !rest.is_empty() && !rest.contains('/'))
            {
                hosts.insert(host.to_string());
            }
        }
    }

    Ok(Json(ServicesResponse {
        services: services.into_iter().collect(),
        service_metadata: metadata_from_hosts(hosts_by_service),
        truncated,
    }))
}

fn metadata_from_hosts(
    hosts_by_service: BTreeMap<String, BTreeSet<String>>,
) -> Vec<ServiceMetadata> {
    hosts_by_service
        .into_iter()
        .map(|(service, hosts)| ServiceMetadata {
            service,
            host_count: hosts.len(),
        })
        .collect()
}

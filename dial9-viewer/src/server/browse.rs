//! `browse` finds all trace files for a given timerange / filter set
use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::server::error::storage_error_response;
use crate::server::metrics::OperationMetrics;
use crate::source_layout::{DayLayout, Version1Services};
use crate::storage::{ObjectInfo, StorageBackend, StorageError};

#[cfg(test)]
use crate::source_layout::LayoutSet;

/// Per-prefix object cap. A 10-minute (or minute) prefix can legitimately fan
/// out across many hosts; 10k absorbs a very busy bucket while still bounding
/// the response. When a single prefix exceeds this, the result is reported as
/// truncated so the UI warns rather than silently showing partial data.
const PER_PREFIX_CAP: usize = 10_000;

/// Bound on how many time prefixes a single browse request may fan out to.
const MAX_PREFIXES: usize = 2_000;

/// Max S3 list calls in flight at once. Overlaps the network-bound list calls
/// without exhausting the connection pool on a wide fan-out.
const LIST_CONCURRENCY: usize = 32;

/// Window at or below which we drop to minute-granularity prefixes. A short
/// focus window over a busy bucket would otherwise lump 10 minutes of every
/// host into a single list call and risk the per-prefix cap.
const MINUTE_GRANULARITY_THRESHOLD_SECS: i64 = 600;

#[derive(Deserialize)]
pub struct BrowseParams {
    pub bucket: Option<String>,
    /// Optional key prefix before the layout root, e.g. `traces`. When omitted
    /// the server's default prefix (if any) is used.
    pub prefix: Option<String>,
    /// Optional exact service value. Empty values are treated as absent.
    pub service: Option<String>,
    /// Opaque layout-discovery result returned by `/api/services`.
    pub layout_hint: Option<String>,
    /// Inclusive start of the window, unix seconds.
    pub from: i64,
    /// Inclusive end of the window, unix seconds.
    pub to: i64,
}

#[derive(Serialize)]
pub struct BrowseResponse {
    pub objects: Vec<ObjectInfo>,
    /// True if discovery or any object list was truncated. The UI shows a
    /// warning so the user knows some traces may be missing.
    pub truncated: bool,
}

/// Granularity of the time prefixes scanned in S3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Granularity {
    /// Two-character hour prefix.
    Hour,
    /// Four-character minute prefix.
    Minute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListingPrefix {
    key: String,
    granularity: Granularity,
}

type BrowseOk = (Extension<OperationMetrics>, Json<BrowseResponse>);

pub async fn browse(
    State(state): State<AppState>,
    creds: MaybeCreds,
    Query(params): Query<BrowseParams>,
) -> Result<BrowseOk, (StatusCode, String)> {
    let service = normalize_service(params.service.as_deref());
    let backend = state.resolve(creds).await?;

    let bucket = params
        .bucket
        .or(state.default_bucket.clone())
        .ok_or((StatusCode::BAD_REQUEST, "bucket is required".to_string()))?;

    if params.to < params.from {
        return Err((
            StatusCode::BAD_REQUEST,
            "`to` must be greater than or equal to `from`".to_string(),
        ));
    }

    // Combine the user's key prefix with the server's default prefix.
    let key_prefix = params
        .prefix
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let base = resolve_base(state.default_prefix.as_deref(), key_prefix);

    // Flat-layout mode: one listing, with no date-prefix fan-out.
    if !state.time_partitioned_source {
        return browse_local(backend, &bucket, &base, service).await;
    }

    browse_s3(
        backend,
        &bucket,
        &base,
        params.from,
        params.to,
        service,
        params.layout_hint.as_deref(),
    )
    .await
}

fn normalize_service(service: Option<&str>) -> Option<&str> {
    service.map(str::trim).filter(|service| !service.is_empty())
}

/// Flat-layout mode: one listing filtered to trace segments.
/// No time filtering — the frontend shows all results, positioned by mtime.
async fn browse_local(
    backend: Arc<dyn StorageBackend>,
    bucket: &str,
    base: &str,
    service: Option<&str>,
) -> Result<BrowseOk, (StatusCode, String)> {
    let page = backend
        .list_objects(bucket, base, PER_PREFIX_CAP)
        .await
        .map_err(storage_error_response)?;
    let objects: Vec<ObjectInfo> = page
        .objects
        .into_iter()
        .filter(|o| crate::ingest::aggregate::is_trace_segment(&o.key))
        // Local flat listings cannot infer a service from legacy buffer paths.
        // Supported S3 layouts provide an exact parsed service value.
        .filter(|o| service.is_none_or(|wanted| key_service(&o.key).as_deref() == Some(wanted)))
        .collect();
    let op = OperationMetrics::browse(objects.len(), 0, page.truncated, false);
    Ok((
        Extension(op),
        Json(BrowseResponse {
            objects,
            truncated: page.truncated,
        }),
    ))
}

/// S3 mode: date-prefix fan-out with overflow refinement.
async fn browse_s3(
    backend: Arc<dyn StorageBackend>,
    bucket: &str,
    base: &str,
    from: i64,
    to: i64,
    service: Option<&str>,
    layout_hint: Option<&str>,
) -> Result<BrowseOk, (StatusCode, String)> {
    let window = to - from;
    let gran = if window < MINUTE_GRANULARITY_THRESHOLD_SECS {
        Granularity::Minute
    } else {
        Granularity::Hour
    };

    let (prefixes, range_truncated) = match service {
        Some(service) => {
            let resolved = crate::source_layout::resolve_service_layouts(
                &*backend,
                bucket,
                base,
                from,
                to,
                service,
                layout_hint,
            )
            .await
            .map_err(storage_error_response)?;
            let (prefixes, truncated) = service_time_prefixes(base, &resolved.days, service, gran);
            (prefixes, truncated || resolved.truncated)
        }
        None => {
            let version1 =
                crate::source_layout::discover_version1_services(&*backend, bucket, base, from, to)
                    .await
                    .map_err(storage_error_response)?;
            let (prefixes, truncated) = unscoped_time_prefixes(base, gran, &version1);
            (prefixes, truncated || version1.truncated())
        }
    };

    // Per-request operational detail — the request-rate/latency signal lives in
    // the per-request EMF metrics now, so keep this at debug to avoid log spam.
    tracing::debug!(
        bucket = %bucket,
        prefixes = prefixes.len(),
        service,
        "browse fan-out"
    );

    // Fan the per-prefix list calls out concurrently (bounded), then merge.
    // The prefixes are disjoint key-spaces (each is a distinct time bucket), so
    // no object can appear under two of them — no dedup needed.
    //
    // `buffered` (not `buffer_unordered`): we correlate each result with its
    // prefix below by position (`prefixes[i]`) to collect overflowed prefixes,
    // so the result order must match the input order. `buffer_unordered` yields
    // in completion order and would misattribute overflows to the wrong prefix.
    // Since we `collect` every result before proceeding, ordering costs no
    // throughput here.
    let results: Vec<Result<crate::storage::ListPage, StorageError>> =
        futures::stream::iter(prefixes.clone())
            .map(|prefix| {
                let backend = backend.clone();
                let bucket = bucket.to_string();
                async move {
                    backend
                        .list_objects(&bucket, &prefix.key, PER_PREFIX_CAP)
                        .await
                }
            })
            .buffered(LIST_CONCURRENCY)
            .collect()
            .await;

    let mut objects = Vec::new();
    let mut truncated = range_truncated;

    // Collect overflowed hour-level prefixes for refinement.
    let mut overflow_prefixes = Vec::new();
    for (i, result) in results.into_iter().enumerate() {
        let page = result.map_err(storage_error_response)?;
        if page.truncated && prefixes[i].granularity == Granularity::Hour {
            overflow_prefixes.push(prefixes[i].clone());
        } else {
            truncated |= page.truncated;
            objects.extend(page.objects);
        }
    }

    // Retry overflowed hour-level prefixes at 10-minute granularity.
    let refined = !overflow_prefixes.is_empty();
    if !overflow_prefixes.is_empty() {
        // Expand each overflowed hour prefix into its 6 ten-minute sub-prefixes.
        let refined: Vec<String> = overflow_prefixes
            .iter()
            .flat_map(|prefix| (0..6).map(move |digit| format!("{}{digit}", prefix.key)))
            .collect();

        tracing::debug!(
            refined_prefixes = refined.len(),
            overflowed_hours = overflow_prefixes.len(),
            "browse refining overflowed hours at 10-minute granularity"
        );

        let refined_results: Vec<Result<crate::storage::ListPage, StorageError>> =
            futures::stream::iter(refined)
                .map(|p| {
                    let backend = backend.clone();
                    let bucket = bucket.to_string();
                    async move { backend.list_objects(&bucket, &p, PER_PREFIX_CAP).await }
                })
                .buffer_unordered(LIST_CONCURRENCY)
                .collect()
                .await;

        for result in refined_results {
            let page = result.map_err(storage_error_response)?;
            truncated |= page.truncated;
            objects.extend(page.objects);
        }
    }

    objects.retain(|object| {
        crate::ingest::aggregate::is_trace_segment(&object.key)
            && service.is_none_or(|wanted| key_service(&object.key).as_deref() == Some(wanted))
    });

    // Operation-specific metrics: `truncated` is silent data loss behind a 200,
    // and `prefixes_fanned_out`/`refined` show how hard the listing worked.
    let op = OperationMetrics::browse(objects.len(), prefixes.len(), truncated, refined);
    Ok((Extension(op), Json(BrowseResponse { objects, truncated })))
}

pub(super) fn key_service(key: &str) -> Option<String> {
    let (_, service, _) = crate::source_key_scope::dated_scope_fields(key)?;
    (!service.is_empty()).then_some(service)
}

pub(super) fn key_host(key: &str) -> Option<String> {
    let (_, _, host) = crate::source_key_scope::dated_scope_fields(key)?;
    (!host.is_empty()).then_some(host)
}

/// Build listing prefixes for one selected service from a resolved per-day
/// migration layout. Historical keys need minute buckets because their service
/// follows the time component; v1 keys can use broad time prefixes because the
/// service precedes time.
fn service_time_prefixes(
    base: &str,
    days: &[DayLayout],
    service: &str,
    version1_granularity: Granularity,
) -> (Vec<ListingPrefix>, bool) {
    let mut prefixes = Vec::new();
    let mut truncated = false;
    let encoded_service = crate::segment_object_key_codec::hive_escape(service);

    for day in days {
        if day.layout.historical() {
            append_time_prefixes(
                day.from,
                day.to,
                Granularity::Minute,
                &mut prefixes,
                &mut truncated,
                |date, time| join_prefix(base, &format!("{date}/{time}/{service}/")),
            );
        }
        if !truncated && day.layout.version1() {
            append_time_prefixes(
                day.from,
                day.to,
                version1_granularity,
                &mut prefixes,
                &mut truncated,
                |date, time| {
                    let suffix =
                        format!("version=1/date={date}/service={encoded_service}/time={time}");
                    join_prefix(base, &suffix)
                },
            );
        }
        if truncated {
            break;
        }
    }
    (prefixes, truncated)
}

/// Explicit unscoped fallback: list historical time buckets plus v1 time
/// buckets for the services discovered beneath each v1 date partition.
fn unscoped_time_prefixes(
    base: &str,
    granularity: Granularity,
    version1: &Version1Services,
) -> (Vec<ListingPrefix>, bool) {
    let mut prefixes = Vec::new();
    let mut truncated = false;

    for day in version1.scanned_days() {
        append_time_prefixes(
            day.from,
            day.to,
            granularity,
            &mut prefixes,
            &mut truncated,
            |date, time| join_prefix(base, &format!("{date}/{time}")),
        );
        if truncated {
            break;
        }

        for service in version1.services_on(&day.date) {
            let encoded_service = crate::segment_object_key_codec::hive_escape(service);
            append_time_prefixes(
                day.from,
                day.to,
                granularity,
                &mut prefixes,
                &mut truncated,
                |date, time| {
                    join_prefix(
                        base,
                        &format!("version=1/date={date}/service={encoded_service}/time={time}"),
                    )
                },
            );
            if truncated {
                break;
            }
        }
        if truncated {
            break;
        }
    }
    (prefixes, truncated)
}

pub(super) fn resolve_base(default_prefix: Option<&str>, key_prefix: Option<&str>) -> String {
    match (default_prefix, key_prefix) {
        (Some(pfx), Some(kp)) => {
            let default = pfx.trim_matches('/');
            let requested = kp.trim_matches('/');
            if default.is_empty()
                || requested == default
                || requested.starts_with(&format!("{default}/"))
            {
                requested.to_string()
            } else {
                format!("{default}/{requested}")
            }
        }
        (Some(pfx), None) => pfx.to_string(),
        (None, Some(kp)) => kp.to_string(),
        (None, None) => String::new(),
    }
}

pub(super) fn historical_minute_prefixes(base: &str, from: i64, to: i64) -> (Vec<String>, bool) {
    let mut listing = Vec::new();
    let mut truncated = false;
    append_time_prefixes(
        from,
        to,
        Granularity::Minute,
        &mut listing,
        &mut truncated,
        |date, time| join_prefix(base, &format!("{date}/{time}")),
    );
    (
        listing.into_iter().map(|prefix| prefix.key).collect(),
        truncated,
    )
}

fn append_time_prefixes(
    from: i64,
    to: i64,
    granularity: Granularity,
    prefixes: &mut Vec<ListingPrefix>,
    truncated: &mut bool,
    mut build: impl FnMut(&str, &str) -> String,
) {
    if *truncated {
        return;
    }
    let step = match granularity {
        Granularity::Hour => 3600,
        Granularity::Minute => 60,
    };
    // Align the start down to the bucket boundary. Epoch 0 is midnight UTC and
    // both hour and minute steps divide the day evenly, so floored alignment
    // (rem_euclid, correct even for pre-1970 inputs) lands on a wall-clock boundary.
    let start = from - from.rem_euclid(step);

    let mut t = start;
    while t <= to {
        if prefixes.len() == MAX_PREFIXES {
            *truncated = true;
            break;
        }
        if let Some((date, time)) = bucket_parts(t, granularity) {
            prefixes.push(ListingPrefix {
                key: build(&date, &time),
                granularity,
            });
        }
        let Some(next) = t.checked_add(step) else {
            break;
        };
        t = next;
    }
}

fn bucket_parts(epoch: i64, gran: Granularity) -> Option<(String, String)> {
    let dt = OffsetDateTime::from_unix_timestamp(epoch).ok()?;
    let date = format!(
        "{:04}-{:02}-{:02}",
        dt.year(),
        u8::from(dt.month()),
        dt.day()
    );
    let time = match gran {
        Granularity::Hour => format!("{:02}", dt.hour()),
        Granularity::Minute => format!("{:02}{:02}", dt.hour(), dt.minute()),
    };
    Some((date, time))
}

/// Join a (possibly empty) base key prefix with a time prefix.
fn join_prefix(base: &str, tail: &str) -> String {
    if base.is_empty() {
        tail.to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), tail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_service_uses_only_the_resolved_layouts() {
        let from = OffsetDateTime::from_unix_timestamp(1_781_032_200).unwrap(); // 19:10
        let to = from.unix_timestamp() + 2 * 60; // 19:12
        let days = vec![DayLayout {
            date: "2026-06-09".to_string(),
            from: from.unix_timestamp(),
            to,
            layout: LayoutSet::Both,
        }];
        let (prefixes, truncated) =
            service_time_prefixes("traces", &days, "payments/api", Granularity::Hour);
        assert!(!truncated);
        assert_eq!(
            prefixes
                .iter()
                .map(|prefix| prefix.key.as_str())
                .collect::<Vec<_>>(),
            vec![
                "traces/2026-06-09/1910/payments/api/",
                "traces/2026-06-09/1911/payments/api/",
                "traces/2026-06-09/1912/payments/api/",
                "traces/version=1/date=2026-06-09/service=payments%2Fapi/time=19",
            ]
        );
    }

    #[test]
    fn crossover_24_hour_service_range_stays_below_the_prefix_cap() {
        let from = 1_781_049_600; // 2026-06-10T00:00:00Z
        let to = from + 24 * 3600 - 1;
        let days = vec![DayLayout {
            date: "2026-06-10".to_string(),
            from,
            to,
            layout: LayoutSet::Both,
        }];
        let (prefixes, truncated) = service_time_prefixes("", &days, "api", Granularity::Hour);
        assert!(!truncated);
        assert_eq!(prefixes.len(), 1440 + 24);
        assert!(prefixes.len() < MAX_PREFIXES);
    }

    #[test]
    fn unscoped_listing_discovers_v1_services_explicitly() {
        let from = 1_781_032_200;
        let days = crate::source_layout::day_layouts(from, from + 60, LayoutSet::Both).unwrap();
        let version1 = Version1Services::default()
            .with_scanned_days(days)
            .with_service_on("payments/api", "2026-06-09");
        let (prefixes, truncated) = unscoped_time_prefixes("", Granularity::Minute, &version1);
        assert!(!truncated);
        assert_eq!(
            prefixes
                .iter()
                .map(|prefix| prefix.key.as_str())
                .collect::<Vec<_>>(),
            vec![
                "2026-06-09/1910",
                "2026-06-09/1911",
                "version=1/date=2026-06-09/service=payments%2Fapi/time=1910",
                "version=1/date=2026-06-09/service=payments%2Fapi/time=1911",
            ]
        );
    }

    #[test]
    fn scope_prefix_already_under_default_is_not_duplicated() {
        assert_eq!(resolve_base(Some("traces"), Some("traces")), "traces");
        assert_eq!(
            resolve_base(Some("traces"), Some("traces/team-a")),
            "traces/team-a"
        );
        assert_eq!(
            resolve_base(Some("traces"), Some("team-a")),
            "traces/team-a"
        );
    }

    #[test]
    fn service_normalization_trims_and_treats_empty_as_absent() {
        assert_eq!(normalize_service(None), None);
        assert_eq!(normalize_service(Some("")), None);
        assert_eq!(normalize_service(Some(" \t ")), None);
        assert_eq!(normalize_service(Some(" api ")), Some("api"));
    }

    #[test]
    fn service_normalization_accepts_slashes() {
        assert_eq!(normalize_service(Some("api/worker")), Some("api/worker"));
    }

    #[test]
    fn known_layout_service_is_exact() {
        assert_eq!(
            key_service("root/2026-06-09/1910/api/host/boot/1-0.bin.gz"),
            Some("api".to_string())
        );
        assert_eq!(
            key_service("root/2026-06-09/1910/api-worker/host/boot/1-0.bin.gz"),
            Some("api-worker".to_string())
        );
        assert_eq!(
            key_service("root/2026-06-09/1910/api/host/group/boot/1-0.bin.gz"),
            Some("api".to_string())
        );
        assert_eq!(key_service("boot/trace.0.bin"), None);
    }

    #[test]
    fn known_layout_host_is_exact() {
        assert_eq!(
            key_host("root/2026-06-09/1910/api/host-a/boot/1-0.bin.gz"),
            Some("host-a".to_string())
        );
        assert_eq!(
            key_host(
                "root/version=1/date=2026-06-09/service=api%2Fworker/time=1910/instance=host%2Fa/boot=boot/1-0.bin.gz"
            ),
            Some("host/a".to_string())
        );
        assert_eq!(
            key_host("root/2026-06-09/1910/api/host/group/boot/1-0.bin.gz"),
            Some("host".to_string())
        );
        assert_eq!(key_host("boot/trace.0.bin"), None);
    }

    /// An empty base adds no leading slash.
    #[test]
    fn empty_base_has_no_leading_slash() {
        let (prefixes, _) = historical_minute_prefixes("", 1_781_032_200, 1_781_032_200);
        assert_eq!(prefixes, vec!["2026-06-09/1910"]);
    }

    /// A range too wide for the prefix cap is reported truncated.
    #[test]
    fn oversized_range_truncates() {
        let (prefixes, truncated) = historical_minute_prefixes("", 0, MAX_PREFIXES as i64 * 60 * 2);
        assert!(truncated);
        assert_eq!(prefixes.len(), MAX_PREFIXES);
    }
}

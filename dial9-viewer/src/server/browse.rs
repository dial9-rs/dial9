//! `browse` finds all trace files for a given timerange / filter set
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::server::error::storage_error_response;
use crate::storage::{ObjectInfo, StorageError};

/// Per-prefix object cap. A 10-minute (or minute) prefix can legitimately fan
/// out across many hosts; 10k absorbs a very busy bucket while still bounding
/// the response. When a single prefix exceeds this, the result is reported as
/// truncated so the UI warns rather than silently showing partial data.
const PER_PREFIX_CAP: usize = 10_000;

/// Bound on how many time prefixes a single browse request may fan out to. At
/// 10-minute granularity this is ~13 days; a wider range is reported as
/// truncated rather than launching an unbounded number of S3 list calls.
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
    /// Optional key prefix (the portion before the date), e.g. `traces`. When
    /// omitted the server's default prefix (if any) is used.
    pub prefix: Option<String>,
    /// Inclusive start of the window, unix seconds.
    pub from: i64,
    /// Inclusive end of the window, unix seconds.
    pub to: i64,
}

#[derive(Serialize)]
pub struct BrowseResponse {
    pub objects: Vec<ObjectInfo>,
    /// True if any list was truncated — a prefix exceeded [`PER_PREFIX_CAP`], or
    /// the range exceeded [`MAX_PREFIXES`]. The UI shows a warning so the user
    /// knows some traces may be missing.
    pub truncated: bool,
}

/// Granularity of the time prefixes scanned in S3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Granularity {
    /// `{date}/{HH}` — a full hour.
    Hour,
    /// `{date}/{HH}{minute/10}` — a 10-minute bucket (matches `HHM0`..=`HHM9`).
    #[cfg_attr(not(test), allow(dead_code))]
    TenMinute,
    /// `{date}/{HHMM}` — a single minute.
    Minute,
}

pub async fn browse(
    State(state): State<AppState>,
    creds: MaybeCreds,
    Query(params): Query<BrowseParams>,
) -> Result<Json<BrowseResponse>, (StatusCode, String)> {
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
    let base = match (&state.default_prefix, key_prefix) {
        (Some(pfx), Some(kp)) => format!("{}/{}", pfx.trim_end_matches('/'), kp),
        (Some(pfx), None) => pfx.clone(),
        (None, Some(kp)) => kp.to_string(),
        (None, None) => String::new(),
    };

    let window = params.to - params.from;
    let gran = if window < MINUTE_GRANULARITY_THRESHOLD_SECS {
        Granularity::Minute
    } else {
        Granularity::Hour
    };

    let (prefixes, range_truncated) = time_prefixes(&base, params.from, params.to, gran);

    tracing::info!(
        bucket = %bucket,
        prefixes = prefixes.len(),
        granularity = ?gran,
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
            .map(|p| {
                let backend = backend.clone();
                let bucket = bucket.clone();
                async move { backend.list_objects(&bucket, &p, PER_PREFIX_CAP).await }
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
        if page.truncated && gran == Granularity::Hour {
            overflow_prefixes.push(prefixes[i].clone());
        } else {
            truncated |= page.truncated;
            objects.extend(page.objects);
        }
    }

    // Retry overflowed hour-level prefixes at 10-minute granularity.
    if !overflow_prefixes.is_empty() {
        // Expand each overflowed hour prefix into its 6 ten-minute sub-prefixes.
        let refined: Vec<String> = overflow_prefixes
            .iter()
            .flat_map(|p| (0..6).map(move |d| format!("{p}{d}")))
            .collect();

        tracing::info!(
            refined_prefixes = refined.len(),
            overflowed_hours = overflow_prefixes.len(),
            "browse refining overflowed hours at 10-minute granularity"
        );

        let refined_results: Vec<Result<crate::storage::ListPage, StorageError>> =
            futures::stream::iter(refined)
                .map(|p| {
                    let backend = backend.clone();
                    let bucket = bucket.clone();
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

    Ok(Json(BrowseResponse { objects, truncated }))
}

/// Build the date+time S3 key prefixes covering `[from, to]` (unix seconds),
/// one per time bucket, each joined onto `base`.
///
/// Keys are laid out as `{base}/{YYYY-MM-DD}/{HHMM}/…` in UTC. A 10-minute
/// bucket is the 3-char prefix `{HH}{minute/10}`; a minute bucket is the full
/// 4-char `HHMM`. Returns the prefixes and whether the range was clamped at
/// [`MAX_PREFIXES`].
fn time_prefixes(base: &str, from: i64, to: i64, gran: Granularity) -> (Vec<String>, bool) {
    let step = match gran {
        Granularity::Hour => 3600,
        Granularity::TenMinute => 600,
        Granularity::Minute => 60,
    };
    // Align the start down to the bucket boundary. Epoch 0 is midnight UTC and
    // both 600 and 60 divide the day evenly, so floored alignment (rem_euclid,
    // correct even for pre-1970 inputs) lands exactly on a wall-clock boundary.
    let start = from - from.rem_euclid(step);

    let mut prefixes = Vec::new();
    let mut truncated = false;
    let mut t = start;
    while t <= to {
        if prefixes.len() >= MAX_PREFIXES {
            truncated = true;
            break;
        }
        if let Some(p) = bucket_prefix(t, gran) {
            prefixes.push(join_prefix(base, &p));
        }
        t += step;
    }
    (prefixes, truncated)
}

/// Format the `{YYYY-MM-DD}/{time}` prefix for the bucket containing `epoch`.
fn bucket_prefix(epoch: i64, gran: Granularity) -> Option<String> {
    let dt = OffsetDateTime::from_unix_timestamp(epoch).ok()?;
    let date = format!(
        "{:04}-{:02}-{:02}",
        dt.year(),
        u8::from(dt.month()),
        dt.day()
    );
    let time = match gran {
        Granularity::Hour => format!("{:02}", dt.hour()),
        // Minute is always a multiple of 10 after alignment, so `minute / 10`
        // is the single tens digit (0..=5): e.g. 19:10 → `191`, 19:50 → `195`.
        Granularity::TenMinute => format!("{:02}{}", dt.hour(), dt.minute() / 10),
        Granularity::Minute => format!("{:02}{:02}", dt.hour(), dt.minute()),
    };
    Some(format!("{date}/{time}"))
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

    /// 19:10–19:35 on 2026-06-09 UTC, 10-minute granularity, should produce the
    /// three 3-char buckets that cover it: `191`, `192`, `193`.
    #[test]
    fn ten_minute_prefixes_cover_range() {
        // 2026-06-09T19:10:00Z and 2026-06-09T19:35:00Z.
        let from = OffsetDateTime::from_unix_timestamp(1_781_032_200).unwrap();
        assert_eq!(from.hour(), 19);
        assert_eq!(from.minute(), 10);
        let to = from.unix_timestamp() + 25 * 60;
        let (prefixes, truncated) =
            time_prefixes("traces", from.unix_timestamp(), to, Granularity::TenMinute);
        assert!(!truncated);
        assert_eq!(
            prefixes,
            vec![
                "traces/2026-06-09/191",
                "traces/2026-06-09/192",
                "traces/2026-06-09/193",
            ]
        );
    }

    /// Hour granularity emits the 2-char `HH` prefix per hour.
    #[test]
    fn hour_prefixes_cover_range() {
        let from = OffsetDateTime::from_unix_timestamp(1_781_032_200).unwrap(); // 19:10
        let to = from.unix_timestamp() + 3 * 3600; // 22:10
        let (prefixes, truncated) =
            time_prefixes("traces", from.unix_timestamp(), to, Granularity::Hour);
        assert!(!truncated);
        assert_eq!(
            prefixes,
            vec![
                "traces/2026-06-09/19",
                "traces/2026-06-09/20",
                "traces/2026-06-09/21",
                "traces/2026-06-09/22",
            ]
        );
    }

    /// A start that is not on a 10-minute boundary must align *down* so the
    /// bucket containing it is included.
    #[test]
    fn unaligned_start_aligns_down() {
        // 19:17:00Z — should still emit the `191` bucket (covers 19:10–19:19).
        let base = OffsetDateTime::from_unix_timestamp(1_781_032_200).unwrap(); // 19:10
        let from = base.unix_timestamp() + 7 * 60; // 19:17
        let (prefixes, _) = time_prefixes("", from, from + 60, Granularity::TenMinute);
        assert_eq!(prefixes, vec!["2026-06-09/191"]);
    }

    /// Minute granularity emits the full 4-char `HHMM` per minute.
    #[test]
    fn minute_prefixes_are_four_char() {
        let from = OffsetDateTime::from_unix_timestamp(1_781_032_200).unwrap(); // 19:10
        let to = from.unix_timestamp() + 2 * 60; // 19:12
        let (prefixes, _) = time_prefixes("p", from.unix_timestamp(), to, Granularity::Minute);
        assert_eq!(
            prefixes,
            vec![
                "p/2026-06-09/1910",
                "p/2026-06-09/1911",
                "p/2026-06-09/1912"
            ]
        );
    }

    /// Empty base prefix yields a bare `{date}/{time}` prefix (no leading slash).
    #[test]
    fn empty_base_has_no_leading_slash() {
        let (prefixes, _) = time_prefixes("", 1_781_032_200, 1_781_032_200, Granularity::TenMinute);
        assert_eq!(prefixes, vec!["2026-06-09/191"]);
    }

    /// A range too wide for the prefix cap is reported truncated.
    #[test]
    fn oversized_range_truncates() {
        let (prefixes, truncated) =
            time_prefixes("", 0, MAX_PREFIXES as i64 * 600 * 2, Granularity::TenMinute);
        assert!(truncated);
        assert_eq!(prefixes.len(), MAX_PREFIXES);
    }
}

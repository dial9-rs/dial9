//! The demand-driven **refinement loop** — the orchestration layer over the
//! [`aggregate`] kit of parts.
//!
//! A query [resolves](resolve) a scope to an ordered, capped set of source
//! files, then [streams folds](fold_stream) of the not-yet-folded ones
//! (idempotently), one file at a time, so the caller can emit an incremental
//! update as each file lands. This is the algorithm shared by every
//! demand-driven endpoint (`/api/flamegraph`, `/api/tokio-stats`): they differ
//! only in which part-files they read back and how they shape the response.
//!
//! The control flow is **stream-driven**: a single held-open request resolves
//! the scope once, emits the already-folded snapshot instantly, then folds up to
//! the [sampling cap](sampling_cap) concurrently and pushes a fresh snapshot as
//! each file completes, closing when the cap is reached. Folding stops when the
//! client disconnects (the fold [`JoinSet`] is dropped, cancelling in-flight
//! folds) and re-folding is safe by idempotency. See `CONTEXT.md` ("Refinement
//! loop") for the vocabulary.

use std::collections::HashSet;
use std::sync::Arc;

use dial9_core::rate_limited;
use futures::stream::{Stream, StreamExt};
use tokio::task::JoinSet;

use crate::ingest::aggregate::{self, AggContext, FoldLimits, Scope};
use crate::storage::ObjectInfo;

/// Floor on the [sampling cap](sampling_cap): a scope always folds at least this
/// many files (when it has that many), so even a tiny scope produces a
/// non-trivial tree. Small because each source file is ~37–50 MB; the coverage
/// label, not a large floor, is what keeps users from over-trusting an early tree.
const BASELINE_FILES: usize = 4;

/// Max source-prefix LISTs issued concurrently. A wide time window expands to
/// one prefix per hour (up to 72), and each LIST is a separate round-trip; run
/// them in parallel so listing latency is bounded by the slowest prefix, not
/// their sum.
const LIST_CONCURRENCY: usize = 24;

/// Default sampling cap: stop folding a scope's tail at
/// `min(CAP_FRACTION × files_matched, CAP_MAX_FILES)`. Tuned low so a scope
/// plateaus quickly; "Fetch more" raises it on demand for deeper sampling.
const CAP_FRACTION: f64 = 0.05;
const CAP_MAX_FILES: usize = 100;

/// Hard ceiling on a client-supplied `max_files` ("Fetch more") override. The
/// override is otherwise unbounded by the request, so without this an arbitrary
/// value could drive a single scope to fold thousands of ~37–50 MB files. The
/// "Fetch more" button steps up gradually, so this is reached only by a crafted
/// request; it bounds the worst case rather than the normal flow.
const CAP_MAX_FILES_OVERRIDE: usize = 2000;

/// Per-request inputs to [`resolve`] that are not derived from the [`Scope`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RefineOpts {
    /// "Fetch more": raise the absolute sampling-cap ceiling for this scope.
    /// `None` uses the default cap.
    pub max_files: Option<usize>,
}

/// What [`resolve`] produced: the capped, ordered prefix of source files for the
/// scope and the set of those already folded (at resolve time), plus the
/// coverage denominators.
///
/// The caller reads its own part-files over the capped set — filtering to the
/// folded ones via [`Resolved::is_folded`] — and shapes the response. Reads and
/// aggregation MUST be scoped to [`capped`](Self::capped), never the whole
/// matched set: counting or aggregating folded files *outside* the cap would let
/// them inflate the numerator and starve the fold budget, permanently stalling
/// refinement well below the cap.
pub(crate) struct Resolved {
    /// The capped prefix of matched source files in [order key] order, as
    /// `(raw_key, full_key)` pairs. `raw_key` is the listed object key; `full_key`
    /// is the bucket-qualified key used for part-file addressing.
    ///
    /// [order key]: aggregate::order_key
    pub capped: Vec<(String, String)>,
    /// The folded-set leaves as of resolve time. Test membership with
    /// [`Resolved::is_folded`]. [`fold_stream`] extends this as files fold.
    folded: HashSet<String>,
    /// Total source files matching the scope (the coverage denominator).
    pub files_matched: usize,
    /// Total bytes of the matched set (for the coverage block).
    pub total_bytes: u64,
    /// Distinct hosts across the full matched set (the scope's fleet breadth, for
    /// the coverage block's fleet-representativeness badge).
    pub hosts_matched: usize,
}

impl Resolved {
    /// Whether the part-files for `full_key` have been folded.
    fn is_folded(&self, full_key: &str) -> bool {
        self.folded.contains(&aggregate::part_leaf_of(full_key))
    }

    /// The folded set, for handing to [`aggregate::fetch_folded_sample_parts`] /
    /// [`aggregate::read_polls_parts`].
    pub fn folded(&self) -> &HashSet<String> {
        &self.folded
    }

    /// The capped full-keys (the aggregation/read scope).
    pub fn capped_full_keys(&self) -> Vec<String> {
        self.capped.iter().map(|(_, full)| full.clone()).collect()
    }

    /// How many capped files have a leaf in `folded` — the coverage numerator.
    /// The streaming loop passes a growing superset of [`folded`](Self::folded)
    /// as files fold; pass `self.folded()` for the resolve-time count.
    pub fn files_folded_in(&self, folded: &HashSet<String>) -> usize {
        self.capped
            .iter()
            .filter(|(_, full)| folded.contains(&aggregate::part_leaf_of(full)))
            .count()
    }

    /// Distinct hosts among the capped files whose leaf is in `folded` — how much
    /// of the scope's fleet breadth the current sample spans.
    pub fn folded_hosts(&self, folded: &HashSet<String>) -> usize {
        self.capped
            .iter()
            .filter(|(_, full)| folded.contains(&aggregate::part_leaf_of(full)))
            .map(|(_, full)| aggregate::host_of(full))
            .collect::<HashSet<_>>()
            .len()
    }

    /// The capped files not yet folded, in order — the streaming fold work-list,
    /// bounded by the cap. `(raw_key, full_key)` pairs.
    pub fn unfolded_capped(&self) -> Vec<(String, String)> {
        self.capped
            .iter()
            .filter(|(_, full)| !self.is_folded(full))
            .cloned()
            .collect()
    }
}

/// One folded file's identity, pushed onto the [`fold_stream`] as it lands so the
/// caller can read the file's part-files and merge them incrementally.
pub(crate) struct Folded {
    /// The listed object key (for fetching the source / labelling).
    pub raw_key: String,
    /// The bucket-qualified key used for part-file addressing.
    pub full_key: String,
}

/// The outcome of attempting to fold one source file, yielded by [`fold_stream`]
/// so the caller sees failures as well as successes. Previously failures were
/// only logged and silently dropped, which made a systematic fold failure (e.g.
/// an unwritable output bucket → `PutObject` AccessDenied) look identical to
/// "no data" in the UI. Surfacing them lets the endpoint report a fold-error
/// count + a sample message to the client.
pub(crate) enum FoldOutcome {
    /// The file folded; read its part-files and merge them.
    Folded(Folded),
    /// The fold failed (fetch/decode/write). The stream continues — the file
    /// stays unfolded for a later request to retry — but the caller counts it.
    Failed {
        /// The listed object key that failed (for labelling / logs).
        raw_key: String,
        /// The error, stringified (may be large; the caller truncates for display).
        error: String,
    },
}

/// Longest fold-error message put on the wire — enough to see the S3 error code /
/// reason without shipping the whole SDK debug dump on every SSE event.
const FOLD_ERROR_MAX_LEN: usize = 300;

/// Running tally of [`FoldOutcome::Failed`] outcomes for one stream: a count plus
/// the most recent message (truncated for the wire). Feeds the `fold_errors` /
/// `fold_error_sample` fields of the [`Coverage`](aggregate::Coverage) block, so
/// a systematic fold failure surfaces in the UI instead of looking like no data.
/// Shared by the flamegraph and tokio-stats streams.
#[derive(Default)]
pub(crate) struct FoldErrors {
    pub count: usize,
    pub sample: Option<String>,
}

impl FoldErrors {
    /// Record one failure, keeping the most recent (truncated) message as the
    /// representative sample: `{filename}: {error}`, capped at
    /// [`FOLD_ERROR_MAX_LEN`] because the full SDK error can be kilobytes and it
    /// rides on every subsequent SSE event.
    pub fn record(&mut self, raw_key: &str, error: &str) {
        self.count += 1;
        let key = raw_key.rsplit('/').next().unwrap_or(raw_key);
        let msg = if key.is_empty() {
            error.to_string()
        } else {
            format!("{key}: {error}")
        };
        self.sample = Some(if msg.chars().count() > FOLD_ERROR_MAX_LEN {
            let truncated: String = msg.chars().take(FOLD_ERROR_MAX_LEN).collect();
            format!("{truncated}…")
        } else {
            msg
        });
    }
}

/// Resolve `scope` to the capped, ordered set of source files and the subset
/// already folded — WITHOUT folding anything.
///
/// 1. List the source scope → filter + order by [order key] → the **matched
///    set** (coverage denominator, fold order). Listing is scoped to
///    time-derived prefixes so a wide bucket isn't fully enumerated.
/// 2. Take the first [`sampling_cap`] files as the **capped prefix**: the
///    representative sample the query folds into and reads over. Reads,
///    coverage counting, and the fold work-list are ALL scoped to this prefix —
///    counting folded files across the whole matched set would let folded files
///    *outside* the cap inflate the count and starve the budget, permanently
///    stalling refinement well below the cap.
/// 3. List the **folded set** (the output `samples/` listing — the record of
///    what's already folded; see ADR-0003).
///
/// Returns `None` when no source file matches the scope (the caller maps this to
/// 404). Actual folding is driven separately by [`fold_stream`].
pub(crate) async fn resolve(agg: &AggContext, scope: &Scope, opts: RefineOpts) -> Option<Resolved> {
    // 1. Matched set: list source objects scoped to the time range. When we have
    //    a time range, generate date/hour prefixes to avoid listing the entire
    //    bucket (which can be 100k+ objects).
    let listing_prefixes = time_scoped_prefixes(&agg.source_prefixes, scope);
    tracing::info!(listing_prefixes = ?listing_prefixes, "resolve: listing prefixes");

    // List the prefixes concurrently: a wide window is many independent LISTs,
    // and serializing them is a major latency driver. Bounded by LIST_CONCURRENCY.
    let per_prefix: Vec<Vec<ObjectInfo>> = futures::stream::iter(listing_prefixes)
        .map(|prefix| async move {
            match agg
                .source
                .list_objects_all(&agg.source_bucket, &prefix)
                .await
            {
                Ok(objs) => {
                    tracing::info!(
                        %prefix,
                        listed = objs.len(),
                        sample_keys = ?objs.iter().take(3).map(|o| &o.key).collect::<Vec<_>>(),
                        "resolve: listed source prefix"
                    );
                    objs
                }
                Err(e) => {
                    tracing::warn!(%prefix, error = %e, "resolve: failed to list source prefix");
                    Vec::new()
                }
            }
        })
        .buffer_unordered(LIST_CONCURRENCY)
        .collect()
        .await;
    let raw_objects: Vec<ObjectInfo> = per_prefix.into_iter().flatten().collect();
    let total_listed = raw_objects.len();
    let (ordered, total_bytes) = aggregate::ordered_full_keys_with_size(
        raw_objects,
        scope,
        agg.segment_duration_secs,
        agg.source_is_local,
        &agg.source_bucket,
    );
    let files_matched = ordered.len();
    // Fleet breadth of the matched set: distinct hosts across every matched file
    // (not just the capped prefix), so coverage can report sample-vs-fleet spread.
    let hosts_matched = ordered
        .iter()
        .map(|(_, full)| aggregate::host_of(full))
        .collect::<HashSet<_>>()
        .len();
    tracing::info!(
        total_listed,
        files_matched,
        hosts_matched,
        sample_matched = ?ordered.iter().take(3).map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        "resolve: scope filter result"
    );
    if files_matched == 0 {
        return None;
    }

    // Sampling cap: never fold more than this many files for the scope.
    let cap = sampling_cap(files_matched, opts.max_files);
    let capped: Vec<(String, String)> = ordered.into_iter().take(cap).collect();

    // Folded set: the output samples/ listing, pruned to this source bucket.
    let folded = aggregate::list_folded_leaves(
        &*agg.output,
        &agg.output_bucket,
        &agg.output_prefix,
        &agg.source_bucket,
    )
    .await;

    Some(Resolved {
        capped,
        folded,
        files_matched,
        total_bytes,
        hosts_matched,
    })
}

/// Fold `to_fold` (the not-yet-folded capped files, in order) concurrently under
/// the process-global `limits`, yielding a [`FoldOutcome`] as each file completes
/// so the caller can merge successes and count failures.
///
/// Concurrency is bounded application-wide by [`FoldLimits`] (fetch + CPU
/// semaphores held in `AppState`), so this can spawn the whole work-list at once
/// without oversubscribing the box — excess folds simply wait on a permit.
/// Folding is best-effort: a file that fails to fold is reported as
/// [`FoldOutcome::Failed`] (it stays unfolded for a later request to retry)
/// rather than failing the stream.
///
/// The returned stream borrows nothing; dropping it (client disconnect) drops
/// the internal [`JoinSet`], cancelling any in-flight folds — folding stops when
/// the client stops listening.
pub(crate) fn fold_stream(
    agg: Arc<AggContext>,
    limits: FoldLimits,
    to_fold: Vec<(String, String)>,
) -> impl Stream<Item = FoldOutcome> {
    // Spawn one task per file up front; the `FoldLimits` semaphores throttle the
    // fetch and CPU stages, so the work-list runs at a bounded concurrency
    // regardless of its length. Each task returns the file's fold outcome.
    let mut tasks: JoinSet<FoldOutcome> = JoinSet::new();
    for (raw_key, full_key) in to_fold {
        let agg = Arc::clone(&agg);
        let limits = limits.clone();
        tasks.spawn(async move {
            match aggregate::fold_one(&agg, &raw_key, &limits).await {
                Ok(()) => FoldOutcome::Folded(Folded { raw_key, full_key }),
                Err(e) => {
                    // Still log (rate-limited) for server-side diagnostics; the
                    // outcome also flows to the client so the failure is visible.
                    rate_limited!(std::time::Duration::from_secs(60), {
                        tracing::warn!(key = %raw_key, error = %e, "fold_stream: failed to fold source file");
                    });
                    FoldOutcome::Failed {
                        raw_key,
                        error: e.to_string(),
                    }
                }
            }
        });
    }

    // Drain the JoinSet as a stream. `join_next` yields in completion order, so
    // faster folds surface first. A panicked/cancelled fold task (rare — the fold
    // body catches its own errors) is reported as a failure too.
    futures::stream::unfold(tasks, |mut tasks| async move {
        match tasks.join_next().await {
            Some(Ok(outcome)) => Some((outcome, tasks)),
            Some(Err(e)) => {
                rate_limited!(std::time::Duration::from_secs(60), {
                    tracing::warn!(error = %e, "fold_stream: fold task failed to join");
                });
                Some((
                    FoldOutcome::Failed {
                        raw_key: String::new(),
                        error: format!("fold task panicked: {e}"),
                    },
                    tasks,
                ))
            }
            None => None, // work-list drained
        }
    })
}

/// How many files a scope may fold before plateauing.
///
/// Default: `min(CAP_FRACTION × matched, CAP_MAX_FILES)` — the fraction keeps
/// small scopes sensible; the absolute ceiling stops a huge scope from chasing a
/// proportionally huge sample. "Fetch more" passes an explicit `max_files`
/// target that *replaces* the default cap for this scope (clamped to the matched
/// set and the [`CAP_MAX_FILES_OVERRIDE`] hard ceiling), letting the user
/// deliberately sample deeper than the default.
///
/// Either way the result is floored at the baseline (so even a tiny scope folds
/// a non-trivial sample) and clamped to the matched set (can't fold more files
/// than exist).
fn sampling_cap(files_matched: usize, max_files_override: Option<usize>) -> usize {
    let target = match max_files_override {
        Some(explicit) => explicit.min(CAP_MAX_FILES_OVERRIDE),
        None => {
            let by_fraction = (files_matched as f64 * CAP_FRACTION).ceil() as usize;
            by_fraction.min(CAP_MAX_FILES)
        }
    };
    target.max(BASELINE_FILES).min(files_matched)
}

/// Generate time-scoped listing prefixes from the scope's time range.
///
/// Key layout: `{base_prefix}/{YYYY-MM-DD}/{HHMM}/{service}/{host}/…`, where
/// `HHMM` is the segment's actual start time **down to the minute** (e.g.
/// `1940`), not rounded to the hour — the producer creates a fresh directory
/// every minute (`1930/`, `1931/`, …, `1939/`).
///
/// When the query window is narrow (≤ 2 hours), we emit **per-minute** prefixes
/// (e.g. `2026-06-22/1303`) padded by 2 minutes on each side. This avoids
/// listing thousands of files in the surrounding hours when only a handful
/// match. For wider windows (> 2 hours) we fall back to **per-hour** prefixes
/// (e.g. `2026-06-22/13`) padded by 1 hour, capped at 72 hours.
///
/// Service/host pruning happens in-memory in `scope_matches` — it can't go into
/// the prefix because both sit *after* `HHMM` in the key.
///
/// Falls back to the raw `source_prefixes` when no time range is given.
fn time_scoped_prefixes(source_prefixes: &[String], scope: &Scope) -> Vec<String> {
    let (Some(start_ns), Some(end_ns)) = (scope.start_ns, scope.end_ns) else {
        return source_prefixes.to_vec();
    };

    let start_secs = start_ns / 1_000_000_000;
    let end_secs = end_ns / 1_000_000_000;
    let span_secs = end_secs - start_secs;

    // Narrow window (≤ 2 hours): use minute-level prefixes with 2-minute padding.
    // This reduces a 1341-file listing to ~10 files for a single segment selection.
    const MINUTE_THRESHOLD_SECS: i64 = 2 * 3600;
    const MINUTE_PAD_SECS: i64 = 2 * 60;

    if span_secs <= MINUTE_THRESHOLD_SECS {
        let padded_start = (start_secs - MINUTE_PAD_SECS) / 60 * 60;
        let padded_end = (end_secs + MINUTE_PAD_SECS) / 60 * 60;

        let mut prefixes = Vec::new();
        for base in source_prefixes {
            let base_slash = if base.is_empty() {
                String::new()
            } else {
                format!("{}/", base.trim_end_matches('/'))
            };
            let mut t = padded_start;
            while t <= padded_end {
                let (date, hhmm) = epoch_to_date_hour(t);
                prefixes.push(format!("{base_slash}{date}/{hhmm}"));
                t += 60;
            }
        }
        prefixes
    } else {
        // Wide window: hour-level prefixes with 1-hour padding.
        let start_hour = (start_secs / 3600 - 1) * 3600;
        let end_hour = (end_secs / 3600 + 1) * 3600;
        let max_hours: i64 = 72;
        let end_hour = end_hour.min(start_hour + max_hours * 3600);

        let mut prefixes = Vec::new();
        for base in source_prefixes {
            let base_slash = if base.is_empty() {
                String::new()
            } else {
                format!("{}/", base.trim_end_matches('/'))
            };
            let mut t = start_hour;
            while t <= end_hour {
                let (date, hhmm) = epoch_to_date_hour(t);
                let hh = &hhmm[..2];
                prefixes.push(format!("{base_slash}{date}/{hh}"));
                t += 3600;
            }
        }
        prefixes
    }
}

/// Convert epoch seconds to ("YYYY-MM-DD", "HHMM") in UTC.
fn epoch_to_date_hour(epoch_secs: i64) -> (String, String) {
    // Days since Unix epoch via the civil-from-days algorithm.
    let secs = epoch_secs.rem_euclid(86400) as u32;
    let days = (epoch_secs - secs as i64).div_euclid(86400) as i32;

    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    let hh = secs / 3600;
    let mm = (secs % 3600) / 60;

    (format!("{y:04}-{m:02}-{d:02}"), format!("{hh:02}{mm:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_cap_floors_at_baseline_and_clamps_to_matched() {
        // Small scope: 5% rounds below the baseline floor, so the baseline wins.
        assert_eq!(sampling_cap(20, None), 4, "max(ceil(0.05*20), 4) = 4");
        // Larger scope: the 5% fraction exceeds the baseline.
        assert_eq!(sampling_cap(100, None), 5, "ceil(0.05*100) = 5");
        // The absolute ceiling caps a huge scope.
        assert_eq!(sampling_cap(100_000, None), CAP_MAX_FILES);
        // Can't fold more files than exist.
        assert_eq!(sampling_cap(3, None), 3, "clamped to matched set");
        // "Fetch more" replaces the default but is still floored + clamped.
        assert_eq!(sampling_cap(100, Some(40)), 40, "explicit override");
        assert_eq!(
            sampling_cap(100, Some(1)),
            4,
            "override still floored at baseline"
        );
        assert_eq!(
            sampling_cap(10, Some(50)),
            10,
            "override clamped to matched"
        );
        // A huge override is bounded by the hard ceiling, not the matched set.
        assert_eq!(
            sampling_cap(1_000_000, Some(999_999)),
            CAP_MAX_FILES_OVERRIDE,
            "override clamped to hard ceiling"
        );
    }

    #[test]
    fn epoch_to_date_hour_utc() {
        // 2026-06-19 13:00:00 UTC = 1781874000
        assert_eq!(
            epoch_to_date_hour(1781874000),
            ("2026-06-19".to_string(), "1300".to_string())
        );
        // 2026-01-01 00:00:00 UTC = 1767225600
        assert_eq!(
            epoch_to_date_hour(1767225600),
            ("2026-01-01".to_string(), "0000".to_string())
        );
    }

    #[test]
    fn time_scoped_prefixes_narrow_uses_minutes() {
        // 1-hour window (≤ 2h threshold) → minute-level prefixes with 2-min pad.
        let start_ns = 1781874000i64 * 1_000_000_000; // 2026-06-19 13:00 UTC
        let end_ns = 1781877600i64 * 1_000_000_000; // 2026-06-19 14:00 UTC
        let scope = Scope {
            start_ns: Some(start_ns),
            end_ns: Some(end_ns),
            service: None,
            hosts: vec![],
        };
        let prefixes = time_scoped_prefixes(&["traces".to_string()], &scope);
        // Minute-level: should contain exact minute prefixes
        assert!(prefixes.contains(&"traces/2026-06-19/1258".to_string())); // 2 min before
        assert!(prefixes.contains(&"traces/2026-06-19/1300".to_string()));
        assert!(prefixes.contains(&"traces/2026-06-19/1359".to_string()));
        assert!(prefixes.contains(&"traces/2026-06-19/1402".to_string())); // 2 min after
        // Should NOT contain bare hour prefixes
        assert!(!prefixes.iter().any(|p| p == "traces/2026-06-19/13"));
    }

    #[test]
    fn time_scoped_prefixes_wide_uses_hours() {
        // 3-hour window (> 2h threshold) → hour-level prefixes with 1-hr pad.
        let start_ns = 1781874000i64 * 1_000_000_000; // 2026-06-19 13:00 UTC
        let end_ns = 1781884800i64 * 1_000_000_000; // 2026-06-19 16:00 UTC (3 hours later)
        let scope = Scope {
            start_ns: Some(start_ns),
            end_ns: Some(end_ns),
            service: None,
            hosts: vec![],
        };
        let prefixes = time_scoped_prefixes(&["traces".to_string()], &scope);
        assert!(prefixes.contains(&"traces/2026-06-19/12".to_string()));
        assert!(prefixes.contains(&"traces/2026-06-19/13".to_string()));
        assert!(prefixes.contains(&"traces/2026-06-19/16".to_string()));
        assert!(prefixes.contains(&"traces/2026-06-19/17".to_string()));
    }

    #[test]
    fn time_scoped_prefixes_empty_base() {
        let start_ns = 1781874000i64 * 1_000_000_000; // 2026-06-19 13:00 UTC
        let end_ns = 1781877600i64 * 1_000_000_000; // 2026-06-19 14:00 UTC
        let scope = Scope {
            start_ns: Some(start_ns),
            end_ns: Some(end_ns),
            service: None,
            hosts: vec![],
        };
        let prefixes = time_scoped_prefixes(&["".to_string()], &scope);
        assert!(prefixes.contains(&"2026-06-19/1300".to_string()));
    }

    #[test]
    fn time_scoped_prefix_matches_per_minute_dir() {
        // A query for 13:00–14:00 must list a segment that landed in
        // the `1340/` per-minute directory.
        let start_ns = 1781874000i64 * 1_000_000_000; // 2026-06-19 13:00 UTC
        let end_ns = 1781877600i64 * 1_000_000_000; // 2026-06-19 14:00 UTC
        let scope = Scope {
            start_ns: Some(start_ns),
            end_ns: Some(end_ns),
            service: None,
            hosts: vec![],
        };
        let prefixes = time_scoped_prefixes(&["".to_string()], &scope);
        let real_key = "2026-06-19/1340/shale/host-a/boot-1/1781876400-0.bin.gz";
        assert!(
            prefixes.iter().any(|p| real_key.starts_with(p.as_str())),
            "no generated prefix is a prefix of {real_key}; prefixes = {prefixes:?}"
        );
    }

    #[test]
    fn time_scoped_single_segment_narrow() {
        // A single 60-second segment selection should produce ~5 minute prefixes
        // (the minute itself + 2 minutes padding on each side), not 3 full hours.
        let start_ns = 1782219780i64 * 1_000_000_000; // the bug scenario
        let end_ns = (1782219780i64 + 60) * 1_000_000_000;
        let scope = Scope {
            start_ns: Some(start_ns),
            end_ns: Some(end_ns),
            service: None,
            hosts: vec![],
        };
        let prefixes = time_scoped_prefixes(&["".to_string()], &scope);
        // Should be a small number of minute-level prefixes, not hundreds
        assert!(
            prefixes.len() <= 7,
            "expected ≤7 prefixes for 60s window, got {}",
            prefixes.len()
        );
        // Must match the actual file path from the bug report
        let real_key = "2026-06-23/1303/shale/ip-10-2-123-116.us-west-2.compute.internal/kxgw-1/1782219780-18603.bin.gz";
        assert!(
            prefixes.iter().any(|p| real_key.starts_with(p.as_str())),
            "no prefix matches {real_key}; prefixes = {prefixes:?}"
        );
    }

    #[test]
    fn time_scoped_no_time_range() {
        let scope = Scope {
            start_ns: None,
            end_ns: None,
            service: None,
            hosts: vec![],
        };
        let prefixes = time_scoped_prefixes(&["traces".to_string()], &scope);
        assert_eq!(prefixes, vec!["traces".to_string()]);
    }
}

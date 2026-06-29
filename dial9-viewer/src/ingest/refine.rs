//! The demand-driven **refinement loop** — the orchestration layer over the
//! [`aggregate`] kit of parts.
//!
//! A query poll resolves a scope to an ordered, capped set of source files,
//! folds a bounded budget of the not-yet-folded ones (idempotently), and reports
//! how much of the scope is now folded. This is the algorithm shared verbatim by
//! every demand-driven endpoint (`/api/flamegraph`, `/tokio-stats`): they differ
//! only in which part-files they read back and how they shape the response.
//!
//! The control flow is **stateless per-poll**: folding happens only during a
//! poll (so it stops when polling stops), re-folding is safe by idempotency, and
//! there is no background task or coordination. The client re-polls; coverage
//! climbs each poll until the [sampling cap](sampling_cap), then freezes. See
//! `CONTEXT.md` ("Refinement loop") for the vocabulary.

use std::collections::HashSet;
use std::sync::Arc;

use futures::stream::StreamExt;
use tokio::task::JoinSet;

use crate::ingest::aggregate::{self, AggContext, FoldLimits, Scope};
use crate::storage::ObjectInfo;

/// How many files the FIRST folding poll folds for a scope with nothing folded
/// yet (the baseline floor). Small because each source file is ~37–50 MB; the
/// coverage label, not a large floor, is what keeps users from over-trusting an
/// early tree. Note the first poll for a scope folds *nothing* (it returns the
/// already-folded set instantly); this baseline applies to the second poll on.
const BASELINE_FILES: usize = 4;

/// How many files each refinement poll folds, once past the baseline. Folded
/// concurrently under the process-global [`FoldLimits`], so this is the per-poll
/// batch size, not a serial cost. Bounds per-request work so a poll still
/// returns promptly.
const REFINE_BATCH_FILES: usize = 12;

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

/// Per-poll inputs to [`refine`] that are not derived from the [`Scope`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RefineOpts {
    /// Whether this poll may fold new source files. A read-only poll (`false`)
    /// returns whatever is already folded, instantly — the UI's first poll for a
    /// scope, so a bare reload shows the existing tree without folding. A
    /// refining poll (`true`) folds a bounded batch and the client re-polls.
    pub refine: bool,
    /// "Fetch more": raise the absolute sampling-cap ceiling for this scope.
    /// `None` uses the default cap.
    pub max_files: Option<usize>,
}

/// What a refinement poll resolved: the capped, ordered prefix of source files
/// for the scope and the set of those already folded (after this poll's
/// folding), plus the coverage denominators.
///
/// The caller reads its own part-files over the capped set — filtering to the
/// folded ones via [`Refined::is_folded`] — and shapes the response. Reads and
/// aggregation MUST be scoped to [`capped`](Self::capped), never the whole
/// matched set: counting or aggregating folded files *outside* the cap would let
/// them inflate the numerator and starve the fold budget, permanently stalling
/// refinement well below the cap.
pub(crate) struct Refined {
    /// The capped prefix of matched source files in [order key] order, as
    /// `(raw_key, full_key)` pairs. `raw_key` is the listed object key; `full_key`
    /// is the bucket-qualified key used for part-file addressing.
    ///
    /// [order key]: aggregate::order_key
    pub capped: Vec<(String, String)>,
    /// The folded-set leaves after this poll. Test membership with
    /// [`Refined::is_folded`].
    folded: HashSet<String>,
    /// Total source files matching the scope (the coverage denominator).
    pub files_matched: usize,
    /// Total bytes of the matched set (for the coverage block).
    pub total_bytes: u64,
    /// Distinct hosts across the full matched set (the scope's fleet breadth, for
    /// the coverage block's fleet-representativeness badge).
    pub hosts_matched: usize,
}

impl Refined {
    /// Whether the part-files for `full_key` have been folded.
    fn is_folded(&self, full_key: &str) -> bool {
        self.folded.contains(&aggregate::part_leaf_of(full_key))
    }

    /// The folded set, for handing to [`aggregate::aggregate`] /
    /// [`aggregate::read_polls_parts`].
    pub fn folded(&self) -> &HashSet<String> {
        &self.folded
    }

    /// The capped full-keys (the aggregation/read scope).
    pub fn capped_full_keys(&self) -> Vec<String> {
        self.capped.iter().map(|(_, full)| full.clone()).collect()
    }

    /// How many capped files are folded — the coverage numerator.
    pub fn files_folded(&self) -> usize {
        self.capped
            .iter()
            .filter(|(_, full)| self.is_folded(full))
            .count()
    }

    /// Distinct hosts among the folded capped files — how much of the scope's
    /// fleet breadth the current sample actually spans.
    pub fn hosts_folded(&self) -> usize {
        self.capped
            .iter()
            .filter(|(_, full)| self.is_folded(full))
            .map(|(_, full)| aggregate::host_of(full))
            .collect::<HashSet<_>>()
            .len()
    }
}

/// Run one demand-driven refinement poll for `scope`.
///
/// 1. List the source scope → filter + order by [order key] → the **matched
///    set** (coverage denominator, fold order). Listing is scoped to
///    time-derived prefixes so a wide bucket isn't fully enumerated.
/// 2. List the **folded set** (the output `samples/` listing — the record of
///    what's already folded; see ADR-0003).
/// 3. On a refining poll, fold a bounded budget of not-yet-folded matched files
///    (baseline on the first fold, a refine-batch later), concurrently under
///    `limits`, stopping at the [sampling cap](sampling_cap).
///
/// Returns `None` when no source file matches the scope (the caller maps this to
/// 404). Folding is best-effort: a file that fails to fold is logged and left
/// unfolded for a later poll to retry; it never fails the whole poll.
pub(crate) async fn refine(
    agg: &AggContext,
    scope: &Scope,
    opts: RefineOpts,
    limits: &FoldLimits,
) -> Option<Refined> {
    // 1. Matched set: list source objects scoped to the time range. When we have
    //    a time range, generate date/hour prefixes to avoid listing the entire
    //    bucket (which can be 100k+ objects).
    let listing_prefixes = time_scoped_prefixes(&agg.source_prefixes, scope);
    tracing::info!(listing_prefixes = ?listing_prefixes, "refine: listing prefixes");

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
                        "refine: listed source prefix"
                    );
                    objs
                }
                Err(e) => {
                    tracing::warn!(%prefix, error = %e, "refine: failed to list source prefix");
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
        "refine: scope filter result"
    );
    if files_matched == 0 {
        return None;
    }

    // Sampling cap: never fold more than this many files for the scope.
    let cap = sampling_cap(files_matched, opts.max_files);

    // 2. Folded set: the output samples/ listing, pruned to this source bucket.
    let mut folded = aggregate::list_folded_leaves(
        &*agg.output,
        &agg.output_bucket,
        &agg.output_prefix,
        &agg.source_bucket,
    )
    .await;

    // The capped prefix: the first `cap` files in order. This is the
    // representative sample we fold into and read over. Budgeting,
    // already-folded counting, and reads are ALL scoped to this prefix —
    // counting folded files across the whole matched set would let folded files
    // *outside* the cap inflate the count and starve the budget (room could go
    // to zero while the in-cap window is barely folded), permanently stalling
    // refinement well below the cap.
    let capped: Vec<(String, String)> = ordered.into_iter().take(cap).collect();

    // 3. Fold a bounded budget of not-yet-folded files within the capped prefix.
    let already_folded_in_cap = capped
        .iter()
        .filter(|(_, full)| folded.contains(&aggregate::part_leaf_of(full)))
        .count();

    if !opts.refine {
        // Read-only poll: return whatever is already folded, instantly.
        tracing::info!(
            files_matched,
            cap,
            already_folded = already_folded_in_cap,
            "refine: read-only poll, serving already-folded set"
        );
    } else {
        // Refining poll: fold a bounded batch of not-yet-folded files (in order),
        // concurrently. Baseline batch when nothing is folded yet, else the
        // refine batch — both clamped to the room left under the cap.
        let budget = if already_folded_in_cap == 0 {
            BASELINE_FILES
        } else {
            REFINE_BATCH_FILES
        };
        let room = cap.saturating_sub(already_folded_in_cap);
        let budget = budget.min(room);

        if budget > 0 {
            let to_fold: Vec<(String, String)> = capped
                .iter()
                .filter(|(_, full)| !folded.contains(&aggregate::part_leaf_of(full)))
                .take(budget)
                .cloned()
                .collect();
            tracing::info!(
                files_matched,
                cap,
                already_folded = already_folded_in_cap,
                folding_now = to_fold.len(),
                fetch_permits = limits.fetch.available_permits(),
                cpu_permits = limits.cpu.available_permits(),
                "refine: folding {} new file(s) this poll",
                to_fold.len()
            );

            // Spawn one task per file and let the process-global `FoldLimits`
            // semaphores bound concurrency, rather than a per-request
            // `buffer_unordered`. Tasks run on the multi-thread runtime (true
            // parallelism), the fetch stage runs at high concurrency, and the CPU
            // decode/encode stage is throttled — both bounded across all
            // concurrent requests, not just within this one poll. The batch is
            // already capped by `budget`, so we spawn a bounded set.
            //
            // Share the context and limits across tasks via `Arc` so each spawn
            // is a cheap refcount bump rather than a deep clone.
            let agg = Arc::new(agg.clone());
            let limits = limits.clone();
            let mut tasks: JoinSet<Option<String>> = JoinSet::new();
            for (raw_key, full_key) in to_fold {
                let agg = Arc::clone(&agg);
                let limits = limits.clone();
                tasks.spawn(async move {
                    match aggregate::fold_one(&agg, &raw_key, &limits).await {
                        Ok(()) => Some(aggregate::part_leaf_of(&full_key)),
                        Err(e) => {
                            tracing::warn!(key = %raw_key, error = %e, "refine: failed to fold source file");
                            None
                        }
                    }
                });
            }

            while let Some(joined) = tasks.join_next().await {
                match joined {
                    Ok(Some(leaf)) => {
                        folded.insert(leaf);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        // A fold task panicked or was cancelled. Log and skip;
                        // the file stays unfolded and a later poll retries.
                        tracing::warn!(error = %e, "refine: fold task failed to join");
                    }
                }
            }
        } else {
            tracing::info!(
                files_matched,
                cap,
                already_folded = already_folded_in_cap,
                "refine: at sampling cap, no new files to fold"
            );
        }
    }

    Some(Refined {
        capped,
        folded,
        files_matched,
        total_bytes,
        hosts_matched,
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
/// Either way the result is floored at the baseline (so the first poll can
/// always return a tree) and clamped to the matched set (can't fold more files
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

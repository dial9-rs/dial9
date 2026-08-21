//! The demand-driven **refinement loop** — the orchestration layer over the
//! [`aggregate`] kit of parts.
//!
//! A query [resolves](resolve) its full ordered source scope plus a capped prefix
//! for new work. Generic aggregate streams first reuse every folded matching
//! part, then [stream folds](fold_stream) for missing files inside that prefix
//! (idempotently), one file at a time, so callers can emit incremental updates.
//! Flamegraph and span-stats share this driver; Tokio stats retains a custom
//! capped-prefix reader for its endpoint-specific accumulator.
//!
//! The control flow is **stream-driven**: a single held-open request resolves
//! the scope once, streams all matching cached parts in bounded cumulative
//! snapshots, then folds missing capped-prefix files concurrently and pushes a
//! fresh snapshot as each file completes. Folding stops when the work-list
//! drains or the client disconnects (the fold [`JoinSet`] is dropped, cancelling
//! in-flight folds), and re-folding is safe by idempotency. See `CONTEXT.md` ("Refinement
//! loop") for the vocabulary.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dial9_core::rate_limited;
use futures::stream::{Stream, StreamExt};
use tokio::task::JoinSet;

use crate::ingest::aggregate::{self, AggContext, FoldLimits, Scope};
use crate::source_layout::{self, DayLayout, LayoutSet, Version1Services};
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
/// `min(CAP_FRACTION × files_matched, `[`default_cap_max`]`)`. Tuned low so a
/// scope plateaus quickly and returns a first tree fast; "Fetch more" raises it
/// on demand for deeper sampling.
const CAP_FRACTION: f64 = 0.05;

/// The default cap ceiling scales with the fold pipeline's concurrency so the
/// first fold saturates the box roughly once and returns immediately, rather
/// than folding a large fixed batch that hammers network egress — on a small
/// task, concurrent ~37–50 MB source GETs beyond what the egress can sustain
/// blow the 30s S3 operation timeout. The pipeline fetches at ~2× parallelism
/// (network-bound; see [`FoldLimits`]), so a ceiling near that folds about one
/// fetch wave. Because folds stream back per file, the user sees the first tree
/// as soon as one file lands regardless — a small default just stops the tail
/// early and leaves the rest to "Fetch more".
const CAP_FILES_PER_CORE: usize = 2;

/// Clamp the parallelism-derived default cap ([`default_cap_max`]) to a sane
/// range: never trivially small on a 1–2 core box, never unbounded on a
/// many-core one.
const CAP_MAX_FILES_MIN: usize = 8;
const CAP_MAX_FILES_MAX: usize = 100;

/// Parallelism-derived default ceiling for the [sampling cap](sampling_cap):
/// `(cores × `[`CAP_FILES_PER_CORE`]`)` clamped to
/// `[`[`CAP_MAX_FILES_MIN`]`, `[`CAP_MAX_FILES_MAX`]`]`. Falls back to 4 cores
/// when the platform can't report parallelism.
fn default_cap_max() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores * CAP_FILES_PER_CORE).clamp(CAP_MAX_FILES_MIN, CAP_MAX_FILES_MAX)
}

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

/// What [`resolve`] produced: the full ordered matched scope, its capped prefix
/// for bounded new-fold work, and the globally discovered folded leaves.
///
/// Cached reads and coverage use [`matched`](Self::matched): once a matching
/// source file has been folded, every compatible query benefits from it. New
/// fold work remains restricted to [`capped`](Self::capped), so cache reuse does
/// not weaken the request's fold-work bound or let out-of-cap cache entries
/// starve missing files inside the deterministic prefix.
pub(crate) struct Resolved {
    /// Every matched source file in stable [order key] order. Generic aggregate
    /// streams seed all folded entries from this scope progressively.
    ///
    /// [order key]: aggregate::order_key
    matched: Vec<(String, String)>,
    /// Part leaf to host for every matched source. Built once during resolve so
    /// each successful merge updates file/host coverage without rescanning and
    /// rehashing the whole matched scope on every SSE event.
    matched_leaf_hosts: HashMap<String, String>,
    /// The capped prefix used only as the bounded new-fold work scope.
    /// `(raw_key, full_key)` pairs.
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

    /// The folded set, for handing to readers such as
    /// [`aggregate::read_polls_parts`].
    pub fn folded(&self) -> &HashSet<String> {
        &self.folded
    }

    /// Already-folded full keys anywhere in the matched aggregation scope, in
    /// stable sampling order. The generic SSE driver consumes these in bounded
    /// batches, so all applicable cached data is reused without delaying the
    /// first useful snapshot.
    pub fn folded_matching_full_keys(&self) -> Vec<String> {
        self.matched
            .iter()
            .filter(|(_, full)| self.is_folded(full))
            .map(|(_, full)| full.clone())
            .collect()
    }

    /// Host for a matched part leaf, if the leaf belongs to this query scope.
    pub fn matched_host_for_leaf(&self, leaf: &str) -> Option<&str> {
        self.matched_leaf_hosts.get(leaf).map(String::as_str)
    }

    /// Number of files in the deterministic prefix eligible for new fold work.
    pub fn fold_work_cap(&self) -> usize {
        self.capped.len()
    }

    /// How many matched files have been successfully merged into this stream.
    pub fn files_folded_in(&self, folded: &HashSet<String>) -> usize {
        self.matched
            .iter()
            .filter(|(_, full)| folded.contains(&aggregate::part_leaf_of(full)))
            .count()
    }

    /// How many capped-prefix files have leaves in `folded`. The custom Tokio
    /// stats stream still aggregates only its capped prefix.
    pub fn capped_files_folded_in(&self, folded: &HashSet<String>) -> usize {
        self.capped
            .iter()
            .filter(|(_, full)| folded.contains(&aggregate::part_leaf_of(full)))
            .count()
    }

    /// Distinct hosts represented by folded leaves inside the capped prefix.
    pub fn capped_folded_hosts(&self, folded: &HashSet<String>) -> usize {
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

    #[cfg(test)]
    pub(crate) fn for_test(capped: Vec<(String, String)>, folded: HashSet<String>) -> Self {
        let files_matched = capped.len();
        let matched_leaf_hosts = capped
            .iter()
            .map(|(_, full)| (aggregate::part_leaf_of(full), aggregate::host_of(full)))
            .collect();
        Self {
            matched: capped.clone(),
            matched_leaf_hosts,
            capped,
            folded,
            files_matched,
            total_bytes: 0,
            hosts_matched: 0,
        }
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

/// Resolve `scope` to the full ordered matched set, its capped new-work prefix,
/// and the globally folded leaves — WITHOUT folding anything.
///
/// 1. List the source scope → filter + order by [order key] → the **matched
///    set** (coverage denominator and cached aggregation scope). Listing is
///    scoped to time-derived prefixes so a wide bucket isn't fully enumerated.
/// 2. Take the first [`sampling_cap`] files as the **capped prefix** used only
///    for bounded new-fold work. Folded matching files outside this prefix are
///    still reusable cached data, but they do not consume or starve the prefix's
///    missing-file work budget.
/// 3. List the **folded set** (the output `samples/` listing — the record of
///    what's already folded; see ADR-0003).
///
/// Returns `None` when no source file matches the scope (the caller maps this to
/// 404). Actual folding is driven separately by [`fold_stream`].
pub(crate) async fn resolve(agg: &AggContext, scope: &Scope, opts: RefineOpts) -> Option<Resolved> {
    // 1. Matched set: list source objects scoped to the time range. When we have
    //    a time range, generate date/hour prefixes to avoid listing the entire
    //    bucket (which can be 100k+ objects).
    let listing_prefixes = time_scoped_prefixes(agg, scope).await;
    tracing::info!(
        listing_prefix_count = listing_prefixes.len(),
        sample_listing_prefixes = ?listing_prefixes.iter().take(5).collect::<Vec<_>>(),
        "resolve: listing prefixes"
    );

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

    // Sampling cap: bound new fold work to this deterministic prefix. Cached
    // reads are intentionally broader and reuse every folded matching file.
    let cap = sampling_cap(files_matched, opts.max_files);
    let capped: Vec<(String, String)> = ordered.iter().take(cap).cloned().collect();

    // Folded set: the output samples/ listing, pruned to this source bucket.
    let folded = aggregate::list_folded_leaves(
        &*agg.output,
        &agg.output_bucket,
        &agg.output_prefix,
        &agg.source_bucket,
        scope.service.as_deref(),
    )
    .await;

    let matched_leaf_hosts = ordered
        .iter()
        .map(|(_, full)| (aggregate::part_leaf_of(full), aggregate::host_of(full)))
        .collect();

    Some(Resolved {
        matched: ordered,
        matched_leaf_hosts,
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
/// Default: `min(CAP_FRACTION × matched, `[`default_cap_max`]`)` — the fraction
/// keeps small scopes sensible; the parallelism-derived ceiling stops a huge
/// scope from chasing a proportionally huge sample and keeps the first fold to
/// about one fetch wave (so it returns fast without saturating egress). "Fetch
/// more" passes an explicit `max_files` target that *replaces* the default cap
/// for this scope (clamped to the matched set and the [`CAP_MAX_FILES_OVERRIDE`]
/// hard ceiling), letting the user deliberately sample deeper than the default.
///
/// Either way the result is floored at the baseline (so even a tiny scope folds
/// a non-trivial sample) and clamped to the matched set (can't fold more files
/// than exist).
fn sampling_cap(files_matched: usize, max_files_override: Option<usize>) -> usize {
    let target = match max_files_override {
        Some(explicit) => explicit.min(CAP_MAX_FILES_OVERRIDE),
        None => {
            let by_fraction = (files_matched as f64 * CAP_FRACTION).ceil() as usize;
            by_fraction.min(default_cap_max())
        }
    };
    target.max(BASELINE_FILES).min(files_matched)
}

#[derive(Clone, Copy)]
enum TimeGranularity {
    Hour,
    Minute,
}

struct ListingWindow {
    start: i64,
    end: i64,
    granularity: TimeGranularity,
}

/// Resolve the source layouts for an aggregation scope, then build the same
/// bounded historical/v1 listing strategy used by the Browser.
async fn time_scoped_prefixes(agg: &AggContext, scope: &Scope) -> Vec<String> {
    let Some(window) = listing_window(scope) else {
        return agg.source_prefixes.clone();
    };

    let mut prefixes = Vec::new();
    for base in &agg.source_prefixes {
        match scope.service.as_deref() {
            Some(service) => {
                let days = match source_layout::resolve_service_layouts(
                    &*agg.source,
                    &agg.source_bucket,
                    base,
                    window.start,
                    window.end,
                    service,
                    None,
                )
                .await
                {
                    Ok(resolved) => resolved.days,
                    Err(error) => {
                        tracing::warn!(%error, %service, %base, "failed to discover source layout; falling back to historical keys");
                        match source_layout::day_layouts(
                            window.start,
                            window.end,
                            LayoutSet::Historical,
                        ) {
                            Ok(days) => days,
                            Err(error) => {
                                tracing::warn!(%error, "failed to build historical source-key days");
                                Vec::new()
                            }
                        }
                    }
                };
                append_service_prefixes(base, service, &window, &days, &mut prefixes);
            }
            None => {
                let version1 = match source_layout::discover_version1_services(
                    &*agg.source,
                    &agg.source_bucket,
                    base,
                    window.start,
                    window.end,
                )
                .await
                {
                    Ok(discovery) => discovery,
                    Err(error) => {
                        tracing::warn!(%error, %base, "failed to discover versioned source layouts; falling back to historical keys");
                        Version1Services::default()
                    }
                };
                let days =
                    match source_layout::day_layouts(window.start, window.end, LayoutSet::Both) {
                        Ok(days) => days,
                        Err(error) => {
                            tracing::warn!(%error, "failed to build source-key days");
                            Vec::new()
                        }
                    };
                append_unscoped_prefixes(base, &window, &days, &version1, &mut prefixes);
            }
        }
    }
    prefixes
}

fn listing_window(scope: &Scope) -> Option<ListingWindow> {
    let (Some(start_ns), Some(end_ns)) = (scope.start_ns, scope.end_ns) else {
        return None;
    };

    let start_secs = start_ns.div_euclid(1_000_000_000);
    let end_secs = end_ns.div_euclid(1_000_000_000);
    let span_secs = end_secs - start_secs;

    // Narrow window (≤ 2 hours): use minute-level prefixes with 2-minute padding.
    // This reduces a 1341-file listing to ~10 files for a single segment selection.
    const MINUTE_THRESHOLD_SECS: i64 = 2 * 3600;
    const MINUTE_PAD_SECS: i64 = 2 * 60;
    const MAX_WINDOW_SECS: i64 = 72 * 3600;

    if span_secs <= MINUTE_THRESHOLD_SECS {
        let padded_start = start_secs.saturating_sub(MINUTE_PAD_SECS).div_euclid(60) * 60;
        let padded_end = end_secs
            .saturating_add(MINUTE_PAD_SECS)
            .div_euclid(60)
            .saturating_mul(60)
            .min(padded_start.saturating_add(MAX_WINDOW_SECS));
        Some(ListingWindow {
            start: padded_start,
            end: padded_end,
            granularity: TimeGranularity::Minute,
        })
    } else {
        let start_hour = start_secs
            .div_euclid(3600)
            .saturating_sub(1)
            .saturating_mul(3600);
        let end_hour = end_secs
            .div_euclid(3600)
            .saturating_add(1)
            .saturating_mul(3600)
            .min(start_hour.saturating_add(MAX_WINDOW_SECS));
        Some(ListingWindow {
            start: start_hour,
            end: end_hour,
            granularity: TimeGranularity::Hour,
        })
    }
}

fn append_service_prefixes(
    base: &str,
    service: &str,
    window: &ListingWindow,
    days: &[DayLayout],
    output: &mut Vec<String>,
) {
    let encoded_service = crate::segment_object_key_codec::hive_escape(service);
    for day in days {
        if day.layout.historical() {
            append_buckets(day.from, day.to, TimeGranularity::Minute, |date, time| {
                output.push(join_prefix(base, &format!("{date}/{time}/{service}/")));
            });
        }
        if day.layout.version1() {
            append_buckets(day.from, day.to, window.granularity, |date, time| {
                output.push(join_prefix(
                    base,
                    &format!("version=1/date={date}/service={encoded_service}/time={time}"),
                ));
            });
        }
    }
}

fn append_unscoped_prefixes(
    base: &str,
    window: &ListingWindow,
    days: &[DayLayout],
    version1: &Version1Services,
    output: &mut Vec<String>,
) {
    for day in days {
        append_buckets(day.from, day.to, window.granularity, |date, time| {
            output.push(join_prefix(base, &format!("{date}/{time}")));
        });
        for service in version1.services_on(&day.date) {
            let encoded_service = crate::segment_object_key_codec::hive_escape(service);
            append_buckets(day.from, day.to, window.granularity, |date, time| {
                output.push(join_prefix(
                    base,
                    &format!("version=1/date={date}/service={encoded_service}/time={time}"),
                ));
            });
        }
    }
}

fn append_buckets(
    from: i64,
    to: i64,
    granularity: TimeGranularity,
    mut append: impl FnMut(&str, &str),
) {
    let step = match granularity {
        TimeGranularity::Hour => 3600,
        TimeGranularity::Minute => 60,
    };
    let mut timestamp = from - from.rem_euclid(step);
    while timestamp <= to {
        let (date, hhmm) = epoch_to_date_hour(timestamp);
        let time = match granularity {
            TimeGranularity::Hour => &hhmm[..2],
            TimeGranularity::Minute => hhmm.as_str(),
        };
        append(&date, time);
        let Some(next) = timestamp.checked_add(step) else {
            break;
        };
        timestamp = next;
    }
}

fn join_prefix(base: &str, tail: &str) -> String {
    if base.is_empty() {
        tail.to_string()
    } else {
        format!("{}/{tail}", base.trim_end_matches('/'))
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
        // The parallelism-derived ceiling caps a huge scope. It's machine-
        // dependent (cores × 2, clamped), so assert against it rather than a
        // literal, and assert the clamp bounds directly.
        assert_eq!(sampling_cap(100_000, None), default_cap_max());
        let cap_max = default_cap_max();
        assert!(
            (CAP_MAX_FILES_MIN..=CAP_MAX_FILES_MAX).contains(&cap_max),
            "default cap ceiling stays within its clamp: {cap_max}"
        );
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
    fn folded_matching_keys_include_out_of_cap_cache_without_starving_work() {
        let full_a = "s3://bucket/2026-01-01/0000/svc/host-a/a.bin".to_string();
        let full_b = "s3://bucket/2026-01-01/0000/svc/host-b/b.bin".to_string();
        let full_c = "s3://bucket/2026-01-01/0000/svc/host-c/c.bin".to_string();
        let outside_cap = "s3://bucket/2026-01-01/0000/svc/host-d/d.bin".to_string();
        let matched = vec![
            ("raw-c".to_string(), full_c.clone()),
            ("raw-a".to_string(), full_a.clone()),
            ("raw-b".to_string(), full_b.clone()),
            ("raw-d".to_string(), outside_cap.clone()),
        ];
        let folded: HashSet<String> = [
            aggregate::part_leaf_of(&full_a),
            aggregate::part_leaf_of(&full_c),
            aggregate::part_leaf_of(&outside_cap),
        ]
        .into_iter()
        .collect();
        let matched_leaf_hosts = matched
            .iter()
            .map(|(_, full)| (aggregate::part_leaf_of(full), aggregate::host_of(full)))
            .collect();
        let resolved = Resolved {
            matched,
            matched_leaf_hosts,
            capped: vec![
                ("raw-c".to_string(), full_c.clone()),
                ("raw-a".to_string(), full_a.clone()),
                ("raw-b".to_string(), full_b.clone()),
            ],
            folded: folded.clone(),
            files_matched: 4,
            total_bytes: 0,
            hosts_matched: 4,
        };

        assert_eq!(
            resolved.folded_matching_full_keys(),
            vec![full_c, full_a, outside_cap],
            "all folded matching keys are seeded in stable order, even beyond the fold cap"
        );
        assert_eq!(resolved.files_folded_in(&folded), 3);
        assert_eq!(
            resolved.unfolded_capped(),
            vec![("raw-b".to_string(), full_b)],
            "out-of-cap cache does not consume or starve missing in-cap fold work"
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
    fn narrow_listing_window_uses_minutes_and_padding() {
        let start_ns = 1781874000i64 * 1_000_000_000; // 2026-06-19 13:00 UTC
        let end_ns = 1781877600i64 * 1_000_000_000; // 2026-06-19 14:00 UTC
        let scope = Scope {
            start_ns: Some(start_ns),
            end_ns: Some(end_ns),
            service: None,
            hosts: vec![],
        };
        let window = listing_window(&scope).unwrap();
        assert!(matches!(window.granularity, TimeGranularity::Minute));
        assert_eq!(window.start, 1781873880); // 12:58
        assert_eq!(window.end, 1781877720); // 14:02
    }

    #[test]
    fn service_prefixes_follow_the_migration_boundary() {
        let first = 1_787_270_400; // 2026-08-21T00:00:00Z
        let days = vec![
            DayLayout {
                date: "2026-08-21".to_string(),
                from: first,
                to: first + 3599,
                layout: LayoutSet::Historical,
            },
            DayLayout {
                date: "2026-08-22".to_string(),
                from: first + 86_400,
                to: first + 86_400 + 3599,
                layout: LayoutSet::Both,
            },
            DayLayout {
                date: "2026-08-23".to_string(),
                from: first + 2 * 86_400,
                to: first + 2 * 86_400 + 3599,
                layout: LayoutSet::Version1,
            },
        ];
        let window = ListingWindow {
            start: first,
            end: first + 2 * 86_400 + 3599,
            granularity: TimeGranularity::Hour,
        };
        let mut prefixes = Vec::new();
        append_service_prefixes("traces", "payments/api", &window, &days, &mut prefixes);

        assert!(prefixes.contains(&"traces/2026-08-21/0000/payments/api/".to_string()));
        assert!(
            !prefixes
                .iter()
                .any(|prefix| prefix.contains("date=2026-08-21"))
        );
        assert!(prefixes.contains(&"traces/2026-08-22/0000/payments/api/".to_string()));
        assert!(prefixes.contains(
            &"traces/version=1/date=2026-08-22/service=payments%2Fapi/time=00".to_string()
        ));
        assert!(prefixes.contains(
            &"traces/version=1/date=2026-08-23/service=payments%2Fapi/time=00".to_string()
        ));
        assert!(
            !prefixes
                .iter()
                .any(|prefix| prefix.starts_with("traces/2026-08-23/"))
        );
    }

    #[test]
    fn unscoped_prefixes_include_discovered_v1_services() {
        let window = ListingWindow {
            start: 1_781_032_200,
            end: 1_781_032_260,
            granularity: TimeGranularity::Minute,
        };
        let days = source_layout::day_layouts(window.start, window.end, LayoutSet::Both).unwrap();
        let version1 = Version1Services::default().with_service_on("payments/api", "2026-06-09");
        let mut prefixes = Vec::new();
        append_unscoped_prefixes("", &window, &days, &version1, &mut prefixes);
        assert!(prefixes.contains(&"2026-06-09/1910".to_string()));
        assert!(
            prefixes.contains(
                &"version=1/date=2026-06-09/service=payments%2Fapi/time=1910".to_string()
            )
        );
        assert!(prefixes.iter().all(|prefix| !prefix.starts_with('/')));
    }

    #[test]
    fn scope_without_time_has_no_listing_window() {
        let scope = Scope {
            start_ns: None,
            end_ns: None,
            service: None,
            hosts: vec![],
        };
        assert!(listing_window(&scope).is_none());
    }
}

//! `/api/flamegraph` endpoint: query aggregated Parquet data and return a flamegraph tree.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum_extra::extract::Query as QueryExtra;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::task::JoinSet;

use crate::ingest::aggregate::{
    self, AggContext, Coverage, FACETS, FacetResult, SampleFilter, Scope,
};
use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
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
///
/// [`FoldLimits`]: crate::ingest::aggregate::FoldLimits
const REFINE_BATCH_FILES: usize = 12;

/// Max source-prefix LISTs issued concurrently. A wide time window expands to
/// one prefix per hour (up to 72), and each LIST is a separate round-trip; run
/// them in parallel so listing latency is bounded by the slowest prefix, not
/// their sum. Higher than [`FOLD_CONCURRENCY`] because a LIST is cheap (no
/// large body, no decode) relative to a fold.
const LIST_CONCURRENCY: usize = 24;

/// Default sampling cap: stop folding a scope's tail at
/// `min(CAP_FRACTION × files_matched, CAP_MAX_FILES)`. Tuned low so a scope
/// plateaus quickly; "Fetch more" raises it on demand for deeper sampling.
const CAP_FRACTION: f64 = 0.05;
const CAP_MAX_FILES: usize = 100;

#[derive(Deserialize)]
pub struct FlamegraphParams {
    pub service: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Host filter. Repeatable (`host=a&host=b`) so a heatmap box spanning many
    /// hosts maps to a host set. Empty = all hosts.
    #[serde(default)]
    pub host: Vec<String>,
    /// Start timestamp in nanoseconds (inclusive)
    pub start_ns: Option<i64>,
    /// End timestamp in nanoseconds (inclusive)
    pub end_ns: Option<i64>,
    /// "Fetch more": raise the absolute sampling-cap ceiling for this scope.
    pub max_files: Option<usize>,
    /// S3 bucket override (used with bring-your-own-credentials).
    pub bucket: Option<String>,
    /// S3 key prefix for source segment listing (scopes the search).
    pub prefix: Option<String>,
    /// Worker-attribution filter: `"worker"` (on-runtime), `"off-worker"`
    /// (off-runtime), or empty/absent for all. Sent by the flamegraph UI's
    /// "Thread" selector.
    pub thread_class: Option<String>,
    /// Source filter: `"cpu"` (on-CPU profile, the default view), `"sched"`
    /// (scheduler context switches), or empty/absent for all. Sent by the
    /// flamegraph UI's "Source" selector.
    pub source: Option<String>,
    /// Spawn location filter: exact match on the task's spawn location string.
    /// Only samples attributed to a poll with this spawn location are counted.
    /// Sent by the flamegraph UI's "Spawn location" selector.
    pub spawn_location: Option<String>,
    /// Whether this poll may fold new source files. The UI's first poll for a
    /// scope omits this (or sets it false) so the response is instant — it just
    /// aggregates whatever is already folded. Subsequent polls set `refine=1` to
    /// fold a batch and progressively refine. Defaults to false so a bare reload
    /// of a previously-loaded scope returns its existing tree without folding.
    #[serde(default)]
    pub refine: bool,
}

#[derive(Serialize)]
pub struct FlamegraphResponse {
    pub tree: FlamegraphNode,
    pub total_samples: usize,
    /// Present in demand-driven mode: how much of the scope has been folded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
    pub metadata: FlamegraphMetadata,
}

#[derive(Serialize, Clone)]
pub struct FlamegraphNode {
    pub name: String,
    pub count: u64,
    #[serde(rename = "self")]
    pub self_count: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<FlamegraphNode>,
}

#[derive(Serialize)]
pub struct FlamegraphMetadata {
    pub service: Option<String>,
    pub hosts: usize,
    pub time_range: Option<String>,
    /// Min timestamp in the result (epoch nanoseconds)
    pub min_timestamp_ns: Option<i64>,
    /// Max timestamp in the result (epoch nanoseconds)
    pub max_timestamp_ns: Option<i64>,
    /// Generic facets array: each entry has name, label, and sorted values.
    /// The UI renders the toolbar entirely from this array.
    pub facets: Vec<FacetResult>,
    /// The resolved scope the server queried, echoed so the UI's header can
    /// render the current selection without re-deriving it from the URL.
    pub scope: ScopeEcho,
}

/// The resolved query scope echoed back to the UI (the selection the server
/// actually applied), so the header reflects backend truth rather than URL
/// params the client guessed at.
#[derive(Serialize)]
pub struct ScopeEcho {
    pub service: Option<String>,
    /// The host filter the query was scoped to (empty = all hosts).
    pub hosts: Vec<String>,
    pub start_ns: Option<i64>,
    pub end_ns: Option<i64>,
    /// Active facet filter values (facet name → selected value, empty = all).
    pub filters: HashMap<String, String>,
}

/// Build a flamegraph tree from (stack_id, count) pairs and a stacks dictionary.
fn build_flamegraph_tree(
    stack_counts: &[(Vec<u8>, u64)],
    stacks_dict: &HashMap<Vec<u8>, Vec<String>>,
) -> FlamegraphNode {
    let mut root = TrieNode::new("(all)".to_string());

    for (stack_id, count) in stack_counts {
        let frames = match stacks_dict.get(stack_id) {
            Some(f) => f,
            None => continue,
        };
        // Frames are stored leaf→root; flamegraph trie inserts root→leaf
        root.count += count;
        let mut node = &mut root;
        for frame in frames.iter().rev() {
            node = node.get_or_insert_child(frame.clone());
            node.count += count;
        }
        // Leaf gets self-time
        node.self_count += count;
    }

    root.into_response()
}

struct TrieNode {
    name: String,
    count: u64,
    self_count: u64,
    children: HashMap<String, TrieNode>,
}

impl TrieNode {
    fn new(name: String) -> Self {
        Self {
            name,
            count: 0,
            self_count: 0,
            children: HashMap::new(),
        }
    }

    fn get_or_insert_child(&mut self, name: String) -> &mut TrieNode {
        self.children
            .entry(name.clone())
            .or_insert_with(|| TrieNode::new(name))
    }

    fn into_response(self) -> FlamegraphNode {
        let mut children: Vec<_> = self
            .children
            .into_values()
            .map(|c| c.into_response())
            .collect();
        // Sort children by count descending for consistent output
        children.sort_by_key(|c| std::cmp::Reverse(c.count));

        FlamegraphNode {
            name: self.name,
            count: self.count,
            self_count: self.self_count,
            children,
        }
    }
}

/// Handler for GET /api/flamegraph.
///
/// When the server is configured for demand-driven aggregation (`AppState.agg`
/// is set), this runs the [refinement loop](run_refinement_loop): fold a
/// bounded budget of source files in [order key] order, aggregate over the
/// folded-in-scope set, and return the tree plus a [`Coverage`] block. The
/// client re-polls to refine.
///
/// When bring-your-own-credentials are supplied with a `bucket` param, the
/// handler builds an ephemeral [`AggContext`] targeting the user's bucket,
/// enabling on-demand aggregation without a server-side `--agg` flag.
///
/// [order key]: aggregate::order_key
pub async fn get_flamegraph(
    State(state): State<AppState>,
    creds: MaybeCreds,
    // `axum_extra`'s Query supports repeated keys (`host=a&host=b`), which the
    // stock `serde_urlencoded`-based extractor does not.
    QueryExtra(params): QueryExtra<FlamegraphParams>,
) -> Result<Json<FlamegraphResponse>, (StatusCode, String)> {
    // If a bucket param is supplied, build a per-request AggContext using
    // whatever backend `resolve` gives (BYOC ephemeral client or the server's
    // ambient identity). This enables on-demand aggregation for BYOC users
    // without requiring the server to be started with `--agg`.
    if let Some(bucket) = &params.bucket {
        let backend = state.resolve(creds)?;
        let source_prefix = params
            .prefix
            .clone()
            .or_else(|| state.default_prefix.clone())
            .unwrap_or_default();
        // Output may target a different, writable bucket than the (often
        // read-only) source. When `--agg-output-bucket` is configured we write
        // there through its own region-aware backend; otherwise we write back
        // into the source bucket through the request's own backend.
        let (output_bucket, output) = match (&state.agg_output_bucket, &state.agg_output_backend) {
            (Some(out_bucket), Some(out_backend)) => (out_bucket.clone(), Arc::clone(out_backend)),
            _ => (bucket.clone(), Arc::clone(&backend)),
        };
        tracing::info!(
            %bucket,
            %output_bucket,
            params_prefix = ?params.prefix,
            default_prefix = ?state.default_prefix,
            resolved_source_prefix = %source_prefix,
            output_prefix = %state.agg_output_prefix,
            "flamegraph: BYOC agg context"
        );
        let agg = AggContext {
            source: backend,
            output,
            source_bucket: bucket.clone(),
            source_is_local: false,
            output_bucket,
            output_prefix: state.agg_output_prefix.clone(),
            source_prefixes: vec![source_prefix],
            segment_duration_secs: state.agg_segment_secs,
        };
        return run_refinement_loop(&agg, &params, &state.fold_limits).await;
    }

    if let Some(agg) = &state.agg {
        return run_refinement_loop(agg, &params, &state.fold_limits).await;
    }

    Err((
        StatusCode::NOT_FOUND,
        "flamegraph requires demand-driven aggregation (start with --agg or supply a bucket)"
            .to_string(),
    ))
}

/// Build the [`Scope`] from query params.
fn scope_from_params(params: &FlamegraphParams) -> Scope {
    Scope {
        start_ns: params.start_ns,
        end_ns: params.end_ns,
        service: params.service.clone(),
        hosts: params.host.clone(),
    }
}

/// Build a [`SampleFilter`] from the query params. Maps named params to the
/// generic facet filter system. For each facet in [`FACETS`], looks up the
/// matching query param; uses the facet's `default_filter` when absent.
fn sample_filter(params: &FlamegraphParams) -> SampleFilter {
    let mut facets = HashMap::new();
    for def in FACETS {
        let value = match def.name {
            "source" => {
                let raw = params
                    .source
                    .clone()
                    .unwrap_or_else(|| def.default_filter.to_string());
                // "all" = no constraint on source.
                if raw == "all" { String::new() } else { raw }
            }
            "thread_class" => params
                .thread_class
                .clone()
                .unwrap_or_else(|| def.default_filter.to_string()),
            "spawn_location" => params
                .spawn_location
                .clone()
                .unwrap_or_else(|| def.default_filter.to_string()),
            "host" => {
                // Host filtering is handled via the scope (multi-value), not
                // a single facet filter. Leave empty = no constraint.
                String::new()
            }
            _ => def.default_filter.to_string(),
        };
        facets.insert(def.name, value);
    }
    SampleFilter {
        start_ns: params.start_ns,
        end_ns: params.end_ns,
        facets,
    }
}

/// One iteration of the demand-driven refinement loop.
///
/// 1. List the source scope → filter + order by [order key] → the **matched
///    set** (coverage denominator, fold order).
/// 2. List the **folded set** (the output `samples/` listing — the record of
///    what's already folded; see ADR-0003).
/// 3. Fold a bounded budget of not-yet-folded matched files (baseline on the
///    first poll, a refine-batch on later polls), stopping at the sampling cap.
/// 4. Aggregate over the folded-in-scope part-files → tree + [`Coverage`].
///
/// Stateless: folding happens only during a poll, re-folding is idempotent, and
/// there is no background task or coordination.
async fn run_refinement_loop(
    agg: &AggContext,
    params: &FlamegraphParams,
    limits: &aggregate::FoldLimits,
) -> Result<Json<FlamegraphResponse>, (StatusCode, String)> {
    let scope = scope_from_params(params);

    tracing::info!(
        source_bucket = %agg.source_bucket,
        source_prefixes = ?agg.source_prefixes,
        service = scope.service.as_deref().unwrap_or("(all)"),
        hosts = ?scope.hosts,
        start_ns = ?scope.start_ns,
        end_ns = ?scope.end_ns,
        max_files = ?params.max_files,
        "flamegraph: starting refinement"
    );

    // 1. Matched set: list source objects scoped to the time range.
    // When we have a time range, generate date/hour prefixes to avoid
    // listing the entire bucket (which can be 100k+ objects).
    let listing_prefixes = time_scoped_prefixes(&agg.source_prefixes, &scope);
    tracing::info!(listing_prefixes = ?listing_prefixes, "flamegraph: listing prefixes");

    // List the prefixes concurrently: a wide window is many independent LISTs,
    // and serializing them is a major latency driver. Bounded by LIST_CONCURRENCY.
    use futures::stream::StreamExt;
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
                        "flamegraph: listed source prefix"
                    );
                    objs
                }
                Err(e) => {
                    tracing::warn!(%prefix, error = %e, "failed to list source prefix");
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
        &scope,
        agg.segment_duration_secs,
        agg.source_is_local,
        &agg.source_bucket,
    );
    let files_matched = ordered.len();
    tracing::info!(
        total_listed,
        files_matched,
        sample_matched = ?ordered.iter().take(3).map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        "flamegraph: scope filter result"
    );
    if files_matched == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            format!(
                "no source files match this scope (listed {} objects from prefixes {:?})",
                total_listed, agg.source_prefixes,
            ),
        ));
    }

    // Sampling cap: never fold more than this many files for the scope.
    let cap = sampling_cap(files_matched, params.max_files);

    // 2. Folded set: the output samples/ listing, pruned to this source bucket.
    let folded = aggregate::list_folded_leaves(
        &*agg.output,
        &agg.output_bucket,
        &agg.output_prefix,
        &agg.source_bucket,
    )
    .await;

    // The capped prefix: the first `cap` files in order. This is the
    // representative sample we fold into and aggregate over. Budgeting,
    // already-folded counting, and aggregation are ALL scoped to this prefix —
    // counting folded files across the whole matched set would let folded files
    // *outside* the cap inflate the count and starve the budget (room could go
    // to zero while the in-cap window is barely folded), permanently stalling
    // refinement well below the cap.
    let capped: Vec<&(String, String)> = ordered.iter().take(cap).collect();

    // 3. Fold a bounded budget of not-yet-folded files within the capped prefix.
    let already_folded_in_cap = capped
        .iter()
        .filter(|(_, full)| folded.contains(&aggregate::part_leaf_of(full)))
        .count();

    let mut folded = folded;
    if !params.refine {
        // Read-only poll: return whatever is already folded, instantly. This is
        // the UI's first poll for a scope, so reloading a previously-loaded
        // range shows its existing tree without re-folding anything.
        tracing::info!(
            files_matched,
            cap,
            already_folded = already_folded_in_cap,
            "flamegraph query: read-only poll, serving already-folded set"
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
                .map(|kv| (*kv).clone())
                .collect();
            tracing::info!(
                files_matched,
                cap,
                already_folded = already_folded_in_cap,
                folding_now = to_fold.len(),
                fetch_permits = limits.fetch.available_permits(),
                cpu_permits = limits.cpu.available_permits(),
                "flamegraph query: folding {} new file(s) this poll",
                to_fold.len()
            );

            // Spawn one task per file and let the process-global `FoldLimits`
            // semaphores bound concurrency, rather than a per-request
            // `buffer_unordered`. Tasks run on the multi-thread runtime (true
            // parallelism), the fetch stage runs at high concurrency (~2×
            // parallelism), and the CPU decode/encode stage is throttled to
            // ~parallelism — both bounded across all concurrent requests, not
            // just within this one poll. The batch is already capped by
            // `budget` (≤ the sampling cap), so we spawn a bounded set.
            //
            // Share the context and limits across tasks via `Arc` so each spawn
            // is a cheap refcount bump rather than a deep clone, and the
            // semaphores stay global rather than per task.
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
                            tracing::warn!(key = %raw_key, error = %e, "failed to fold source file");
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
                        // the file simply stays unfolded and a later poll retries.
                        tracing::warn!(error = %e, "fold task failed to join");
                    }
                }
            }
        } else {
            tracing::info!(
                files_matched,
                cap,
                already_folded = already_folded_in_cap,
                "flamegraph query: at sampling cap, no new files to fold"
            );
        }
    }

    // 4. Aggregate over the capped prefix (the first `cap` files in order).
    //    Aggregating the capped prefix — not every folded file — keeps
    //    `samples_folded` and `files_folded` consistent even when a prior
    //    "fetch more" left more files folded in this scope than the current cap.
    let capped_keys: Vec<String> = capped.iter().map(|(_, full)| full.clone()).collect();
    let result = aggregate::aggregate(
        &*agg.output,
        &agg.output_bucket,
        &agg.output_prefix,
        &capped_keys,
        &folded,
        sample_filter(params),
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let files_folded = capped_keys
        .iter()
        .filter(|full| folded.contains(&aggregate::part_leaf_of(full)))
        .count();

    let coverage = Coverage {
        files_matched,
        files_folded,
        samples_folded: result.total_samples,
        total_bytes,
    };

    let pct = if files_matched > 0 {
        (files_folded as f64 / files_matched as f64) * 100.0
    } else {
        0.0
    };
    tracing::info!(
        files_folded,
        files_matched,
        coverage_pct = format!("{pct:.1}"),
        samples = result.total_samples,
        hosts = result.hosts,
        "flamegraph query: returning tree ({files_folded}/{files_matched} files, {pct:.1}%)"
    );

    let tree = build_flamegraph_tree(&result.stack_counts, &result.stacks_dict);

    // Build the active filters echo from the SampleFilter we actually applied.
    let active_filter = sample_filter(params);
    let filters: HashMap<String, String> = active_filter
        .facets
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();

    Ok(Json(FlamegraphResponse {
        tree,
        total_samples: result.total_samples,
        coverage: Some(coverage),
        metadata: FlamegraphMetadata {
            service: params.service.clone(),
            hosts: result.hosts,
            time_range: match (&params.from, &params.to) {
                (Some(f), Some(t)) => Some(format!("{f}–{t}")),
                _ => None,
            },
            min_timestamp_ns: result.min_ts,
            max_timestamp_ns: result.max_ts,
            facets: result.facets,
            scope: ScopeEcho {
                service: params.service.clone(),
                hosts: params.host.clone(),
                start_ns: params.start_ns,
                end_ns: params.end_ns,
                filters,
            },
        },
    }))
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
/// Service/host pruning happens in-memory in [`scope_matches`] — it
/// can't go into the prefix because both sit *after* `HHMM` in the key.
///
/// Falls back to the raw `source_prefixes` when no time range is given.
pub(crate) fn time_scoped_prefixes(source_prefixes: &[String], scope: &Scope) -> Vec<String> {
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

/// How many files a scope may fold before plateauing.
///
/// Default: `min(CAP_FRACTION × matched, CAP_MAX_FILES)` — the fraction keeps
/// small scopes sensible; the absolute ceiling stops a huge scope from chasing a
/// proportionally huge sample. "Fetch more" passes an explicit `max_files`
/// target that *replaces* the default cap for this scope (clamped to the matched
/// set), letting the user deliberately sample deeper than the default.
///
/// Either way the result is floored at the baseline (so the first poll can
/// always return a tree) and clamped to the matched set (can't fold more files
/// than exist).
fn sampling_cap(files_matched: usize, max_files_override: Option<usize>) -> usize {
    let target = match max_files_override {
        Some(explicit) => explicit,
        None => {
            let by_fraction = (files_matched as f64 * CAP_FRACTION).ceil() as usize;
            by_fraction.min(CAP_MAX_FILES)
        }
    };
    target.max(BASELINE_FILES).min(files_matched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_flamegraph_tree() {
        let mut stacks_dict = HashMap::new();
        // Stack A: main → foo → bar (leaf→root stored as bar, foo, main)
        let stack_a = vec![0u8; 16];
        stacks_dict.insert(
            stack_a.clone(),
            vec!["bar".to_string(), "foo".to_string(), "main".to_string()],
        );
        // Stack B: main → foo → baz
        let mut stack_b = vec![0u8; 16];
        stack_b[0] = 1;
        stacks_dict.insert(
            stack_b.clone(),
            vec!["baz".to_string(), "foo".to_string(), "main".to_string()],
        );

        let stack_counts = vec![(stack_a, 10), (stack_b, 5)];
        let tree = build_flamegraph_tree(&stack_counts, &stacks_dict);

        assert_eq!(tree.name, "(all)");
        assert_eq!(tree.count, 15);
        assert_eq!(tree.children.len(), 1); // "main"
        let main_node = &tree.children[0];
        assert_eq!(main_node.name, "main");
        assert_eq!(main_node.count, 15);
        let foo_node = &main_node.children[0];
        assert_eq!(foo_node.name, "foo");
        assert_eq!(foo_node.count, 15);
        assert_eq!(foo_node.children.len(), 2); // "bar" and "baz"
    }

    #[test]
    fn test_epoch_to_date_hour() {
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
    fn test_time_scoped_prefixes_narrow_uses_minutes() {
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
    fn test_time_scoped_prefixes_wide_uses_hours() {
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
    fn test_time_scoped_prefixes_empty_base() {
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
    fn test_time_scoped_prefix_matches_per_minute_dir() {
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
    fn test_time_scoped_single_segment_narrow() {
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
    fn test_time_scoped_no_time_range() {
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

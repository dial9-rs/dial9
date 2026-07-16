//! `/api/flamegraph` endpoint: stream an aggregated flamegraph tree over
//! Server-Sent Events, refining as source files fold.
//!
//! One request holds the connection open: it [resolves](refine::resolve) the
//! scope, emits the already-folded snapshot immediately, then folds up to the
//! [sampling cap](refine) and pushes a fresh full-tree snapshot as each file
//! lands, closing when the cap is reached. Each SSE `data:` frame is one
//! [`FlamegraphResponse`] JSON object — the same shape the UI rendered per poll
//! before — so the client just re-renders on every event.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::Arc;

use axum::Extension;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum_extra::extract::Query as QueryExtra;
use futures::stream::{self, Stream, StreamExt};
use serde::{Deserialize, Serialize};

use crate::ingest::aggregate::{
    self, AggContext, AggSnapshot, Coverage, FACETS, FacetResult, FlamegraphAccum,
    PollDurationBucket, SampleFilter, Scope,
};
use crate::ingest::refine::{self, FoldErrors, FoldOutcome, RefineOpts, Resolved};
use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::server::metrics::OperationMetrics;

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
    /// Clamped server-side to a hard ceiling (see `sampling_cap`), so a crafted
    /// request can't drive an unbounded fold.
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
    /// Poll-duration band, lower bound in nanoseconds (inclusive). Keeps only
    /// samples inside a poll at least this long. Sent by the flamegraph UI's
    /// "Poll duration" min input. This is *poll* duration, not request latency.
    pub min_poll_ns: Option<i64>,
    /// Poll-duration band, upper bound in nanoseconds (inclusive). Keeps only
    /// samples inside a poll at most this long.
    pub max_poll_ns: Option<i64>,
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
    /// Sample-weighted poll-duration histogram (log₂ ns buckets): the minimap
    /// over the poll-duration band picker. Bar height = samples you'd select by
    /// brushing that range. Accumulated pre-band, so it always shows the full
    /// distribution the band selects from.
    pub poll_duration_histogram: Vec<PollDurationBucket>,
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
    /// Active poll-duration band (nanoseconds, inclusive), echoed so the header
    /// and diff links reflect the backend's applied slice. Null = no bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_poll_ns: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_poll_ns: Option<i64>,
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
        // `or_insert_with_key` clones the name only when a new node is inserted;
        // on the hot path (child already present) it's just a move into the
        // lookup with no allocation.
        self.children
            .entry(name)
            .or_insert_with_key(|name| TrieNode::new(name.clone()))
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

/// Handler for GET /api/flamegraph — a Server-Sent Events stream.
///
/// [Resolves](refine::resolve) the scope, primes an incremental
/// [`FlamegraphAccum`] over the already-folded set, and emits an initial
/// snapshot. Then it folds the not-yet-folded capped files in [order key] order
/// (up to the [sampling cap](refine)), pushing a fresh full-tree SSE event as
/// each file lands, and closes when the work-list drains. The client re-renders
/// on every event; there is no re-polling.
///
/// The aggregation context comes from [`AppState::agg_context_for`]: a `bucket`
/// param builds a per-request bring-your-own-credentials context; otherwise the
/// server's `--agg` context is used. Absent both → 404.
///
/// [order key]: aggregate::order_key
pub async fn get_flamegraph(
    State(state): State<AppState>,
    creds: MaybeCreds,
    // `axum_extra`'s Query supports repeated keys (`host=a&host=b`), which the
    // stock `serde_urlencoded`-based extractor does not.
    QueryExtra(params): QueryExtra<FlamegraphParams>,
) -> Result<
    (
        Extension<OperationMetrics>,
        Sse<impl Stream<Item = Result<Event, Infallible>>>,
    ),
    (StatusCode, String),
> {
    let Some(agg) = state
        .agg_context_for(params.bucket.as_deref(), params.prefix.as_deref(), creds)
        .await?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            "flamegraph requires demand-driven aggregation (start with --agg or supply a bucket)"
                .to_string(),
        ));
    };

    let scope = scope_from_params(&params);
    let opts = RefineOpts {
        max_files: params.max_files,
    };

    // Resolve up front so a scope with no matching files still maps to 404
    // (rather than opening an empty stream). Folding happens lazily in the stream.
    let Some(resolved) = refine::resolve(&agg, &scope, opts).await else {
        return Err((
            StatusCode::NOT_FOUND,
            "no source files match this scope".to_string(),
        ));
    };

    // Operation-specific metrics, attached at response-head time — all the
    // middleware can see for a streamed body (the folding happens after the
    // headers go out). Coverage here is therefore the RESOLVE-TIME snapshot:
    // how much of the scope was already folded when the stream opened. Samples
    // are not known until part-files are read inside the stream, so they are
    // reported as absent rather than a misleading zero.
    let op = OperationMetrics::flamegraph(
        resolved.files_matched as u32,
        resolved.files_folded_in(resolved.folded()) as u32,
        None,
    );

    let stream = flamegraph_stream(agg, resolved, &params, state.fold_limits.clone());
    Ok((
        Extension(op),
        Sse::new(stream).keep_alive(KeepAlive::default()),
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
        min_poll_ns: params.min_poll_ns,
        max_poll_ns: params.max_poll_ns,
        facets,
    }
}

/// Immutable per-request context threaded through the stream: the resolved
/// scope, the fixed sample filter, and the params fields needed to shape each
/// event's metadata. Holding an `Arc<AggContext>` lets both the read-back GETs
/// and the fold tasks share it without re-cloning per file.
struct StreamCtx {
    agg: Arc<AggContext>,
    resolved: Resolved,
    filter: SampleFilter,
    service: Option<String>,
    hosts: Vec<String>,
    from: Option<String>,
    to: Option<String>,
    start_ns: Option<i64>,
    end_ns: Option<i64>,
    min_poll_ns: Option<i64>,
    max_poll_ns: Option<i64>,
}

/// The mutable state carried through the folding phase: the incremental
/// accumulator, the growing folded-leaf set, and the running fold-error tally.
/// Boxed inside [`Phase`] so the enum isn't dominated by this variant's size.
struct FoldState {
    accum: FlamegraphAccum,
    folded: HashSet<String>,
    /// Files whose fold failed this stream, and the most recent error message —
    /// surfaced in the coverage block so a systematic failure isn't silent.
    errors: FoldErrors,
}

/// Phase of the SSE fold state machine driven by [`flamegraph_stream`]'s
/// `unfold`. `Start` primes and emits the already-folded snapshot; `Folding`
/// pulls one folded file at a time, merges it, and emits a refined snapshot.
enum Phase {
    Start,
    Folding(Box<FoldState>),
}

/// Build the SSE event stream for one flamegraph request.
///
/// The first `unfold` step primes an accumulator over the already-folded set and
/// emits an instant snapshot (like the old read-only first poll). Each later step
/// pulls one file off [`fold_stream`], reads + merges its part-files, and emits a
/// refined snapshot, closing when the work-list drains. Dropping the returned
/// stream (client disconnect) drops the fold stream, cancelling in-flight folds.
fn flamegraph_stream(
    agg: AggContext,
    resolved: Resolved,
    params: &FlamegraphParams,
    limits: aggregate::FoldLimits,
    // All borrowed data is cloned out of `params` into `StreamCtx` before the
    // stream is built, so the returned stream captures no borrows (`use<>`).
) -> impl Stream<Item = Result<Event, Infallible>> + use<> {
    let agg = Arc::new(agg);
    let ctx = Arc::new(StreamCtx {
        agg: Arc::clone(&agg),
        filter: sample_filter(params),
        service: params.service.clone(),
        hosts: params.host.clone(),
        from: params.from.clone(),
        to: params.to.clone(),
        start_ns: params.start_ns,
        end_ns: params.end_ns,
        min_poll_ns: params.min_poll_ns,
        max_poll_ns: params.max_poll_ns,
        resolved,
    });

    // `Box::pin` so the fold stream is `Unpin` and we can `.next()` it inside the
    // `unfold` step. Bounded concurrency comes from the shared `FoldLimits`.
    let folds = Box::pin(refine::fold_stream(
        agg,
        limits,
        ctx.resolved.unfolded_capped(),
    ));

    stream::unfold(
        (ctx, folds, Phase::Start),
        |(ctx, mut folds, phase)| async move {
            match phase {
                Phase::Start => {
                    // Prime the accumulator over the already-folded set, concurrently.
                    let seed = aggregate::fetch_folded_sample_parts(
                        &*ctx.agg.output,
                        &ctx.agg.output_bucket,
                        &ctx.agg.output_prefix,
                        &ctx.resolved.capped_full_keys(),
                        ctx.resolved.folded(),
                    )
                    .await;
                    let mut accum = FlamegraphAccum::new(ctx.filter.clone());
                    for (samples, dict) in seed {
                        if let Err(e) = accum.merge(samples, dict) {
                            rate_limited_warn("flamegraph: seed merge failed", &e);
                        }
                    }
                    let folded = ctx.resolved.folded().clone();
                    let errors = FoldErrors::default();
                    let event = snapshot_event(&ctx, &accum, &folded, &errors);
                    let state = Box::new(FoldState {
                        accum,
                        folded,
                        errors,
                    });
                    Some((Ok(event), (ctx, folds, Phase::Folding(state))))
                }
                Phase::Folding(mut state) => {
                    // Pull the next fold outcome; `None` = work-list drained → close.
                    match folds.next().await? {
                        FoldOutcome::Folded(f) => {
                            if let Some((samples, dict)) = aggregate::fetch_sample_parts(
                                &*ctx.agg.output,
                                &ctx.agg.output_bucket,
                                &ctx.agg.output_prefix,
                                &f.full_key,
                            )
                            .await
                                && let Err(e) = state.accum.merge(samples, dict)
                            {
                                rate_limited_warn("flamegraph: merge failed", &e);
                            }
                            state.folded.insert(aggregate::part_leaf_of(&f.full_key));
                        }
                        FoldOutcome::Failed { raw_key, error } => {
                            // Count it and carry a sample message so the client can
                            // show that folding is failing (e.g. unwritable output).
                            state.errors.record(&raw_key, &error);
                        }
                    }
                    let event = snapshot_event(&ctx, &state.accum, &state.folded, &state.errors);
                    Some((Ok(event), (ctx, folds, Phase::Folding(state))))
                }
            }
        },
    )
}

/// Rate-limited warn for the per-file merge path (reachable once per folded file
/// on a large scope), so a systematic decode failure can't spam the log.
fn rate_limited_warn(msg: &str, err: &anyhow::Error) {
    use dial9_core::rate_limited;
    rate_limited!(std::time::Duration::from_secs(60), {
        tracing::warn!("{msg}: {err}");
    });
}

/// Build one SSE `data:` event from the accumulator's current snapshot and the
/// coverage implied by `folded` (a growing superset of the resolved folded set)
/// plus the running fold-error tally.
fn snapshot_event(
    ctx: &StreamCtx,
    accum: &FlamegraphAccum,
    folded: &HashSet<String>,
    errors: &FoldErrors,
) -> Event {
    let snap = accum.snapshot();
    let files_matched = ctx.resolved.files_matched;
    let files_folded = ctx.resolved.files_folded_in(folded);
    let coverage = Coverage {
        files_matched,
        files_folded,
        samples_folded: snap.total_samples,
        total_bytes: ctx.resolved.total_bytes,
        hosts_matched: ctx.resolved.hosts_matched,
        hosts_folded: ctx.resolved.folded_hosts(folded),
        fold_errors: errors.count,
        fold_error_sample: errors.sample.clone(),
    };
    let resp = build_response(ctx, &snap, coverage);
    // `json_data` only fails if the value can't serialize; our response always
    // can, so fall back to an empty comment event rather than propagating.
    Event::default().json_data(&resp).unwrap_or_else(|e| {
        rate_limited_warn("flamegraph: event serialize failed", &anyhow::anyhow!(e));
        Event::default().comment("serialize error")
    })
}

/// Shape a [`FlamegraphResponse`] from an [`AggSnapshot`] + [`Coverage`].
fn build_response(ctx: &StreamCtx, snap: &AggSnapshot, coverage: Coverage) -> FlamegraphResponse {
    let tree = build_flamegraph_tree(&snap.stack_counts, snap.stacks_dict);

    // Echo the active filter values back to the UI (facet name → selected value).
    let filters: HashMap<String, String> = ctx
        .filter
        .facets
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();

    FlamegraphResponse {
        tree,
        total_samples: snap.total_samples,
        coverage: Some(coverage),
        metadata: FlamegraphMetadata {
            service: ctx.service.clone(),
            hosts: snap.hosts,
            time_range: match (&ctx.from, &ctx.to) {
                (Some(f), Some(t)) => Some(format!("{f}–{t}")),
                _ => None,
            },
            min_timestamp_ns: snap.min_ts,
            max_timestamp_ns: snap.max_ts,
            facets: snap.facets.clone(),
            poll_duration_histogram: snap.poll_duration_histogram.clone(),
            scope: ScopeEcho {
                service: ctx.service.clone(),
                hosts: ctx.hosts.clone(),
                start_ns: ctx.start_ns,
                end_ns: ctx.end_ns,
                min_poll_ns: ctx.min_poll_ns,
                max_poll_ns: ctx.max_poll_ns,
                filters,
            },
        },
    }
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
}

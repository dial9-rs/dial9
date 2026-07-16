//! `/api/flamegraph` endpoint: stream an aggregated flamegraph tree over
//! Server-Sent Events, refining as source files fold.
//!
//! One request holds the connection open: it [resolves](refine::resolve) the
//! scope, streams already-folded parts in bounded cumulative snapshots, then
//! folds missing capped-prefix files and pushes a fresh full-tree snapshot as
//! each file lands, closing when the bounded work-list drains. Each SSE `data:`
//! frame is one
//! [`FlamegraphResponse`] JSON object — the same shape the UI rendered per poll
//! before — so the client just re-renders on every event.

use std::collections::HashMap;
use std::convert::Infallible;

use axum::Extension;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum_extra::extract::Query as QueryExtra;
use futures::stream::Stream;
use hex;
use serde::{Deserialize, Serialize};

use crate::ingest::aggregate::{
    self, AggContext, AggSnapshot, Coverage, FACETS, FacetResult, FlamegraphAccum,
    PollDurationBucket, SampleFilter, Scope,
};
use crate::ingest::refine::{self, FoldErrors, Folded, RefineOpts, Resolved};
use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::server::fold_stream;
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
    /// Region for ambient-credential S3 reads, carried by browse deep links.
    pub aws_region: Option<String>,
    /// Worker-attribution filter: `"worker"` (on-runtime), `"off-worker"`
    /// (off-runtime), or empty/absent for all. Sent by the flamegraph UI's
    /// "Thread" selector.
    pub thread_class: Option<String>,
    /// Source filter: `"cpu"` (on-CPU profile, the default view), `"sched"`
    /// (scheduler context switches), or empty/absent for all. Sent by the
    /// flamegraph UI's "Source" selector.
    pub source: Option<String>,
    /// Phase filter: `"on_cpu"` maps to `source=cpu`, `"blocking"` maps to
    /// `source=sched`. A convenience alias for the span explorer's phase picker.
    /// Takes precedence over `source` when both are set. Invalid values → 400.
    pub phase: Option<String>,
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
    /// Span type UID filter (hex-encoded 16 bytes). When present, only samples
    /// whose `enclosing_spans` list contains a membership matching this type UID
    /// are included. Used by the span explorer to build per-span-type flamegraphs.
    pub span_type_uid: Option<String>,
    /// Minimum span elapsed_ns for the span filter (inclusive). Keeps only
    /// samples enclosed by a matching span at least this long.
    pub min_span_ns: Option<i64>,
    /// Maximum span elapsed_ns for the span filter (inclusive).
    pub max_span_ns: Option<i64>,
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
    RawQuery(raw_query): RawQuery,
) -> Result<
    (
        Extension<OperationMetrics>,
        Sse<impl Stream<Item = Result<Event, Infallible>>>,
    ),
    (StatusCode, String),
> {
    // ── Validate span filter parameters FIRST (before agg_context_for) ───────
    // `axum_extra::Query` maps an explicitly empty optional value to `None`, so
    // retain the raw query solely to distinguish `?span_type_uid=` / `?phase=`
    // from an absent parameter. Both explicit empty values are invalid.
    // Parse with percent-decoding so that encoded names like `%73pan_type_uid=`
    // are rejected identically to their literal equivalents.
    if raw_query.as_deref().is_some_and(|query| {
        query.split('&').any(|part| {
            let (raw_key, _) = part.split_once('=').unwrap_or((part, ""));
            let decoded_key = urlencoding::decode(raw_key).unwrap_or_default();
            let key = decoded_key.as_ref();
            // An explicit key with empty (or absent) value is invalid for these
            // two parameters — they must either be absent or non-empty.
            let value_part = part.split_once('=').map(|(_, v)| v);
            let value_empty = value_part.is_none() || value_part == Some("");
            (key == "span_type_uid" || key == "phase") && value_empty
        })
    }) {
        return Err((
            StatusCode::BAD_REQUEST,
            "span_type_uid and phase must not be empty".to_string(),
        ));
    }
    // Malformed UID, negative/inverted bounds, bounds without type → 400.
    // Validation runs before any backend access so a malformed request is
    // rejected cheaply, even when no agg context is configured.
    if let Some(ref hex_str) = params.span_type_uid {
        if hex_str.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                "span_type_uid must not be empty".to_string(),
            ));
        }
        match hex::decode(hex_str) {
            Ok(bytes) if bytes.len() == 16 => {} // valid
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "invalid span_type_uid: must be 32 hex chars (16 bytes), got {hex_str:?}"
                    ),
                ));
            }
        }
    }
    if let (Some(min), Some(max)) = (params.min_span_ns, params.max_span_ns) {
        if min < 0 || max < 0 {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "span duration bounds must be non-negative: min_span_ns={min}, max_span_ns={max}"
                ),
            ));
        }
        if min > max {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("inverted span duration bounds: min_span_ns={min} > max_span_ns={max}"),
            ));
        }
    } else if params.min_span_ns.is_some_and(|v| v < 0) || params.max_span_ns.is_some_and(|v| v < 0)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "span duration bounds must be non-negative".to_string(),
        ));
    }
    // Bounds without type: if min/max span bounds are set but span_type_uid is absent, 400.
    // (Empty span_type_uid is already rejected above, so only None reaches here.)
    if (params.min_span_ns.is_some() || params.max_span_ns.is_some())
        && params.span_type_uid.is_none()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "min_span_ns/max_span_ns require span_type_uid".to_string(),
        ));
    }
    // Validate phase parameter: only "on_cpu" and "blocking" are valid.
    if let Some(ref phase) = params.phase
        && !phase.is_empty()
        && phase != "on_cpu"
        && phase != "blocking"
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("invalid phase: must be 'on_cpu' or 'blocking', got {phase:?}"),
        ));
    }

    // ── Resolve aggregation context (after validation) ───────────────────────
    let Some(agg) = state
        .agg_context_for(
            params.bucket.as_deref(),
            params.prefix.as_deref(),
            params.aws_region.as_deref(),
            creds,
        )
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
                // phase takes precedence: on_cpu→cpu, blocking→sched.
                let effective_source = match params.phase.as_deref() {
                    Some("on_cpu") => Some("cpu".to_string()),
                    Some("blocking") => Some("sched".to_string()),
                    _ => params.source.clone(),
                };
                let raw = effective_source.unwrap_or_else(|| def.default_filter.to_string());
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

    // Parse span_type_uid from hex if provided. Pre-validated by the handler,
    // so malformed hex should not reach here — but fail closed defensively.
    let span_type_uid = match params.span_type_uid.as_deref() {
        Some(hex_str) if !hex_str.is_empty() => {
            match hex::decode(hex_str) {
                Ok(bytes) if bytes.len() == 16 => {
                    let mut uid = [0u8; 16];
                    uid.copy_from_slice(&bytes);
                    Some(uid)
                }
                _ => {
                    // Unreachable after handler validation; fail closed without warning.
                    Some([0u8; 16])
                }
            }
        }
        _ => None,
    };

    SampleFilter {
        start_ns: params.start_ns,
        end_ns: params.end_ns,
        min_poll_ns: params.min_poll_ns,
        max_poll_ns: params.max_poll_ns,
        facets,
        span_type_uid,
        min_span_ns: params.min_span_ns,
        max_span_ns: params.max_span_ns,
    }
}

/// Immutable per-request context shared by the [`FoldSink`] adapter: the resolved
/// scope-derived fields needed to shape each event's metadata, and the fixed
/// sample filter. All borrowed data is cloned out of `params` before the stream
/// is built, so the returned stream captures no borrows (`use<>`).
struct StreamCtx {
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

/// The flamegraph [`FoldSink`] adapter: owns the incremental [`FlamegraphAccum`]
/// and the per-request [`StreamCtx`]. Supplies the three operations that differ
/// from span-stats; the folded-set discipline lives in the driver.
struct FlamegraphSink {
    ctx: StreamCtx,
    accum: FlamegraphAccum,
}

impl fold_stream::FoldSink for FlamegraphSink {
    async fn seed_batch(
        &mut self,
        agg: &AggContext,
        full_keys: &[String],
    ) -> Vec<fold_stream::PartOutcome> {
        // Prime the accumulator from this bounded cached batch concurrently.
        // Results are keyed by leaf hash so completion order doesn't matter.
        let seed = aggregate::fetch_folded_sample_parts(
            &*agg.output,
            &agg.output_bucket,
            &agg.output_prefix,
            full_keys,
        )
        .await;
        // Only mark a leaf folded after its required samples GET and full merge
        // succeed. The stacks dictionary is optional enrichment by the persisted
        // part-file contract; its absence is represented as `None`.
        // The driver applies the membership rule.
        let mut outcomes = Vec::with_capacity(seed.len());
        for (leaf, result) in seed {
            let outcome = match result {
                Ok((samples, dict)) => match self.accum.merge(samples, dict) {
                    Ok(()) => fold_stream::PartOutcome::Folded { leaf },
                    Err(e) => {
                        fold_stream::rate_limited_warn("flamegraph: seed merge failed", &e);
                        fold_stream::PartOutcome::Failed {
                            key: leaf,
                            error: format!("merge: {e}"),
                        }
                    }
                },
                Err(msg) => fold_stream::PartOutcome::Failed {
                    key: leaf,
                    error: msg,
                },
            };
            outcomes.push(outcome);
        }
        outcomes
    }

    async fn fold_one(&mut self, agg: &AggContext, f: &Folded) -> fold_stream::PartOutcome {
        // Only mark the leaf folded after the required samples fetch and full
        // merge succeed; the stacks dictionary is optional enrichment.
        // Failures increment errors.
        match aggregate::fetch_sample_parts(
            &*agg.output,
            &agg.output_bucket,
            &agg.output_prefix,
            &f.full_key,
        )
        .await
        {
            Some((samples, dict)) => match self.accum.merge(samples, dict) {
                Ok(()) => fold_stream::PartOutcome::Folded {
                    leaf: aggregate::part_leaf_of(&f.full_key),
                },
                Err(e) => {
                    fold_stream::rate_limited_warn("flamegraph: merge failed", &e);
                    fold_stream::PartOutcome::Failed {
                        key: f.raw_key.clone(),
                        error: format!("merge: {e}"),
                    }
                }
            },
            None => {
                // GET failed — leaf stays unfolded.
                fold_stream::PartOutcome::Failed {
                    key: f.raw_key.clone(),
                    error: "sample parts GET failed (not found)".to_string(),
                }
            }
        }
    }

    fn snapshot_event(
        &self,
        resolved: &Resolved,
        files_folded: usize,
        hosts_folded: usize,
        errors: &FoldErrors,
    ) -> Event {
        let snap = self.accum.snapshot();
        let coverage = fold_stream::coverage_from(
            resolved,
            files_folded,
            hosts_folded,
            errors,
            snap.total_samples,
        );
        let resp = build_response(&self.ctx, &snap, coverage);
        // `json_data` only fails if the value can't serialize; our response always
        // can, so fall back to an empty comment event rather than propagating.
        Event::default().json_data(&resp).unwrap_or_else(|e| {
            fold_stream::rate_limited_warn(
                "flamegraph: event serialize failed",
                &anyhow::anyhow!(e),
            );
            Event::default().comment("serialize error")
        })
    }
}

/// Build the SSE event stream for one flamegraph request.
///
/// Cached already-folded parts are merged in bounded batches, with a cumulative
/// snapshot emitted after each batch. Once cached state is exhausted, each later
/// step pulls one file off [`refine::fold_stream`], reads + merges its part-files,
/// and emits a refined snapshot, closing when the work-list drains. Dropping the
/// returned stream (client disconnect) drops the fold stream, cancelling
/// in-flight folds.
fn flamegraph_stream(
    agg: AggContext,
    resolved: Resolved,
    params: &FlamegraphParams,
    limits: aggregate::FoldLimits,
    // All borrowed data is cloned out of `params` into `StreamCtx` before the
    // stream is built, so the returned stream captures no borrows (`use<>`).
) -> impl Stream<Item = Result<Event, Infallible>> + use<> {
    let ctx = StreamCtx {
        filter: sample_filter(params),
        service: params.service.clone(),
        hosts: params.host.clone(),
        from: params.from.clone(),
        to: params.to.clone(),
        start_ns: params.start_ns,
        end_ns: params.end_ns,
        min_poll_ns: params.min_poll_ns,
        max_poll_ns: params.max_poll_ns,
    };
    let accum = FlamegraphAccum::new(ctx.filter.clone());
    fold_stream::drive(agg, resolved, limits, FlamegraphSink { ctx, accum })
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

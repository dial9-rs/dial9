//! `/api/flamegraph` endpoint: query aggregated Parquet data and return a flamegraph tree.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum_extra::extract::Query as QueryExtra;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::ingest::aggregate::{
    self, AggContext, Coverage, FACETS, FacetResult, SampleFilter, Scope,
};
use crate::ingest::refine::{self, RefineOpts};
use crate::server::AppState;
use crate::server::credentials::MaybeCreds;

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

/// Handler for GET /api/flamegraph.
///
/// Runs the demand-driven [refinement loop](crate::ingest::refine::refine): fold
/// a bounded budget of source files in [order key] order, aggregate over the
/// folded-in-scope set, and return the tree plus a [`Coverage`] block. The
/// client re-polls to refine.
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
) -> Result<Json<FlamegraphResponse>, (StatusCode, String)> {
    let Some(agg) =
        state.agg_context_for(params.bucket.as_deref(), params.prefix.as_deref(), creds)?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            "flamegraph requires demand-driven aggregation (start with --agg or supply a bucket)"
                .to_string(),
        ));
    };
    flamegraph_response(&agg, &params, &state.fold_limits).await
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

/// Build the `/api/flamegraph` response for one demand-driven poll: run the
/// shared [refinement loop](refine::refine) to resolve + fold the capped set,
/// aggregate the samples part-files over it, and shape the flamegraph tree +
/// [`Coverage`].
async fn flamegraph_response(
    agg: &AggContext,
    params: &FlamegraphParams,
    limits: &aggregate::FoldLimits,
) -> Result<Json<FlamegraphResponse>, (StatusCode, String)> {
    let scope = scope_from_params(params);
    let opts = RefineOpts {
        refine: params.refine,
        max_files: params.max_files,
    };

    let Some(refined) = refine::refine(agg, &scope, opts, limits).await else {
        return Err((
            StatusCode::NOT_FOUND,
            "no source files match this scope".to_string(),
        ));
    };

    // Aggregate over the capped prefix (not every folded file): this keeps
    // `samples_folded` and `files_folded` consistent even when a prior "fetch
    // more" left more files folded in this scope than the current cap.
    let filter = sample_filter(params);
    let result = aggregate::aggregate(
        &*agg.output,
        &agg.output_bucket,
        &agg.output_prefix,
        &refined.capped_full_keys(),
        refined.folded(),
        filter.clone(),
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let files_matched = refined.files_matched;
    let files_folded = refined.files_folded();
    let coverage = Coverage {
        files_matched,
        files_folded,
        samples_folded: result.total_samples,
        total_bytes: refined.total_bytes,
        hosts_matched: refined.hosts_matched,
        hosts_folded: refined.hosts_folded(),
    };

    let pct = (files_folded as f64 / files_matched as f64) * 100.0;
    tracing::info!(
        files_folded,
        files_matched,
        coverage_pct = format!("{pct:.1}"),
        samples = result.total_samples,
        hosts = result.hosts,
        "flamegraph: returning tree ({files_folded}/{files_matched} files, {pct:.1}%)"
    );

    let tree = build_flamegraph_tree(&result.stack_counts, &result.stacks_dict);

    // Echo the active filter values back to the UI (facet name → selected value).
    let filters: HashMap<String, String> = filter
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

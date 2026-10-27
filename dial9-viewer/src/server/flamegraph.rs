//! `/api/flamegraph` endpoint: stream an aggregated flamegraph tree over
//! Server-Sent Events, refining as source files fold.
//!
//! One request holds the connection open: it [resolves](refine::resolve) the
//! scope, streams coverage plus bounded partial trees while already-folded and
//! missing capped-prefix files merge, then emits the full bounded tree after the
//! work-list drains. Legacy response formats retain their cumulative snapshot
//! behavior.

use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::convert::Infallible;

use axum::Extension;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum_extra::extract::Query as QueryExtra;
use futures::stream::Stream;
use hex;
use serde::{Deserialize, Serialize, Serializer};

use crate::ingest::aggregate::{
    self, AggContext, AggSnapshot, Coverage, FACETS, FacetResult, FlamegraphAccum, FrameId,
    PollDurationBucket, SampleFilter, Scope, StackDictionary,
};
use crate::ingest::refine::{self, FoldErrors, Folded, RefineOpts, Resolved};
use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::server::fold_stream;
use crate::server::metrics::{OperationMetrics, UxRequestLifecycle, track_sse_ux};

const DEFAULT_FLAMEGRAPH_NODE_BUDGET: usize = 50_000;

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
    /// Response tree encoding. The canonical UI requests `flat-v1`; absent or
    /// unknown values retain the legacy nested-name response.
    pub format: Option<String>,
    /// Frame identity currently focused by the aggregate inspect view. The
    /// bounded projection retains one deterministic path to this frame.
    pub inspect: Option<String>,
}

#[derive(Serialize)]
pub struct FlamegraphResponse {
    /// Present for the negotiated coverage/partial/final protocol; omitted from
    /// legacy snapshots.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    pub tree: FlamegraphTree,
    pub total_samples: usize,
    /// Present in demand-driven mode: how much of the scope has been folded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
    pub metadata: FlamegraphMetadata,
}

#[derive(Serialize)]
struct FlamegraphCoverage {
    kind: &'static str,
    total_samples: usize,
    coverage: Coverage,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum FlamegraphTree {
    Legacy(FlamegraphNode),
    Flat(FlatFlamegraphTree),
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
pub struct FlatFlamegraphTree {
    pub format: &'static str,
    /// Frame names indexed by each node row's frame field.
    pub frames: Vec<String>,
    /// Preorder rows: `[parent_node, frame, count, self_count]`. The root is
    /// row zero; every other parent precedes its children.
    pub nodes: Vec<FlatFlamegraphNode>,
    /// Exact trie node count before projection.
    pub total_nodes: usize,
    /// Exact trie nodes represented only through synthetic `[other]` rows.
    pub omitted_nodes: usize,
    /// True when a requested inspect frame was found and its path retained.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspect_retained: Option<bool>,
}

/// One `flat-v1` row. Named fields so call sites read clearly; serialized
/// positionally as `[parent, frame, count, self_count]` so the payload carries
/// no per-node property names.
pub struct FlatFlamegraphNode {
    /// Row index of this node's parent. Every parent precedes its children;
    /// row zero is the root, whose parent field is ignored.
    pub parent: u32,
    /// Index into [`FlatFlamegraphTree::frames`].
    pub frame: u32,
    pub count: u64,
    pub self_count: u64,
}

impl Serialize for FlatFlamegraphNode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (self.parent, self.frame, self.count, self.self_count).serialize(serializer)
    }
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
    stack_counts: &[([u8; 16], u64)],
    stacks_dict: &StackDictionary,
) -> FlamegraphTrie {
    let mut trie = FlamegraphTrie::new(stacks_dict.root());

    for (stack_id, count) in stack_counts {
        let frames = match stacks_dict.get(stack_id) {
            Some(f) => f,
            None => continue,
        };
        // Frames are stored leaf→root; flamegraph trie inserts root→leaf
        trie.nodes[0].count += count;
        let mut node = 0;
        for frame in frames.iter().rev() {
            node = trie.get_or_insert_child(node, *frame);
            trie.nodes[node].count += count;
        }
        // Only the sampled leaf gets self-time; callers retain inclusive counts.
        trie.nodes[node].self_count += count;
    }

    trie
}

struct TrieNode {
    frame: FrameId,
    parent: usize,
    count: u64,
    self_count: u64,
}

struct FlamegraphTrie {
    nodes: Vec<TrieNode>,
    edges: HashMap<(usize, FrameId), usize>,
}

impl FlamegraphTrie {
    fn new(root_frame: FrameId) -> Self {
        Self {
            nodes: vec![TrieNode {
                frame: root_frame,
                parent: 0,
                count: 0,
                self_count: 0,
            }],
            edges: HashMap::new(),
        }
    }

    fn get_or_insert_child(&mut self, parent: usize, frame: FrameId) -> usize {
        let key = (parent, frame);
        if let Some(child) = self.edges.get(&key) {
            return *child;
        }
        let child = self.nodes.len();
        self.nodes.push(TrieNode {
            frame,
            parent,
            count: 0,
            self_count: 0,
        });
        self.edges.insert(key, child);
        child
    }

    /// Group every non-root node under its parent, each sibling run ordered by
    /// descending count then frame name.
    ///
    /// Counting-sort into the `starts` prefix sum, then sort each sibling slice:
    /// Σ kᵢ log kᵢ comparisons instead of the N log N a single global sort keyed
    /// on `parent` would cost, and the comparator only touches nodes that are
    /// actually siblings.
    fn ordered_children(&self, stacks_dict: &StackDictionary) -> OrderedChildren {
        let mut starts = vec![0usize; self.nodes.len() + 1];
        for node in self.nodes.iter().skip(1) {
            starts[node.parent + 1] += 1;
        }
        for i in 1..starts.len() {
            starts[i] += starts[i - 1];
        }

        let mut nodes = vec![0usize; self.nodes.len() - 1];
        let mut next = starts.clone();
        for (id, node) in self.nodes.iter().enumerate().skip(1) {
            nodes[next[node.parent]] = id;
            next[node.parent] += 1;
        }

        for parent in 0..self.nodes.len() {
            nodes[starts[parent]..starts[parent + 1]].sort_unstable_by(|a, b| {
                let a = &self.nodes[*a];
                let b = &self.nodes[*b];
                b.count.cmp(&a.count).then_with(|| {
                    stacks_dict
                        .resolve(a.frame)
                        .cmp(stacks_dict.resolve(b.frame))
                })
            });
        }

        OrderedChildren { nodes, starts }
    }

    fn legacy_node(
        &self,
        node_id: usize,
        stacks_dict: &StackDictionary,
        children: &OrderedChildren,
    ) -> FlamegraphNode {
        let node = &self.nodes[node_id];
        FlamegraphNode {
            name: stacks_dict.resolve(node.frame).to_string(),
            count: node.count,
            self_count: node.self_count,
            children: children
                .of(node_id)
                .iter()
                .map(|child| self.legacy_node(*child, stacks_dict, children))
                .collect(),
        }
    }

    fn into_legacy(mut self, stacks_dict: &StackDictionary) -> FlamegraphNode {
        self.edges = HashMap::new();
        let children = self.ordered_children(stacks_dict);
        self.legacy_node(0, stacks_dict, &children)
    }

    #[cfg(test)]
    fn append_flat(
        &self,
        node_id: usize,
        parent_wire_id: u32,
        frames: &WireFrames,
        children: &OrderedChildren,
        rows: &mut Vec<FlatFlamegraphNode>,
    ) {
        let wire_id =
            u32::try_from(rows.len()).expect("a flamegraph cannot contain more than u32 nodes");
        let node = &self.nodes[node_id];
        rows.push(FlatFlamegraphNode {
            parent: parent_wire_id,
            frame: frames.ids[&node.frame],
            count: node.count,
            self_count: node.self_count,
        });
        for child in children.of(node_id) {
            self.append_flat(*child, wire_id, frames, children, rows);
        }
    }

    #[cfg(test)]
    fn into_flat_tree(mut self, stacks_dict: &StackDictionary) -> FlatFlamegraphTree {
        self.edges = HashMap::new();
        let frames = WireFrames::new(&self.nodes, stacks_dict);
        let children = self.ordered_children(stacks_dict);
        let mut nodes = Vec::with_capacity(self.nodes.len());
        self.append_flat(0, 0, &frames, &children, &mut nodes);
        FlatFlamegraphTree {
            format: "flat-v1",
            frames: frames.names,
            nodes,
            total_nodes: self.nodes.len(),
            omitted_nodes: 0,
            inspect_retained: None,
        }
    }

    fn projection(
        &self,
        stacks_dict: &StackDictionary,
        children: &OrderedChildren,
        node_budget: usize,
        inspect: Option<&str>,
    ) -> Projection {
        let mut preorder = Vec::with_capacity(self.nodes.len());
        children.append_preorder(0, &mut preorder);
        let mut rank = vec![0usize; self.nodes.len()];
        for (index, node) in preorder.into_iter().enumerate() {
            rank[node] = index;
        }

        let inspected = inspect.and_then(|name| {
            (0..self.nodes.len())
                .filter(|node| stacks_dict.resolve(self.nodes[*node].frame) == name)
                .max_by_key(|node| (self.nodes[*node].count, Reverse(rank[*node])))
        });

        let mut selected = vec![false; self.nodes.len()];
        selected[0] = true;
        if let Some(mut node) = inspected {
            loop {
                selected[node] = true;
                if node == 0 {
                    break;
                }
                node = self.nodes[node].parent;
            }
        }

        let mut omitted_children = vec![0usize; self.nodes.len()];
        let mut selected_nodes = 0usize;
        let mut collapsed_nodes = 0usize;
        for parent in 0..self.nodes.len() {
            if !selected[parent] {
                continue;
            }
            selected_nodes += 1;
            let omitted = children
                .of(parent)
                .iter()
                .filter(|child| !selected[**child])
                .count();
            omitted_children[parent] = omitted;
            collapsed_nodes += usize::from(omitted > 0);
        }

        // A path is normally only a few dozen frames. If a caller supplies an
        // impossibly small test/config budget, prefer honoring the hard cap to
        // claiming that the inspect path was retained.
        let mut inspect_retained = inspected.is_some();
        if selected_nodes + collapsed_nodes > node_budget {
            selected.fill(false);
            selected[0] = true;
            omitted_children.fill(0);
            omitted_children[0] = children.of(0).len();
            selected_nodes = 1;
            collapsed_nodes = usize::from(omitted_children[0] > 0);
            inspect_retained = false;
        }

        let mut output_nodes = selected_nodes + collapsed_nodes;
        let mut frontier = BinaryHeap::new();
        let mut queued = vec![false; self.nodes.len()];
        for parent in 0..self.nodes.len() {
            if !selected[parent] {
                continue;
            }
            for child in children.of(parent) {
                if !selected[*child] && !queued[*child] {
                    frontier.push((self.nodes[*child].count, Reverse(rank[*child]), *child));
                    queued[*child] = true;
                }
            }
        }

        while let Some((_, _, node)) = frontier.pop() {
            let parent = self.nodes[node].parent;
            debug_assert!(selected[parent]);
            let parent_still_collapsed = omitted_children[parent] > 1;
            let child_collapsed = !children.of(node).is_empty();
            // Selecting this node adds one real row, may remove its parent's
            // `[other]`, and may add a new `[other]` beneath the selected node.
            let delta = usize::from(parent_still_collapsed) + usize::from(child_collapsed);
            if output_nodes + delta > node_budget {
                continue;
            }

            selected[node] = true;
            selected_nodes += 1;
            output_nodes += delta;
            omitted_children[parent] -= 1;
            omitted_children[node] = children.of(node).len();
            for child in children.of(node) {
                frontier.push((self.nodes[*child].count, Reverse(rank[*child]), *child));
            }
        }

        Projection {
            selected,
            output_nodes,
            omitted_nodes: self.nodes.len() - selected_nodes,
            inspect_retained: inspect.map(|_| inspect_retained),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn append_projected_flat(
        &self,
        node_id: usize,
        parent_wire_id: u32,
        stacks_dict: &StackDictionary,
        frames: &WireFrames,
        children: &OrderedChildren,
        projection: &Projection,
        rows: &mut Vec<FlatFlamegraphNode>,
    ) {
        let wire_id =
            u32::try_from(rows.len()).expect("a flamegraph cannot contain more than u32 nodes");
        let node = &self.nodes[node_id];
        rows.push(FlatFlamegraphNode {
            parent: if node_id == 0 {
                wire_id
            } else {
                parent_wire_id
            },
            frame: frames.ids[&node.frame],
            count: node.count,
            self_count: node.self_count,
        });

        let omitted_count: u64 = children
            .of(node_id)
            .iter()
            .filter(|child| !projection.selected[**child])
            .map(|child| self.nodes[*child].count)
            .sum();
        let mut emitted_other = omitted_count == 0;
        for child in children
            .of(node_id)
            .iter()
            .filter(|child| projection.selected[**child])
        {
            let child_node = &self.nodes[*child];
            if !emitted_other
                && (omitted_count > child_node.count
                    || (omitted_count == child_node.count
                        && "[other]" < stacks_dict.resolve(child_node.frame)))
            {
                rows.push(FlatFlamegraphNode {
                    parent: wire_id,
                    frame: frames.other.expect("projected trees intern [other]"),
                    count: omitted_count,
                    self_count: omitted_count,
                });
                emitted_other = true;
            }
            self.append_projected_flat(
                *child,
                wire_id,
                stacks_dict,
                frames,
                children,
                projection,
                rows,
            );
        }
        if !emitted_other {
            rows.push(FlatFlamegraphNode {
                parent: wire_id,
                frame: frames.other.expect("projected trees intern [other]"),
                count: omitted_count,
                self_count: omitted_count,
            });
        }
    }

    fn into_projected_flat_tree(
        mut self,
        stacks_dict: &StackDictionary,
        node_budget: usize,
        inspect: Option<&str>,
    ) -> FlatFlamegraphTree {
        self.edges = HashMap::new();
        let children = self.ordered_children(stacks_dict);
        let projection = self.projection(stacks_dict, &children, node_budget, inspect);
        let frames = WireFrames::new_selected(&self.nodes, stacks_dict, &projection.selected, true);
        let mut nodes = Vec::with_capacity(projection.output_nodes);
        self.append_projected_flat(
            0,
            0,
            stacks_dict,
            &frames,
            &children,
            &projection,
            &mut nodes,
        );
        debug_assert_eq!(nodes.len(), projection.output_nodes);
        FlatFlamegraphTree {
            format: "flat-v1",
            frames: frames.names,
            nodes,
            total_nodes: self.nodes.len(),
            omitted_nodes: projection.omitted_nodes,
            inspect_retained: projection.inspect_retained,
        }
    }
}

struct OrderedChildren {
    nodes: Vec<usize>,
    starts: Vec<usize>,
}

impl OrderedChildren {
    fn of(&self, parent: usize) -> &[usize] {
        &self.nodes[self.starts[parent]..self.starts[parent + 1]]
    }

    fn append_preorder(&self, parent: usize, output: &mut Vec<usize>) {
        output.push(parent);
        for child in self.of(parent) {
            self.append_preorder(*child, output);
        }
    }
}

struct Projection {
    selected: Vec<bool>,
    output_nodes: usize,
    omitted_nodes: usize,
    inspect_retained: Option<bool>,
}

struct WireFrames {
    names: Vec<String>,
    ids: HashMap<FrameId, u32>,
    other: Option<u32>,
}

impl WireFrames {
    #[cfg(test)]
    fn new(nodes: &[TrieNode], stacks_dict: &StackDictionary) -> Self {
        Self::new_selected(nodes, stacks_dict, &vec![true; nodes.len()], false)
    }

    fn new_selected(
        nodes: &[TrieNode],
        stacks_dict: &StackDictionary,
        selected: &[bool],
        include_other: bool,
    ) -> Self {
        let mut frames: Vec<_> = nodes
            .iter()
            .zip(selected)
            .filter_map(|(node, selected)| selected.then_some(node.frame))
            .collect();
        frames.sort_unstable_by(|a, b| stacks_dict.resolve(*a).cmp(stacks_dict.resolve(*b)));
        frames.dedup();
        let ids = frames
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                (
                    *frame,
                    u32::try_from(index).expect("a flamegraph cannot contain more than u32 frames"),
                )
            })
            .collect();
        let mut names: Vec<String> = frames
            .iter()
            .map(|frame| stacks_dict.resolve(*frame).to_string())
            .collect();
        let other = include_other.then(|| {
            if let Some(index) = names.iter().position(|name| name == "[other]") {
                u32::try_from(index).expect("a flamegraph cannot contain more than u32 frames")
            } else {
                let index = u32::try_from(names.len())
                    .expect("a flamegraph cannot contain more than u32 frames");
                names.push("[other]".to_string());
                index
            }
        });
        Self { names, ids, other }
    }
}

/// Handler for GET /api/flamegraph — a Server-Sent Events stream.
///
/// [Resolves](refine::resolve) the scope, primes an incremental
/// [`FlamegraphAccum`] over the already-folded set, and emits initial coverage.
/// Then it folds the not-yet-folded capped files in [order key] order (up to the
/// [sampling cap](refine)), pushing coverage as each file lands, bounded partial
/// trees at exponential checkpoints, and one full bounded tree when the
/// work-list drains. There is no re-polling.
///
/// The aggregation context comes from [`AppState::agg_context_for`]: a `bucket`
/// param builds a per-request bring-your-own-credentials context; otherwise the
/// server's `--agg` context is used. Absent both → 404.
///
/// [order key]: aggregate::order_key
pub async fn get_flamegraph(
    State(state): State<AppState>,
    creds: MaybeCreds,
    ux: Option<Extension<UxRequestLifecycle>>,
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

    let stream = track_sse_ux(
        flamegraph_stream(agg, resolved, &params, state.fold_limits.clone()),
        ux.map(|Extension(ux)| ux),
    );
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
    wire_format: WireFormat,
    inspect: Option<String>,
}

#[derive(Clone, Copy)]
enum WireFormat {
    Legacy,
    FlatV1,
}

#[derive(Clone, Copy)]
enum TreeEventKind {
    Partial,
    Final,
}

impl TreeEventKind {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Partial => "partial",
            Self::Final => "final",
        }
    }
}

struct PartialTreeCadence {
    next_files: Cell<usize>,
}

impl PartialTreeCadence {
    fn new() -> Self {
        Self {
            next_files: Cell::new(1),
        }
    }

    fn should_emit(&self, files_folded: usize, total_samples: usize) -> bool {
        let next = self.next_files.get();
        if total_samples == 0 || files_folded < next {
            return false;
        }

        let next = files_folded
            .checked_add(1)
            .and_then(usize::checked_next_power_of_two)
            .unwrap_or(usize::MAX);
        self.next_files.set(next);
        true
    }
}

/// The flamegraph [`FoldSink`] adapter: owns the incremental [`FlamegraphAccum`]
/// and the per-request [`StreamCtx`]. Supplies the three operations that differ
/// from span-stats; the folded-set discipline lives in the driver.
struct FlamegraphSink {
    ctx: StreamCtx,
    accum: FlamegraphAccum,
    partial_cadence: PartialTreeCadence,
}

impl FlamegraphSink {
    fn coverage(
        &self,
        resolved: &Resolved,
        files_folded: usize,
        folded_set_id: &str,
        target_folded_set_id: Option<&str>,
        hosts_folded: usize,
        errors: &FoldErrors,
    ) -> Coverage {
        fold_stream::coverage_from(
            resolved,
            files_folded,
            folded_set_id,
            target_folded_set_id,
            hosts_folded,
            errors,
            self.accum.total_samples(),
        )
    }

    fn tree_event(&self, coverage: Coverage, kind: TreeEventKind) -> Event {
        let snap = self.accum.snapshot();
        let resp = build_response(&self.ctx, &snap, coverage, kind);
        flamegraph_json_event(&resp)
    }
}

fn flamegraph_json_event(value: &impl Serialize) -> Event {
    // Serialization only fails for an unsupported value. Keep the stream alive
    // with a comment so one bad event cannot tear down an in-flight fold.
    Event::default().json_data(value).unwrap_or_else(|e| {
        fold_stream::rate_limited_warn("flamegraph: event serialize failed", &anyhow::anyhow!(e));
        Event::default().comment("serialize error")
    })
}

impl fold_stream::FoldSink for FlamegraphSink {
    fn seed_batch_size(&self, first_batch: bool) -> usize {
        if first_batch && matches!(self.ctx.wire_format, WireFormat::FlatV1) {
            1
        } else {
            Self::SEED_BATCH_SIZE
        }
    }

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
        folded_set_id: &str,
        target_folded_set_id: Option<&str>,
        hosts_folded: usize,
        errors: &FoldErrors,
    ) -> Event {
        let coverage = self.coverage(
            resolved,
            files_folded,
            folded_set_id,
            target_folded_set_id,
            hosts_folded,
            errors,
        );
        match self.ctx.wire_format {
            WireFormat::FlatV1
                if self
                    .partial_cadence
                    .should_emit(files_folded, self.accum.total_samples()) =>
            {
                self.tree_event(coverage, TreeEventKind::Partial)
            }
            WireFormat::FlatV1 => flamegraph_json_event(&FlamegraphCoverage {
                kind: "coverage",
                total_samples: self.accum.total_samples(),
                coverage,
            }),
            WireFormat::Legacy => self.tree_event(coverage, TreeEventKind::Final),
        }
    }

    fn final_event(
        &self,
        resolved: &Resolved,
        files_folded: usize,
        folded_set_id: &str,
        target_folded_set_id: Option<&str>,
        hosts_folded: usize,
        errors: &FoldErrors,
    ) -> Option<Event> {
        matches!(self.ctx.wire_format, WireFormat::FlatV1).then(|| {
            let coverage = self.coverage(
                resolved,
                files_folded,
                folded_set_id,
                target_folded_set_id,
                hosts_folded,
                errors,
            );
            self.tree_event(coverage, TreeEventKind::Final)
        })
    }
}

/// Build the SSE event stream for one flamegraph request.
///
/// Cached already-folded parts are merged in bounded batches, with coverage
/// emitted after each batch. Once cached state is exhausted, each later step
/// pulls one file off [`refine::fold_stream`], reads + merges its part-files,
/// and emits refined coverage. Flat-v1 adds bounded full-depth trees at exponential
/// file-coverage checkpoints and one final tree after the work-list drains;
/// legacy formats retain cumulative trees. Dropping the returned stream
/// (client disconnect) drops the fold stream, cancelling in-flight folds.
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
        wire_format: match params.format.as_deref() {
            Some("flat-v1") => WireFormat::FlatV1,
            _ => WireFormat::Legacy,
        },
        inspect: params.inspect.clone(),
    };
    let accum = FlamegraphAccum::new(ctx.filter.clone());
    fold_stream::drive(
        agg,
        resolved,
        limits,
        FlamegraphSink {
            ctx,
            accum,
            partial_cadence: PartialTreeCadence::new(),
        },
    )
}

/// Shape a [`FlamegraphResponse`] from an [`AggSnapshot`] + [`Coverage`].
fn build_response(
    ctx: &StreamCtx,
    snap: &AggSnapshot,
    coverage: Coverage,
    kind: TreeEventKind,
) -> FlamegraphResponse {
    // Build complete stacks before projection: depth cutoffs invent caller
    // self-time and can hide an already-loaded inspect frame during refinement.
    let trie = build_flamegraph_tree(&snap.stack_counts, snap.stacks_dict);
    let tree = match ctx.wire_format {
        WireFormat::Legacy => FlamegraphTree::Legacy(trie.into_legacy(snap.stacks_dict)),
        WireFormat::FlatV1 => FlamegraphTree::Flat(trie.into_projected_flat_tree(
            snap.stacks_dict,
            DEFAULT_FLAMEGRAPH_NODE_BUDGET,
            ctx.inspect.as_deref(),
        )),
    };

    // Echo the active filter values back to the UI (facet name → selected value).
    let filters: HashMap<String, String> = ctx
        .filter
        .facets
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();

    FlamegraphResponse {
        kind: matches!(ctx.wire_format, WireFormat::FlatV1).then_some(kind.wire_name()),
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
        let mut stacks = HashMap::new();
        // Stack A: main → foo → bar (leaf→root stored as bar, foo, main)
        let stack_a = [0u8; 16];
        stacks.insert(
            stack_a,
            vec!["bar".to_string(), "foo".to_string(), "main".to_string()],
        );
        // Stack B: main → foo → baz
        let mut stack_b = [0u8; 16];
        stack_b[0] = 1;
        stacks.insert(
            stack_b,
            vec!["baz".to_string(), "foo".to_string(), "main".to_string()],
        );
        let stacks_dict = StackDictionary::from_stacks(stacks);

        let stack_counts = vec![(stack_a, 10), (stack_b, 5)];
        let tree = build_flamegraph_tree(&stack_counts, &stacks_dict).into_legacy(&stacks_dict);

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
    fn flat_tree_serializes_each_frame_name_once() {
        let repeated = "very_long_symbol_name_".repeat(64);
        let mut stacks = HashMap::new();
        for i in 0..8u8 {
            let mut stack_id = [0u8; 16];
            stack_id[0] = i;
            stacks.insert(
                stack_id,
                vec![format!("leaf_{i}"), repeated.clone(), format!("root_{i}")],
            );
        }
        let stacks_dict = StackDictionary::from_stacks(stacks);
        let stack_counts: Vec<_> = (0..8u8)
            .map(|i| {
                let mut stack_id = [0u8; 16];
                stack_id[0] = i;
                (stack_id, 1)
            })
            .collect();

        let tree = build_flamegraph_tree(&stack_counts, &stacks_dict).into_flat_tree(&stacks_dict);
        let json = serde_json::to_string(&tree).unwrap();

        assert_eq!(
            json.matches(&repeated).count(),
            1,
            "the frame table must own the repeated symbol once"
        );
        assert_eq!(tree.nodes[0].count, 8);
        assert_eq!(
            tree.frames.iter().filter(|name| *name == &repeated).count(),
            1
        );
    }

    #[test]
    fn flat_tree_preserves_counts_and_parent_order() {
        let mut stacks = HashMap::new();
        let a = [1u8; 16];
        let b = [2u8; 16];
        stacks.insert(
            a,
            vec!["left".to_string(), "main".to_string(), "root".to_string()],
        );
        stacks.insert(
            b,
            vec!["right".to_string(), "main".to_string(), "root".to_string()],
        );
        let stacks_dict = StackDictionary::from_stacks(stacks);
        let tree =
            build_flamegraph_tree(&[(a, 4), (b, 3)], &stacks_dict).into_flat_tree(&stacks_dict);

        assert_eq!(tree.format, "flat-v1");
        assert_eq!(tree.nodes.len(), 5);
        let root = &tree.nodes[0];
        assert_eq!((root.parent, root.count, root.self_count), (0, 7, 0));
        for (index, node) in tree.nodes.iter().enumerate().skip(1) {
            assert!(
                usize::try_from(node.parent).unwrap() < index,
                "node {index} must follow parent {}",
                node.parent
            );
        }
        let self_total: u64 = tree.nodes.iter().map(|node| node.self_count).sum();
        assert_eq!(self_total, 7);
    }

    #[test]
    fn flat_tree_is_deterministic_across_input_order() {
        let mut stacks = HashMap::new();
        let a = [1u8; 16];
        let b = [2u8; 16];
        stacks.insert(a, vec!["z-leaf".to_string(), "shared".to_string()]);
        stacks.insert(b, vec!["a-leaf".to_string(), "shared".to_string()]);
        let stacks_dict = StackDictionary::from_stacks(stacks);

        let first =
            build_flamegraph_tree(&[(a, 5), (b, 5)], &stacks_dict).into_flat_tree(&stacks_dict);
        let second =
            build_flamegraph_tree(&[(b, 5), (a, 5)], &stacks_dict).into_flat_tree(&stacks_dict);

        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
    }

    /// The rayon work-stealing fan-out shape that motivated the compact wire
    /// formats: a handful of very long monomorphized symbols recombining into
    /// thousands of distinct root→leaf paths, so legacy nodes repeat a whole
    /// symbol at every one of them.
    ///
    /// The shape is synthesized rather than sampled from a live rayon pool.
    /// Sampling made every byte count here depend on the host — core count sets
    /// how deep rayon actually splits, so CI's small runners produced a
    /// different tree than a workstation and the ratios below failed there. The
    /// frame names are real monomorphizations captured from
    /// `examples/rayon-fanout`, which is where the end-to-end measurement on a
    /// genuine trace lives.
    mod rayon_fanout {
        use super::*;
        use std::collections::{HashMap, HashSet};

        /// Real rayon monomorphizations (204..945 bytes) captured from the
        /// example binary's symbol table.
        fn frame_names() -> Vec<&'static str> {
            include_str!("testdata/rayon_symbols.txt")
                .lines()
                .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
                .collect()
        }

        /// How deep the synthetic split tree goes. Each level doubles the path
        /// count, so this is the dial on fan-out: depth 11 is 2,048 leaves.
        const SPLIT_DEPTH: u32 = 11;

        /// The `(dictionary, stack_counts)` pair the tree builder consumes,
        /// describing a `SPLIT_DEPTH`-deep binary split tree.
        ///
        /// Every level draws its two frames from `frame_names()`, cycling — the
        /// left/right pair at a level stands in for the two closures of one
        /// `join_context` monomorphization, which is where rayon's real path
        /// cardinality comes from. Because the pool of names is far smaller than
        /// the depth, ~10 distinct symbols cover 2,048 distinct paths.
        ///
        /// Samples are taken at every depth from 4 up, not just at the leaves,
        /// standing in for a sampler that catches threads mid-split.
        fn fan_out_stacks() -> (StackDictionary, Vec<([u8; 16], u64)>) {
            let names = frame_names();
            let mut stacks: HashMap<[u8; 16], Vec<String>> = HashMap::new();
            let mut stack_counts = Vec::new();

            for depth in 4..=SPLIT_DEPTH {
                for path in 0..(1u32 << depth) {
                    // Leaf→root order, the orientation `StackDictionary` wants.
                    let mut frames = vec![format!("score_document_{}", path % 3)];
                    for level in (0..depth).rev() {
                        let side = usize::from(path >> level & 1 == 1);
                        frames.push(names[(2 * level as usize + side) % names.len()].to_string());
                    }
                    frames.push("rayon_fanout::handle_request".to_string());

                    let mut id = [0u8; 16];
                    id[..4].copy_from_slice(&path.to_le_bytes());
                    id[4] = depth as u8;
                    stacks.insert(id, frames);
                    // Uneven weights so sibling ordering is exercised.
                    stack_counts.push((id, u64::from(path % 7) + 1));
                }
            }

            stack_counts.sort_unstable();
            (StackDictionary::from_stacks(stacks), stack_counts)
        }

        /// Guards the fixture's defining property, so an edit that accidentally
        /// flattened it could not silently weaken the size assertions below:
        /// the fan-out is in the paths, not in the symbol count.
        #[test]
        fn a_few_rayon_symbols_recombine_into_many_distinct_paths() {
            let (dict, stack_counts) = fan_out_stacks();
            let distinct_stacks = stack_counts.len();
            let distinct_frames: HashSet<_> = stack_counts
                .iter()
                .filter_map(|(id, _)| dict.get(id))
                .flatten()
                .collect();

            assert!(
                distinct_stacks >= 64,
                "split depths should produce many distinct stacks, got {distinct_stacks}"
            );
            assert!(
                distinct_frames.len() * 4 <= distinct_stacks,
                "the fan-out is paths, not symbols: {} frames over {distinct_stacks} stacks",
                distinct_frames.len()
            );
        }

        #[test]
        fn flat_rows_shrink_the_rayon_fan_out_wire_size() {
            let (dict, stack_counts) = fan_out_stacks();
            let nodes = build_flamegraph_tree(&stack_counts, &dict).nodes.len();

            let legacy = serde_json::to_string(
                &build_flamegraph_tree(&stack_counts, &dict).into_legacy(&dict),
            )
            .unwrap()
            .len();
            let flat = serde_json::to_string(
                &build_flamegraph_tree(&stack_counts, &dict).into_flat_tree(&dict),
            )
            .unwrap()
            .len();

            println!(
                "rayon fan-out: {} stacks, {nodes} trie nodes\n  \
                 legacy {legacy:>10} bytes ({:>6.1} B/node)\n  \
                 flat   {flat:>10} bytes ({:>6.1} B/node)",
                stack_counts.len(),
                legacy as f64 / nodes as f64,
                flat as f64 / nodes as f64,
            );

            // The frame table absorbs the repeated monomorphized symbols and the
            // rows drop the per-node property names, so per-node cost falls from
            // "one whole symbol" to four integers and six delimiters. This
            // fixture measures 16.8x (235.1 -> 14.0 B/node); the bound leaves
            // room for frame-name churn without letting a regression to
            // per-node names through.
            assert!(
                flat * 8 < legacy,
                "flat rows should be an order of magnitude under legacy: {flat} vs {legacy}"
            );
            assert!(
                flat / nodes < 32,
                "a flat row should cost tens of bytes, not hundreds: {} B/node",
                flat / nodes
            );
        }

        /// Both encodings must describe the same tree. Rebuilding the
        /// nested form from the flat rows — the same walk the UI decoder does —
        /// and comparing against the legacy response pins parent order, frame
        /// resolution, and count placement over 20k+ real fan-out nodes.
        #[test]
        fn flat_rows_rebuild_the_legacy_rayon_tree() {
            let (dict, stack_counts) = fan_out_stacks();
            let flat = build_flamegraph_tree(&stack_counts, &dict).into_flat_tree(&dict);

            let mut children_of = vec![Vec::new(); flat.nodes.len()];
            for (index, node) in flat.nodes.iter().enumerate().skip(1) {
                children_of[usize::try_from(node.parent).unwrap()].push(index);
            }
            fn nest(
                index: usize,
                tree: &FlatFlamegraphTree,
                children_of: &[Vec<usize>],
            ) -> serde_json::Value {
                let node = &tree.nodes[index];
                let mut value = serde_json::json!({
                    "name": tree.frames[node.frame as usize],
                    "count": node.count,
                    "self": node.self_count,
                });
                if !children_of[index].is_empty() {
                    value["children"] = children_of[index]
                        .iter()
                        .map(|child| nest(*child, tree, children_of))
                        .collect();
                }
                value
            }

            assert_eq!(
                nest(0, &flat, &children_of),
                serde_json::to_value(
                    build_flamegraph_tree(&stack_counts, &dict).into_legacy(&dict)
                )
                .unwrap()
            );
        }

        /// The SSE stream re-sends the whole tree after every folded file, so the
        /// bytes a client actually receives are the per-event size times the
        /// refinement count — the number the encoding has to move.
        #[test]
        fn flat_rows_shrink_the_streamed_sse_total() {
            let (dict, stack_counts) = fan_out_stacks();
            // Stand in for a scope that folds in eight refinements, each adding
            // a shard of the stack population.
            let events = 8usize;
            let mut legacy_total = 0usize;
            let mut flat_total = 0usize;
            let mut flat_largest = 0usize;
            for event in 1..=events {
                let seen = &stack_counts[..stack_counts.len() * event / events];
                legacy_total +=
                    serde_json::to_string(&build_flamegraph_tree(seen, &dict).into_legacy(&dict))
                        .unwrap()
                        .len();
                let flat = serde_json::to_string(
                    &build_flamegraph_tree(seen, &dict).into_flat_tree(&dict),
                )
                .unwrap()
                .len();
                flat_total += flat;
                flat_largest = flat_largest.max(flat);
            }

            println!(
                "rayon fan-out over {events} SSE events: legacy {legacy_total} bytes, \
                 flat {flat_total} bytes (largest single event {flat_largest})"
            );

            assert!(
                flat_total * 8 < legacy_total,
                "flat rows should be an order of magnitude under the legacy streamed total: \
                 {flat_total} vs {legacy_total}"
            );
            // Re-sending the full tree per refinement still dominates: the
            // stream costs several times its own final event in either encoding.
            assert!(
                flat_total > flat_largest * 3,
                "the per-event resend multiplier is the remaining cost: \
                 {flat_total} over a {flat_largest}-byte largest event"
            );
        }

        #[test]
        fn flat_rows_conserve_the_rayon_fan_out_counts() {
            let (dict, stack_counts) = fan_out_stacks();
            let total: u64 = stack_counts.iter().map(|(_, count)| count).sum();
            let tree = build_flamegraph_tree(&stack_counts, &dict).into_flat_tree(&dict);

            assert_eq!(tree.nodes[0].count, total, "root must carry every sample");
            assert_eq!(
                tree.nodes.iter().map(|node| node.self_count).sum::<u64>(),
                total,
                "self counts must sum to the sample total"
            );
            for (index, node) in tree.nodes.iter().enumerate().skip(1) {
                let parent = usize::try_from(node.parent).unwrap();
                assert!(parent < index, "node {index} must follow parent {parent}");
                assert!(
                    node.count <= tree.nodes[parent].count,
                    "node {index} cannot outweigh its parent"
                );
            }
        }
    }

    #[test]
    fn projected_tree_respects_budget_and_conserves_collapsed_counts() {
        let mut stacks = HashMap::new();
        let mut stack_counts = Vec::new();
        for i in 0..20u8 {
            let mut stack_id = [0u8; 16];
            stack_id[0] = i;
            stacks.insert(stack_id, vec![format!("leaf-{i}"), format!("branch-{i}")]);
            stack_counts.push((stack_id, u64::from(i) + 1));
        }
        let stacks_dict = StackDictionary::from_stacks(stacks);
        let tree = build_flamegraph_tree(&stack_counts, &stacks_dict).into_projected_flat_tree(
            &stacks_dict,
            10,
            None,
        );

        assert!(tree.nodes.len() <= 10);
        assert_eq!(tree.total_nodes, 41);
        assert!(tree.omitted_nodes > 0);
        assert!(tree.frames.iter().any(|name| name == "[other]"));

        let mut child_totals = vec![0u64; tree.nodes.len()];
        for (index, node) in tree.nodes.iter().enumerate().skip(1) {
            child_totals[usize::try_from(node.parent).unwrap()] += node.count;
            assert!(usize::try_from(node.parent).unwrap() < index);
        }
        for (index, node) in tree.nodes.iter().enumerate() {
            assert_eq!(
                node.count,
                node.self_count + child_totals[index],
                "projected node {index} must conserve its inclusive count"
            );
        }
    }

    #[test]
    fn projected_tree_retains_inspected_path() {
        let hot = [1u8; 16];
        let cold = [2u8; 16];
        let mut stacks = HashMap::new();
        stacks.insert(hot, vec!["hot-leaf".to_string(), "hot-root".to_string()]);
        stacks.insert(
            cold,
            vec!["focus-frame".to_string(), "cold-root".to_string()],
        );
        let stacks_dict = StackDictionary::from_stacks(stacks);
        let tree = build_flamegraph_tree(&[(hot, 1_000), (cold, 1)], &stacks_dict)
            .into_projected_flat_tree(&stacks_dict, 4, Some("focus-frame"));

        let emitted_names: Vec<_> = tree
            .nodes
            .iter()
            .map(|node| tree.frames[usize::try_from(node.frame).unwrap()].as_str())
            .collect();
        assert_eq!(tree.inspect_retained, Some(true));
        assert!(emitted_names.contains(&"cold-root"));
        assert!(emitted_names.contains(&"focus-frame"));
        assert!(tree.nodes.len() <= 4);
    }

    #[test]
    fn projected_tree_is_deterministic_across_input_order() {
        let mut stacks = HashMap::new();
        let a = [1u8; 16];
        let b = [2u8; 16];
        let c = [3u8; 16];
        stacks.insert(a, vec!["z-leaf".to_string(), "shared".to_string()]);
        stacks.insert(b, vec!["a-leaf".to_string(), "shared".to_string()]);
        stacks.insert(c, vec!["focus".to_string(), "cold".to_string()]);
        let stacks_dict = StackDictionary::from_stacks(stacks);

        let first = build_flamegraph_tree(&[(a, 5), (b, 5), (c, 1)], &stacks_dict)
            .into_projected_flat_tree(&stacks_dict, 6, Some("focus"));
        let second = build_flamegraph_tree(&[(c, 1), (b, 5), (a, 5)], &stacks_dict)
            .into_projected_flat_tree(&stacks_dict, 6, Some("focus"));

        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
    }

    #[test]
    fn partial_tree_cadence_advances_at_power_of_two_crossings() {
        let cadence = PartialTreeCadence::new();

        assert!(!cadence.should_emit(0, 0));
        assert!(cadence.should_emit(1, 10));
        assert!(!cadence.should_emit(1, 10));
        assert!(cadence.should_emit(2, 20));
        assert!(!cadence.should_emit(3, 30));
        assert!(cadence.should_emit(25, 250));
        assert!(!cadence.should_emit(25, 250));
        assert_eq!(cadence.next_files.get(), 32);
    }

    #[rstest::rstest]
    #[case(32, 1)]
    #[case(2, 2_500)]
    #[case(2, DEFAULT_FLAMEGRAPH_NODE_BUDGET + 10)]
    fn streamed_tree_matches_final_projection(#[case] depth: usize, #[case] branches: usize) {
        let mut stacks = HashMap::new();
        let mut stack_counts = Vec::new();
        for branch in 0..branches {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&(branch as u64).to_le_bytes());
            let mut frames = vec![format!("leaf-{branch}")];
            frames.extend((1..depth).map(|level| format!("caller-{level}")));
            stacks.insert(id, frames);
            stack_counts.push((id, 1));
        }
        let stacks_dict = StackDictionary::from_stacks(stacks);
        let snapshot = AggSnapshot {
            stack_counts,
            stacks_dict: &stacks_dict,
            total_samples: branches,
            hosts: 1,
            min_ts: Some(1),
            max_ts: Some(2),
            facets: Vec::new(),
            poll_duration_histogram: Vec::new(),
        };
        let ctx = StreamCtx {
            filter: SampleFilter::default(),
            service: None,
            hosts: Vec::new(),
            from: None,
            to: None,
            start_ns: None,
            end_ns: None,
            min_poll_ns: None,
            max_poll_ns: None,
            wire_format: WireFormat::FlatV1,
            inspect: Some("leaf-0".to_string()),
        };
        let coverage = Coverage {
            files_matched: 32,
            files_folded: 32,
            folded_set_id: None,
            target_folded_set_id: None,
            fold_work_cap: 128,
            samples_folded: branches,
            total_bytes: 0,
            hosts_matched: 1,
            hosts_folded: 1,
            fold_errors: 0,
            fold_error_sample: None,
        };

        // Cached reconstruction can emit a partial at exactly the coverage of
        // the completed view. It must preserve that view's paths and self-time.
        let partial = build_response(&ctx, &snapshot, coverage.clone(), TreeEventKind::Partial);
        let final_response = build_response(&ctx, &snapshot, coverage, TreeEventKind::Final);
        assert_eq!(
            serde_json::to_value(&partial.tree).unwrap(),
            serde_json::to_value(&final_response.tree).unwrap()
        );
        let FlamegraphTree::Flat(tree) = partial.tree else {
            panic!("flat-v1 request must return a flat tree");
        };
        assert_eq!(tree.inspect_retained, Some(true));
        assert!(tree.nodes.len() <= DEFAULT_FLAMEGRAPH_NODE_BUDGET);
        assert_eq!(
            tree.omitted_nodes == 0,
            branches + depth <= DEFAULT_FLAMEGRAPH_NODE_BUDGET
        );
        assert_eq!(
            tree.nodes.iter().map(|node| node.self_count).sum::<u64>(),
            branches as u64
        );
        for node in &tree.nodes {
            let name = &tree.frames[node.frame as usize];
            assert_eq!(
                node.self_count,
                if name == "[other]" {
                    node.count
                } else {
                    u64::from(name.starts_with("leaf-"))
                },
                "only sampled leaves and explicit omissions have self-time: {name}"
            );
        }
    }

    #[test]
    fn coverage_event_carries_no_tree_or_metadata() {
        let event = FlamegraphCoverage {
            kind: "coverage",
            total_samples: 42,
            coverage: Coverage {
                files_matched: 10,
                files_folded: 3,
                folded_set_id: Some("folded".to_string()),
                target_folded_set_id: None,
                fold_work_cap: 4,
                samples_folded: 42,
                total_bytes: 1_024,
                hosts_matched: 2,
                hosts_folded: 1,
                fold_errors: 0,
                fold_error_sample: None,
            },
        };

        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json["kind"], "coverage");
        assert_eq!(json["total_samples"], 42);
        assert_eq!(json["coverage"]["files_folded"], 3);
        assert!(json.get("tree").is_none());
        assert!(json.get("metadata").is_none());
    }
}

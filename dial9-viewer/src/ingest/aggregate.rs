//! Sampling based, demand-driven aggregation
//!
//! Instead of batch-aggregating an entire window up front, a query folds
//! [source files] one at a time, in a deterministic pseudo-random [order key]
//! order that is uniform across host and time, so the first few files are a
//! representative spread of the scope. Results are served over whatever subset
//! has been folded so far, with a [coverage] report, and refined as more files
//! fold.
//!
//! Key properties (see `docs/adr/0003-folded-set-is-the-output-listing.md`):
//!
//! - **The output part-file's existence is the record that a file is folded.**
//!   There is no manifest and no skip-set. A zero-sample file still writes an
//!   empty part-file, so it is never re-fetched.
//! - **Folding is idempotent.** A source file folds to a deterministically
//!   named part-file (`samples/service=…/date=…/host=…/{blake3(source_key)}`),
//!   so re-folding writes the same key.
//! - **Aggregation reads part-files through the `StorageBackend`**, so it works
//!   identically over S3, the local FS, and the simulated S3 used in tests.
//!
//! [source files]: crate::ingest::aggregate
//! [order key]: order_key
//! [coverage]: Coverage

use crate::storage::{ObjectInfo, StorageBackend};
use arrow::array::Array;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Semaphore;

use super::decode;
use super::parquet_writer;

/// Scheduling version baked into the [`order_key`] hash input. Bump to change
/// the fetch-order permutation. Lives ONLY here, never in an output path: the
/// folded samples are order-independent and must survive a bump untouched.
pub(crate) const ORDER_VERSION: u32 = 1;

/// Storage-format version baked into the output key path
/// (`{output_prefix}/v{N}/…`). Bump when changing *what* we persist and we want
/// a deliberate recompute; reads/writes then target a fresh empty tree that
/// repopulates lazily. The old tree is abandoned and GC'd out-of-band.
pub(crate) const SAMPLES_FORMAT_VERSION: u32 = 3;

/// Default raw-trace segment duration, in seconds. A source file covers
/// `[epoch, epoch + segment_duration)`; the [`Scope`] time filter pads by this
/// so a file that *started* just before the window but runs into it is not
/// dropped. Configurable per deployment (the producer's rotation period).
pub(crate) const DEFAULT_SEGMENT_DURATION_SECS: i64 = 60;

/// The deterministic pseudo-random total order over source files:
/// `BLAKE3(ORDER_VERSION_le ++ source_key)`. Uniform across host and time, so
/// the first K files in this order are a representative spread of a scope
/// rather than one host's earliest minutes.
fn order_key(source_key: &str) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&ORDER_VERSION.to_le_bytes());
    hasher.update(source_key.as_bytes());
    let mut id = [0u8; 16];
    id.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    id
}

/// Content-addressed leaf name for a source file's output part-files: the first
/// 128 bits of `BLAKE3(source_key)`, hex-encoded (32 chars). Stable + unique per
/// source file, so folding is idempotent; the part-files are named after this
/// leaf, so it is also the membership token of the folded set. 128 bits is
/// ample collision resistance for any realistic file count (same width as
/// `stack_id`), and the shorter leaf keeps the partitioned key well within
/// filesystem path-component limits.
pub(crate) fn part_leaf_of(source_key: &str) -> String {
    let hash = blake3::hash(source_key.as_bytes());
    hash.to_hex()[..32].to_string()
}

/// `{output_prefix}/v{SAMPLES_FORMAT_VERSION}/bucket={source_bucket}` — the root
/// of the current storage-format generation for one source bucket. The output
/// store is namespaced by source bucket so that bring-your-own-credentials
/// sources fold into isolated, independently-prunable/GC-able trees and the
/// folded-set LIST never mixes buckets.
fn versioned_root(output_prefix: &str, source_bucket: &str) -> String {
    let p = output_prefix.trim_end_matches('/');
    debug_assert!(!p.is_empty(), "output_prefix must not be empty");
    let bucket = bucket_segment(source_bucket);
    format!("{p}/v{SAMPLES_FORMAT_VERSION}/bucket={bucket}")
}

/// The source bucket a `source_key` came from: the `{bucket}` in
/// `s3://{bucket}/{key}`, or `"local"` for a bare (local-FS) key. This is what
/// namespaces the output path, so it is derived from the key itself — the
/// per-file path functions never need it threaded in separately.
fn parse_source_bucket(source_key: &str) -> String {
    if let Some(rest) = source_key.strip_prefix("s3://") {
        rest.split_once('/')
            .map(|(b, _)| b)
            .unwrap_or(rest)
            .to_string()
    } else {
        "local".to_string()
    }
}

/// Sanitize a bucket name for use as a path segment. S3 bucket names are already
/// path-safe (lowercase, digits, `-`, `.`), but guard against `/` defensively.
fn bucket_segment(source_bucket: &str) -> String {
    source_bucket.replace('/', "_")
}

/// Output key for a source file's samples part-file: a Hive-partitioned path
/// (`bucket=…/samples/service=…/date=…/host=…/{hash}.parquet`) so the folded-set
/// LIST is scope-prunable and DataFusion-style partition pruning is possible.
/// The hash is only the leaf; the source bucket is derived from `source_key`.
fn samples_part_key(output_prefix: &str, source_key: &str) -> String {
    let (date, service, host) = parse_scope_fields(source_key);
    format!(
        "{root}/samples/service={service}/date={date}/host={host}/{leaf}.parquet",
        root = versioned_root(output_prefix, &parse_source_bucket(source_key)),
        leaf = part_leaf_of(source_key),
    )
}

/// Output key for a source file's stacks-dictionary part-file. Content-addressed
/// stack_ids dedup naturally across files when the dicts are merged.
fn dict_part_key(output_prefix: &str, source_key: &str) -> String {
    format!(
        "{root}/dict/stacks/{leaf}.parquet",
        root = versioned_root(output_prefix, &parse_source_bucket(source_key)),
        leaf = part_leaf_of(source_key),
    )
}

fn polls_part_key(output_prefix: &str, source_key: &str) -> String {
    format!(
        "{root}/polls/{leaf}.parquet",
        root = versioned_root(output_prefix, &parse_source_bucket(source_key)),
        leaf = part_leaf_of(source_key),
    )
}

/// The `samples/` prefix for one source bucket under the versioned root — the
/// folded-set LIST target. Pruned to a single source bucket.
fn samples_prefix(output_prefix: &str, source_bucket: &str) -> String {
    format!("{}/samples/", versioned_root(output_prefix, source_bucket))
}

/// A query's selection: an optional wall-clock time range (epoch nanoseconds)
/// and optional service / host filters. Translated to the matched set.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    pub start_ns: Option<i64>,
    pub end_ns: Option<i64>,
    /// Exact service match (the `service=` path component).
    pub service: Option<String>,
    /// Host filter. Empty = all hosts. Non-empty = the host path component must
    /// equal one of these (a *set*, because a heatmap box selection spans many
    /// hosts). A single entry behaves like an exact single-host filter.
    pub hosts: Vec<String>,
}

/// Parse `(date, service, host)` from a source key, anchored on the
/// `YYYY-MM-DD` date component so a leading prefix (e.g. `traces/`) does not
/// shift the positions. Layout: `…/{date}/{HHMM}/{service}/{host}/{boot}/{file}`.
fn parse_scope_fields(key: &str) -> (String, String, String) {
    let path = strip_s3(key);
    let parts: Vec<&str> = path.split('/').collect();
    if let Some(anchor) = parts.iter().position(|p| is_date(p)) {
        let date = parts.get(anchor).copied().unwrap_or("").to_string();
        let service = parts.get(anchor + 2).copied().unwrap_or("").to_string();
        let host = parts.get(anchor + 3).copied().unwrap_or("").to_string();
        (date, service, host)
    } else {
        // No date anchor — fall back to the legacy fixed-index parse.
        let date = parts.first().copied().unwrap_or("").to_string();
        let service = parts.get(2).copied().unwrap_or("").to_string();
        let host = parts.get(3).copied().unwrap_or("").to_string();
        (date, service, host)
    }
}

/// The host path component of a source key (empty when the key has no host
/// segment). Used to count distinct hosts for the coverage's fleet-spread badge.
pub(crate) fn host_of(key: &str) -> String {
    parse_scope_fields(key).2
}

/// Parse the file start time (epoch SECONDS) from the filename `{ts}-{i}.bin.gz`.
fn parse_epoch_secs(key: &str) -> Option<i64> {
    let file = key.rsplit('/').next()?;
    let stem = file.split('.').next()?; // strip .bin.gz
    let ts = stem.split('-').next()?; // {ts}-{i}
    ts.parse::<i64>().ok()
}

fn strip_s3(key: &str) -> &str {
    if let Some(rest) = key.strip_prefix("s3://") {
        rest.split_once('/').map_or(rest, |(_, p)| p)
    } else {
        key
    }
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..].iter().all(u8::is_ascii_digit)
}

/// True if a raw source key is a trace segment (not our own Parquet output).
fn is_trace_segment(key: &str) -> bool {
    (key.ends_with(".bin.gz") || key.ends_with(".bin"))
        && !key.contains("/samples/")
        && !key.contains("/dict/")
}

/// Filter a raw source listing down to the files a [`Scope`] selects, then sort
/// by [`order_key`]. The result is the ordered matched set: the coverage
/// denominator and the fold order.
fn matched_and_ordered(
    objects: Vec<ObjectInfo>,
    scope: &Scope,
    segment_duration_secs: i64,
) -> Vec<ObjectInfo> {
    let mut matched: Vec<ObjectInfo> = objects
        .into_iter()
        .filter(|o| is_trace_segment(&o.key))
        .filter(|o| scope_matches(&o.key, scope, segment_duration_secs))
        .collect();
    matched.sort_by_key(|o| order_key(&o.key));
    matched
}

fn scope_matches(key: &str, scope: &Scope, segment_duration_secs: i64) -> bool {
    let (_date, service, host) = parse_scope_fields(key);
    if let Some(want) = &scope.service
        && &service != want
    {
        return false;
    }
    if !scope.hosts.is_empty() && !scope.hosts.iter().any(|h| h == &host) {
        return false;
    }
    // Interval-overlap on wall-clock time, padding by the segment duration so a
    // file that started before the window but runs into it is kept.
    // Uses half-open interval semantics: file range [start, end) overlaps query
    // [start_ns, end_ns) iff file_start < query_end && file_end > query_start.
    if scope.start_ns.is_some() || scope.end_ns.is_some() {
        let Some(epoch_secs) = parse_epoch_secs(key) else {
            // Can't place it in time — keep it rather than silently drop data.
            return true;
        };
        let file_start_ns = epoch_secs.saturating_mul(1_000_000_000);
        let file_end_ns = (epoch_secs + segment_duration_secs).saturating_mul(1_000_000_000);
        if let Some(start) = scope.start_ns
            && file_end_ns <= start
        {
            return false;
        }
        if let Some(end) = scope.end_ns
            && file_start_ns >= end
        {
            return false;
        }
    }
    true
}

/// The canonical source key recorded in part-files and passed to the decoder
/// (which derives host/service/date): a bare key for a local source,
/// `s3://{bucket}/{key}` for S3.
fn full_source_key(source_is_local: bool, source_bucket: &str, key: &str) -> String {
    if source_is_local {
        key.to_string()
    } else {
        format!("s3://{source_bucket}/{key}")
    }
}

/// Process-global concurrency limits for the demand-driven fold pipeline.
///
/// These are shared across all in-flight `/api/flamegraph` requests (held in
/// `AppState` and cloned per request — the inner `Arc<Semaphore>`s are shared),
/// so total fold work is bounded *application-wide* rather than per request.
/// Without this, N concurrent polls each running their own bounded batch could
/// still oversubscribe the box by a factor of N.
///
/// The two stages are bounded independently because they bottleneck on
/// different resources:
///
/// - [`fetch`](Self::fetch): network-bound source GETs (~37–50 MB each). These
///   spend almost all their time waiting on the network, so we run them at high
///   concurrency (~2× available parallelism by default).
/// - [`cpu`](Self::cpu): gunzip + decode + parquet-encode, run on blocking
///   threads. Sized to ~= available parallelism so concurrent folds don't
///   oversubscribe the cores and inflate every fold's wall time.
///
/// Part-file writes (small, network-bound) run ungated: they are cheap relative
/// to the fetch and we don't want to hold a CPU permit across their I/O.
#[derive(Clone)]
pub(crate) struct FoldLimits {
    /// Bounds concurrent source fetches (network-bound).
    pub fetch: Arc<Semaphore>,
    /// Bounds concurrent decode/encode work (CPU-bound).
    pub cpu: Arc<Semaphore>,
}

impl FoldLimits {
    /// Construct with explicit permit counts. Each is clamped to at least 1 so a
    /// zero never deadlocks the pipeline.
    pub(crate) fn new(fetch_permits: usize, cpu_permits: usize) -> Self {
        Self {
            fetch: Arc::new(Semaphore::new(fetch_permits.max(1))),
            cpu: Arc::new(Semaphore::new(cpu_permits.max(1))),
        }
    }

    /// Default sizing derived from available CPU parallelism: fetch at 2×
    /// parallelism (network-bound, mostly waiting), CPU at 1× parallelism.
    pub(crate) fn from_available_parallelism() -> Self {
        let par = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self::new(par * 2, par)
    }
}

impl Default for FoldLimits {
    fn default() -> Self {
        Self::from_available_parallelism()
    }
}

/// Encoded part-file buffers produced by the CPU stage of a fold, ready to write.
struct EncodedParts {
    samples_buf: Vec<u8>,
    dict_buf: Vec<u8>,
    polls_buf: Vec<u8>,
}

/// CPU-bound stage of a fold: gunzip + decode + parquet-encode over
/// already-fetched bytes. Pure and synchronous so the caller can run it on a
/// blocking thread under a CPU concurrency permit, decoupled from the
/// network-bound fetch and write stages. (The parquet encode previously ran on
/// the async executor thread; moving it here keeps CPU work off the runtime.)
fn decode_and_encode(bytes: &[u8], full_key: &str) -> anyhow::Result<EncodedParts> {
    let raw = maybe_gunzip(bytes);
    let (samples, stacks, polls) = decode::decode_samples(&raw, full_key)
        .map_err(|e| anyhow::anyhow!("decode {full_key}: {e}"))?;

    // Always encode the samples part-file, even with zero rows: its existence is
    // the record that this file is folded, so it is never re-fetched.
    let metadata = HashMap::new();
    let mut samples_buf = Vec::new();
    parquet_writer::write_samples(&mut samples_buf, &samples, &metadata)?;

    let stacks_map: HashMap<[u8; 16], Vec<String>> = stacks.into_iter().collect();
    let mut dict_buf = Vec::new();
    parquet_writer::write_stacks_dict(&mut dict_buf, &stacks_map)?;

    let mut polls_buf = Vec::new();
    parquet_writer::write_polls(&mut polls_buf, &polls)?;

    Ok(EncodedParts {
        samples_buf,
        dict_buf,
        polls_buf,
    })
}

/// Write stage of a fold: the two (small) part-file PUTs, issued concurrently.
async fn write_parts(
    output: &dyn StorageBackend,
    output_bucket: &str,
    output_prefix: &str,
    full_key: &str,
    encoded: EncodedParts,
) -> anyhow::Result<()> {
    let part_key = samples_part_key(output_prefix, full_key);
    let dict_key = dict_part_key(output_prefix, full_key);
    let polls_key = polls_part_key(output_prefix, full_key);
    let (samples_res, dict_res, polls_res) = tokio::join!(
        output.put_object(output_bucket, &part_key, encoded.samples_buf),
        output.put_object(output_bucket, &dict_key, encoded.dict_buf),
        output.put_object(output_bucket, &polls_key, encoded.polls_buf),
    );
    samples_res.map_err(|e| anyhow::anyhow!("write samples {part_key}: {e}"))?;
    dict_res.map_err(|e| anyhow::anyhow!("write dict {dict_key}: {e}"))?;
    polls_res.map_err(|e| anyhow::anyhow!("write polls {polls_key}: {e}"))?;
    Ok(())
}

/// Fold one source file: fetch, decode, and write its samples + stacks-dict
/// part-files to deterministic keys, gating the network fetch and the CPU
/// decode/encode on two separate, caller-supplied (process-global) semaphores.
///
/// The fetch permit is released before CPU work starts, and the CPU permit is
/// released before the part-file writes, so each global limit bounds only the
/// stage it is meant to bound. A zero-sample file still writes an empty samples
/// part-file (the "folded" record). Idempotent: re-folding writes the same keys.
pub(crate) async fn fold_one(
    agg: &AggContext,
    raw_key: &str,
    limits: &FoldLimits,
) -> anyhow::Result<()> {
    // Stage 1 — fetch (network-bound). Permit held only for the duration of the GET.
    let bytes = {
        let _permit = limits
            .fetch
            .acquire()
            .await
            .expect("fetch semaphore is never closed");
        agg.source
            .get_object(&agg.source_bucket, raw_key)
            .await
            .map_err(|e| anyhow::anyhow!("fetch {raw_key}: {e}"))?
    };

    // Stage 2 — decode + encode (CPU-bound) on a blocking thread, gated so
    // concurrent folds don't oversubscribe the cores.
    let full_key = full_source_key(agg.source_is_local, &agg.source_bucket, raw_key);
    let decode_key = full_key.clone();
    let encoded = {
        let _permit = limits
            .cpu
            .acquire()
            .await
            .expect("cpu semaphore is never closed");
        tokio::task::spawn_blocking(move || decode_and_encode(&bytes, &decode_key))
            .await
            .map_err(|e| anyhow::anyhow!("decode task panicked: {e}"))??
    };

    // Stage 3 — write part-files (small, network-bound). Ungated.
    write_parts(
        &*agg.output,
        &agg.output_bucket,
        &agg.output_prefix,
        &full_key,
        encoded,
    )
    .await
}

/// LIST the folded set for one source bucket: the source-file leaf hashes that
/// already have a samples part-file under that bucket's versioned root. Pruned
/// to `source_bucket`, so a BYOC source's folded set never mixes with another's.
pub(crate) async fn list_folded_leaves(
    output: &dyn StorageBackend,
    output_bucket: &str,
    output_prefix: &str,
    source_bucket: &str,
) -> HashSet<String> {
    let prefix = samples_prefix(output_prefix, source_bucket);
    let objects = output
        .list_objects_all(output_bucket, &prefix)
        .await
        .unwrap_or_else(|e| {
            // Treating an error as "nothing folded" makes the refinement loop
            // re-fold files it already processed, wasting the whole budget on
            // redundant work. We can't cheaply propagate (callers expect a set),
            // but we must not swallow it silently.
            tracing::warn!(
                bucket = %output_bucket,
                prefix = %prefix,
                error = %e,
                "list_folded_leaves: failed to list folded set; treating as empty \
                 (already-folded files may be re-folded this round)"
            );
            Vec::new()
        });
    objects
        .iter()
        .filter_map(|o| {
            let name = o.key.rsplit('/').next()?;
            name.strip_suffix(".parquet").map(|s| s.to_string())
        })
        .collect()
}

/// How much of a scope has been folded so far. Reported on every query.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Coverage {
    pub files_matched: usize,
    pub files_folded: usize,
    pub samples_folded: usize,
    /// Total bytes of all matched source files in the scope.
    pub total_bytes: u64,
    /// Distinct hosts across the matched set (the scope's fleet breadth).
    pub hosts_matched: usize,
    /// Distinct hosts among the folded files (how much of that breadth the
    /// current sample actually spans), so the UI can show fleet-representativeness
    /// e.g. "8 / 40 hosts".
    pub hosts_folded: usize,
}

/// Wire value of the `CpuProfile` CPU-sample source (periodic on-CPU sample).
const SOURCE_CPU_PROFILE: u8 = 0;
/// Wire value of the `SchedEvent` CPU-sample source (context switch, off-CPU).
/// Mirrors `CpuSampleSource::SchedEvent` and the JS `source === 1` check.
const SOURCE_SCHED_EVENT: u8 = 1;

// ─── Generic facet system ────────────────────────────────────────────────────
//
// Each facet is defined once in [`FACETS`]. The read loop extracts facet values
// generically, records them for the toolbar, and applies an optional exact-match
// filter. Adding a new facet requires only one new entry here (+ the Parquet
// column from ingest).

/// A facet definition: how to read a column and produce a string value.
#[derive(Clone)]
pub(crate) struct FacetDef {
    /// Query parameter / filter key name (e.g. `"source"`, `"thread_class"`).
    pub name: &'static str,
    /// Human label for the toolbar selector.
    pub label: &'static str,
    /// How to extract this facet's value from a row. Virtual facets derive from
    /// non-string columns; direct facets just read a nullable Utf8 column.
    pub kind: FacetKind,
    /// Default filter value when the param is absent. `"cpu"` for source (the
    /// on-CPU default), empty string for all others (= no constraint).
    pub default_filter: &'static str,
}

#[derive(Clone)]
pub(crate) enum FacetKind {
    /// Read from a UInt8 column and map wire values to labels.
    MappedU8 {
        column: &'static str,
        map: &'static [(u8, &'static str)],
        /// Fallback value when the column is absent (backwards compat with older
        /// part-files that predate this column).
        absent_value: &'static str,
    },
    /// Derived from a nullable column: `"worker"` if non-null, `"off-worker"` if null.
    NullDerived {
        column: &'static str,
        present_label: &'static str,
        absent_label: &'static str,
        /// Label when the entire column is missing from an old part-file.
        missing_column_label: &'static str,
    },
    /// Read directly from a nullable Utf8 column. Null rows produce no value
    /// (excluded from facet set and never match a filter).
    DirectString { column: &'static str },
}

/// The facet registry. Order here is the toolbar display order.
pub(crate) const FACETS: &[FacetDef] = &[
    FacetDef {
        name: "source",
        label: "Source",
        kind: FacetKind::MappedU8 {
            column: "source",
            map: &[(SOURCE_CPU_PROFILE, "cpu"), (SOURCE_SCHED_EVENT, "sched")],
            absent_value: "cpu",
        },
        default_filter: "cpu",
    },
    FacetDef {
        name: "thread_class",
        label: "Thread",
        kind: FacetKind::NullDerived {
            column: "worker_id",
            present_label: "worker",
            absent_label: "off-worker",
            missing_column_label: "worker",
        },
        default_filter: "",
    },
    FacetDef {
        name: "host",
        label: "Host",
        kind: FacetKind::DirectString { column: "host" },
        default_filter: "",
    },
    FacetDef {
        name: "spawn_location",
        label: "Task",
        kind: FacetKind::DirectString {
            column: "spawn_location",
        },
        default_filter: "",
    },
];

/// One facet's response: name + label + sorted distinct values.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct FacetResult {
    pub name: &'static str,
    pub label: &'static str,
    pub values: Vec<String>,
}

/// A generic set of active facet filters: name → required value. An entry with
/// an empty string means "no constraint" (match all). Entries are matched by
/// exact equality against the facet value extracted for each row.
pub(crate) type FacetFilters = HashMap<&'static str, String>;

/// Accumulates distinct facet values across part-files. One `HashSet<String>`
/// per facet definition.
struct FacetAccum {
    /// Distinct values per facet (indexed same as [`FACETS`]).
    sets: Vec<HashSet<String>>,
    /// Distinct hosts that passed ALL filters (for the "N hosts" badge).
    matched_hosts: HashSet<String>,
}

impl FacetAccum {
    fn new() -> Self {
        Self {
            sets: FACETS.iter().map(|_| HashSet::new()).collect(),
            matched_hosts: HashSet::new(),
        }
    }

    fn into_results(self) -> Vec<FacetResult> {
        FACETS
            .iter()
            .zip(self.sets)
            .map(|(def, set)| {
                let mut values: Vec<String> = set.into_iter().collect();
                values.sort();
                FacetResult {
                    name: def.name,
                    label: def.label,
                    values,
                }
            })
            .collect()
    }
}

/// The combined per-query filter: time range + per-facet exact-match filters.
#[derive(Debug, Clone, Default)]
pub(crate) struct SampleFilter {
    /// Optional time range filter (epoch nanoseconds, half-open: [start, end)).
    pub start_ns: Option<i64>,
    pub end_ns: Option<i64>,
    /// Per-facet filters. Key = facet name, value = required value. Empty string
    /// or absent = no constraint. For "source", the default is "cpu" (set by the
    /// endpoint when the param is absent).
    pub facets: FacetFilters,
}

/// Aggregated result of folding + reading the in-scope part-files.
pub(crate) struct AggResult {
    pub stack_counts: Vec<(Vec<u8>, u64)>,
    pub stacks_dict: HashMap<Vec<u8>, Vec<String>>,
    pub total_samples: usize,
    pub hosts: usize,
    pub min_ts: Option<i64>,
    pub max_ts: Option<i64>,
    /// Generic facet results: each facet's distinct values in the scope.
    pub facets: Vec<FacetResult>,
}

/// Aggregate the given folded part-files (by their source keys) into stack_id
/// counts + a merged stacks dictionary, reading each part-file through the
/// `StorageBackend`. Only `source_keys` whose part-file exists are read; missing
/// ones (not yet folded) are skipped.
///
/// Each file's two GETs (samples + dict) are issued concurrently with
/// `tokio::join!`, halving the round-trips per file. The per-file Parquet decode
/// and HashMap merge then run serially as files are read, so the shared
/// accumulators need no locking.
pub(crate) async fn aggregate(
    output: &dyn StorageBackend,
    bucket: &str,
    output_prefix: &str,
    source_keys: &[String],
    folded: &HashSet<String>,
    filter: SampleFilter,
) -> anyhow::Result<AggResult> {
    let mut counts: HashMap<[u8; 16], u64> = HashMap::new();
    let mut dict: HashMap<Vec<u8>, Vec<String>> = HashMap::new();
    let mut accum = FacetAccum::new();
    let mut total_samples = 0usize;
    let mut min_ts: Option<i64> = None;
    let mut max_ts: Option<i64> = None;

    for sk in source_keys {
        if !folded.contains(&part_leaf_of(sk)) {
            continue;
        }
        let part_key = samples_part_key(output_prefix, sk);
        let dict_key = dict_part_key(output_prefix, sk);
        let (samples, dict_data) = tokio::join!(
            output.get_object(bucket, &part_key),
            output.get_object(bucket, &dict_key),
        );
        let data = match samples {
            Ok(d) => d,
            Err(_) => continue,
        };
        read_samples_part(
            data,
            &filter,
            &mut counts,
            &mut accum,
            &mut total_samples,
            &mut min_ts,
            &mut max_ts,
        )?;
        if let Ok(dict_data) = dict_data {
            read_dict_part(dict_data, &mut dict)?;
        }
    }

    let stack_counts: Vec<(Vec<u8>, u64)> =
        counts.into_iter().map(|(k, v)| (k.to_vec(), v)).collect();

    let hosts = accum.matched_hosts.len().max(1);
    let facets = accum.into_results();

    Ok(AggResult {
        stack_counts,
        stacks_dict: dict,
        total_samples,
        hosts,
        min_ts,
        max_ts,
        facets,
    })
}

/// Concurrency for polls part-file GETs. A GET is a single round-trip with a
/// small body (one file's polls), so this can run wide.
const POLLS_READ_CONCURRENCY: usize = 24;

/// Fetch the `polls/` part-file bytes for each folded source key in
/// `source_keys`, concurrently. Returns `(raw_source_key, polls_bytes)` for the
/// keys whose part-file is both folded and present; not-yet-folded or missing
/// ones are skipped. The caller decodes the Parquet itself (the polls schema is
/// the tokio-stats endpoint's concern, not the aggregator's), so the part-key
/// path scheme stays private to this module.
pub(crate) async fn read_polls_parts(
    output: &dyn StorageBackend,
    bucket: &str,
    output_prefix: &str,
    source_keys: &[(String, String)],
    folded: &HashSet<String>,
) -> Vec<(String, Vec<u8>)> {
    use futures::stream::StreamExt;
    let fetches: Vec<(String, String)> = source_keys
        .iter()
        .filter(|(_, full)| folded.contains(&part_leaf_of(full)))
        .map(|(raw, full)| (raw.clone(), polls_part_key(output_prefix, full)))
        .collect();

    futures::stream::iter(fetches)
        .map(|(raw_key, polls_key)| async move {
            output
                .get_object(bucket, &polls_key)
                .await
                .ok()
                .map(|data| (raw_key, data))
        })
        .buffer_unordered(POLLS_READ_CONCURRENCY)
        .filter_map(|x| async { x })
        .collect()
        .await
}

fn read_samples_part(
    data: Vec<u8>,
    filter: &SampleFilter,
    counts: &mut HashMap<[u8; 16], u64>,
    accum: &mut FacetAccum,
    total_samples: &mut usize,
    min_ts: &mut Option<i64>,
    max_ts: &mut Option<i64>,
) -> anyhow::Result<()> {
    // `Bytes::from(Vec<u8>)` reuses the allocation (no copy); threading the
    // owned buffer in from the caller avoids the round-trip through `&[u8]`.
    let reader = ::parquet::arrow::arrow_reader::ParquetRecordBatchReader::try_new(
        bytes::Bytes::from(data),
        4096,
    )?;
    for batch in reader {
        let batch = batch?;
        let stack_col = batch.column_by_name("stack_id").and_then(|c| {
            c.as_any()
                .downcast_ref::<arrow::array::FixedSizeBinaryArray>()
        });
        let Some(stack_arr) = stack_col else { continue };
        let ts_arr = batch
            .column_by_name("timestamp_ns")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::Int64Array>());

        // Pre-resolve column references for each facet in this batch.
        let facet_cols: Vec<ResolvedFacetCol> = FACETS
            .iter()
            .map(|def| resolve_facet_col(&batch, def))
            .collect();

        for i in 0..batch.num_rows() {
            // Time range filter.
            if let Some(ts) = ts_arr {
                let v = ts.value(i);
                if filter.start_ns.is_some_and(|start| v < start) {
                    continue;
                }
                if filter.end_ns.is_some_and(|end| v >= end) {
                    continue;
                }
            }

            // Extract facet values for this row and record them (pre-filter).
            let mut row_values: Vec<Option<String>> = Vec::with_capacity(FACETS.len());
            for (fi, col) in facet_cols.iter().enumerate() {
                let val = extract_facet_value(col, i);
                if let Some(ref v) = val {
                    accum.sets[fi].insert(v.clone());
                }
                row_values.push(val);
            }

            // Apply facet filters: every active filter must match.
            let mut passes = true;
            for (fi, def) in FACETS.iter().enumerate() {
                if let Some(wanted) = filter.facets.get(def.name) {
                    if wanted.is_empty() {
                        continue;
                    }
                    match &row_values[fi] {
                        Some(v) if v == wanted => {}
                        _ => {
                            passes = false;
                            break;
                        }
                    }
                }
            }
            if !passes {
                continue;
            }

            // Count this sample.
            let mut id = [0u8; 16];
            id.copy_from_slice(stack_arr.value(i));
            *counts.entry(id).or_insert(0) += 1;
            *total_samples += 1;
            if let Some(ts) = ts_arr {
                let v = ts.value(i);
                *min_ts = Some(min_ts.map_or(v, |m| m.min(v)));
                *max_ts = Some(max_ts.map_or(v, |m| m.max(v)));
            }
            // Track matched hosts for the "N hosts" badge.
            if let Some(ref h) = row_values[host_facet_index()] {
                accum.matched_hosts.insert(h.clone());
            }
        }
    }
    Ok(())
}

/// Index of the "host" facet in [`FACETS`]. A missing "host" facet is a
/// developer error (the facet table is a compile-time constant), so this panics
/// rather than silently picking the wrong column.
fn host_facet_index() -> usize {
    FACETS
        .iter()
        .position(|f| f.name == "host")
        .expect("FACETS must define a \"host\" facet")
}

enum ResolvedFacetCol<'a> {
    MappedU8 {
        arr: Option<&'a arrow::array::UInt8Array>,
        map: &'static [(u8, &'static str)],
        absent_value: &'static str,
    },
    NullDerived {
        arr: Option<&'a arrow::array::UInt32Array>,
        present_label: &'static str,
        absent_label: &'static str,
        missing_column_label: &'static str,
    },
    DirectString {
        arr: Option<&'a arrow::array::StringArray>,
    },
}

fn resolve_facet_col<'a>(
    batch: &'a arrow::record_batch::RecordBatch,
    def: &FacetDef,
) -> ResolvedFacetCol<'a> {
    match &def.kind {
        FacetKind::MappedU8 {
            column,
            map,
            absent_value,
        } => {
            let arr = batch
                .column_by_name(column)
                .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt8Array>());
            ResolvedFacetCol::MappedU8 {
                arr,
                map,
                absent_value,
            }
        }
        FacetKind::NullDerived {
            column,
            present_label,
            absent_label,
            missing_column_label,
        } => {
            let arr = batch
                .column_by_name(column)
                .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt32Array>());
            ResolvedFacetCol::NullDerived {
                arr,
                present_label,
                absent_label,
                missing_column_label,
            }
        }
        FacetKind::DirectString { column } => {
            let arr = batch
                .column_by_name(column)
                .and_then(|c| c.as_any().downcast_ref::<arrow::array::StringArray>());
            ResolvedFacetCol::DirectString { arr }
        }
    }
}

fn extract_facet_value(col: &ResolvedFacetCol, i: usize) -> Option<String> {
    match col {
        ResolvedFacetCol::MappedU8 {
            arr,
            map,
            absent_value,
        } => {
            let label = match arr {
                Some(a) => {
                    let v = a.value(i);
                    map.iter().find(|(k, _)| *k == v).map_or("", |(_, l)| l)
                }
                None => absent_value,
            };
            if label.is_empty() {
                None
            } else {
                Some(label.to_string())
            }
        }
        ResolvedFacetCol::NullDerived {
            arr,
            present_label,
            absent_label,
            missing_column_label,
        } => {
            let label = match arr {
                Some(a) => {
                    if a.is_null(i) {
                        absent_label
                    } else {
                        present_label
                    }
                }
                None => missing_column_label,
            };
            Some(label.to_string())
        }
        ResolvedFacetCol::DirectString { arr } => match arr {
            Some(a) if !a.is_null(i) => Some(a.value(i).to_string()),
            _ => None,
        },
    }
}

fn read_dict_part(data: Vec<u8>, dict: &mut HashMap<Vec<u8>, Vec<String>>) -> anyhow::Result<()> {
    // `Bytes::from(Vec<u8>)` reuses the allocation (no copy).
    let reader = ::parquet::arrow::arrow_reader::ParquetRecordBatchReader::try_new(
        bytes::Bytes::from(data),
        4096,
    )?;
    for batch in reader {
        let batch = batch?;
        let stack_arr = batch.column_by_name("stack_id").and_then(|c| {
            c.as_any()
                .downcast_ref::<arrow::array::FixedSizeBinaryArray>()
        });
        let frames_arr = batch
            .column_by_name("frames")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::ListArray>());
        let (Some(stack_arr), Some(frames_arr)) = (stack_arr, frames_arr) else {
            continue;
        };
        for i in 0..batch.num_rows() {
            let id = stack_arr.value(i).to_vec();
            if dict.contains_key(&id) {
                continue;
            }
            let frame_list = frames_arr.value(i);
            if let Some(str_arr) = frame_list
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
            {
                let frames: Vec<String> = (0..str_arr.len())
                    .map(|j| str_arr.value(j).to_string())
                    .collect();
                dict.insert(id, frames);
            }
        }
    }
    Ok(())
}

fn maybe_gunzip(data: &[u8]) -> Vec<u8> {
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        use std::io::Read;
        let mut decoder = flate2::read::GzDecoder::new(data);
        let mut out = Vec::new();
        match decoder.read_to_end(&mut out) {
            Ok(_) => out,
            Err(_) => data.to_vec(),
        }
    } else {
        data.to_vec()
    }
}

/// The ordered matched-set keys (as `(raw_key, full_key)` pairs) for a scope,
/// given a raw source listing, plus the total bytes of all matched files (for
/// the coverage block). Used by the refinement loop.
pub(crate) fn ordered_full_keys_with_size(
    objects: Vec<ObjectInfo>,
    scope: &Scope,
    segment_duration_secs: i64,
    source_is_local: bool,
    source_bucket: &str,
) -> (Vec<(String, String)>, u64) {
    let matched = matched_and_ordered(objects, scope, segment_duration_secs);
    let total_bytes: u64 = matched.iter().map(|o| o.size.max(0) as u64).sum();
    let keys = matched
        .into_iter()
        .map(|o| {
            let full = full_source_key(source_is_local, source_bucket, &o.key);
            (o.key, full)
        })
        .collect();
    (keys, total_bytes)
}

/// Shared `Arc`-friendly handle bundle the server uses to run the refinement
/// loop without re-reading config each call.
#[derive(Clone)]
pub struct AggContext {
    pub source: Arc<dyn StorageBackend>,
    pub output: Arc<dyn StorageBackend>,
    pub source_bucket: String,
    pub source_is_local: bool,
    pub output_bucket: String,
    pub output_prefix: String,
    pub source_prefixes: Vec<String>,
    pub segment_duration_secs: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_filter_source_and_thread() {
        use std::collections::HashMap;

        // Default filter: source=cpu, thread_class="" (all), others empty.
        let def = SampleFilter {
            facets: HashMap::from([
                ("source", "cpu".to_string()),
                ("thread_class", String::new()),
            ]),
            ..Default::default()
        };
        // The filter works by exact match on extracted values. Verify the
        // data-driven filtering contract: non-empty = must match, empty = pass.
        assert_eq!(def.facets.get("source"), Some(&"cpu".to_string()));
        assert_eq!(def.facets.get("thread_class"), Some(&String::new()));

        // A filter with source=sched should be expressible.
        let sched = SampleFilter {
            facets: HashMap::from([("source", "sched".to_string())]),
            ..Default::default()
        };
        assert_eq!(sched.facets.get("source"), Some(&"sched".to_string()));

        // FacetDef registry has our expected facets.
        let names: Vec<&str> = FACETS.iter().map(|f| f.name).collect();
        assert!(names.contains(&"source"));
        assert!(names.contains(&"thread_class"));
        assert!(names.contains(&"host"));
        assert!(names.contains(&"spawn_location"));
    }

    #[test]
    fn order_key_is_deterministic_and_versioned() {
        let a = order_key("2026-06-19/1300/shale/host-a/boot/1-0.bin.gz");
        let b = order_key("2026-06-19/1300/shale/host-a/boot/1-0.bin.gz");
        assert_eq!(a, b, "same key → same order");
        let c = order_key("2026-06-19/1300/shale/host-b/boot/1-0.bin.gz");
        assert_ne!(a, c, "different key → different order (almost surely)");
    }

    #[test]
    fn parse_scope_fields_handles_prefix() {
        let (d, s, h) = parse_scope_fields(
            "traces/2026-04-09/1910/checkout-api/us-east-1/abcd/1744224000-3.bin.gz",
        );
        assert_eq!(d, "2026-04-09");
        assert_eq!(s, "checkout-api");
        assert_eq!(h, "us-east-1");
    }

    #[test]
    fn parse_scope_fields_handles_s3_uri() {
        let (d, s, h) =
            parse_scope_fields("s3://bkt/2026-06-19/1300/shale/host-a/boot-1/1-0.bin.gz");
        assert_eq!(d, "2026-06-19");
        assert_eq!(s, "shale");
        assert_eq!(h, "host-a");
    }

    #[test]
    fn parse_epoch_from_filename() {
        assert_eq!(
            parse_epoch_secs("2026-04-09/1910/svc/host/boot/1744224000-3.bin.gz"),
            Some(1744224000)
        );
    }

    #[test]
    fn part_keys_are_partitioned_with_hash_leaf() {
        let sk = "s3://bkt/2026-06-19/1300/shale/host-a/boot-1/1-0.bin.gz";
        let pk = samples_part_key("flamegraph-data", sk);
        // Output is namespaced by source bucket, then partitioned by scope.
        assert!(pk.starts_with(
            "flamegraph-data/v3/bucket=bkt/samples/service=shale/date=2026-06-19/host=host-a/"
        ));
        assert!(pk.ends_with(".parquet"));
        // Leaf is the content hash, idempotent across calls.
        assert_eq!(pk, samples_part_key("flamegraph-data", sk));
    }

    #[test]
    fn parse_source_bucket_from_key() {
        assert_eq!(
            parse_source_bucket("s3://my-bucket/2026-06-19/1300/svc/host/boot/1-0.bin.gz"),
            "my-bucket"
        );
        // Bare (local) keys have no bucket → the "local" namespace.
        assert_eq!(
            parse_source_bucket("2026-06-19/1300/svc/host/boot/1-0.bin.gz"),
            "local"
        );
    }

    #[test]
    fn output_namespaced_by_source_bucket_isolates_buckets() {
        // Same scope path, two different source buckets → different output roots,
        // so their folded sets and LISTs never mix.
        let a = samples_part_key("out", "s3://bucket-a/2026-06-19/1300/svc/h/b/1-0.bin.gz");
        let b = samples_part_key("out", "s3://bucket-b/2026-06-19/1300/svc/h/b/1-0.bin.gz");
        assert!(a.contains("/bucket=bucket-a/"));
        assert!(b.contains("/bucket=bucket-b/"));
        assert_ne!(a, b);
        // And the per-bucket LIST prefixes are disjoint.
        assert_ne!(
            samples_prefix("out", "bucket-a"),
            samples_prefix("out", "bucket-b")
        );
    }

    #[test]
    fn scope_filters_by_service_host_and_time() {
        let scope = Scope {
            start_ns: Some(1_744_224_000_000_000_000),
            end_ns: Some(1_744_224_100_000_000_000),
            service: Some("shale".to_string()),
            hosts: vec!["host-a".to_string()],
        };
        // In service, host, and time window.
        assert!(scope_matches(
            "2026-04-09/1910/shale/host-a/boot/1744224050-0.bin.gz",
            &scope,
            60
        ));
        // Wrong service.
        assert!(!scope_matches(
            "2026-04-09/1910/other/host-a/boot/1744224050-0.bin.gz",
            &scope,
            60
        ));
        // Wrong host (exact match, not substring): host-a must not match host-ab.
        assert!(!scope_matches(
            "2026-04-09/1910/shale/host-z/boot/1744224050-0.bin.gz",
            &scope,
            60
        ));
        // Far in the future — outside window.
        assert!(!scope_matches(
            "2026-04-09/1910/shale/host-a/boot/1744999999-0.bin.gz",
            &scope,
            60
        ));
    }

    #[test]
    fn scope_host_set_matches_union_exactly() {
        let scope = Scope {
            hosts: vec!["host-a".to_string(), "host-c".to_string()],
            ..Default::default()
        };
        assert!(scope_matches("d/h/svc/host-a/boot/1-0.bin.gz", &scope, 60));
        assert!(scope_matches("d/h/svc/host-c/boot/1-0.bin.gz", &scope, 60));
        // Not in the set.
        assert!(!scope_matches("d/h/svc/host-b/boot/1-0.bin.gz", &scope, 60));
        // Exact match, not prefix/substring: "host-a" must not match "host-aa".
        assert!(!scope_matches(
            "d/h/svc/host-aa/boot/1-0.bin.gz",
            &scope,
            60
        ));
        // Empty host set = all hosts.
        let all = Scope::default();
        assert!(scope_matches("d/h/svc/any-host/boot/1-0.bin.gz", &all, 60));
    }

    #[test]
    fn scope_time_overlap_keeps_boundary_file() {
        // File starts 30s before the window opens but runs into it (60s segment).
        let scope = Scope {
            start_ns: Some(1_744_224_000_000_000_000),
            end_ns: Some(1_744_224_100_000_000_000),
            ..Default::default()
        };
        assert!(
            scope_matches("d/h/svc/host/boot/1744223970-0.bin.gz", &scope, 60),
            "file [t-30, t+30) overlaps window opening at t"
        );
    }

    #[test]
    fn matched_set_is_ordered_by_order_key() {
        let objs: Vec<ObjectInfo> = (0..20)
            .map(|i| ObjectInfo {
                key: format!("2026-06-19/1300/shale/host-{i}/boot/1-0.bin.gz"),
                size: 1,
                last_modified: None,
            })
            .collect();
        let ordered = matched_and_ordered(objs.clone(), &Scope::default(), 60);
        assert_eq!(ordered.len(), 20);
        // The order must match sorting by order_key, and be a permutation (not
        // the original lexicographic order in general).
        let mut by_key = ordered.clone();
        by_key.sort_by_key(|o| order_key(&o.key));
        assert_eq!(
            ordered.iter().map(|o| &o.key).collect::<Vec<_>>(),
            by_key.iter().map(|o| &o.key).collect::<Vec<_>>()
        );
    }
}

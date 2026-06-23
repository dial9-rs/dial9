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

/// Public accessor for the polls part-key, used by the health endpoint.
pub(crate) fn polls_part_key_pub(output_prefix: &str, source_key: &str) -> String {
    polls_part_key(output_prefix, source_key)
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
        && !key.contains("/_manifest/")
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
pub struct FoldLimits {
    /// Bounds concurrent source fetches (network-bound).
    pub fetch: Arc<Semaphore>,
    /// Bounds concurrent decode/encode work (CPU-bound).
    pub cpu: Arc<Semaphore>,
}

impl FoldLimits {
    /// Construct with explicit permit counts. Each is clamped to at least 1 so a
    /// zero never deadlocks the pipeline.
    pub fn new(fetch_permits: usize, cpu_permits: usize) -> Self {
        Self {
            fetch: Arc::new(Semaphore::new(fetch_permits.max(1))),
            cpu: Arc::new(Semaphore::new(cpu_permits.max(1))),
        }
    }

    /// Default sizing derived from available CPU parallelism: fetch at 2×
    /// parallelism (network-bound, mostly waiting), CPU at 1× parallelism.
    pub fn from_available_parallelism() -> Self {
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
        .unwrap_or_default();
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
pub struct Coverage {
    pub files_matched: usize,
    pub files_folded: usize,
    pub samples_folded: usize,
}

/// Wire value of the `CpuProfile` CPU-sample source (periodic on-CPU sample).
const SOURCE_CPU_PROFILE: u8 = 0;
/// Wire value of the `SchedEvent` CPU-sample source (context switch, off-CPU).
/// Mirrors `CpuSampleSource::SchedEvent` and the JS `source === 1` check.
const SOURCE_SCHED_EVENT: u8 = 1;

/// Which CPU-sample sources to count when aggregating. Both kinds are *stored*
/// on disk (so a future off-CPU view can read them); this only controls what a
/// given query folds into its tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceFilter {
    /// On-CPU profiling samples only (exclude `SchedEvent`). The default, and
    /// what the viewer's flamegraph shows (`isCpuProfileSample` / `source !== 1`
    /// in trace_analysis.js): conflating the two yields a flamegraph dominated
    /// by `schedule` frames. The frontend sends this as `source=cpu`.
    #[default]
    CpuProfile,
    /// Scheduler context-switch (`SchedEvent`) samples only (`source=sched`).
    Sched,
    /// Every source (`source=` empty / "all").
    All,
}

/// Which worker-attribution class to count. On-runtime = the sample is
/// attributed to a runtime worker (`worker_id` present); off-runtime = not.
/// Mirrors the viewer's `workerId !== 255` split and the frontend's
/// `thread_class=worker|off-worker` selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThreadFilter {
    /// Both worker and off-worker samples (`thread_class=` empty / "all").
    #[default]
    All,
    /// Runtime workers only (`worker_id` present) — `thread_class=worker`.
    Worker,
    /// Off-worker only (`worker_id` absent) — `thread_class=off-worker`.
    OffWorker,
}

/// The combined per-query sample filter applied while reading part-files. Both
/// dimensions default to the viewer's default view (on-CPU profile samples,
/// all threads).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SampleFilter {
    pub source: SourceFilter,
    pub thread: ThreadFilter,
    /// Optional time range filter (epoch nanoseconds, half-open: [start, end)).
    /// Samples outside this range are excluded during aggregation.
    pub start_ns: Option<i64>,
    pub end_ns: Option<i64>,
}

impl SampleFilter {
    /// Whether a sample with the given wire `source` and worker attribution is
    /// counted. `worker_present` is true iff the stored `worker_id` is non-null.
    fn includes(self, source: u8, worker_present: bool) -> bool {
        let source_ok = match self.source {
            SourceFilter::CpuProfile => source == SOURCE_CPU_PROFILE,
            SourceFilter::Sched => source == SOURCE_SCHED_EVENT,
            SourceFilter::All => true,
        };
        let thread_ok = match self.thread {
            ThreadFilter::All => true,
            ThreadFilter::Worker => worker_present,
            ThreadFilter::OffWorker => !worker_present,
        };
        source_ok && thread_ok
    }
}

/// Aggregated result of folding + reading the in-scope part-files.
pub(crate) struct AggResult {
    pub stack_counts: Vec<(Vec<u8>, u64)>,
    pub stacks_dict: HashMap<Vec<u8>, Vec<String>>,
    pub total_samples: usize,
    pub hosts: usize,
    pub min_ts: Option<i64>,
    pub max_ts: Option<i64>,
    /// Facets available in this scope's time window, recorded **independent of
    /// the counting filter** (see [`Facets`]). These drive the data-driven
    /// flamegraph toolbar: the UI offers only the dimensions that actually have
    /// data, rather than a hard-coded option list.
    ///
    /// Host names present in the scope (sorted), regardless of the
    /// source/thread filter. Empty if no `host` column is present.
    pub host_names: Vec<String>,
    /// Sample sources present in the scope (`"cpu"` / `"sched"`, sorted),
    /// regardless of the active source filter.
    pub sources_present: Vec<String>,
    /// Worker-attribution classes present in the scope (`"worker"` /
    /// `"off-worker"`, sorted), regardless of the active thread filter.
    pub thread_classes_present: Vec<String>,
}

/// Available-facet accumulator, populated as part-files are read.
///
/// Unlike the counting accumulators (`counts`, `total_samples`), facet presence
/// is recorded **before** the source/thread filter is applied — it only honors
/// the time-range filter. This is deliberate: the toolbar must offer every
/// dimension that has data *in the window*, not just the dimension currently
/// selected. Otherwise switching the Source selector to `sched` would make
/// `cpu` vanish from the options (and vice-versa).
#[derive(Default)]
struct Facets {
    /// Distinct host names present in the window, recorded regardless of the
    /// source/thread filter — drives the toolbar's host selector.
    hosts: HashSet<String>,
    /// Distinct host names that passed the *counting* filter — drives the
    /// `hosts` count badge ("N hosts" of the displayed tree).
    matched_hosts: HashSet<String>,
    /// Distinct wire source values seen (`SOURCE_CPU_PROFILE` / `SOURCE_SCHED_EVENT`).
    sources: HashSet<u8>,
    /// Whether any on-runtime (worker-attributed) sample was seen.
    saw_worker: bool,
    /// Whether any off-runtime sample was seen.
    saw_off_worker: bool,
}

impl Facets {
    /// Sorted source labels present (`"cpu"` before `"sched"`).
    fn source_labels(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .sources
            .iter()
            .filter_map(|s| match *s {
                SOURCE_CPU_PROFILE => Some("cpu".to_string()),
                SOURCE_SCHED_EVENT => Some("sched".to_string()),
                _ => None,
            })
            .collect();
        v.sort();
        v
    }

    /// Sorted thread-class labels present (`"off-worker"` before `"worker"`).
    fn thread_labels(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.saw_off_worker {
            v.push("off-worker".to_string());
        }
        if self.saw_worker {
            v.push("worker".to_string());
        }
        v
    }
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
    let mut facets = Facets::default();
    let mut total_samples = 0usize;
    let mut min_ts: Option<i64> = None;
    let mut max_ts: Option<i64> = None;

    for sk in source_keys {
        if !folded.contains(&part_leaf_of(sk)) {
            continue;
        }
        let part_key = samples_part_key(output_prefix, sk);
        let dict_key = dict_part_key(output_prefix, sk);
        // Fetch samples + dict concurrently (two small GETs per file).
        let (samples, dict_data) = tokio::join!(
            output.get_object(bucket, &part_key),
            output.get_object(bucket, &dict_key),
        );
        let data = match samples {
            Ok(d) => d,
            Err(_) => continue,
        };
        read_samples_part(
            &data,
            filter,
            &mut counts,
            &mut facets,
            &mut total_samples,
            &mut min_ts,
            &mut max_ts,
        )?;
        if let Ok(dict_data) = dict_data {
            read_dict_part(&dict_data, &mut dict)?;
        }
    }

    let stack_counts: Vec<(Vec<u8>, u64)> =
        counts.into_iter().map(|(k, v)| (k.to_vec(), v)).collect();

    // `host_names` is the in-window facet set (any source/thread); fall back to
    // the counted hosts if no separate facet hosts were recorded.
    let mut host_names: Vec<String> = if facets.hosts.is_empty() {
        facets.matched_hosts.iter().cloned().collect()
    } else {
        facets.hosts.iter().cloned().collect()
    };
    host_names.sort();

    Ok(AggResult {
        stack_counts,
        stacks_dict: dict,
        total_samples,
        hosts: facets.matched_hosts.len().max(1),
        min_ts,
        max_ts,
        host_names,
        sources_present: facets.source_labels(),
        thread_classes_present: facets.thread_labels(),
    })
}

fn read_samples_part(
    data: &[u8],
    filter: SampleFilter,
    counts: &mut HashMap<[u8; 16], u64>,
    facets: &mut Facets,
    total_samples: &mut usize,
    min_ts: &mut Option<i64>,
    max_ts: &mut Option<i64>,
) -> anyhow::Result<()> {
    let reader = ::parquet::arrow::arrow_reader::ParquetRecordBatchReader::try_new(
        bytes::Bytes::from(data.to_vec()),
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
        let host_arr = batch
            .column_by_name("host")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::StringArray>());
        // `source` is a UInt8 column. If it's absent (older part-files predate
        // it) every row is treated as the default on-CPU source.
        let source_arr = batch
            .column_by_name("source")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt8Array>());
        // `worker_id` is a nullable UInt32 column: null = off-runtime. Absent in
        // pre-v2 part-files; those are treated as on-runtime (worker present) so
        // a thread filter never silently drops legacy data.
        let worker_arr = batch
            .column_by_name("worker_id")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt32Array>());

        for i in 0..batch.num_rows() {
            let source = source_arr.map_or(SOURCE_CPU_PROFILE, |a| a.value(i));
            let worker_present = worker_arr.is_none_or(|a| !a.is_null(i));

            // Time range filter: applies to BOTH counting and facet presence, so
            // a sample outside [start_ns, end_ns) contributes to neither.
            if let Some(ts) = ts_arr {
                let v = ts.value(i);
                if filter.start_ns.is_some_and(|start| v < start) {
                    continue;
                }
                if filter.end_ns.is_some_and(|end| v >= end) {
                    continue;
                }
            }

            // Record available facets BEFORE the source/thread filter: the
            // toolbar must offer every dimension present in the window, not just
            // the currently-selected one (else picking `source=sched` would make
            // `cpu` disappear from the options). A missing column degrades
            // safely — `source` defaults to on-CPU, `worker_id` to present.
            facets.sources.insert(source);
            if worker_present {
                facets.saw_worker = true;
            } else {
                facets.saw_off_worker = true;
            }
            if let Some(h) = host_arr
                && !h.is_null(i)
            {
                facets.hosts.insert(h.value(i).to_string());
            }

            // Counting filter: source + thread. Default (CpuProfile, all
            // threads) matches the viewer's on-CPU flamegraph. Both sources and
            // both worker classes are stored; this only decides what this query
            // counts.
            if !filter.includes(source, worker_present) {
                continue;
            }
            let mut id = [0u8; 16];
            id.copy_from_slice(stack_arr.value(i));
            *counts.entry(id).or_insert(0) += 1;
            *total_samples += 1;
            if let Some(ts) = ts_arr {
                let v = ts.value(i);
                *min_ts = Some(min_ts.map_or(v, |m| m.min(v)));
                *max_ts = Some(max_ts.map_or(v, |m| m.max(v)));
            }
            if let Some(h) = host_arr
                && !h.is_null(i)
            {
                facets.matched_hosts.insert(h.value(i).to_string());
            }
        }
    }
    Ok(())
}

fn read_dict_part(data: &[u8], dict: &mut HashMap<Vec<u8>, Vec<String>>) -> anyhow::Result<()> {
    let reader = ::parquet::arrow::arrow_reader::ParquetRecordBatchReader::try_new(
        bytes::Bytes::from(data.to_vec()),
        4096,
    )?;
    for batch in reader.flatten() {
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

/// Convenience: the ordered matched-set keys (full source keys) for a scope,
/// given a raw source listing. Used by the refinement loop.
pub(crate) fn ordered_full_keys(
    objects: Vec<ObjectInfo>,
    scope: &Scope,
    segment_duration_secs: i64,
    source_is_local: bool,
    source_bucket: &str,
) -> Vec<(String, String)> {
    matched_and_ordered(objects, scope, segment_duration_secs)
        .into_iter()
        .map(|o| {
            let full = full_source_key(source_is_local, source_bucket, &o.key);
            (o.key, full)
        })
        .collect()
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
        let cpu = SOURCE_CPU_PROFILE;
        let sched = SOURCE_SCHED_EVENT;

        // Default: on-CPU profile, all threads — the viewer's default view.
        let def = SampleFilter::default();
        assert!(
            def.includes(cpu, true),
            "default keeps on-CPU worker samples"
        );
        assert!(def.includes(cpu, false), "default keeps on-CPU off-worker");
        assert!(!def.includes(sched, true), "default drops sched");

        // Source = sched only.
        let sched_only = SampleFilter {
            source: SourceFilter::Sched,
            thread: ThreadFilter::All,
            ..Default::default()
        };
        assert!(sched_only.includes(sched, true));
        assert!(!sched_only.includes(cpu, true));

        // Thread = worker / off-worker (on-CPU source).
        let worker = SampleFilter {
            source: SourceFilter::CpuProfile,
            thread: ThreadFilter::Worker,
            ..Default::default()
        };
        assert!(
            worker.includes(cpu, true),
            "worker keeps attributed samples"
        );
        assert!(!worker.includes(cpu, false), "worker drops off-runtime");
        let off = SampleFilter {
            source: SourceFilter::CpuProfile,
            thread: ThreadFilter::OffWorker,
            ..Default::default()
        };
        assert!(off.includes(cpu, false), "off-worker keeps unattributed");
        assert!(!off.includes(cpu, true), "off-worker drops attributed");

        // Source = all keeps both kinds.
        let all = SampleFilter {
            source: SourceFilter::All,
            thread: ThreadFilter::All,
            ..Default::default()
        };
        assert!(all.includes(cpu, true) && all.includes(sched, false));
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

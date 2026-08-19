//! Per-request operational metrics, published via metrique.
//!
//! One [`RequestMetrics`] entry is emitted for every HTTP request that reaches
//! the API router (see [`record_request_metrics`]).
//!
//! ## Dimensions
//!
//! Metrics are emitted with two EMF dimension sets:
//!
//! - `["operation"]` — per-route (the matched path template, e.g.
//!   `/api/browse`, `/api/object`), so you can alarm a single noisy endpoint.
//! - `[]` (the empty set) — a service-wide aggregate across all operations.

use std::time::{Duration, SystemTime};

use axum::extract::{MatchedPath, Request};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use dial9_core::handle::Dial9Handle;
use dial9_metrique::{Dial9Context, Dial9Stream, Interned, SpanName};
use metrique::ServiceMetrics;
use metrique::emf::Emf;
use metrique::local::{LocalFormat, OutputStyle};
use metrique::timers::Timer;
use metrique::unit::{Byte, Count, Millisecond};
use metrique::unit_of_work::metrics;
use metrique::writer::sink::AttachHandle;
use metrique::writer::{AttachGlobalEntrySinkExt, EntryIoStream, FormatExt};

const CLOUDWATCH_NAMESPACE: &str = "dial9_viewer";
const SESSION_ID_HEADER: &str = "x-dial9-session-id";
const UUID_TEXT_LEN: usize = 36;

/// Operation label used when a request did not match any registered route
/// (e.g. a stray request that fell through to the API router). Keeps the
/// `operation` dimension's cardinality bounded — we never emit the raw URI.
const UNMATCHED_OPERATION: &str = "unmatched";

#[metrics(emf::dimension_sets = [["operation"], []])]
struct RequestMetrics {
    #[metrics(flatten)]
    dial9: Dial9Context,

    /// Stamp the metric at request *start*. Without this the timestamp would be
    /// taken when the entry is flushed (after the handler runs), skewing it by
    /// the request's own latency.
    #[metrics(timestamp)]
    timestamp: SystemTime,

    #[metrics(flags(Interned, SpanName))]
    operation: String,

    /// HTTP status code as a string (e.g. `"200"`). Emitted as a plain value,
    /// not a metric or dimension: it's there for CloudWatch Logs Insights /
    /// Contributor Insights without fragmenting the `fault`/`error` counts.
    status_code: String,

    /// Opaque, tab-scoped UUID supplied by the viewer. Plain log field only:
    /// deliberately neither a metric nor a dimension, so it can support
    /// privacy-preserving session counts without creating metric cardinality.
    session_id: Option<String>,

    /// Always `1`. The request-rate denominator.
    #[metrics(unit = Count)]
    count: u32,

    /// `1` on a `5xx` response, else `0` — the availability signal.
    #[metrics(unit = Count)]
    fault: u32,

    /// `1` on a `4xx` response, else `0` — client errors, kept separate from
    /// `fault` so they don't trip an availability alarm.
    #[metrics(unit = Count)]
    error: u32,

    /// Time to produce the response (headers). Auto-stops when the entry
    /// closes; we stop it explicitly so streaming bodies don't inflate it.
    #[metrics(unit = Millisecond)]
    latency: Timer,

    /// Operation-specific detail, set by the handler (see [`OperationMetrics`]).
    /// `None` for endpoints that emit no extra detail, so the entry carries only
    /// the generic fields above. Flattened, so when present its fields and the
    /// `op_detail` tag appear inline on the same entry.
    #[metrics(flatten)]
    op_detail: Option<OperationMetrics>,
}

/// Per-operation metric detail, attached by a handler to the response so the
/// [`record_request_metrics`] middleware can fold it into the one entry for
/// that request. Each variant carries the signals the generic `count`/`fault`/
/// `error`/`latency` fields **cannot** see — chiefly silent degradation and
/// failures that don't surface as a non-2xx status.
///
/// This is a metrique *entry enum* (`tag` + `subfield`): the `op_detail` tag
/// records the variant name, and each variant's fields are flattened onto the
/// request entry. See the metrique `enums.rs` example.
#[derive(Clone)]
#[metrics(tag(name = "op_detail"), subfield)]
pub enum OperationMetrics {
    /// `GET /api/browse` — trace-file listing.
    Browse(#[metrics(flatten)] BrowseMetrics),
    /// `GET /api/flamegraph` — on-demand fold.
    Flamegraph(#[metrics(flatten)] FlamegraphMetrics),
    /// `GET /api/tokio-stats` — long-poll aggregation.
    TokioStats(#[metrics(flatten)] TokioStatsMetrics),
}

/// Detail for `GET /api/browse`.
#[derive(Clone)]
#[metrics(subfield)]
pub struct BrowseMetrics {
    /// Trace files returned to the client.
    objects_returned: usize,
    /// Time prefixes the request fanned out to (S3 list calls).
    prefixes_fanned_out: usize,
    /// `1` when results were capped (per-prefix or range cap) — **silent data
    /// loss behind a `200`**, invisible to the status-code metrics. Alarm here
    /// to catch users seeing partial trace lists.
    #[metrics(unit = Count)]
    truncated: u32,
    /// `1` when an hour-granularity prefix overflowed and was re-listed at
    /// 10-minute granularity (extra S3 round-trips → slower, busier bucket).
    #[metrics(unit = Count)]
    refined: u32,
}

/// Detail for `GET /api/flamegraph`.
///
/// The endpoint streams (SSE): op detail is attached at response-head time, so
/// these are RESOLVE-TIME values — the scope size and how much of it was
/// already folded when the stream opened. What the stream folds afterwards is
/// not visible here (the middleware reads the extension before the body runs).
#[derive(Clone)]
#[metrics(subfield)]
pub struct FlamegraphMetrics {
    /// Source segment files in scope.
    files_matched: u32,
    /// Files already folded when the stream opened (the instantly-served
    /// snapshot).
    files_folded: u32,
    /// Resolve-time coverage percent (`files_folded / files_matched * 100`).
    /// Persistently low values mean scopes keep opening cold — folds are not
    /// sticking (e.g. output-bucket write failures) or scopes never repeat.
    #[metrics(unit = metrique::unit::Percent)]
    coverage_pct: f64,
    /// Stack samples folded into the tree. `None` (absent) on the streaming
    /// path, where samples are only known after part-files are read inside the
    /// stream body.
    samples: Option<u64>,
}

/// Detail for `GET /api/tokio-stats`.
///
/// Streaming endpoint: resolve-time values, same caveat as
/// [`FlamegraphMetrics`].
#[derive(Clone)]
#[metrics(subfield)]
pub struct TokioStatsMetrics {
    /// Source segment files in scope.
    files_matched: u32,
    /// Files already folded when the stream opened.
    files_folded: u32,
    /// Long polls above the notable-duration floor (the signal the endpoint
    /// exists to surface). `None` (absent) on the streaming path, where polls
    /// are only read inside the stream body.
    notable_polls: Option<u64>,
}

/// Standalone metric for `GET /api/object`, emitted when the response body
/// finishes streaming — **not** part of [`RequestMetrics`].
///
/// `/api/object` streams the object bytes, so the outcome that matters
/// (`truncated_mid_stream`) is only known *after* the `200` status line and
/// headers are already sent — i.e. after the request middleware has returned
/// its entry. A streamed truncation is therefore invisible both to the HTTP
/// status and to [`RequestMetrics::fault`]. This entry closes that gap: the
/// handler arms it on the byte stream so it is appended when the stream is
/// dropped (normal end or truncation), carrying the same `operation` dimension
/// as the request metric so both line up in CloudWatch.
#[metrics(emf::dimension_sets = [["operation"], []])]
pub struct ObjectStreamMetrics {
    #[metrics(flatten)]
    dial9: Dial9Context,

    #[metrics(timestamp)]
    timestamp: SystemTime,
    /// Matched route (`/api/object`), to align with the request metric's
    /// `operation` dimension.
    #[metrics(flags(Interned, SpanName))]
    operation: String,
    /// Always `1`: the count of completed object streams (the denominator for a
    /// mid-stream-truncation rate alarm).
    #[metrics(unit = Count)]
    count: u32,
    /// `1` when the body errored after the `200` headers were sent, so the
    /// client received a truncated object — the status-code `fault` metric is
    /// structurally blind to this. Alarm on the `truncated_mid_stream` / `count`
    /// ratio for streamed-response integrity.
    ///
    /// `pub` so the `/api/object` handler can flip it through the guard's
    /// `DerefMut` from the mid-stream error closure (it lives in a sibling
    /// module). Set it to `1` via the guard, e.g. `guard.truncated_mid_stream = 1`.
    #[metrics(unit = Count)]
    pub truncated_mid_stream: u32,
}

impl ObjectStreamMetrics {
    /// Arm a stream-scoped metric guard for an `/api/object` response. Append it
    /// to the global sink on drop (stream end). The handler flips
    /// `truncated_mid_stream` on the guard if a chunk errors mid-stream.
    pub fn arm(operation: impl Into<String>) -> ObjectStreamMetricsGuard {
        ObjectStreamMetrics {
            dial9: Dial9Context::capture(),
            timestamp: SystemTime::now(),
            operation: operation.into(),
            count: 1,
            truncated_mid_stream: 0,
        }
        .append_on_drop(ServiceMetrics::sink_or_discard())
    }
}

/// Standalone stream-lifetime metric for `GET /api/span-stats`, emitted when the
/// SSE body finishes streaming — **not** part of [`RequestMetrics`].
///
/// The span-stats response streams: headers and the first cumulative snapshot
/// flush in a few hundred ms, then the fold/seed work streams for many seconds
/// afterwards. [`RequestMetrics::latency`] stops at response-headers time (see
/// [`record_request_metrics`]), so it measures only time-to-first-byte, and the
/// [`FlamegraphMetrics`] coverage the handler attaches is a *resolve-time*
/// snapshot. Neither can see how long the stream actually ran or how much it
/// folded. This entry closes that gap: the handler arms it and the fold driver
/// holds it for the life of the response body, so it is appended when the stream
/// is dropped (normal end **or** client disconnect). It carries the same
/// `operation` dimension as the request metric so both line up in CloudWatch.
#[metrics(emf::dimension_sets = [["operation"], []])]
pub struct SpanStatsStreamMetrics {
    #[metrics(flatten)]
    dial9: Dial9Context,

    #[metrics(timestamp)]
    timestamp: SystemTime,
    /// Matched route (`/api/span-stats`), to align with the request metric's
    /// `operation` dimension.
    #[metrics(flags(Interned, SpanName))]
    operation: String,
    /// Always `1`: the count of completed span-stats streams (the denominator
    /// for the stream-duration and coverage rates).
    #[metrics(unit = Count)]
    count: u32,
    /// `1` when the stream ended with at least one fold error (a GET/decode/merge
    /// failure recorded in the coverage block). The HTTP status is a `200` the
    /// instant the headers flush, so a stream that then fails to fold parts is
    /// invisible to [`RequestMetrics::fault`]. `pub` so the driver can flip it
    /// through the guard's `DerefMut`.
    #[metrics(unit = Count)]
    pub failed: u32,
    /// Wall-clock from stream start (arm time) to stream end (guard drop). This
    /// is the real end-to-end duration the request metric's `latency` cannot see.
    /// Auto-stops when the entry is dropped, so it measures the full body
    /// lifetime.
    #[metrics(unit = Millisecond)]
    stream_duration: Timer,
    /// Source segment files in scope (final, = resolve-time `files_matched`).
    /// `pub` so the driver can set it from the resolved scope.
    #[metrics(unit = Count)]
    pub files_matched: u32,
    /// Files folded by the time the stream ended (FINAL, not resolve-time):
    /// `files_seeded + files_folded_cold`.
    #[metrics(unit = Count)]
    pub files_folded: u32,
    /// Files served from already-folded spans part-files (GET + merge only, no
    /// raw-trace decode) — the seeding phase. High values mean the stream time
    /// is spent re-reading Parquet parts, not cold-folding.
    #[metrics(unit = Count)]
    pub files_seeded: u32,
    /// Files cold-folded this stream (raw trace fetched, decoded, encoded) — the
    /// folding phase. High values mean the stream time is spent folding raw
    /// traces from scratch.
    #[metrics(unit = Count)]
    pub files_folded_cold: u32,
    /// Wall-clock spent downloading already-folded spans Parquet parts. Seed
    /// batches fetch concurrently, so this records batch elapsed time rather
    /// than summing each object's latency; it therefore remains comparable to
    /// `stream_duration`.
    #[metrics(unit = Millisecond)]
    pub download_duration: Duration,
    /// Wall-clock time spent decoding Parquet record batches and materializing
    /// typed span rows from Arrow arrays. Equals `reader_setup_duration` +
    /// `batch_decode_duration` + `row_materialize_duration`.
    #[metrics(unit = Millisecond)]
    pub parse_duration: Duration,
    /// Wall-clock time spent applying span filters and merging rows into
    /// histograms, exemplars, facets, and composition accumulators.
    #[metrics(unit = Millisecond)]
    pub query_duration: Duration,

    // ── Parse sub-phases (sum == parse_duration) ─────────────────────────────
    /// Parquet footer parsing and Arrow reader construction
    /// (`ParquetRecordBatchReader::try_new`).
    #[metrics(unit = Millisecond)]
    pub reader_setup_duration: Duration,
    /// Advancing the Parquet reader through column chunks and decoding them into
    /// Arrow `RecordBatch`es.
    #[metrics(unit = Millisecond)]
    pub batch_decode_duration: Duration,
    /// Walking decoded Arrow arrays and constructing owned `SpanStatsRow` values
    /// with validated fields, exemplars, attributes, and composition.
    #[metrics(unit = Millisecond)]
    pub row_materialize_duration: Duration,

    // ── Work counters ────────────────────────────────────────────────────────
    /// Total bytes presented to the Parquet reader (compressed part-file sizes).
    #[metrics(unit = Byte)]
    pub parquet_bytes: u64,
    /// Arrow RecordBatches successfully decoded across all part-files.
    #[metrics(unit = Count)]
    pub record_batches_decoded: u64,
    /// SpanStatsRow values materialized (post-validation, pre-accumulator
    /// filtering).
    #[metrics(unit = Count)]
    pub rows_materialized: u64,
    /// Total attribute key/value entries materialized across all rows.
    #[metrics(unit = Count)]
    pub attribute_entries: u64,
}

/// Cumulative span-stats work measured inside one seed or fold operation. The
/// fold-stream driver drains this from the endpoint sink after each operation
/// and adds it to [`SpanStatsStreamMetrics`].
///
/// ## Parse sub-phases
///
/// `parse` = `reader_setup` + `batch_decode` + `row_materialize`. The three
/// sub-phases decompose the cumulative `parse_duration` into:
///
/// - **reader_setup**: In-memory Parquet footer parsing and Arrow schema
///   negotiation (`ParquetRecordBatchReader::try_new`).
/// - **batch_decode**: Advancing the Parquet reader through column chunks and
///   decoding them into Arrow `RecordBatch`es (`reader.next()`).
/// - **row_materialize**: Walking the decoded Arrow arrays and constructing
///   owned `SpanStatsRow` values with validated fields, exemplars, attributes,
///   and composition — the seam between columnar Arrow data and our row-oriented
///   domain model.
///
/// The invariant `parse == reader_setup + batch_decode + row_materialize` is
/// maintained so existing dashboards on `parse_duration` remain valid.
///
/// ## Work counters
///
/// Alongside durations, low-cost counters track throughput:
///
/// - **parquet_bytes**: Total bytes presented to the Parquet reader (compressed
///   on-wire size of the spans part-file).
/// - **record_batches_decoded**: Number of Arrow `RecordBatch`es successfully
///   decoded from all part-files in this operation.
/// - **rows_materialized**: `SpanStatsRow` values produced (after time-filter
///   and null/negative validation, before attribute/cardinality filtering in the
///   accumulator).
/// - **attribute_entries**: Total key/value attribute pairs materialized across
///   all rows (sum of per-row attribute map lengths).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SpanStatsPhaseDurations {
    pub(crate) download: Duration,
    pub(crate) parse: Duration,
    pub(crate) query: Duration,

    // ── Parse sub-phases (sum == parse) ──────────────────────────────────────
    /// Parquet footer parsing and Arrow reader construction.
    pub(crate) reader_setup: Duration,
    /// Decoding column chunks into Arrow RecordBatches.
    pub(crate) batch_decode: Duration,
    /// Walking Arrow arrays to produce owned SpanStatsRow values.
    pub(crate) row_materialize: Duration,

    // ── Work counters ────────────────────────────────────────────────────────
    /// Bytes presented to ParquetRecordBatchReader (compressed part size).
    pub(crate) parquet_bytes: u64,
    /// RecordBatches successfully decoded.
    pub(crate) record_batches_decoded: u64,
    /// SpanStatsRow values produced (post-validation, pre-accumulator filter).
    pub(crate) rows_materialized: u64,
    /// Total attribute key/value entries across all materialized rows.
    pub(crate) attribute_entries: u64,
}

impl std::ops::AddAssign for SpanStatsPhaseDurations {
    fn add_assign(&mut self, rhs: Self) {
        self.download += rhs.download;
        self.parse += rhs.parse;
        self.query += rhs.query;

        self.reader_setup += rhs.reader_setup;
        self.batch_decode += rhs.batch_decode;
        self.row_materialize += rhs.row_materialize;

        self.parquet_bytes += rhs.parquet_bytes;
        self.record_batches_decoded += rhs.record_batches_decoded;
        self.rows_materialized += rhs.rows_materialized;
        self.attribute_entries += rhs.attribute_entries;
    }
}

impl SpanStatsStreamMetrics {
    /// Arm a stream-scoped metric guard for a `/api/span-stats` response. The
    /// fold driver holds the guard for the life of the body and updates the
    /// coverage fields as it seeds/folds; the entry (with the elapsed
    /// `stream_duration`) is appended to the global sink on drop (stream end or
    /// client disconnect).
    pub fn arm(operation: impl Into<String>) -> SpanStatsStreamMetricsGuard {
        SpanStatsStreamMetrics {
            dial9: Dial9Context::capture(),
            timestamp: SystemTime::now(),
            operation: operation.into(),
            count: 1,
            failed: 0,
            stream_duration: Timer::start_now(),
            files_matched: 0,
            files_folded: 0,
            files_seeded: 0,
            files_folded_cold: 0,
            download_duration: Duration::ZERO,
            parse_duration: Duration::ZERO,
            query_duration: Duration::ZERO,
            reader_setup_duration: Duration::ZERO,
            batch_decode_duration: Duration::ZERO,
            row_materialize_duration: Duration::ZERO,
            parquet_bytes: 0,
            record_batches_decoded: 0,
            rows_materialized: 0,
            attribute_entries: 0,
        }
        .append_on_drop(ServiceMetrics::sink_or_discard())
    }
}

/// One entry per source file folded, carrying the per-phase timing breakdown of
/// processing that file — emitted so we can see *where* fold time goes without a
/// profiler. This is the metric that answers "why is the server slow": it splits
/// a fold into fetch (S3 GET) → gunzip → the individual decode phases →
/// parquet-encode → write (S3 PUTs), alongside the file's size and event count.
///
/// Standalone (not part of [`RequestMetrics`]): one HTTP request folds many
/// files, so folding to the request entry would collapse per-file detail. Each
/// fold appends its own entry to the global sink on drop, sharing the same empty
/// global dimension set so per-file timings aggregate across the fleet.
///
/// All phase durations are wall-clock and measured in sequence, so they sum
/// (modulo scheduling gaps) to `total`.
#[metrics(emf::dimension_sets = [[]])]
pub struct FoldFileMetrics {
    #[metrics(flatten)]
    dial9: Dial9Context,

    #[metrics(timestamp)]
    timestamp: SystemTime,

    /// Always `1`: the count of folded files (denominator for per-file rates).
    #[metrics(unit = Count)]
    count: u32,

    /// `1` when the fold failed (fetch/decode/write error). The phase timings up
    /// to the failure point are still emitted; later phases are zero.
    #[metrics(unit = Count)]
    failed: u32,

    /// Compressed size of the source object as fetched from S3 (bytes).
    #[metrics(unit = Byte)]
    source_bytes: u64,

    /// Decompressed size handed to the decoder (bytes). The ratio to
    /// `source_bytes` is the gzip ratio.
    #[metrics(unit = Byte)]
    decompressed_bytes: u64,

    /// Total events decoded off the wire (all event types).
    #[metrics(unit = Count)]
    events_decoded: u64,

    /// Span enter/exit/close events (subset of `events_decoded`) — the span path
    /// is the one under scrutiny, so it gets its own count.
    #[metrics(unit = Count)]
    span_events_decoded: u64,

    // ── Phase timings (sum ≈ total) ──────────────────────────────────────────
    /// S3 GET of the source object.
    #[metrics(unit = Millisecond)]
    fetch: Duration,
    /// gunzip of the fetched bytes.
    #[metrics(unit = Millisecond)]
    gunzip: Duration,
    /// One-pass wire decode into typed events.
    #[metrics(unit = Millisecond)]
    wire_decode: Duration,
    /// Sort the event stream by timestamp.
    #[metrics(unit = Millisecond)]
    sort_events: Duration,
    /// Reconstruct the poll timeline from park/unpark/poll events.
    #[metrics(unit = Millisecond)]
    poll_reconstruct: Duration,
    /// Sample loop: symbolication + per-sample poll attribution.
    #[metrics(unit = Millisecond)]
    sample_resolve: Duration,
    /// Legacy span reconstruction (enter/exit pairing, close segmentation).
    #[metrics(unit = Millisecond)]
    span_resolve: Duration,
    /// Sweep-line sample→span attribution.
    #[metrics(unit = Millisecond)]
    sample_attribution: Duration,
    /// Parquet-encode of the four part-files (samples, dict, polls, spans).
    #[metrics(unit = Millisecond)]
    parquet_encode: Duration,
    /// S3 PUTs of the four part-files.
    #[metrics(unit = Millisecond)]
    write_parts: Duration,
    /// Wall-clock time for the whole fold (fetch → write), including any time
    /// spent waiting on the fold concurrency semaphores.
    #[metrics(unit = Millisecond)]
    total: Duration,
}

/// Builder for a [`FoldFileMetrics`] entry. The fold path fills phases in as it
/// runs, then calls [`emit`](Self::emit) once to append the entry to the global
/// sink. Kept separate from the metric struct so the metric's fields stay
/// private (new phases are non-breaking).
pub struct FoldFileMetricsBuilder {
    dial9: Dial9Context,
    source_bytes: u64,
    decompressed_bytes: u64,
    events_decoded: u64,
    span_events_decoded: u64,
    fetch: Duration,
    gunzip: Duration,
    wire_decode: Duration,
    sort_events: Duration,
    poll_reconstruct: Duration,
    sample_resolve: Duration,
    span_resolve: Duration,
    sample_attribution: Duration,
    parquet_encode: Duration,
    write_parts: Duration,
    total: Duration,
    failed: bool,
}

impl Default for FoldFileMetricsBuilder {
    fn default() -> Self {
        Self {
            dial9: Dial9Context::capture(),
            source_bytes: 0,
            decompressed_bytes: 0,
            events_decoded: 0,
            span_events_decoded: 0,
            fetch: Duration::ZERO,
            gunzip: Duration::ZERO,
            wire_decode: Duration::ZERO,
            sort_events: Duration::ZERO,
            poll_reconstruct: Duration::ZERO,
            sample_resolve: Duration::ZERO,
            span_resolve: Duration::ZERO,
            sample_attribution: Duration::ZERO,
            parquet_encode: Duration::ZERO,
            write_parts: Duration::ZERO,
            total: Duration::ZERO,
            failed: false,
        }
    }
}

impl FoldFileMetricsBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn source_bytes(&mut self, n: u64) -> &mut Self {
        self.source_bytes = n;
        self
    }
    pub fn decompressed_bytes(&mut self, n: u64) -> &mut Self {
        self.decompressed_bytes = n;
        self
    }
    pub fn fetch(&mut self, d: Duration) -> &mut Self {
        self.fetch = d;
        self
    }
    pub fn gunzip(&mut self, d: Duration) -> &mut Self {
        self.gunzip = d;
        self
    }
    /// Set every decode sub-phase at once from the decoder's own [`DecodeStats`].
    pub fn decode_phases(&mut self, s: &crate::ingest::decode::DecodeStats) -> &mut Self {
        self.wire_decode = s.wire_decode;
        self.sort_events = s.sort_events;
        self.poll_reconstruct = s.poll_reconstruct;
        self.sample_resolve = s.sample_resolve;
        self.span_resolve = s.span_resolve;
        self.sample_attribution = s.sample_attribution;
        self.events_decoded = s.events_decoded;
        self.span_events_decoded = s.span_events_decoded;
        self
    }
    pub fn parquet_encode(&mut self, d: Duration) -> &mut Self {
        self.parquet_encode = d;
        self
    }
    pub fn write_parts(&mut self, d: Duration) -> &mut Self {
        self.write_parts = d;
        self
    }
    pub fn total(&mut self, d: Duration) -> &mut Self {
        self.total = d;
        self
    }
    pub fn failed(&mut self, failed: bool) -> &mut Self {
        self.failed = failed;
        self
    }

    /// Append the assembled entry to the global metrics sink. A no-op sink is
    /// used when none is attached (tests), matching the rest of this module.
    pub fn emit(self) {
        FoldFileMetrics {
            dial9: self.dial9,
            timestamp: SystemTime::now(),
            count: 1,
            failed: self.failed as u32,
            source_bytes: self.source_bytes,
            decompressed_bytes: self.decompressed_bytes,
            events_decoded: self.events_decoded,
            span_events_decoded: self.span_events_decoded,
            fetch: self.fetch,
            gunzip: self.gunzip,
            wire_decode: self.wire_decode,
            sort_events: self.sort_events,
            poll_reconstruct: self.poll_reconstruct,
            sample_resolve: self.sample_resolve,
            span_resolve: self.span_resolve,
            sample_attribution: self.sample_attribution,
            parquet_encode: self.parquet_encode,
            write_parts: self.write_parts,
            total: self.total,
        }
        .append_on_drop(ServiceMetrics::sink_or_discard());
    }
}

// Constructors keep the metric fields private (so new fields stay
// non-breaking) while letting handlers build the detail with plain bool/number
// args. `u32` flag fields map from `bool` here, in one place.
impl OperationMetrics {
    /// Browse detail. `truncated`: results were capped (data loss behind a
    /// 200). `refined`: an hour prefix overflowed and was re-listed finer.
    pub fn browse(
        objects_returned: usize,
        prefixes_fanned_out: usize,
        truncated: bool,
        refined: bool,
    ) -> Self {
        Self::Browse(BrowseMetrics {
            objects_returned,
            prefixes_fanned_out,
            truncated: truncated as u32,
            refined: refined as u32,
        })
    }

    /// Flamegraph detail. `coverage_pct` is computed here from the folded /
    /// matched file counts (0.0 when nothing matched, to avoid NaN).
    pub fn flamegraph(files_matched: u32, files_folded: u32, samples: Option<u64>) -> Self {
        let coverage_pct = if files_matched == 0 {
            0.0
        } else {
            (files_folded as f64 / files_matched as f64) * 100.0
        };
        Self::Flamegraph(FlamegraphMetrics {
            files_matched,
            files_folded,
            coverage_pct,
            samples,
        })
    }

    /// Tokio-stats detail.
    pub fn tokio_stats(files_matched: u32, files_folded: u32, notable_polls: Option<u64>) -> Self {
        Self::TokioStats(TokioStatsMetrics {
            files_matched,
            files_folded,
            notable_polls,
        })
    }
}

/// Axum middleware that emits one [`RequestMetrics`] entry per request to the
/// global [`ServiceMetrics`] sink.
///
/// Wired via [`axum::middleware::from_fn`], so it carries no state. The
/// [`MatchedPath`] is read from the request extensions (axum inserts it during
/// routing, so it is available to a router `layer`); requests that matched no
/// route fall back to [`UNMATCHED_OPERATION`].
pub async fn record_request_metrics(req: Request, next: Next) -> Response {
    // Read the matched-path template *before* `next.run` consumes the request.
    let operation = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| UNMATCHED_OPERATION.to_string());
    let session_id = validated_session_id(req.headers());

    let mut metrics = RequestMetrics {
        dial9: Dial9Context::capture(),
        timestamp: SystemTime::now(),
        operation,
        status_code: String::new(),
        session_id,
        count: 1,
        fault: 0,
        error: 0,
        latency: Timer::start_now(),
        op_detail: None,
    }
    // `sink_or_discard` (not `sink`) so this never panics when no sink is
    // attached: the deployed `serve` path attaches one at startup, but tests
    // that build the router directly do not. Entries are forwarded to the
    // attached (or test) sink if present, else silently dropped.
    .append_on_drop(ServiceMetrics::sink_or_discard());

    let mut response = next.run(req).await;

    // Stop the timer at response-headers time (not body completion): for a
    // streamed body the bytes flow after this middleware returns.
    metrics.latency.stop();
    let status = response.status();
    metrics.status_code = status.as_u16().to_string();
    if status.is_server_error() {
        metrics.fault = 1;
    } else if status.is_client_error() {
        metrics.error = 1;
    }

    // Fold in any operation-specific detail the handler stashed on the response
    // (see `OperationMetrics`). Removed so it does not linger in the response
    // extensions sent to the client.
    metrics.op_detail = response.extensions_mut().remove::<OperationMetrics>();

    response
}

/// Read only canonical, lower-case UUID text from the session header. Invalid
/// or missing values are omitted from the log entry; they never reject the
/// request. Checking the exact byte length before parsing bounds allocation and
/// prevents arbitrary high-cardinality or log-injection content from reaching
/// structured logs.
fn validated_session_id(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(SESSION_ID_HEADER)?.as_bytes();
    if raw.len() != UUID_TEXT_LEN {
        return None;
    }
    let text = std::str::from_utf8(raw).ok()?;
    let parsed = uuid::Uuid::parse_str(text).ok()?;
    (parsed.hyphenated().to_string() == text).then(|| text.to_string())
}

/// Attach the process-global [`ServiceMetrics`] sink for request metrics, once,
/// at startup. Hold the returned [`AttachHandle`] for the life of the process;
/// dropping it flushes and detaches the sink.
///
/// - `local == false` (deployed): EMF to **stdout**, the format CloudWatch
///   ingests from the log stream.
/// - `local == true` (`--local`): metrique's human-readable [`LocalFormat`] to
///   stdout, for local runs.
///
/// When called from a dial9-instrumented runtime, the stream is also connected
/// to that runtime's current recorder and opted-in entries become dial9 spans.
/// Without an active recorder, the dial9 side of the stream is inert.
pub fn attach_request_metrics(local: bool) -> AttachHandle {
    let dial9 = Dial9Handle::current();
    if local {
        attach_request_metrics_to_stream(
            &dial9,
            LocalFormat::new(OutputStyle::Pretty).output_to_makewriter(|| std::io::stdout().lock()),
        )
    } else {
        // `vec![vec![]]` = a single empty global dimension set, which the
        // entry-level `dimension_sets` are cartesian-joined onto, yielding the
        // final `["operation"]` and `[]` sets.
        attach_request_metrics_to_stream(
            &dial9,
            Emf::builder(CLOUDWATCH_NAMESPACE.to_string(), vec![vec![]])
                .build()
                .output_to_makewriter(|| std::io::stdout().lock()),
        )
    }
}

fn attach_request_metrics_to_stream(
    dial9: &Dial9Handle,
    output: impl EntryIoStream + Send + 'static,
) -> AttachHandle {
    ServiceMetrics::attach_to_stream(Dial9Stream::tee(dial9, output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use dial9_core::buffer::DiskBuffer;
    use dial9_core::recorder::recorder;
    use dial9_core::schema_extensions::{ROLE_KEY, SPAN_TYPE_KEY, roles, span_types};
    use metrique::test_util::{TestEntrySink, test_entry_sink};
    use std::collections::{HashMap, HashSet};
    use tower::ServiceExt; // for `oneshot`

    struct FieldRecordingStream(std::sync::Arc<std::sync::Mutex<HashSet<String>>>);

    impl EntryIoStream for FieldRecordingStream {
        fn next(
            &mut self,
            entry: &impl metrique::writer::Entry,
        ) -> Result<(), metrique::writer::IoStreamError> {
            struct FieldCollector<'a>(&'a mut HashSet<String>);

            impl<'a> metrique::writer::EntryWriter<'a> for FieldCollector<'_> {
                fn timestamp(&mut self, _timestamp: SystemTime) {}

                fn value(
                    &mut self,
                    name: impl Into<std::borrow::Cow<'a, str>>,
                    _value: &(impl metrique::writer::Value + ?Sized),
                ) {
                    self.0.insert(name.into().into_owned());
                }

                fn config(&mut self, _config: &'a dyn metrique::writer::EntryConfig) {}
            }

            entry.write(&mut FieldCollector(&mut self.0.lock().unwrap()));
            Ok(())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn viewer_metrics_are_recorded_as_dial9_spans() {
        #[derive(Debug)]
        struct RecordedSchema {
            fields: HashSet<String>,
            annotations: HashMap<String, HashMap<String, String>>,
        }

        #[derive(Debug, serde::Deserialize)]
        struct DecodedRequestMetrics {
            operation: String,
            session_id: Option<String>,
        }

        let dir = tempfile::tempdir().unwrap();
        let trace_path = dir.path().join("viewer-metrics.bin");
        let recorder = recorder(DiskBuffer::single_file(&trace_path).unwrap()).build();
        recorder.enable();

        let downstream_fields = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));
        let attached = attach_request_metrics_to_stream(
            recorder.handle(),
            FieldRecordingStream(downstream_fields.clone()),
        );

        drop(
            RequestMetrics {
                dial9: Dial9Context::capture(),
                timestamp: SystemTime::now(),
                operation: "/api/browse".to_string(),
                status_code: "200".to_string(),
                session_id: Some("123e4567-e89b-42d3-a456-426614174000".to_string()),
                count: 1,
                fault: 0,
                error: 0,
                latency: Timer::start_now(),
                op_detail: None,
            }
            .append_on_drop(ServiceMetrics::sink_or_discard()),
        );
        drop(ObjectStreamMetrics::arm("/api/object"));
        drop(SpanStatsStreamMetrics::arm("/api/span-stats"));
        FoldFileMetricsBuilder::new().emit();

        // Drain metrique while the recorder can still accept the queued entries.
        drop(attached);
        recorder.graceful_shutdown(Duration::from_secs(5));

        let downstream_fields = downstream_fields.lock().unwrap();
        assert!(
            downstream_fields.contains("operation") && downstream_fields.contains("session_id"),
            "ordinary metric fields must reach the non-dial9 stream: {downstream_fields:#?}"
        );
        assert!(
            !downstream_fields
                .iter()
                .any(|name| name.starts_with("dial9.")),
            "dial9 context fields leaked into the normal metric stream: {downstream_fields:#?}"
        );

        let sealed_trace = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| path.to_string_lossy().contains(".bin"))
            .expect("recorder must seal a trace segment");
        let data = std::fs::read(sealed_trace).unwrap();
        let mut decoder = dial9_trace_format::decoder::Decoder::new(&data).unwrap();
        let mut schemas = HashMap::new();
        let mut request_metrics = None;
        decoder
            .for_each_event(|event| {
                if !event.name.starts_with("metrique:") {
                    return;
                }
                if event.name == "metrique:RequestMetrics" {
                    request_metrics = Some(
                        event
                            .deserialize::<DecodedRequestMetrics>()
                            .expect("request metrics event must deserialize"),
                    );
                }
                let fields: HashSet<_> = event
                    .schema
                    .fields()
                    .iter()
                    .map(|field| field.name().to_string())
                    .collect();
                let mut annotations: HashMap<String, HashMap<String, String>> = HashMap::new();
                for annotation in event.schema.annotations() {
                    let field = event.schema.fields()[usize::from(annotation.field_index())].name();
                    annotations
                        .entry(field.to_string())
                        .or_default()
                        .insert(annotation.key().to_string(), annotation.value().to_string());
                }
                schemas.insert(
                    event.name.to_string(),
                    RecordedSchema {
                        fields,
                        annotations,
                    },
                );
            })
            .unwrap();

        let expected = [
            "metrique:RequestMetrics",
            "metrique:ObjectStreamMetrics",
            "metrique:SpanStatsStreamMetrics",
            "metrique:FoldFileMetrics",
        ];
        for name in expected {
            let schema = schemas
                .get(name)
                .unwrap_or_else(|| panic!("missing dial9 span schema {name}: {schemas:#?}"));
            assert_eq!(
                schema.annotations[dial9_metrique::field_names::SPAN_DURATION_NS][SPAN_TYPE_KEY],
                span_types::METRIQUE,
                "{name} is not annotated as a metrique span"
            );
            assert_eq!(
                schema.annotations[dial9_metrique::field_names::SPAN_DURATION_NS][ROLE_KEY],
                roles::SPAN_DURATION,
                "{name} has no span duration role"
            );
        }

        for name in [
            "metrique:RequestMetrics",
            "metrique:ObjectStreamMetrics",
            "metrique:SpanStatsStreamMetrics",
        ] {
            assert_eq!(
                schemas[name].annotations["operation"][ROLE_KEY],
                roles::SPAN_NAME,
                "{name} does not use operation as its span name"
            );
        }
        assert!(
            schemas["metrique:RequestMetrics"]
                .fields
                .contains("session_id"),
            "session UUID must remain available for per-session trace correlation"
        );
        let request_metrics = request_metrics.expect("request metrics event must be recorded");
        assert_eq!(
            request_metrics.session_id.as_deref(),
            Some("123e4567-e89b-42d3-a456-426614174000")
        );
        assert_eq!(
            request_metrics.operation, "/api/browse",
            "the operation selected as span.name must retain its value"
        );
    }

    /// Drive one request through the middleware against a test sink installed
    /// on the current (single-threaded) tokio runtime, and return the single
    /// captured entry. The runtime-scoped guard keeps parallel tests isolated.
    async fn run_with_session(
        route: &str,
        uri: &str,
        status: StatusCode,
        session_id: Option<&str>,
    ) -> metrique::test_util::TestEntry {
        let TestEntrySink { inspector, sink } = test_entry_sink();
        let _guard = ServiceMetrics::set_test_sink_on_current_tokio_runtime(sink);

        let app: Router = Router::new()
            .route(route, get(move || async move { status }))
            .layer(axum::middleware::from_fn(record_request_metrics));

        let mut request = Request::builder().uri(uri);
        if let Some(session_id) = session_id {
            request = request.header(SESSION_ID_HEADER, session_id);
        }
        let resp = app
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), status);

        let entries = inspector.entries();
        assert_eq!(entries.len(), 1, "exactly one metric entry per request");
        entries.into_iter().next().unwrap()
    }

    async fn run(route: &str, uri: &str, status: StatusCode) -> metrique::test_util::TestEntry {
        run_with_session(route, uri, status, None).await
    }

    #[tokio::test]
    async fn server_error_is_a_fault() {
        let e = run(
            "/items/{id}",
            "/items/42",
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .await;
        // Operation is the matched template, not the concrete `/items/42`.
        assert_eq!(e.values["operation"], "/items/{id}");
        assert_eq!(e.values["status_code"], "500");
        assert_eq!(e.metrics["count"].as_u64(), 1);
        assert_eq!(e.metrics["fault"].as_u64(), 1);
        assert_eq!(e.metrics["error"].as_u64(), 0);
        // Latency metric is present (a single observation).
        assert_eq!(e.metrics["latency"].num_observations(), 1);
    }

    #[tokio::test]
    async fn client_error_is_an_error_not_a_fault() {
        let e = run("/x", "/x", StatusCode::BAD_REQUEST).await;
        assert_eq!(e.metrics["fault"].as_u64(), 0);
        assert_eq!(e.metrics["error"].as_u64(), 1);
        assert_eq!(e.values["status_code"], "400");
    }

    #[tokio::test]
    async fn success_is_neither_fault_nor_error() {
        let e = run("/x", "/x", StatusCode::OK).await;
        assert_eq!(e.metrics["fault"].as_u64(), 0);
        assert_eq!(e.metrics["error"].as_u64(), 0);
        assert_eq!(e.metrics["count"].as_u64(), 1);
    }

    #[tokio::test]
    async fn valid_session_id_is_a_plain_log_field() {
        let session_id = "123e4567-e89b-42d3-a456-426614174000";
        let e = run_with_session("/x", "/x", StatusCode::OK, Some(session_id)).await;
        assert_eq!(e.values["session_id"], session_id);
        assert!(!e.metrics.contains_key("session_id"));
    }

    #[tokio::test]
    async fn missing_or_invalid_session_id_is_accepted_and_omitted() {
        let missing = run("/x", "/x", StatusCode::OK).await;
        assert!(!missing.values.contains_key("session_id"));

        let invalid = run_with_session(
            "/x",
            "/x",
            StatusCode::OK,
            Some("00000000-0000-0000-0000-00000000000z"),
        )
        .await;
        assert!(!invalid.values.contains_key("session_id"));

        let oversized = run_with_session(
            "/x",
            "/x",
            StatusCode::OK,
            Some("123e4567-e89b-42d3-a456-426614174000-extra"),
        )
        .await;
        assert!(!oversized.values.contains_key("session_id"));
    }

    #[tokio::test]
    async fn no_op_detail_when_handler_attaches_none() {
        // A plain handler attaches no `OperationMetrics`, so the op-specific
        // fields and the `op_detail` tag are absent.
        let e = run("/x", "/x", StatusCode::OK).await;
        assert!(!e.values.contains_key("op_detail"));
        assert!(!e.metrics.contains_key("truncated"));
    }

    /// Drive one request whose handler attaches `op` to the response, and return
    /// the folded entry.
    async fn run_with_op(route: &str, op: OperationMetrics) -> metrique::test_util::TestEntry {
        let TestEntrySink { inspector, sink } = test_entry_sink();
        let _guard = ServiceMetrics::set_test_sink_on_current_tokio_runtime(sink);

        let app: Router = Router::new()
            .route(route, get(move || async move { axum::Extension(op) }))
            .layer(axum::middleware::from_fn(record_request_metrics));

        app.oneshot(Request::builder().uri(route).body(Body::empty()).unwrap())
            .await
            .unwrap();

        let entries = inspector.entries();
        assert_eq!(entries.len(), 1, "exactly one metric entry per request");
        entries.into_iter().next().unwrap()
    }

    #[tokio::test]
    async fn browse_op_detail_folds_into_entry() {
        // 7 objects, 4 prefixes, capped (truncated), not refined.
        let e = run_with_op("/api/browse", OperationMetrics::browse(7, 4, true, false)).await;
        // The enum tag records the variant.
        assert_eq!(e.values["op_detail"], "Browse");
        assert_eq!(e.metrics["objects_returned"].as_u64(), 7);
        assert_eq!(e.metrics["prefixes_fanned_out"].as_u64(), 4);
        assert_eq!(e.metrics["truncated"].as_u64(), 1);
        assert_eq!(e.metrics["refined"].as_u64(), 0);
        // Generic fields are still present on the same entry.
        assert_eq!(e.metrics["count"].as_u64(), 1);
    }

    #[tokio::test]
    async fn flamegraph_op_detail_computes_coverage() {
        // 50 of 200 files folded → 25% coverage.
        let e = run_with_op(
            "/api/flamegraph",
            OperationMetrics::flamegraph(200, 50, Some(9000)),
        )
        .await;
        assert_eq!(e.values["op_detail"], "Flamegraph");
        assert_eq!(e.metrics["files_matched"].as_u64(), 200);
        assert_eq!(e.metrics["files_folded"].as_u64(), 50);
        assert_eq!(e.metrics["coverage_pct"].as_f64(), 25.0);
        assert_eq!(e.metrics["samples"].as_u64(), 9000);
    }

    #[tokio::test]
    async fn flamegraph_unknown_samples_are_absent_not_zero() {
        // The streaming path can't know samples at response-head time; the
        // field must be ABSENT (never a fake 0 that would drag averages down).
        let e = run_with_op(
            "/api/flamegraph",
            OperationMetrics::flamegraph(200, 50, None),
        )
        .await;
        assert!(!e.metrics.contains_key("samples"));
        assert_eq!(e.metrics["coverage_pct"].as_f64(), 25.0);
    }

    #[tokio::test]
    async fn flamegraph_coverage_is_zero_when_nothing_matched() {
        // Guard against NaN (0/0) when the scope matched no files.
        let e = run_with_op("/api/flamegraph", OperationMetrics::flamegraph(0, 0, None)).await;
        assert_eq!(e.metrics["coverage_pct"].as_f64(), 0.0);
    }

    #[test]
    fn span_stats_phase_durations_add_each_phase_independently() {
        let mut total = SpanStatsPhaseDurations {
            download: Duration::from_millis(10),
            parse: Duration::from_millis(20),
            query: Duration::from_millis(30),
            reader_setup: Duration::from_millis(5),
            batch_decode: Duration::from_millis(8),
            row_materialize: Duration::from_millis(7),
            parquet_bytes: 1000,
            record_batches_decoded: 2,
            rows_materialized: 50,
            attribute_entries: 100,
        };
        total += SpanStatsPhaseDurations {
            download: Duration::from_millis(1),
            parse: Duration::from_millis(2),
            query: Duration::from_millis(3),
            reader_setup: Duration::from_millis(1),
            batch_decode: Duration::from_millis(1),
            row_materialize: Duration::from_millis(0),
            parquet_bytes: 500,
            record_batches_decoded: 1,
            rows_materialized: 25,
            attribute_entries: 40,
        };

        assert_eq!(total.download, Duration::from_millis(11));
        assert_eq!(total.parse, Duration::from_millis(22));
        assert_eq!(total.query, Duration::from_millis(33));
        assert_eq!(total.reader_setup, Duration::from_millis(6));
        assert_eq!(total.batch_decode, Duration::from_millis(9));
        assert_eq!(total.row_materialize, Duration::from_millis(7));
        assert_eq!(total.parquet_bytes, 1500);
        assert_eq!(total.record_batches_decoded, 3);
        assert_eq!(total.rows_materialized, 75);
        assert_eq!(total.attribute_entries, 140);
    }

    #[tokio::test]
    async fn object_stream_metric_is_a_separate_entry() {
        let TestEntrySink { inspector, sink } = test_entry_sink();
        let _guard = ServiceMetrics::set_test_sink_on_current_tokio_runtime(sink);

        // Arm, then simulate a mid-stream truncation, then drop → one entry.
        let mut guard = ObjectStreamMetrics::arm("/api/object");
        guard.truncated_mid_stream = 1;
        drop(guard);

        let entries = inspector.entries();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.values["operation"], "/api/object");
        assert_eq!(e.metrics["count"].as_u64(), 1);
        assert_eq!(e.metrics["truncated_mid_stream"].as_u64(), 1);
    }

    #[tokio::test]
    async fn object_stream_metric_clean_when_not_truncated() {
        let TestEntrySink { inspector, sink } = test_entry_sink();
        let _guard = ServiceMetrics::set_test_sink_on_current_tokio_runtime(sink);

        drop(ObjectStreamMetrics::arm("/api/object"));

        let e = inspector.get(0);
        assert_eq!(e.metrics["truncated_mid_stream"].as_u64(), 0);
    }

    #[tokio::test]
    async fn span_stats_stream_metric_carries_duration_and_final_coverage() {
        let TestEntrySink { inspector, sink } = test_entry_sink();
        let _guard = ServiceMetrics::set_test_sink_on_current_tokio_runtime(sink);

        // Arm, let a little wall-clock elapse, set FINAL coverage split, drop → one entry.
        let mut guard = SpanStatsStreamMetrics::arm("/api/span-stats");
        guard.files_matched = 1678;
        guard.files_seeded = 154;
        guard.files_folded_cold = 20;
        guard.files_folded = 174;
        guard.download_duration = Duration::from_millis(120);
        guard.parse_duration = Duration::from_millis(230);
        guard.query_duration = Duration::from_millis(340);
        // Sub-phases that sum to parse_duration.
        guard.reader_setup_duration = Duration::from_millis(50);
        guard.batch_decode_duration = Duration::from_millis(80);
        guard.row_materialize_duration = Duration::from_millis(100);
        // Work counters.
        guard.parquet_bytes = 12_345_678;
        guard.record_batches_decoded = 42;
        guard.rows_materialized = 150_000;
        guard.attribute_entries = 300_000;
        // Non-zero elapsed so stream_duration is a real observation.
        tokio::time::sleep(Duration::from_millis(5)).await;
        drop(guard);

        let entries = inspector.entries();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        // Aligns with the request metric's `operation` dimension.
        assert_eq!(e.values["operation"], "/api/span-stats");
        assert_eq!(e.metrics["count"].as_u64(), 1);
        assert_eq!(e.metrics["failed"].as_u64(), 0);
        // Final coverage fields, seeded-vs-cold split summing to files_folded.
        assert_eq!(e.metrics["files_matched"].as_u64(), 1678);
        assert_eq!(e.metrics["files_folded"].as_u64(), 174);
        assert_eq!(e.metrics["files_seeded"].as_u64(), 154);
        assert_eq!(e.metrics["files_folded_cold"].as_u64(), 20);
        // Each phase is emitted as one cumulative stream observation.
        assert_eq!(e.metrics["download_duration"].num_observations(), 1);
        assert_eq!(e.metrics["parse_duration"].num_observations(), 1);
        assert_eq!(e.metrics["query_duration"].num_observations(), 1);
        // Sub-phase durations are also present.
        assert_eq!(e.metrics["reader_setup_duration"].num_observations(), 1);
        assert_eq!(e.metrics["batch_decode_duration"].num_observations(), 1);
        assert_eq!(e.metrics["row_materialize_duration"].num_observations(), 1);
        // Work counters.
        assert_eq!(e.metrics["parquet_bytes"].as_u64(), 12_345_678);
        assert_eq!(e.metrics["record_batches_decoded"].as_u64(), 42);
        assert_eq!(e.metrics["rows_materialized"].as_u64(), 150_000);
        assert_eq!(e.metrics["attribute_entries"].as_u64(), 300_000);
        // The stream-duration timer emitted a single observation (a real elapsed).
        assert_eq!(e.metrics["stream_duration"].num_observations(), 1);
    }

    #[tokio::test]
    async fn span_stats_stream_metric_flags_failure() {
        let TestEntrySink { inspector, sink } = test_entry_sink();
        let _guard = ServiceMetrics::set_test_sink_on_current_tokio_runtime(sink);

        let mut guard = SpanStatsStreamMetrics::arm("/api/span-stats");
        guard.failed = 1;
        drop(guard);

        let e = inspector.get(0);
        assert_eq!(e.metrics["failed"].as_u64(), 1);
    }

    #[tokio::test]
    async fn fold_file_metric_carries_phases_size_and_counts() {
        let TestEntrySink { inspector, sink } = test_entry_sink();
        let _guard = ServiceMetrics::set_test_sink_on_current_tokio_runtime(sink);

        let mut b = FoldFileMetricsBuilder::new();
        b.source_bytes(1_257_686)
            .decompressed_bytes(2_314_769)
            .fetch(Duration::from_millis(120))
            .gunzip(Duration::from_millis(5))
            .parquet_encode(Duration::from_millis(8))
            .write_parts(Duration::from_millis(30));
        let decode = crate::ingest::decode::DecodeStats {
            events_decoded: 1_300_000,
            span_events_decoded: 4_368,
            wire_decode: Duration::from_millis(20),
            span_resolve: Duration::from_millis(3),
            ..Default::default()
        };
        b.decode_phases(&decode).total(Duration::from_millis(200));
        b.emit();

        let e = inspector.get(0);
        assert_eq!(e.metrics["count"].as_u64(), 1);
        assert_eq!(e.metrics["failed"].as_u64(), 0);
        assert_eq!(e.metrics["source_bytes"].as_u64(), 1_257_686);
        assert_eq!(e.metrics["decompressed_bytes"].as_u64(), 2_314_769);
        assert_eq!(e.metrics["events_decoded"].as_u64(), 1_300_000);
        assert_eq!(e.metrics["span_events_decoded"].as_u64(), 4_368);
        // A representative phase and the total are present.
        assert_eq!(e.metrics["wire_decode"].num_observations(), 1);
        assert_eq!(e.metrics["span_resolve"].num_observations(), 1);
        assert_eq!(e.metrics["total"].num_observations(), 1);
    }

    #[tokio::test]
    async fn fold_file_metric_marks_failure() {
        let TestEntrySink { inspector, sink } = test_entry_sink();
        let _guard = ServiceMetrics::set_test_sink_on_current_tokio_runtime(sink);

        let mut b = FoldFileMetricsBuilder::new();
        b.failed(true).total(Duration::from_millis(50));
        b.emit();

        let e = inspector.get(0);
        assert_eq!(e.metrics["failed"].as_u64(), 1);
    }
}

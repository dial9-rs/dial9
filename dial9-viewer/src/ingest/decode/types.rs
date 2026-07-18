//! Stable decoded output types exposed by [`super::decode_samples`].
//!
//! Clock-domain newtypes stay private to decoding. These primitive timestamp
//! fields are the output/Parquet seam and preserve the existing public surface.

use std::collections::HashMap;

// ─── Public types ────────────────────────────────────────────────────────────

/// A resolved CPU sample ready for Parquet output.
#[derive(Debug, Clone)]
pub struct ResolvedSample {
    pub timestamp_ns: u64,
    pub stack_id: [u8; 16],
    /// Runtime worker this sample is attributed to, or `None` when it cannot be
    /// attributed to a worker (a non-runtime thread, or the producer's
    /// `WorkerId::UNKNOWN`/`BLOCKING` sentinels — see [`decode_samples`]). The
    /// on/off-runtime split downstream is exactly `Some` vs `None`; there is no
    /// in-band sentinel value.
    pub worker_id: Option<u32>,
    pub source: u8,
    pub source_key: String,
    /// Extracted from source_key path
    pub host: String,
    pub service: String,
    pub date: String,
    /// Duration of the enclosing poll span (ns), or `None` if the sample didn't
    /// land inside a poll (off-worker, between polls, etc.).
    pub poll_duration_ns: Option<u64>,
    /// The spawn location of the task that was being polled when this sample
    /// fired, or `None` if the sample didn't land inside a poll or the task
    /// has no recorded spawn location.
    pub spawn_location: Option<String>,
    /// Tracing spans whose locally observed entered intervals enclose this
    /// sample's timestamp. Typically zero, one, or two entries. Populated
    /// during span resolution (stage 3). Never lifecycle envelopes — an async
    /// span that is exited (waiting) does NOT claim samples during its idle gap.
    pub enclosing_spans: Vec<EnclosingSpanSummary>,
}

/// A reconstructed poll span: one invocation of `Future::poll` on a task.
///
/// Public because it is the third element of [`DecodeResult`], the return type
/// of the public [`decode_samples`].
#[derive(Debug, Clone)]
pub struct ResolvedPoll {
    pub start_ns: u64,
    pub end_ns: u64,
    pub duration_ns: u64,
    pub worker_id: u32,
    pub task_id: u64,
    pub spawn_loc: Option<String>,
    /// CPU profile samples that landed inside this poll.
    pub cpu_sample_count: u32,
    /// Off-CPU / scheduler samples that landed inside this poll.
    pub sched_sample_count: u32,
    pub host: String,
    pub service: String,
    pub date: String,
}

/// A decoded tracing span close summary from the source file, ready for the
/// `spans/` Parquet table. One row per `SpanCloseEvent` observed.
#[derive(Debug, Clone)]
pub struct ResolvedSpan {
    /// Stable 16-byte identity: BLAKE3(boot_id || span_instance_id)[..16].
    pub span_uid: [u8; 16],
    /// Grouping identity: BLAKE3(kind || target || name || file || line)[..16].
    pub span_type_uid: [u8; 16],
    pub kind: &'static str,
    pub name: String,
    pub target: String,
    pub callsite_file: Option<String>,
    pub callsite_line: Option<u32>,
    /// Lifecycle start timestamp (wall-clock epoch ns).
    pub start_ns: u64,
    /// Lifecycle close timestamp (wall-clock epoch ns).
    pub end_ns: u64,
    /// `end_ns - start_ns`.
    pub elapsed_ns: u64,
    /// Producer-accumulated active thread time (from enter/exit intervals).
    pub active_ns: Option<u64>,
    /// Union of locally-observed entered intervals (within this source file).
    pub observed_active_wall_ns: u64,
    /// Locally classifiable elapsed time.
    pub detail_coverage_ns: u64,
    /// Whether all detail fits within this source file.
    pub details_complete: bool,
    /// Concurrent/re-entrant execution observed.
    pub concurrent: bool,
    /// Explicit stable parent span uid (if available).
    pub parent_span_uid: Option<[u8; 16]>,
    /// Final close-time attributes.
    pub attributes: Vec<(String, String)>,
    // ── Five-way time attribution (all nullable until effective metadata
    //    and wake classification are consumed) ─────────────────────────────
    /// Estimated on-CPU time. Null until profiler metadata is available.
    pub on_cpu_ns_est: Option<u64>,
    /// Estimated synchronous blocking time. Null until profiler metadata.
    pub blocked_ns_est: Option<u64>,
    /// Async wait (not-ready) time. Null until wake classification.
    pub async_wait_ns: Option<u64>,
    /// Scheduling delay (ready-to-poll). Null until wake classification.
    pub scheduler_delay_ns: Option<u64>,
    /// Unclassified elapsed time. Invariant:
    /// elapsed_ns = on_cpu_ns_est + blocked_ns_est + async_wait_ns
    ///            + scheduler_delay_ns + unknown_ns
    /// (nullable estimates contribute zero when null).
    pub unknown_ns: u64,
    /// CPU samples enclosed by this span.
    pub cpu_sample_count: u32,
    /// Scheduler samples enclosed by this span.
    pub sched_sample_count: u32,
    /// Attribution algorithm version (monotonically increasing).
    pub attribution_version: u16,
    /// Bitfield: missing/lost/ambiguous inputs for attribution.
    /// Bit 0: profiler metadata missing
    /// Bit 1: sample drops detected or attribution accounting invalid
    /// Bit 2: worker/tid attribution ambiguous
    /// Bit 3: wake classification unavailable
    pub attribution_flags: u32,
    /// Whether `active_ns` saturated (hit u64::MAX). When true, the reported
    /// `active_ns` is a lower bound.
    pub saturated: bool,
    /// Whether loss of enter/exit events is observable for this span.
    /// When false, the backend cannot distinguish "zero loss" from "unknown
    /// loss" and must treat gaps as Unknown.
    pub loss_observable: bool,
    /// Number of unmatched exit events for this span instance (exits without
    /// a corresponding enter on the same tid). Non-zero degrades completeness.
    pub unbalanced_exits: u32,
    /// Number of unmatched enter events for this span instance (enters that
    /// were never popped by a corresponding exit, including producer-reported
    /// unbalanced enters). Non-zero degrades completeness.
    pub unbalanced_enters: u32,
    /// Quality of the span identity anchor.
    /// - `"metadata"`: boot_id from SegmentMetadata (authoritative, stable
    ///   across files from the same process).
    /// - `"path"`: boot_id extracted from a namespaced source key path matching
    ///   the `{4-alpha}-{pid}` format. Stable across segments from the same
    ///   process (same boot_id directory). Authoritative.
    /// - `"flat"`: genuinely flat or legacy path with no identifiable boot_id
    ///   directory. Cannot claim cross-file stability. Low quality.
    pub identity_quality: &'static str,
    /// Source key origin.
    pub source_key: String,
    pub host: String,
    pub service: String,
    pub date: String,
}

/// A compact span membership attached to each enriched sample row (the
/// `enclosing_spans` list). One entry per span whose *locally observed entered
/// interval* encloses the sample's timestamp — never lifecycle envelopes.
///
/// OTAP-aligned: carries only the identity, duration, and completeness needed
/// for the hot flamegraph filter path. Full metadata lives in `spans/` only.
#[derive(Debug, Clone, PartialEq)]
pub struct EnclosingSpanSummary {
    pub span_uid: [u8; 16],
    pub span_type_uid: [u8; 16],
    pub elapsed_ns: u64,
    pub details_complete: bool,
}

/// Return type for [`super::decode_samples`]: resolved samples, stacks
/// dictionary, poll spans, and tracing span close summaries.
pub type DecodeResult = (
    Vec<ResolvedSample>,
    HashMap<[u8; 16], Vec<String>>,
    Vec<ResolvedPoll>,
    Vec<ResolvedSpan>,
);

/// Per-phase timing and counts for one `decode_samples` call, returned by
/// [`super::decode_samples_with_stats`]. Purely observational — used by the fold
/// pipeline to emit a per-file metric so we can see where decode time goes
/// (wire decode vs. poll reconstruction vs. span resolution vs. attribution)
/// without a profiler. Durations are wall-clock for each phase, measured in
/// sequence, so they sum (modulo rounding) to the total decode time.
#[derive(Debug, Clone, Default)]
pub struct DecodeStats {
    /// Total events decoded off the wire (samples + park/unpark + poll
    /// start/end + span enter/exit/close). The headline "how big is this file"
    /// number.
    pub events_decoded: u64,
    /// Legacy span enter + exit + close events (subset of `events_decoded`).
    pub span_events_decoded: u64,
    /// Phase: one-pass wire decode (`decode_trace`).
    pub wire_decode: std::time::Duration,
    /// Phase: sort the event vector by timestamp.
    pub sort_events: std::time::Duration,
    /// Phase: reconstruct the poll timeline from park/unpark/poll events.
    pub poll_reconstruct: std::time::Duration,
    /// Phase: the sample loop (symbolication + per-sample poll attribution).
    pub sample_resolve: std::time::Duration,
    /// Phase: legacy span reconstruction (`resolve_legacy_spans`).
    pub span_resolve: std::time::Duration,
    /// Phase: sweep-line sample→span attribution.
    pub sample_attribution: std::time::Duration,
}

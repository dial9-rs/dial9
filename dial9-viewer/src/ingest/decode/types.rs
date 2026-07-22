//! Decoded output types at the internal Parquet boundary.
//!
//! Clock-domain newtypes stay private to decoding. These primitive timestamp
//! fields are the output/Parquet seam.

use std::collections::HashMap;

/// A resolved CPU sample ready for Parquet output.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedSample {
    pub(crate) timestamp_ns: u64,
    pub(crate) stack_id: [u8; 16],
    /// Runtime worker this sample is attributed to, or `None` when it cannot be
    /// attributed to a worker (a non-runtime thread, or the producer's
    /// `WorkerId::UNKNOWN`/`BLOCKING` sentinels — see [`decode_samples`]). The
    /// on/off-runtime split downstream is exactly `Some` vs `None`; there is no
    /// in-band sentinel value.
    pub(crate) worker_id: Option<u32>,
    pub(crate) source: u8,
    pub(crate) source_key: String,
    /// Extracted from source_key path
    pub(crate) host: String,
    pub(crate) service: String,
    pub(crate) date: String,
    /// Duration of the enclosing poll span (ns), or `None` if the sample didn't
    /// land inside a poll (off-worker, between polls, etc.).
    pub(crate) poll_duration_ns: Option<u64>,
    /// The spawn location of the task that was being polled when this sample
    /// fired, or `None` if the sample didn't land inside a poll or the task
    /// has no recorded spawn location.
    pub(crate) spawn_location: Option<String>,
    /// Tracing spans whose locally observed entered intervals enclose this
    /// sample's timestamp. Typically zero, one, or two entries. Populated
    /// during span resolution (stage 3). Never lifecycle envelopes — an async
    /// span that is exited (waiting) does NOT claim samples during its idle gap.
    pub(crate) enclosing_spans: Vec<EnclosingSpanSummary>,
}

/// A reconstructed poll span: one invocation of `Future::poll` on a task.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedPoll {
    pub(crate) start_ns: u64,
    pub(crate) end_ns: u64,
    pub(crate) duration_ns: u64,
    pub(crate) worker_id: u32,
    pub(crate) task_id: u64,
    pub(crate) spawn_loc: Option<String>,
    /// CPU profile samples that landed inside this poll.
    pub(crate) cpu_sample_count: u32,
    /// Off-CPU / scheduler samples that landed inside this poll.
    pub(crate) sched_sample_count: u32,
    pub(crate) host: String,
    pub(crate) service: String,
    pub(crate) date: String,
}

/// A decoded tracing span close summary from the source file, ready for the
/// `spans/` Parquet table. One row per `SpanCloseEvent` observed.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedSpan {
    /// Stable 16-byte identity: BLAKE3(boot_id || span_instance_id)[..16].
    pub(crate) span_uid: [u8; 16],
    /// Grouping identity: BLAKE3(kind || target || name || file || line)[..16].
    pub(crate) span_type_uid: [u8; 16],
    pub(crate) kind: &'static str,
    pub(crate) name: String,
    pub(crate) target: String,
    pub(crate) callsite_file: Option<String>,
    pub(crate) callsite_line: Option<u32>,
    /// Lifecycle start timestamp (wall-clock epoch ns).
    pub(crate) start_ns: u64,
    /// Lifecycle close timestamp (wall-clock epoch ns).
    pub(crate) end_ns: u64,
    /// `end_ns - start_ns`.
    pub(crate) elapsed_ns: u64,
    /// Producer-accumulated active thread time (from enter/exit intervals).
    pub(crate) active_ns: Option<u64>,
    /// Union of locally-observed entered intervals (within this source file).
    pub(crate) observed_active_wall_ns: u64,
    /// Locally classifiable elapsed time.
    pub(crate) detail_coverage_ns: u64,
    /// Whether all detail fits within this source file.
    pub(crate) details_complete: bool,
    /// Concurrent/re-entrant execution observed.
    pub(crate) concurrent: bool,
    /// Explicit stable parent span uid (if available).
    pub(crate) parent_span_uid: Option<[u8; 16]>,
    /// Final close-time attributes.
    pub(crate) attributes: Vec<(String, String)>,
    // ── Five-way time attribution (all nullable until effective metadata
    //    and wake classification are consumed) ─────────────────────────────
    /// Estimated on-CPU time. Null until profiler metadata is available.
    pub(crate) on_cpu_ns_est: Option<u64>,
    /// Estimated synchronous blocking time. Null until profiler metadata.
    pub(crate) blocked_ns_est: Option<u64>,
    /// Async wait (not-ready) time. Null until wake classification.
    pub(crate) async_wait_ns: Option<u64>,
    /// Scheduling delay (ready-to-poll). Null until wake classification.
    pub(crate) scheduler_delay_ns: Option<u64>,
    /// Unclassified elapsed time. Invariant:
    /// elapsed_ns = on_cpu_ns_est + blocked_ns_est + async_wait_ns
    ///            + scheduler_delay_ns + unknown_ns
    /// (nullable estimates contribute zero when null).
    pub(crate) unknown_ns: u64,
    /// CPU samples enclosed by this span.
    pub(crate) cpu_sample_count: u32,
    /// Scheduler samples enclosed by this span.
    pub(crate) sched_sample_count: u32,
    /// Attribution algorithm version (monotonically increasing).
    pub(crate) attribution_version: u16,
    /// Bitfield: missing/lost/ambiguous inputs for attribution.
    /// Bit 0: profiler metadata missing
    /// Bit 1: sample drops detected or attribution accounting invalid
    /// Bit 2: worker/tid attribution ambiguous
    /// Bit 3: wake classification unavailable
    pub(crate) attribution_flags: u32,
    /// Number of unmatched exit events for this span instance (exits without
    /// a corresponding enter on the same tid). Non-zero degrades completeness.
    pub(crate) unbalanced_exits: u32,
    /// Number of unmatched enter events for this span instance (enters that
    /// were never popped by a corresponding exit, including producer-reported
    /// unbalanced enters). Non-zero degrades completeness.
    pub(crate) unbalanced_enters: u32,
    /// Quality of the span identity anchor.
    /// - `"metadata"`: boot_id from SegmentMetadata (authoritative, stable
    ///   across files from the same process).
    /// - `"path"`: boot_id extracted from a namespaced source key path matching
    ///   the `{4-alpha}-{pid}` format. Stable across segments from the same
    ///   process (same boot_id directory). Authoritative.
    /// - `"flat"`: genuinely flat or legacy path with no identifiable boot_id
    ///   directory. Cannot claim cross-file stability. Low quality.
    pub(crate) identity_quality: &'static str,
    /// Source key origin.
    pub(crate) source_key: String,
    pub(crate) host: String,
    pub(crate) service: String,
    pub(crate) date: String,
}

/// A compact span membership attached to each enriched sample row (the
/// `enclosing_spans` list). One entry per span whose *locally observed entered
/// interval* encloses the sample's timestamp — never lifecycle envelopes.
///
/// OTAP-aligned: carries only the identity, duration, and completeness needed
/// for the hot flamegraph filter path. Full metadata lives in `spans/` only.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EnclosingSpanSummary {
    pub(crate) span_uid: [u8; 16],
    pub(crate) span_type_uid: [u8; 16],
    pub(crate) elapsed_ns: u64,
    pub(crate) details_complete: bool,
}

/// Return type for [`super::decode_samples`]: resolved samples, stacks
/// dictionary, poll spans, and tracing span close summaries.
pub(crate) type DecodeResult = (
    Vec<ResolvedSample>,
    HashMap<[u8; 16], Vec<String>>,
    Vec<ResolvedPoll>,
    Vec<ResolvedSpan>,
);

//! Operational metrics published via metrique.

use crate::background_task::pipeline_metrics::{MetriqueResult, PipelineMetrics};
use crate::telemetry::recorder::FlushStats;
use metrique::timers::Timer;
use metrique::unit::{Byte, Microsecond, Millisecond};
use metrique::unit_of_work::metrics;

/// Distinguishes the type of operation a metric entry describes.
#[derive(Clone, Copy, Debug)]
#[metrics(value(string))]
pub(crate) enum Operation {
    Flush,
    ProcessSegment,
    TlDrain,
    WorkerCycle,
}

/// Metrics emitted by the flush thread each cycle.
#[metrics(rename_all = "PascalCase")]
#[derive(Debug)]
pub(crate) struct FlushMetrics {
    pub operation: Operation,
    #[metrics(flatten)]
    pub stats: FlushStats,
    /// Wall-clock time spent draining and writing.
    #[metrics(unit = Microsecond)]
    pub flush_duration: Timer,

    /// The last flush during shutdown
    pub last_flush: bool,

    /// True when writing segment metadata failed during the final flush.
    pub write_metadata_failed: bool,

    /// True when finalizing (sealing) the segment failed during the final flush.
    pub finalize_failed: bool,
}

/// Metrics emitted once per worker cycle
#[metrics(rename_all = "PascalCase")]
#[derive(Debug)]
pub(crate) struct WorkerCycleMetrics {
    pub operation: Operation,
    /// Segments waiting in the memory ring after this cycle's pop.
    /// `None` on disk.
    pub memory_queued_segments: Option<u64>,
    /// Encoded bytes resident in the memory ring after this cycle's pop.
    /// `None` on disk.
    #[metrics(unit = metrique::unit::Byte)]
    pub memory_queued_bytes: Option<u64>,
    /// Segments claimed by the worker and not yet released.
    pub in_flight_segments: u64,
    /// Current bytes held by in-flight `SegmentData` at sample time.
    /// Reflects processor mutations via `SegmentAccounting::adjust`.
    #[metrics(unit = metrique::unit::Byte)]
    pub in_flight_bytes: u64,
    /// High-water of `in_flight_bytes` observed across the event window.
    /// `None` on disk (no per-stage mutation tracking).
    #[metrics(unit = metrique::unit::Byte)]
    pub memory_peak_in_flight_bytes: Option<u64>,
    /// Segments evicted during this event's window (disk: `evict_oldest`,
    /// memory: ring overflow).
    pub segments_evicted: u64,
    /// Segments handed into the pipeline during this cycle.
    pub segments_dispatched: u64,
}

pub(crate) use dial9_core::metrics::TlDrainStats;

/// Metrics emitted every time the flush thread runs the intrusive
/// thread-local buffer drain (~every 30s, plus on shutdown).
///
/// `events_flushed > 0` means idle/silent threads were holding events
/// that would otherwise have crossed a trace file rotation.
/// `buffers_locked` vs `buffers_flushed` shows how many locks were
/// taken for buffers that turned out to be empty (e.g., a thread that
/// self-flushed after the epoch bump but before we upgraded the
/// `Weak`).
#[metrics(rename_all = "PascalCase")]
#[derive(Debug)]
pub(crate) struct TlDrainMetrics {
    pub operation: Operation,
    /// Wall-clock time spent in `drain_all_tl_buffers`.
    #[metrics(unit = Microsecond)]
    pub duration: Timer,
    #[metrics(flatten)]
    pub stats: TlDrainStats,
    /// True when this drain ran as part of shutdown finalization.
    pub last_drain: bool,
}

/// Metrics emitted per sealed segment processed by the background worker.
#[metrics(rename_all = "PascalCase")]
#[derive(Debug)]
pub(crate) struct SegmentProcessMetrics {
    pub operation: Operation,
    #[metrics(unit = Millisecond)]
    pub total_time: Timer,
    #[metrics(flatten)]
    pub status: Option<MetriqueResult>,
    pub segment_index: u32,
    #[metrics(unit = Byte)]
    pub uncompressed_size: u64,
    #[metrics(unit = Byte)]
    pub compressed_size: Option<u64>,
    /// True when the segment file lacks a valid SegmentMetadata header.
    pub invalid_file_header: bool,
    /// True when a processor panicked while processing this segment.
    pub panicked: bool,
    /// The panic message, if a processor panicked.
    pub panic_message: Option<String>,
    /// Per-processor metrics, keyed by processor name.
    #[metrics(flatten)]
    pub pipeline: PipelineMetrics,
}

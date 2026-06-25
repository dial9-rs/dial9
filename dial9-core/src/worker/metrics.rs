//! Operational metrics published via metrique.

use crate::worker::pipeline_metrics::{MetriqueResult, PipelineMetrics};
use metrique::timers::Timer;
use metrique::unit::{Byte, Millisecond};
use metrique::unit_of_work::metrics;

/// Distinguishes the type of background-worker operation a metric entry describes.
#[derive(Clone, Copy, Debug)]
#[metrics(value(string))]
pub(crate) enum Operation {
    ProcessSegment,
    WorkerCycle,
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

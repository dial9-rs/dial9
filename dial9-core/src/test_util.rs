//! Test-only helpers for sibling-crate tests.
//!
//! Available under the `test-util` feature.

use crate::buffer::{BufferMode, SegmentWriter};
use crate::shared_state::SharedState;

/// Flush the calling thread's buffered events into `shared`'s collector,
/// leaving them queued.
pub fn drain_thread_local(shared: &SharedState) {
    crate::encoder::drain_to_collector(&shared.collector);
}

/// Drain everything recorded in `shared` into raw encoded-segment bytes, one
/// entry per batch.
pub fn drain_encoded_batches(shared: &SharedState) -> Vec<Vec<u8>> {
    crate::encoder::drain_to_collector(&shared.collector);
    let mut out = Vec::new();
    while let Some(batch) = shared.collector.next() {
        out.push(batch.into_encoded_bytes());
    }
    out
}

/// Drain everything recorded in `shared` through `writer`, transcoding each
/// batch into the writer's active segment. Mirrors the flush loop's
/// collector -> writer step
pub fn drain_into<M: BufferMode>(
    shared: &SharedState,
    writer: &mut SegmentWriter<M>,
) -> std::io::Result<()> {
    crate::encoder::drain_to_collector(&shared.collector);
    while let Some(batch) = shared.collector.next() {
        writer.write_encoded_batch(&batch)?;
    }
    Ok(())
}

/// Encode a single event and write it into `writer`'s active segment as its own
/// batch. For tests that build a trace file from individual events.
pub fn write_event<M: BufferMode>(
    writer: &mut SegmentWriter<M>,
    event: &dyn crate::encoder::Encodable,
) -> std::io::Result<()> {
    let bytes = crate::encoder::encode_single(event);
    writer.write_encoded_batch(&crate::collector::Batch::new(bytes, 1))
}

#[cfg(feature = "pipeline")]
pub use pipeline_helpers::*;

/// Worker-pipeline drivers for sibling-crate processor tests. They build the
/// `Fs` + `WorkerLoop` + dump channel internally and expose only opaque handles
/// plus already-public types, so callers (e.g. `dial9-utils`) can exercise a real
/// `SegmentProcessor` end-to-end without touching the worker internals.
#[cfg(feature = "pipeline")]
mod pipeline_helpers {
    use crate::dump::{DumpId, DumpTrigger};
    use crate::fs::Fs;
    use crate::pipeline::SegmentProcessor;
    use crate::worker::WorkerLoop;
    use std::io::Write as _;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    fn dev_null_sink() -> metrique::writer::BoxEntrySink {
        metrique::writer::sink::DevNullSink::boxed()
    }

    /// Valid trace bytes whose first clock anchor reports `epoch_secs`.
    fn segment_with_epoch(epoch_secs: u64) -> Vec<u8> {
        use dial9_trace_format::encoder::Encoder;
        let mut enc = Encoder::new_to(Vec::new()).unwrap();
        enc.write_infallible(&crate::format::ClockSyncEvent {
            timestamp_ns: 1,
            realtime_ns: epoch_secs * 1_000_000_000,
        });
        enc.into_inner()
    }

    /// A fresh `DumpId`, for tests that hand-build a `DumpCompletion`.
    pub fn new_dump_id() -> DumpId {
        DumpId::new()
    }

    /// A disk-backed `SegmentRef` for sibling-crate processor tests that need
    /// a segment handle without a real seal. `SealedSegment`'s fields are
    /// `pub(crate)`, so this is the cross-crate constructor.
    pub fn disk_segment(
        path: impl Into<std::path::PathBuf>,
        index: u32,
    ) -> crate::sealed::SegmentRef {
        crate::sealed::SegmentRef::Disk(crate::sealed::SealedSegment::new_for_test(
            path.into(),
            index,
        ))
    }

    /// Build a `DumpCompletion` for sibling-crate tests that exercise
    /// `finalize_dump` directly. The struct is `#[non_exhaustive]`, so it
    /// can't be built with a struct literal cross-crate; this is the
    /// constructor those tests use.
    pub fn new_dump_completion(
        dump_id: DumpId,
        triggered_at: std::time::SystemTime,
        time_range: (std::time::SystemTime, std::time::SystemTime),
        segments_processed: usize,
        metadata: Vec<(String, String)>,
        failed: bool,
    ) -> crate::dump::DumpCompletion {
        crate::dump::DumpCompletion {
            dump_id,
            triggered_at,
            time_range,
            segments_processed,
            metadata,
            failed,
        }
    }

    /// Run `processors` over an in-memory pipeline seeded with `segments` (one
    /// segment per blob), in continuous mode until the writer is marked done
    /// and the ring drains. Wrap the call in `tokio::time::timeout` to guard
    /// against a hang.
    pub async fn run_pipeline_continuous(
        segments: Vec<Vec<u8>>,
        processors: Vec<Box<dyn SegmentProcessor>>,
        poll_interval: Duration,
    ) -> std::io::Result<()> {
        let fs = Fs::new_in_memory(64 * 1024 * 1024, 1024)?;
        for (index, bytes) in segments.iter().enumerate() {
            let mut handle = fs.create_segment(Path::new("x"))?;
            handle.write_all(bytes)?;
            fs.seal(handle, Path::new("x"), index as u32)?;
        }
        fs.mark_writer_done();
        let stop = CancellationToken::new();
        let mut worker =
            WorkerLoop::new(fs, poll_interval, processors, stop, dev_null_sink(), None);
        worker.run().await;
        Ok(())
    }

    /// An in-memory triggered pipeline: seal segments with [`seal`](Self::seal),
    /// fire a dump via [`trigger`](Self::trigger), then
    /// [`shutdown`](Self::shutdown).
    pub struct TriggeredPipeline {
        /// On-demand dump trigger for this pipeline.
        pub trigger: DumpTrigger,
        fs: Arc<Fs>,
        stop: CancellationToken,
        join: JoinHandle<()>,
    }

    impl TriggeredPipeline {
        /// Seal one in-memory segment at `index` carrying a clock anchor at
        /// `epoch_secs`.
        pub fn seal(&self, index: u32, epoch_secs: u64) {
            let mut handle = self.fs.create_segment(Path::new("x")).unwrap();
            handle.write_all(&segment_with_epoch(epoch_secs)).unwrap();
            self.fs.seal(handle, Path::new("x"), index).unwrap();
        }

        /// Stop the worker and join its task.
        pub async fn shutdown(self) {
            self.stop.cancel();
            let _ = self.join.await;
        }
    }

    /// Spawn an in-memory triggered worker running `processors`.
    pub fn spawn_triggered_pipeline(
        processors: Vec<Box<dyn SegmentProcessor>>,
    ) -> TriggeredPipeline {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = crate::dump::channel();
        let stop = CancellationToken::new();
        let mut worker = WorkerLoop::new(
            Arc::clone(&fs),
            Duration::from_millis(10),
            processors,
            stop.clone(),
            dev_null_sink(),
            Some(rx),
        );
        let join = tokio::spawn(async move { worker.run().await });
        TriggeredPipeline {
            trigger,
            fs,
            stop,
            join,
        }
    }
}

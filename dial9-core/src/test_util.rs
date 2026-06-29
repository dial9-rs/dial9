//! Test-only helpers for sibling-crate tests.
//!
//! Available under the `test-util` feature.

use crate::shared_state::SharedState;
use crate::writer::{SegmentWriter, WriterMode};

/// Flush the calling thread's buffered events into `shared`'s collector,
/// leaving them queued.
pub fn drain_thread_local(shared: &SharedState) {
    crate::buffer::drain_to_collector(&shared.collector);
}

/// Drain everything recorded in `shared` into raw encoded-segment bytes, one
/// entry per batch.
pub fn drain_encoded_batches(shared: &SharedState) -> Vec<Vec<u8>> {
    crate::buffer::drain_to_collector(&shared.collector);
    let mut out = Vec::new();
    while let Some(batch) = shared.collector.next() {
        out.push(batch.into_encoded_bytes());
    }
    out
}

/// Drain everything recorded in `shared` through `writer`, transcoding each
/// batch into the writer's active segment. Mirrors the flush loop's
/// collector -> writer step
pub fn drain_into<M: WriterMode>(
    shared: &SharedState,
    writer: &mut SegmentWriter<M>,
) -> std::io::Result<()> {
    crate::buffer::drain_to_collector(&shared.collector);
    while let Some(batch) = shared.collector.next() {
        writer.write_encoded_batch(&batch)?;
    }
    Ok(())
}

/// Encode a single event and write it into `writer`'s active segment as its own
/// batch. For tests that build a trace file from individual events.
pub fn write_event<M: WriterMode>(
    writer: &mut SegmentWriter<M>,
    event: &dyn crate::buffer::Encodable,
) -> std::io::Result<()> {
    let bytes = crate::buffer::encode_single(event);
    writer.write_encoded_batch(&crate::collector::Batch::new(bytes, 1))
}

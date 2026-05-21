//! In-memory `Fs` variant.
//!
//! `MemFs` keeps sealed segments in a slot-bounded ring (`BoundedQueue`).
//! The writer pushes via `seal`, the worker pops one segment per
//! `take_files` cycle. On overflow, `force_push` drops the oldest. The
//! shutdown handoff rides `writer_done` (Acquire/Release) plus a `Notify`
//! for wakeups (lost wakeup avoided via enable-before-recheck).

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::background_task::sealed::{MemorySegment, SegmentRef};
use crate::primitives::BoundedQueue;
use crate::primitives::sync::Arc;
use crate::primitives::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::rate_limit::rate_limited;

use super::{ActiveHandle, RemoveReason, SegmentAccounting, TakenFiles, TakenSegment};

/// Active in-memory write accumulator.
pub(crate) struct MemActiveWriter {
    pub(super) buf: Vec<u8>,
}

impl Write for MemActiveWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct MemSealedSegment {
    index: u32,
    size: u64,
    bytes: Bytes,
}

struct MemChannel {
    queue: BoundedQueue<MemSealedSegment>,
    queued_bytes: AtomicU64,
    in_flight_bytes: Arc<AtomicU64>,
    in_flight_segments: Arc<AtomicU64>,
    in_flight_bytes_peak: Arc<AtomicU64>,
    dropped: AtomicU64,
    writer_done: AtomicBool,
    notify: Notify,
}

/// In-memory segment channel.
pub(crate) struct MemFs {
    channel: Arc<MemChannel>,
}

impl MemFs {
    /// Build a memory channel with a ring capacity of `max_segments` slots.
    /// Returns `Err(InvalidInput)` when `max_segments` is zero.
    pub(crate) fn with_capacity(max_segments: usize) -> io::Result<Self> {
        if max_segments == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_segments must be > 0",
            ));
        }
        Ok(Self {
            channel: Arc::new(MemChannel {
                queue: BoundedQueue::new(max_segments),
                queued_bytes: AtomicU64::new(0),
                in_flight_bytes: Arc::new(AtomicU64::new(0)),
                in_flight_segments: Arc::new(AtomicU64::new(0)),
                in_flight_bytes_peak: Arc::new(AtomicU64::new(0)),
                dropped: AtomicU64::new(0),
                writer_done: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        })
    }

    pub(super) fn create_handle(&self, _path: &Path) -> io::Result<ActiveHandle> {
        Ok(ActiveHandle::Mem(MemActiveWriter { buf: Vec::new() }))
    }

    pub(super) fn seal_handle(
        &self,
        active_handle: ActiveHandle,
        _active_path: &Path,
        index: u32,
    ) -> io::Result<SegmentRef> {
        let ActiveHandle::Mem(writer) = active_handle else {
            return Err(io::Error::other(
                "MemFs::seal: disk handle passed to mem backend",
            ));
        };
        let bytes = Bytes::from(writer.buf); // zero-copy Vec → Bytes
        let size = bytes.len() as u64;
        let ch = &self.channel;

        // Reserve the queued-bytes slot before the push so observers see
        // the bytes accounted for the moment the slot becomes visible.
        ch.queued_bytes.fetch_add(size, Ordering::AcqRel);
        if let Some(evicted) = ch.queue.force_push(MemSealedSegment { index, size, bytes }) {
            ch.queued_bytes.fetch_sub(evicted.size, Ordering::AcqRel);
            ch.dropped.fetch_add(1, Ordering::Relaxed);
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!(
                    target: "dial9_worker",
                    "memory segment evicted (ring full): segment {} dropped",
                    evicted.index
                );
            });
        }

        ch.notify.notify_one();
        Ok(SegmentRef::Memory(MemorySegment { index, size }))
    }

    pub(super) fn remove_sealed_inner(&self, _seg: &SegmentRef, _reason: RemoveReason) {}

    pub(super) fn remove_active_inner(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn take_files_inner(&self) -> TakenFiles {
        let ch = &self.channel;
        // Per-cycle delta: swap-to-zero so each emit reports drops that
        // occurred since the previous take_files.
        let segments_dropped = ch.dropped.swap(0, Ordering::AcqRel);

        let in_flight_now = ch.in_flight_bytes.load(Ordering::Acquire);
        let peak = ch
            .in_flight_bytes_peak
            .swap(in_flight_now, Ordering::AcqRel);

        let Some(slot) = ch.queue.pop() else {
            return TakenFiles {
                segments: vec![],
                queued_segments: Some(ch.queue.len() as u64),
                queued_bytes: Some(ch.queued_bytes.load(Ordering::Acquire)),
                in_flight_segments: ch.in_flight_segments.load(Ordering::Relaxed),
                in_flight_bytes: in_flight_now,
                in_flight_bytes_peak: Some(peak),
                segments_dropped,
            };
        };

        let size = slot.size;
        ch.queued_bytes.fetch_sub(size, Ordering::AcqRel);
        let in_flight_total = ch.in_flight_bytes.fetch_add(size, Ordering::AcqRel) + size;
        ch.in_flight_segments.fetch_add(1, Ordering::AcqRel);
        // The just-popped segment seeds the next cycle's peak.
        ch.in_flight_bytes_peak
            .fetch_max(in_flight_total, Ordering::AcqRel);

        let accounting = SegmentAccounting {
            in_flight_bytes: Arc::clone(&ch.in_flight_bytes),
            in_flight_segments: Arc::clone(&ch.in_flight_segments),
            in_flight_bytes_peak: Arc::clone(&ch.in_flight_bytes_peak),
            size,
        };
        let taken = TakenSegment::memory(
            MemorySegment {
                index: slot.index,
                size,
            },
            slot.bytes,
            accounting,
        );

        TakenFiles {
            segments: vec![taken],
            queued_segments: Some(ch.queue.len() as u64),
            queued_bytes: Some(ch.queued_bytes.load(Ordering::Acquire)),
            in_flight_segments: ch.in_flight_segments.load(Ordering::Relaxed),
            in_flight_bytes: ch.in_flight_bytes.load(Ordering::Relaxed),
            in_flight_bytes_peak: Some(peak),
            segments_dropped,
        }
    }

    pub(super) async fn wait_for_more_inner(
        &self,
        stop: &CancellationToken,
        _poll_interval: Duration,
    ) {
        let ch = &self.channel;
        // Register the notified future *before* loading writer_done so any
        // notify_one between the run loop's earlier check and this await
        // becomes a stored permit consumed here.
        let notified = ch.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if ch.writer_done.load(Ordering::Acquire) {
            return;
        }
        tokio::select! {
            _ = stop.cancelled() => {}
            _ = &mut notified => {}
        }
    }

    pub(super) fn writer_done_inner(&self) -> bool {
        self.channel.writer_done.load(Ordering::Acquire)
    }

    pub(super) fn mark_writer_done_inner(&self) {
        self.channel.writer_done.store(true, Ordering::Release);
        self.channel.notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn mem_fs_seal_take_roundtrip() {
        let mem = MemFs::with_capacity(16).unwrap();
        let handle = mem
            .create_handle(Path::new("mem://trace.0.bin.active"))
            .unwrap();
        let ActiveHandle::Mem(mut w) = handle else {
            panic!()
        };
        w.buf.extend_from_slice(b"hello bytes");
        let handle = ActiveHandle::Mem(w);

        let seg_ref = mem
            .seal_handle(handle, Path::new("mem://trace.0.bin.active"), 0)
            .unwrap();
        check!(matches!(seg_ref, SegmentRef::Memory(_)));
        check!(seg_ref.index() == 0);

        let taken = mem.take_files_inner();
        check!(taken.segments.len() == 1);

        let (loaded_ref, payload, _acct) =
            taken.segments.into_iter().next().unwrap().load().unwrap();
        check!(loaded_ref.index() == 0);
        check!(payload.into_bytes().as_ref() == b"hello bytes");
    }

    #[test]
    fn mem_fs_slot_cap_eviction() {
        // slot_cap = 1. Pushing the second segment evicts the first.
        let mem = MemFs::with_capacity(1).unwrap();

        for index in 0..2u32 {
            let handle = mem.create_handle(Path::new("dummy")).unwrap();
            let ActiveHandle::Mem(mut w) = handle else {
                panic!()
            };
            w.buf.resize(60, index as u8);
            mem.seal_handle(ActiveHandle::Mem(w), Path::new("dummy"), index)
                .unwrap();
        }

        check!(mem.channel.dropped.load(Ordering::SeqCst) == 1);
        // Only the most recent segment remains.
        let t = mem.take_files_inner();
        check!(t.segments.len() == 1);
        check!(t.segments[0].seg_ref.index() == 1);
    }

    #[test]
    fn mem_fs_rejects_invalid_sizes() {
        let Err(e) = MemFs::with_capacity(0) else {
            panic!("expected error for max_segments == 0");
        };
        check!(e.kind() == io::ErrorKind::InvalidInput);
    }

    #[test]
    fn mem_fs_queued_segments_after_pop() {
        let mem = MemFs::with_capacity(16).unwrap();
        for i in 0..3u32 {
            let handle = mem.create_handle(Path::new("x")).unwrap();
            let ActiveHandle::Mem(mut w) = handle else {
                panic!()
            };
            w.buf.push(i as u8);
            mem.seal_handle(ActiveHandle::Mem(w), Path::new("x"), i)
                .unwrap();
        }
        let t = mem.take_files_inner();
        check!(t.segments.len() == 1);
        check!(
            t.queued_segments == Some(2),
            "two segments still waiting in the ring"
        );
        check!(t.queued_bytes == Some(2), "two 1-byte segments queued");

        let _ = mem.take_files_inner();
        let t = mem.take_files_inner();
        check!(t.segments.len() == 1);
        check!(t.queued_segments == Some(0), "ring drained");
        check!(t.queued_bytes == Some(0));

        let t = mem.take_files_inner();
        check!(t.segments.is_empty());
        check!(t.queued_segments == Some(0));
    }

    #[test]
    fn mem_fs_take_pops_one_at_a_time() {
        let mem = MemFs::with_capacity(16).unwrap();
        for i in 0..3u32 {
            let handle = mem.create_handle(Path::new("dummy")).unwrap();
            let ActiveHandle::Mem(mut w) = handle else {
                panic!()
            };
            w.buf.push(i as u8);
            mem.seal_handle(ActiveHandle::Mem(w), Path::new("dummy"), i)
                .unwrap();
        }

        for _ in 0..3 {
            let t = mem.take_files_inner();
            check!(t.segments.len() == 1);
        }
        let t = mem.take_files_inner();
        check!(t.segments.is_empty());
    }

    #[test]
    fn mem_fs_remove_sealed_is_noop() {
        let mem = MemFs::with_capacity(16).unwrap();
        let seg = SegmentRef::Memory(MemorySegment { index: 0, size: 10 });
        // Should not panic
        mem.remove_sealed_inner(&seg, RemoveReason::Eviction);
        mem.remove_sealed_inner(&seg, RemoveReason::Terminal);
    }
}

#[cfg(all(test, shuttle))]
mod shuttle_tests {
    use super::*;
    use assert2::check;

    fn seal_one(mem: &MemFs, index: u32, size: usize) {
        let handle = mem.create_handle(Path::new("x")).unwrap();
        let ActiveHandle::Mem(mut w) = handle else {
            unreachable!("mem backend yields a mem handle")
        };
        w.buf.resize(size, 0u8);
        mem.seal_handle(ActiveHandle::Mem(w), Path::new("x"), index)
            .unwrap();
    }

    /// Worker side: drain until the writer is done and the ring is empty.
    /// Loading each segment drops its `SegmentAccounting`, releasing in-flight.
    /// `segments_dropped` is per-cycle, so we accumulate each emit.
    fn drain(mem: &MemFs, consumed: &AtomicU64, dropped: &AtomicU64) {
        loop {
            let t = mem.take_files_inner();
            dropped.fetch_add(t.segments_dropped, Ordering::Relaxed);
            for seg in t.segments {
                let _ = seg.load().unwrap();
                consumed.fetch_add(1, Ordering::Relaxed);
            }
            if mem.writer_done_inner() {
                // writer_done is Acquire and stored after every force_push, so
                // the remaining queue is fully visible. Drain to empty.
                loop {
                    let t = mem.take_files_inner();
                    dropped.fetch_add(t.segments_dropped, Ordering::Relaxed);
                    if t.segments.is_empty() {
                        return;
                    }
                    for seg in t.segments {
                        let _ = seg.load().unwrap();
                        consumed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            shuttle::thread::yield_now();
        }
    }

    fn run_scenario(slots: usize, seg_size: usize, count: u32, expect_no_eviction: bool) {
        let mem = Arc::new(MemFs::with_capacity(slots).unwrap());
        let consumed = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));

        let writer = {
            let mem = Arc::clone(&mem);
            crate::primitives::thread::spawn(move || {
                for i in 0..count {
                    seal_one(&mem, i, seg_size);
                }
                mem.mark_writer_done_inner();
            })
        };
        let worker = {
            let mem = Arc::clone(&mem);
            let consumed = Arc::clone(&consumed);
            let dropped = Arc::clone(&dropped);
            crate::primitives::thread::spawn(move || drain(&mem, &consumed, &dropped))
        };
        writer.join().unwrap();
        worker.join().unwrap();

        let consumed = consumed.load(Ordering::Relaxed);
        let dropped = dropped.load(Ordering::Relaxed);

        // Every segment is either consumed exactly once or evicted exactly
        // once, never both, never lost.
        check!(consumed + dropped == count as u64);
        if expect_no_eviction {
            check!(dropped == 0);
            check!(consumed == count as u64);
        }
        // Gauges fully settle once the writer is done and the ring is drained.
        check!(mem.channel.in_flight_segments.load(Ordering::Relaxed) == 0);
        check!(mem.channel.in_flight_bytes.load(Ordering::Relaxed) == 0);
    }

    fn scenario_no_eviction() {
        run_scenario(16, 16, 3, true);
    }

    fn scenario_with_eviction() {
        // Slot cap = 2; the writer outruns the worker so force_push evicts
        // under contention.
        run_scenario(2, 16, 4, false);
    }

    #[test]
    fn shuttle_handoff_no_loss_pct() {
        shuttle::check_pct(scenario_no_eviction, 5_000, 3);
    }

    #[test]
    fn shuttle_handoff_no_loss_determinism() {
        shuttle::check_uncontrolled_nondeterminism(scenario_no_eviction, 5_000);
    }

    #[test]
    fn shuttle_eviction_accounting_pct() {
        shuttle::check_pct(scenario_with_eviction, 5_000, 3);
    }
}

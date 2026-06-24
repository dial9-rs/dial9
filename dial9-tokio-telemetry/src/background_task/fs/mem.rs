//! In-memory `Fs` variant.
//!
//! `MemFs` keeps sealed segments in a byte-bounded ring. On each `seal`,
//! the oldest segments are dropped until `queued_bytes <=
//! max_total_size`. The worker pops one segment per `take_files` cycle.
//!
//! The shutdown handoff rides `writer_done` (Acquire/Release) plus a
//! `Notify` for wakeups.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::background_task::sealed::{MemorySegment, SegmentRef};
use crate::primitives::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::primitives::sync::{Arc, Mutex};
use crate::rate_limit::rate_limited;

use super::{ActiveHandle, EpochWindow, RemoveReason, SegmentAccounting, TakenFiles, TakenSegment};

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

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
    bytes: Bytes,
    /// 0 for a fresh seal, incremented each time the worker re-enqueues
    /// after a retryable failure.
    retry_count: u32,
    /// Creation epoch parsed from the segment header at seal time, used by
    /// the triggered worker's windowed pop.
    epoch_secs: u64,
    /// Wall-clock epoch when the segment sealed; together with
    /// `epoch_secs` it gives the span the windowed pop matches against.
    seal_secs: u64,
}

/// Cap on retryable-failure re-enqueues for a memory segment.
pub(crate) const MEMORY_RETRY_BUDGET: u32 = 3;

/// Holds the deque + bookkeeping that must move together under the lock.
struct Queue {
    segments: VecDeque<MemSealedSegment>,
    /// Sum of `bytes.len()` across `segments`.
    bytes: u64,
    /// Segments evicted since the last `take_files` swap.
    dropped: u64,
}

struct MemChannel {
    max_total_size: u64,
    queue: Mutex<Queue>,
    in_flight_bytes: Arc<AtomicU64>,
    in_flight_segments: Arc<AtomicU64>,
    in_flight_bytes_peak: Arc<AtomicU64>,
    writer_done: AtomicBool,
    notify: Notify,
}

/// In-memory segment channel.
pub(crate) struct MemFs {
    channel: Arc<MemChannel>,
}

impl MemFs {
    /// Build a memory channel with a byte budget.
    pub(crate) fn with_capacity(max_total_size: u64, segment_size_hint: u64) -> io::Result<Self> {
        if max_total_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_total_size must be > 0",
            ));
        }

        #[allow(unknown_lints, clippy::manual_checked_ops)]
        let slots = if segment_size_hint == 0 {
            1
        } else {
            (max_total_size / segment_size_hint).max(1) as usize
        };
        Ok(Self {
            channel: Arc::new(MemChannel {
                max_total_size,
                queue: Mutex::new(Queue {
                    segments: VecDeque::with_capacity(slots),
                    bytes: 0,
                    dropped: 0,
                }),
                in_flight_bytes: Arc::new(AtomicU64::new(0)),
                in_flight_segments: Arc::new(AtomicU64::new(0)),
                in_flight_bytes_peak: Arc::new(AtomicU64::new(0)),
                writer_done: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        })
    }

    pub(super) fn create_segment(&self, _path: &Path) -> io::Result<ActiveHandle> {
        Ok(ActiveHandle::Mem(MemActiveWriter { buf: Vec::new() }))
    }

    pub(super) fn seal(
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
        let (epoch_secs, _) =
            crate::background_task::sealed::creation_epoch_secs(&bytes, _active_path);
        let seal_secs = now_epoch_secs();
        let ch = &self.channel;

        let (evicted, first_idx, last_idx) = {
            let mut q = ch.queue.lock().unwrap();
            // Evict-first under the lock: keeps `q.bytes <= max_total_size`
            // at every observable moment, no transient overshoot.
            let mut evicted = 0u64;
            let mut first: Option<u32> = None;
            let mut last: Option<u32> = None;
            while q.bytes + size > ch.max_total_size
                && let Some(old) = q.segments.pop_front()
            {
                q.bytes -= old.bytes.len() as u64;
                evicted += 1;
                first.get_or_insert(old.index);
                last = Some(old.index);
            }
            q.dropped += evicted;
            q.segments.push_back(MemSealedSegment {
                index,
                bytes,
                retry_count: 0,
                epoch_secs,
                seal_secs,
            });
            q.bytes += size;
            (evicted, first, last)
        };

        if let (Some(first), Some(last)) = (first_idx, last_idx) {
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!(
                    target: "dial9_worker",
                    "memory segment evicted (over byte budget): {evicted} segment(s) dropped, indices {first}..={last}",
                );
            });
        }

        ch.notify.notify_one();
        Ok(SegmentRef::Memory(MemorySegment { index, size }))
    }

    pub(super) fn remove_sealed(&self, _seg: &SegmentRef, _reason: RemoveReason) {}

    /// Re-enqueue `bytes` for re-dispense on the next `take_files` cycle.
    ///
    /// `attempt` is the new retry count this segment carries; `epochs` is
    /// the `(creation, seal)` pair the slot originally carried.
    /// Pushed to the front so a single failing segment cycles back ahead of fresh work.
    pub(super) fn release_for_retry(
        &self,
        index: u32,
        bytes: Bytes,
        attempt: u32,
        epochs: (u64, u64),
    ) {
        let size = bytes.len() as u64;
        let ch = &self.channel;
        {
            let mut q = ch.queue.lock().unwrap();
            q.segments.push_front(MemSealedSegment {
                index,
                bytes,
                retry_count: attempt,
                epoch_secs: epochs.0,
                seal_secs: epochs.1,
            });
            q.bytes += size;
        }
        ch.notify.notify_one();
    }

    pub(super) fn remove_active(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn take_files(&self) -> TakenFiles {
        self.take_files_inner(None)
    }

    /// Windowed pop for the triggered worker: the oldest slot whose
    /// `[creation, seal]` span overlaps one of `windows`. Non-matching
    /// slots stay in the ring (history is preserved for later dumps); still
    /// at most one segment per call so the in-flight memory bound is
    /// unchanged.
    pub(super) fn take_files_matching(&self, windows: &[EpochWindow]) -> TakenFiles {
        self.take_files_inner(Some(windows))
    }

    fn take_files_inner(&self, windows: Option<&[EpochWindow]>) -> TakenFiles {
        let ch = &self.channel;

        // Floor peak at current in-flight, this cycle's pop seeds the next.
        let in_flight_now = ch.in_flight_bytes.load(Ordering::Acquire);
        let peak = ch
            .in_flight_bytes_peak
            .swap(in_flight_now, Ordering::AcqRel);

        // Pop + drop-counter snapshot under one lock so the metric matches
        // the queue state we sampled.
        let (popped, queued_segments, queued_bytes, segments_dropped) = {
            let mut q = ch.queue.lock().unwrap();
            let popped = match windows {
                None => q.segments.pop_front(),
                Some(ws) => q
                    .segments
                    .iter()
                    .position(|s| ws.iter().any(|w| w.overlaps(s.epoch_secs, s.seal_secs)))
                    .and_then(|i| q.segments.remove(i)),
            };
            if let Some(s) = &popped {
                q.bytes -= s.bytes.len() as u64;
            }
            let segments_dropped = std::mem::take(&mut q.dropped);
            (popped, q.segments.len() as u64, q.bytes, segments_dropped)
        };

        let Some(slot) = popped else {
            return TakenFiles {
                segments: vec![],
                queued_segments: Some(queued_segments),
                queued_bytes: Some(queued_bytes),
                in_flight_segments: ch.in_flight_segments.load(Ordering::Relaxed),
                in_flight_bytes: in_flight_now,
                in_flight_bytes_peak: Some(peak),
                segments_dropped,
            };
        };

        let size = slot.bytes.len() as u64;
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
            slot.retry_count,
            (slot.epoch_secs, slot.seal_secs),
        );

        TakenFiles {
            segments: vec![taken],
            queued_segments: Some(queued_segments),
            queued_bytes: Some(queued_bytes),
            in_flight_segments: ch.in_flight_segments.load(Ordering::Relaxed),
            in_flight_bytes: ch.in_flight_bytes.load(Ordering::Relaxed),
            in_flight_bytes_peak: Some(peak),
            segments_dropped,
        }
    }

    pub(super) async fn wait_for_more(&self, stop: &CancellationToken, _poll_interval: Duration) {
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

    pub(super) fn writer_done(&self) -> bool {
        self.channel.writer_done.load(Ordering::Acquire)
    }

    /// Test-only: override the seal epoch of a queued slot so tests can
    /// simulate segments sealed in the past.
    #[cfg(test)]
    pub(super) fn set_seal_secs_for_test(&self, index: u32, seal_secs: u64) {
        let mut q = self.channel.queue.lock().unwrap();
        for s in q.segments.iter_mut().filter(|s| s.index == index) {
            s.seal_secs = seal_secs;
        }
    }

    pub(super) fn mark_writer_done(&self) {
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
        let mem = MemFs::with_capacity(64 * 1024, 1024).unwrap();
        let handle = mem
            .create_segment(Path::new("mem://trace.0.bin.active"))
            .unwrap();
        let ActiveHandle::Mem(mut w) = handle else {
            panic!()
        };
        w.buf.extend_from_slice(b"hello bytes");
        let handle = ActiveHandle::Mem(w);

        let seg_ref = mem
            .seal(handle, Path::new("mem://trace.0.bin.active"), 0)
            .unwrap();
        check!(matches!(seg_ref, SegmentRef::Memory(_)));
        check!(seg_ref.index() == 0);

        let taken = mem.take_files();
        check!(taken.segments.len() == 1);

        let (loaded_ref, payload, _acct) =
            taken.segments.into_iter().next().unwrap().load().unwrap();
        check!(loaded_ref.index() == 0);
        check!(payload.into_bytes().as_ref() == b"hello bytes");
    }

    #[test]
    fn mem_fs_byte_budget_eviction() {
        // Budget = 60 bytes; segment 0 fills it. Segment 1 push triggers
        // eviction of segment 0.
        let mem = MemFs::with_capacity(60, 60).unwrap();

        for index in 0..2u32 {
            let handle = mem.create_segment(Path::new("dummy")).unwrap();
            let ActiveHandle::Mem(mut w) = handle else {
                panic!()
            };
            w.buf.resize(60, index as u8);
            mem.seal(ActiveHandle::Mem(w), Path::new("dummy"), index)
                .unwrap();
        }

        // Only the most recent segment remains.
        let t = mem.take_files();
        check!(t.segments_dropped == 1, "one eviction reported");
        check!(t.segments.len() == 1);
        check!(t.segments[0].seg_ref.index() == 1);
    }

    #[test]
    fn mem_fs_byte_budget_multi_evict() {
        // Budget = 100 bytes; push three 60-byte segments. Each new push
        // evicts everything that overflows. Final state: just segment 2.
        let mem = MemFs::with_capacity(100, 60).unwrap();
        for index in 0..3u32 {
            let handle = mem.create_segment(Path::new("dummy")).unwrap();
            let ActiveHandle::Mem(mut w) = handle else {
                panic!()
            };
            w.buf.resize(60, index as u8);
            mem.seal(ActiveHandle::Mem(w), Path::new("dummy"), index)
                .unwrap();
        }
        let t = mem.take_files();
        check!(t.segments_dropped == 2);
        check!(t.segments.len() == 1);
        check!(t.segments[0].seg_ref.index() == 2);
    }

    #[test]
    fn mem_fs_rejects_zero_budget() {
        let Err(e) = MemFs::with_capacity(0, 1024) else {
            panic!("expected error for max_total_size == 0");
        };
        check!(e.kind() == io::ErrorKind::InvalidInput);
    }

    #[test]
    fn mem_fs_queued_segments_after_pop() {
        let mem = MemFs::with_capacity(64 * 1024, 1024).unwrap();
        for i in 0..3u32 {
            let handle = mem.create_segment(Path::new("x")).unwrap();
            let ActiveHandle::Mem(mut w) = handle else {
                panic!()
            };
            w.buf.push(i as u8);
            mem.seal(ActiveHandle::Mem(w), Path::new("x"), i).unwrap();
        }
        let t = mem.take_files();
        check!(t.segments.len() == 1);
        check!(
            t.queued_segments == Some(2),
            "two segments still waiting in the ring"
        );
        check!(t.queued_bytes == Some(2), "two 1-byte segments queued");

        let _ = mem.take_files();
        let t = mem.take_files();
        check!(t.segments.len() == 1);
        check!(t.queued_segments == Some(0), "ring drained");
        check!(t.queued_bytes == Some(0));

        let t = mem.take_files();
        check!(t.segments.is_empty());
        check!(t.queued_segments == Some(0));
    }

    #[test]
    fn mem_fs_take_pops_one_at_a_time() {
        let mem = MemFs::with_capacity(64 * 1024, 1024).unwrap();
        for i in 0..3u32 {
            let handle = mem.create_segment(Path::new("dummy")).unwrap();
            let ActiveHandle::Mem(mut w) = handle else {
                panic!()
            };
            w.buf.push(i as u8);
            mem.seal(ActiveHandle::Mem(w), Path::new("dummy"), i)
                .unwrap();
        }

        for _ in 0..3 {
            let t = mem.take_files();
            check!(t.segments.len() == 1);
        }
        let t = mem.take_files();
        check!(t.segments.is_empty());
    }

    #[test]
    fn mem_fs_remove_sealed_is_noop() {
        let mem = MemFs::with_capacity(64 * 1024, 1024).unwrap();
        let seg = SegmentRef::Memory(MemorySegment { index: 0, size: 10 });
        // Should not panic
        mem.remove_sealed(&seg, RemoveReason::Eviction);
        mem.remove_sealed(&seg, RemoveReason::Terminal);
    }
}

#[cfg(all(test, shuttle))]
mod shuttle_tests {
    use super::*;
    use assert2::check;

    fn seal_one(mem: &MemFs, index: u32, size: usize) {
        let handle = mem.create_segment(Path::new("x")).unwrap();
        let ActiveHandle::Mem(mut w) = handle else {
            unreachable!("mem backend yields a mem handle")
        };
        w.buf.resize(size, 0u8);
        mem.seal(ActiveHandle::Mem(w), Path::new("x"), index)
            .unwrap();
    }

    /// Worker side: drain until the writer is done and the ring is empty.
    /// Loading each segment drops its `SegmentAccounting`, releasing in-flight.
    /// `segments_dropped` is per-cycle, so we accumulate each emit.
    fn drain(mem: &MemFs, consumed: &AtomicU64, dropped: &AtomicU64) {
        loop {
            let t = mem.take_files();
            dropped.fetch_add(t.segments_dropped, Ordering::Relaxed);
            for seg in t.segments {
                let _ = seg.load().unwrap();
                consumed.fetch_add(1, Ordering::Relaxed);
            }
            if mem.writer_done() {
                // writer_done is stored (Release) after the seal-time queue push,
                // loading it Acquire here makes the remaining queue fully visible.
                // Drain to empty.
                loop {
                    let t = mem.take_files();
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

    fn run_scenario(budget: u64, seg_size: usize, count: u32, expect_no_eviction: bool) {
        let mem = Arc::new(MemFs::with_capacity(budget, seg_size as u64).unwrap());
        let consumed = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));

        let writer = {
            let mem = Arc::clone(&mem);
            crate::primitives::thread::spawn(move || {
                for i in 0..count {
                    seal_one(&mem, i, seg_size);
                }
                mem.mark_writer_done();
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
        // Budget room for many 16-byte segments; nothing should evict.
        run_scenario(1 << 16, 16, 3, true);
    }

    fn scenario_with_eviction() {
        // Budget fits ~2 segments; the writer outruns the worker so the
        // byte-budget loop evicts under contention.
        run_scenario(40, 16, 4, false);
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

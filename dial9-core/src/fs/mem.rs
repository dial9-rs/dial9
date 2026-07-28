//! In-memory `Fs` variant.
//!
//! `MemFs` keeps sealed segments in a byte-bounded ring. On each `seal`,
//! the oldest segments are dropped until `queued_bytes <= max_total_size`.
//! While the sole current-data checkpoint is active, its snapshot plus the
//! newly sealed checkpoint segment are retained; that one segment may
//! temporarily exceed the ring budget until the worker takes it. The worker
//! pops one segment per `take_files` cycle.
//!
//! The shutdown handoff rides `writer_done` (Acquire/Release) plus a
//! `tokio::sync::Notify` for wakeups.

#[cfg(feature = "pipeline")]
use std::collections::HashSet;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::Notify;

use crate::primitives::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::primitives::sync::{Arc, Mutex};
use crate::rate_limit::rate_limited;
use crate::sealed::{MemorySegment, SegmentRef};

use super::{ActiveHandle, RemoveReason};
#[cfg(feature = "pipeline")]
use super::{EpochWindow, SegmentAccounting, TakenFiles, TakenSegment};

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
    #[cfg_attr(not(feature = "pipeline"), allow(dead_code))]
    retry_count: u32,
    /// Creation epoch parsed from the segment header at seal time, used by
    /// the triggered worker's windowed pop.
    #[cfg_attr(not(feature = "pipeline"), allow(dead_code))]
    epoch_secs: u64,
    /// Wall-clock epoch when the segment sealed; together with
    /// `epoch_secs` it gives the span the windowed pop matches against.
    #[cfg_attr(not(feature = "pipeline"), allow(dead_code))]
    seal_secs: u64,
}

/// Cap on retryable-failure re-enqueues for a memory segment.
#[cfg(feature = "pipeline")]
pub(crate) const MEMORY_RETRY_BUDGET: u32 = 3;

/// Holds the deque + bookkeeping that must move together under the lock.
struct Queue {
    segments: VecDeque<MemSealedSegment>,
    /// Sum of `bytes.len()` across `segments`.
    bytes: u64,
    /// Segments evicted since the last `take_files` swap.
    dropped: u64,
    /// Segments belonging to a checkpoint that the worker has not taken yet.
    /// The sole active checkpoint may temporarily keep its active segment
    /// beyond the ring budget until the worker takes the protected snapshot.
    /// Later, unprotected production segments evict themselves while every
    /// retained entry is protected, so the overshoot is bounded to that one
    /// checkpoint segment (though a batch may overshoot its size threshold).
    #[cfg(feature = "pipeline")]
    checkpoint_protected: HashSet<u32>,
}

struct MemChannel {
    max_total_size: u64,
    queue: Mutex<Queue>,
    #[cfg_attr(not(feature = "pipeline"), allow(dead_code))]
    in_flight_bytes: Arc<AtomicU64>,
    #[cfg_attr(not(feature = "pipeline"), allow(dead_code))]
    in_flight_segments: Arc<AtomicU64>,
    #[cfg_attr(not(feature = "pipeline"), allow(dead_code))]
    in_flight_bytes_peak: Arc<AtomicU64>,
    /// Serializes current-data checkpoint snapshots so protected memory can
    /// exceed the ring budget by at most one checkpoint segment.
    #[cfg(feature = "pipeline")]
    checkpoint_active: AtomicBool,
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
                    #[cfg(feature = "pipeline")]
                    checkpoint_protected: HashSet::new(),
                }),
                in_flight_bytes: Arc::new(AtomicU64::new(0)),
                in_flight_segments: Arc::new(AtomicU64::new(0)),
                in_flight_bytes_peak: Arc::new(AtomicU64::new(0)),
                #[cfg(feature = "pipeline")]
                checkpoint_active: AtomicBool::new(false),
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
        let (epoch_secs, _) = crate::sealed::creation_epoch_secs(&bytes, _active_path);
        let seal_secs = now_epoch_secs();
        let ch = &self.channel;

        let (evicted, first_idx, last_idx) = {
            let mut q = ch.queue.lock().unwrap();
            q.segments.push_back(MemSealedSegment {
                index,
                bytes,
                retry_count: 0,
                epoch_secs,
                seal_secs,
            });
            q.bytes += size;

            // The sole active checkpoint may add its active segment beyond
            // the ring budget until the worker takes it. Evict only
            // unprotected entries; later production segments evict themselves
            // if the retained snapshot alone fills the budget. Without a
            // checkpoint this is the same oldest-first policy as before.
            let mut evicted = 0u64;
            let mut first: Option<u32> = None;
            let mut last: Option<u32> = None;
            while q.bytes > ch.max_total_size {
                #[cfg(feature = "pipeline")]
                let evict_pos = q
                    .segments
                    .iter()
                    .position(|segment| !q.checkpoint_protected.contains(&segment.index));
                #[cfg(not(feature = "pipeline"))]
                let evict_pos = (!q.segments.is_empty()).then_some(0);
                let Some(evict_pos) = evict_pos else {
                    break;
                };
                let old = q
                    .segments
                    .remove(evict_pos)
                    .expect("eviction position came from the queue");
                q.bytes -= old.bytes.len() as u64;
                evicted += 1;
                first.get_or_insert(old.index);
                last = Some(old.index);
            }
            q.dropped += evicted;
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

    #[cfg(feature = "pipeline")]
    pub(super) fn protect_checkpoint_segments(&self, current_index: Option<u32>) -> Vec<u32> {
        let mut q = self.channel.queue.lock().unwrap();
        let mut indices: Vec<u32> = q.segments.iter().map(|segment| segment.index).collect();
        if let Some(index) = current_index {
            indices.push(index);
        }
        q.checkpoint_protected.extend(indices.iter().copied());
        indices
    }

    #[cfg(feature = "pipeline")]
    pub(super) fn try_reserve_checkpoint(&self) -> bool {
        self.channel
            .checkpoint_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    #[cfg(feature = "pipeline")]
    pub(super) fn finish_checkpoint(&self) {
        self.channel
            .checkpoint_active
            .store(false, Ordering::Release);
    }

    #[cfg(feature = "pipeline")]
    pub(super) fn checkpoint_segment_is_protected(&self, index: u32) -> bool {
        self.channel
            .queue
            .lock()
            .unwrap()
            .checkpoint_protected
            .contains(&index)
    }

    #[cfg(feature = "pipeline")]
    pub(super) fn release_checkpoint_segment(&self, index: u32) {
        let ch = &self.channel;
        let mut q = ch.queue.lock().unwrap();
        if !q.checkpoint_protected.remove(&index) {
            return;
        }

        // If the worker never took a protected entry (for example it stopped
        // before registering the request), restore the ring budget now.
        while q.bytes > ch.max_total_size {
            let Some(pos) = q
                .segments
                .iter()
                .position(|segment| !q.checkpoint_protected.contains(&segment.index))
            else {
                break;
            };
            let old = q
                .segments
                .remove(pos)
                .expect("eviction position came from the queue");
            q.bytes -= old.bytes.len() as u64;
            q.dropped += 1;
        }
    }

    /// Re-enqueue `bytes` for re-dispense on the next `take_files` cycle.
    ///
    /// `attempt` is the new retry count this segment carries; `epochs` is
    /// the `(creation, seal)` pair the slot originally carried.
    /// Pushed to the front so a single failing segment cycles back ahead of fresh work.
    #[cfg(feature = "pipeline")]
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

    #[cfg(feature = "pipeline")]
    pub(super) fn take_files(&self) -> TakenFiles {
        self.take_files_inner(None)
    }

    /// Windowed pop for the triggered worker: the oldest slot whose
    /// `[creation, seal]` span overlaps one of `windows`. Non-matching
    /// slots stay in the ring (history is preserved for later dumps); still
    /// at most one segment per call so the in-flight memory bound is
    /// unchanged.
    #[cfg(feature = "pipeline")]
    pub(super) fn take_files_matching(&self, windows: &[EpochWindow]) -> TakenFiles {
        self.take_files_inner(Some(windows))
    }

    #[cfg(feature = "pipeline")]
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
                q.checkpoint_protected.remove(&s.index);
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

    /// Park until the ring may have new work or the writer is done.
    ///
    /// Lost-wakeup safe: `enable()` registers the waiter *before* the
    /// condition check, so a concurrent `notify_one()` between registration
    /// and `.await` is not missed.
    #[cfg(feature = "pipeline")]
    pub(super) async fn wait_for_wakeup(&self) {
        let ch = &self.channel;
        let notified = ch.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.has_pending() || ch.writer_done.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    #[cfg(feature = "pipeline")]
    fn has_pending(&self) -> bool {
        !self.channel.queue.lock().unwrap().segments.is_empty()
    }

    #[cfg(feature = "pipeline")]
    pub(super) fn writer_done(&self) -> bool {
        self.channel.writer_done.load(Ordering::Acquire)
    }

    /// Test-only: override the seal epoch of a queued slot so tests can
    /// simulate segments sealed in the past.
    #[cfg(all(test, feature = "pipeline"))]
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

#[cfg(all(test, feature = "pipeline"))]
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
    fn checkpoint_snapshot_uses_in_flight_reserve_instead_of_evicting() {
        let mem = MemFs::with_capacity(4, 4).unwrap();
        check!(mem.try_reserve_checkpoint());
        let first = mem.create_segment(Path::new("first")).unwrap();
        let ActiveHandle::Mem(mut first) = first else {
            unreachable!("memory backend yields a memory handle")
        };
        first.buf.resize(4, 1);
        mem.seal(ActiveHandle::Mem(first), Path::new("first"), 0)
            .unwrap();

        let protected = mem.protect_checkpoint_segments(Some(1));
        check!(protected == vec![0, 1]);

        let checkpoint = mem.create_segment(Path::new("checkpoint")).unwrap();
        let ActiveHandle::Mem(mut checkpoint) = checkpoint else {
            unreachable!("memory backend yields a memory handle")
        };
        checkpoint.buf.resize(4, 2);
        mem.seal(ActiveHandle::Mem(checkpoint), Path::new("checkpoint"), 1)
            .unwrap();

        let first_taken = mem.take_files();
        check!(first_taken.segments.len() == 1);
        check!(first_taken.segments[0].seg_ref.index() == 0);
        check!(first_taken.segments_dropped == 0);

        let checkpoint_taken = mem.take_files();
        check!(checkpoint_taken.segments.len() == 1);
        check!(checkpoint_taken.segments[0].seg_ref.index() == 1);
        check!(checkpoint_taken.segments_dropped == 0);
        mem.finish_checkpoint();
    }

    #[test]
    fn active_checkpoint_bounds_memory_overshoot_and_rejects_another() {
        let mem = MemFs::with_capacity(4, 4).unwrap();
        check!(mem.try_reserve_checkpoint());
        check!(!mem.try_reserve_checkpoint());

        let first = mem.create_segment(Path::new("first")).unwrap();
        let ActiveHandle::Mem(mut first) = first else {
            unreachable!("memory backend yields a memory handle")
        };
        first.buf.resize(4, 1);
        mem.seal(ActiveHandle::Mem(first), Path::new("first"), 0)
            .unwrap();
        let protected = mem.protect_checkpoint_segments(Some(1));

        for index in 1..=32 {
            let segment = mem.create_segment(Path::new("later")).unwrap();
            let ActiveHandle::Mem(mut segment) = segment else {
                unreachable!("memory backend yields a memory handle")
            };
            segment.buf.resize(4, 2);
            mem.seal(ActiveHandle::Mem(segment), Path::new("later"), index)
                .unwrap();
        }

        {
            let q = mem.channel.queue.lock().unwrap();
            check!(q.bytes == 8);
            check!(q.segments.len() == 2);
            check!(q.segments[0].index == 0);
            check!(q.segments[1].index == 1);
        }

        for index in protected {
            mem.release_checkpoint_segment(index);
        }
        mem.finish_checkpoint();
        check!(mem.try_reserve_checkpoint());
        mem.finish_checkpoint();

        let q = mem.channel.queue.lock().unwrap();
        check!(q.bytes <= mem.channel.max_total_size);
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

    crate::shuttle_test! {
        default;
        // Budget room for many 16-byte segments; nothing should evict.
        fn shuttle_handoff_no_loss() {
            run_scenario(1 << 16, 16, 3, true);
        }
    }

    crate::shuttle_test! {
        default;
        // Budget fits ~2 segments; the writer outruns the worker so the
        // byte-budget loop evicts under contention.
        fn shuttle_eviction_accounting() {
            run_scenario(40, 16, 4, false);
        }
    }
}

//! Filesystem abstraction for the writer-worker seam.
//!
//! `Fs` covers the full segment lifecycle (create, seal, remove, scan) for
//! two backends, selected at construction time:
//!
//! - `Fs::Disk(DiskFs)`: real filesystem. See [`disk`].
//! - `Fs::Mem(MemFs)`: in-process ring channel. See [`mem`].

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use crate::background_task::payload::Payload;
use crate::background_task::sealed::{MemorySegment, SealedSegment, SegmentRef};
use crate::primitives::fs;
use crate::primitives::sync::Arc;
use crate::primitives::sync::atomic::{AtomicU64, Ordering};

mod disk;
mod mem;

use disk::DiskFs;
pub(crate) use mem::MEMORY_RETRY_BUDGET;
use mem::{MemActiveWriter, MemFs};

/// Segments reserved outside the ring so `max_total_size` cap includes them.
/// 1 active buffer + 1 in-flight segment.
pub(crate) const PIPELINE_RESERVE_SEGMENTS: u64 = 2;

/// Retained trace artifacts found at writer construction.
#[derive(Debug, Default)]
pub(crate) struct DiscoveredArtifacts {
    pub(crate) closed_files: VecDeque<(SegmentRef, u64)>,
    pub(crate) next_active_index: u32,
}

pub(crate) enum RemoveReason {
    /// Writer-side backpressure shed. Counts toward `dropped_segments`.
    Eviction,
    /// Worker cleanup after terminal pipeline failure.
    Terminal,
}

/// In-flight byte accounting for memory-backed segments. `size` is the
/// last payload length the worker reported via [`adjust`](Self::adjust);
/// drop returns that to the atomic.
#[derive(Debug)]
pub(crate) struct SegmentAccounting {
    pub(crate) in_flight_bytes: Arc<AtomicU64>,
    pub(crate) in_flight_segments: Arc<AtomicU64>,
    pub(crate) in_flight_bytes_peak: Arc<AtomicU64>,
    pub(crate) size: u64,
}

impl SegmentAccounting {
    /// Re-balance `in_flight_bytes` after a processor mutated the payload.
    pub(crate) fn adjust(&mut self, new_size: u64) {
        if new_size == self.size {
            return;
        }
        let total = if new_size > self.size {
            let delta = new_size - self.size;
            let prev = self.in_flight_bytes.fetch_add(delta, Ordering::AcqRel);
            prev + delta
        } else {
            let delta = self.size - new_size;
            let prev = self.in_flight_bytes.fetch_sub(delta, Ordering::AcqRel);
            debug_assert!(
                prev >= delta,
                "in_flight_bytes underflow on adjust: prev={prev} sub={delta}"
            );
            prev - delta
        };
        self.in_flight_bytes_peak.fetch_max(total, Ordering::AcqRel);
        self.size = new_size;
    }
}

impl Drop for SegmentAccounting {
    fn drop(&mut self) {
        let prev_bytes = self.in_flight_bytes.fetch_sub(self.size, Ordering::AcqRel);
        debug_assert!(
            prev_bytes >= self.size,
            "in_flight_bytes underflow: prev={prev_bytes} sub={}",
            self.size
        );
        let prev_count = self.in_flight_segments.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            prev_count >= 1,
            "in_flight_segments underflow: prev={prev_count}"
        );
    }
}

/// Active-segment write handle.
pub(crate) enum ActiveHandle {
    Disk(fs::File),
    Mem(MemActiveWriter),
}

impl Write for ActiveHandle {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match self {
            ActiveHandle::Disk(f) => f.write(data),
            ActiveHandle::Mem(m) => m.write(data),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            ActiveHandle::Disk(f) => f.flush(),
            ActiveHandle::Mem(m) => m.flush(),
        }
    }
}

/// Memory-only state attached to a `TakenSegment`.
pub(crate) struct MemoryPayload {
    pub(crate) bytes: Bytes,
    pub(crate) accounting: SegmentAccounting,
    pub(crate) retry_count: u32,
    /// `(creation, seal)` epochs carried from the ring slot so the worker
    /// and retry re-enqueue never re-parse them.
    pub(crate) epochs: (u64, u64),
}

/// A claim returned by `Fs::take_files`. Memory comes with payload in hand,
/// disk loads lazily on `load()` so peak in-flight memory stays at one segment.
pub(crate) struct TakenSegment {
    pub(crate) seg_ref: SegmentRef,
    pre_loaded: Option<MemoryPayload>,
}

impl TakenSegment {
    pub(crate) fn disk(seg: SealedSegment) -> Self {
        Self {
            seg_ref: SegmentRef::Disk(seg),
            pre_loaded: None,
        }
    }

    pub(super) fn memory(
        seg: MemorySegment,
        bytes: Bytes,
        accounting: SegmentAccounting,
        retry_count: u32,
        epochs: (u64, u64),
    ) -> Self {
        Self {
            seg_ref: SegmentRef::Memory(seg),
            pre_loaded: Some(MemoryPayload {
                bytes,
                accounting,
                retry_count,
                epochs,
            }),
        }
    }

    /// Cheap clone of the original seal'd bytes, for memory
    /// retry re-enqueue. `None` for disk (retry re-reads the file).
    pub(crate) fn original_bytes(&self) -> Option<Bytes> {
        self.pre_loaded.as_ref().map(|m| m.bytes.clone())
    }

    /// Re-enqueue count this dispense carries. `None` for disk.
    pub(crate) fn retry_count(&self) -> Option<u32> {
        self.pre_loaded.as_ref().map(|m| m.retry_count)
    }

    /// `(creation, seal)` epochs the ring slot carried. `None` for disk
    /// (the worker derives them from the header and file mtime).
    pub(crate) fn mem_epochs(&self) -> Option<(u64, u64)> {
        self.pre_loaded.as_ref().map(|m| m.epochs)
    }

    /// Load the segment payload.
    /// - disk: reads the file (`Err(NotFound)` if it vanished between scan and load).
    /// - memory: zero-copy `Bytes`.
    pub(crate) fn load(self) -> io::Result<(SegmentRef, Payload, Option<SegmentAccounting>)> {
        match self.pre_loaded {
            Some(MemoryPayload {
                bytes, accounting, ..
            }) => Ok((self.seg_ref, Payload::from_bytes(bytes), Some(accounting))),
            None => {
                // None = disk segment, should always have a path.
                let Some(path) = self.seg_ref.disk_path() else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "TakenSegment with no payload and no disk path",
                    ));
                };
                let bytes = fs::read(path)?;
                Ok((self.seg_ref, Payload::from_vec(bytes), None))
            }
        }
    }
}

/// Per-cycle snapshot returned by `Fs::take_files`.
pub(crate) struct TakenFiles {
    pub(crate) segments: Vec<TakenSegment>,
    /// Segments still in the memory ring after this cycle's pop. `None` on disk.
    pub(crate) queued_segments: Option<u64>,
    /// Encoded bytes still in the memory ring after this cycle's pop. `None` on disk.
    pub(crate) queued_bytes: Option<u64>,
    pub(crate) in_flight_segments: u64,
    pub(crate) in_flight_bytes: u64,
    /// High-water of total in-flight bytes observed during the prior
    /// cycle. `None` on disk (no per-stage tracking).
    pub(crate) in_flight_bytes_peak: Option<u64>,
    /// Segments evicted during this cycle (per-cycle delta).
    pub(crate) segments_dropped: u64,
}

/// Closed epoch window a triggered dump matches segments against.
///
/// A segment matches when its `[creation, seal]` span overlaps the window,
/// so a segment that started before the window but holds in-window data is
/// still captured.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EpochWindow {
    /// `None`: unbounded look-back (`dump_current_data`).
    pub(crate) start_secs: Option<u64>,
    pub(crate) end_secs: u64,
}

impl EpochWindow {
    pub(crate) fn overlaps(&self, start_secs: u64, seal_secs: u64) -> bool {
        start_secs <= self.end_secs && self.start_secs.is_none_or(|s| seal_secs >= s)
    }
}

/// Unified filesystem abstraction covering the writer↔worker seam.
pub(crate) enum Fs {
    Disk(DiskFs),
    Mem(MemFs),
}

impl Fs {
    /// Create a new active-segment write handle.
    pub(crate) fn create_segment(&self, path: &Path) -> io::Result<ActiveHandle> {
        match self {
            Fs::Disk(d) => d.create_segment(path),
            Fs::Mem(m) => m.create_segment(path),
        }
    }

    pub(crate) fn new_disk(base_path: &Path) -> Arc<Self> {
        Arc::new(Fs::Disk(DiskFs::from_base_path(base_path)))
    }

    /// Ring budget = `max_total_size - PIPELINE_RESERVE_SEGMENTS * max_segment_size`.
    pub(crate) fn new_in_memory(
        max_total_size: u64,
        max_segment_size: u64,
    ) -> io::Result<Arc<Self>> {
        let reserve = PIPELINE_RESERVE_SEGMENTS.saturating_mul(max_segment_size);
        let ring_budget = max_total_size.checked_sub(reserve).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_total_size below pipeline reserve",
            )
        })?;
        Ok(Arc::new(Fs::Mem(MemFs::with_capacity(
            ring_budget,
            max_segment_size,
        )?)))
    }

    /// Scan for trace artifacts left by previous writer lifetimes.
    /// Memory: default (no restart story).
    pub(crate) fn discover_existing(&self) -> io::Result<DiscoveredArtifacts> {
        match self {
            Fs::Disk(d) => d.discover_existing(),
            Fs::Mem(_) => Ok(DiscoveredArtifacts::default()),
        }
    }

    /// Seal `active_handle` as segment `index`.
    ///
    /// Disk: closes the file handle then renames `active_path` → sealed path.
    /// Memory: extracts the in-memory write buffer and pushes it to the ring.
    ///
    /// Returns `Err(NotFound)` when the active file was removed externally
    /// (disk only). Caller should abandon and start fresh.
    pub(crate) fn seal(
        &self,
        active_handle: ActiveHandle,
        active_path: &Path,
        index: u32,
    ) -> io::Result<SegmentRef> {
        match self {
            Fs::Disk(d) => d.seal(active_handle, active_path, index),
            Fs::Mem(m) => m.seal(active_handle, active_path, index),
        }
    }

    /// Remove a sealed segment.
    ///
    /// Disk: unlinks the file plus any extension-renamed siblings, drops the
    /// claim entry, bumps `dropped_segments` when `reason == Eviction`.
    /// Memory: no-op (bytes already left the ring on pop).
    pub(crate) fn remove_sealed(&self, seg: &SegmentRef, reason: RemoveReason) {
        match self {
            Fs::Disk(d) => d.remove_sealed(seg, reason),
            Fs::Mem(m) => m.remove_sealed(seg, reason),
        }
    }

    /// Discard an active-segment handle without sealing.
    pub(crate) fn remove_active(&self, path: &Path) -> io::Result<()> {
        match self {
            Fs::Disk(d) => d.remove_active(path),
            Fs::Mem(m) => m.remove_active(path),
        }
    }

    /// Return newly-visible sealed segments plus backpressure gauges.
    ///
    /// Each segment is dispensed at most once (claim-set dedup for disk,
    /// pop-once for memory). Memory mode pops at most one segment per call to
    /// bound peak in-flight memory to one segment regardless of backlog.
    pub(crate) fn take_files(&self) -> TakenFiles {
        match self {
            Fs::Disk(d) => d.take_files(),
            Fs::Mem(m) => m.take_files(),
        }
    }

    /// Like [`Self::take_files`], but only dispense segments whose
    /// `[creation, seal]` span overlaps one of `windows`. Used by the
    /// triggered worker so out-of-window history stays in the ring for
    /// later dumps.
    ///
    /// Memory: pops the oldest matching slot, leaving non-matching slots in
    /// place (still at most one segment per call). Disk: returns all new
    /// claims; the worker filters after reading the header and releases
    /// unmatched claims.
    pub(crate) fn take_files_matching(&self, windows: &[EpochWindow]) -> TakenFiles {
        match self {
            Fs::Disk(d) => d.take_files(),
            Fs::Mem(m) => m.take_files_matching(windows),
        }
    }

    /// Wait for new segments to potentially appear.
    ///
    /// Disk: sleeps `poll_interval` or until stop fires.
    /// Memory: awaits the ring `Notify` or stop, with lost-wakeup protection.
    pub(crate) async fn wait_for_more(&self, stop: &CancellationToken, poll_interval: Duration) {
        match self {
            Fs::Disk(d) => d.wait_for_more(stop, poll_interval).await,
            Fs::Mem(m) => m.wait_for_more(stop, poll_interval).await,
        }
    }

    /// Returns `true` once `DiskWriter::finalize` has run.
    pub(crate) fn writer_done(&self) -> bool {
        match self {
            Fs::Disk(d) => d.writer_done(),
            Fs::Mem(m) => m.writer_done(),
        }
    }

    /// Signal that the writer has sealed its final segment. Memory also
    /// pings `Notify` so a parked worker wakes.
    pub(crate) fn mark_writer_done(&self) {
        match self {
            Fs::Disk(d) => d.mark_writer_done(),
            Fs::Mem(m) => m.mark_writer_done(),
        }
    }

    /// Mark a previously dispensed segment as available for re-dispensing on
    /// the next `take_files`.
    ///
    /// Disk: drops the claim entry.
    /// Memory: no-op. Memory retry goes through [`Self::release_for_retry`]
    /// which carries the bytes back into the ring.
    pub(crate) fn release_claim(&self, seg: &SegmentRef) {
        match self {
            Fs::Disk(d) => d.release_claim(seg.index()),
            Fs::Mem(_) => {}
        }
    }

    /// Re-enqueue a memory segment after a retryable failure.
    ///
    /// Caller owns the [`MEMORY_RETRY_BUDGET`] check, this method always
    /// pushes. `epochs` is the `(creation, seal)` pair the slot originally
    /// carried. Disk segments do not use this path, they retry via
    /// [`Self::release_claim`] + directory rescan.
    pub(crate) fn release_for_retry(
        &self,
        seg: &SegmentRef,
        bytes: bytes::Bytes,
        attempt: u32,
        epochs: (u64, u64),
    ) {
        match self {
            Fs::Mem(m) => m.release_for_retry(seg.index(), bytes, attempt, epochs),
            Fs::Disk(_) => unreachable!("release_for_retry called on disk segment"),
        }
    }

    /// Whether one `take_files_matching` call dispenses every matching
    /// segment at once (disk claims the whole backlog; memory pops one slot
    /// per call). The triggered worker uses this to decide if a pass that
    /// ended on a retry still covered all other matching work.
    pub(crate) fn take_is_exhaustive(&self) -> bool {
        matches!(self, Fs::Disk(_))
    }

    /// Test-only: override the seal epoch of a queued memory slot so tests
    /// can simulate segments sealed in the past.
    #[cfg(test)]
    pub(crate) fn set_seal_secs_for_test(&self, index: u32, seal_secs: u64) {
        match self {
            Fs::Mem(m) => m.set_seal_secs_for_test(index, seal_secs),
            Fs::Disk(_) => panic!("set_seal_secs_for_test is memory-only"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use std::path::PathBuf;

    #[test]
    fn segment_ref_disk_display() {
        let seg = SegmentRef::Disk(SealedSegment {
            path: PathBuf::from("/tmp/trace.3.bin"),
            index: 3,
        });
        check!(seg.index() == 3);
        check!(seg.to_string().to_string() == "/tmp/trace.3.bin");
        check!(seg.disk_path() == Some(Path::new("/tmp/trace.3.bin")));
    }

    #[test]
    fn segment_ref_memory_display() {
        let seg = SegmentRef::Memory(MemorySegment {
            index: 7,
            size: 1024,
        });
        check!(seg.index() == 7);
        check!(seg.to_string().to_string() == "mem://7");
        check!(seg.disk_path().is_none());
    }

    #[test]
    fn accounting_adjust_tracks_payload_size() {
        let bytes = Arc::new(AtomicU64::new(500));
        let count = Arc::new(AtomicU64::new(1));
        let peak = Arc::new(AtomicU64::new(500));
        let mut acct = SegmentAccounting {
            in_flight_bytes: Arc::clone(&bytes),
            in_flight_segments: Arc::clone(&count),
            in_flight_bytes_peak: Arc::clone(&peak),
            size: 500,
        };
        // Grow: symbolize-like stage.
        acct.adjust(900);
        check!(bytes.load(Ordering::SeqCst) == 900);
        check!(peak.load(Ordering::SeqCst) == 900);
        check!(acct.size == 900);
        // Shrink: gzip-like stage.
        acct.adjust(200);
        check!(bytes.load(Ordering::SeqCst) == 200);
        check!(peak.load(Ordering::SeqCst) == 900);
        check!(acct.size == 200);
        // No-op.
        acct.adjust(200);
        check!(bytes.load(Ordering::SeqCst) == 200);
        // Drop returns the last observed size, leaving the gauge balanced.
        drop(acct);
        check!(bytes.load(Ordering::SeqCst) == 0);
        check!(count.load(Ordering::SeqCst) == 0);
    }

    #[test]
    fn accounting_drop_decrements() {
        let bytes = Arc::new(AtomicU64::new(1000));
        let count = Arc::new(AtomicU64::new(1));
        let peak = Arc::new(AtomicU64::new(0));
        {
            let _acct = SegmentAccounting {
                in_flight_bytes: Arc::clone(&bytes),
                in_flight_segments: Arc::clone(&count),
                in_flight_bytes_peak: peak,
                size: 500,
            };
        }
        check!(bytes.load(Ordering::SeqCst) == 500);
        check!(count.load(Ordering::SeqCst) == 0);
    }

    #[test]
    fn taken_segment_disk_lazy_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.0.bin");
        std::fs::write(&path, b"disk bytes").unwrap();
        let seg = SealedSegment {
            path: path.clone(),
            index: 0,
        };
        let taken = TakenSegment::disk(seg);
        let (seg_ref, payload, acct) = taken.load().unwrap();
        check!(seg_ref.index() == 0);
        check!(payload.into_bytes().as_ref() == b"disk bytes");
        check!(acct.is_none());
    }

    #[test]
    fn taken_segment_disk_notfound() {
        let seg = SealedSegment {
            path: PathBuf::from("/nonexistent/trace.0.bin"),
            index: 0,
        };
        let taken = TakenSegment::disk(seg);
        let err = taken.load().unwrap_err();
        check!(err.kind() == io::ErrorKind::NotFound);
    }
}

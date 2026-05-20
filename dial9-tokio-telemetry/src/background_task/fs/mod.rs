//! Filesystem abstraction for the writer-worker seam.
//!
//! `Fs` covers the full segment lifecycle (create, seal, remove, scan) for
//! two backends, selected at construction time:
//!
//! - `Fs::Disk(DiskFs)`: real filesystem. See [`disk`].
//! - `Fs::Mem(MemFs)`: in-process ring channel. See [`mem`].

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use crate::background_task::payload::Payload;
use crate::background_task::sealed::{MemorySegment, SealedSegment, SegmentRef};
use crate::primitives::sync::Arc;
use crate::primitives::sync::atomic::{AtomicU64, Ordering};

mod disk;
mod mem;

use disk::DiskFs;
use mem::{MemActiveWriter, MemFs};

pub(crate) enum RemoveReason {
    /// Writer-side backpressure shed. Counts toward `dropped_segments`.
    Eviction,
    /// Worker cleanup after terminal pipeline failure.
    Terminal,
}

/// In-flight byte accounting for memory-backed segments. Decrements on drop.
#[derive(Debug)]
pub(crate) struct SegmentAccounting {
    pub(crate) in_flight_bytes: Arc<AtomicU64>,
    pub(crate) in_flight_count: Arc<AtomicU64>,
    pub(crate) size: u64,
}

impl Drop for SegmentAccounting {
    fn drop(&mut self) {
        let prev_bytes = self.in_flight_bytes.fetch_sub(self.size, Ordering::AcqRel);
        debug_assert!(
            prev_bytes >= self.size,
            "in_flight_bytes underflow: prev={prev_bytes} sub={}",
            self.size
        );
        let prev_count = self.in_flight_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            prev_count >= 1,
            "in_flight_count underflow: prev={prev_count}"
        );
    }
}

/// Active-segment write handle.
pub(crate) enum ActiveHandle {
    Disk(std::fs::File),
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

/// A claim returned by `Fs::take_files`. Memory comes with payload in hand,
/// disk loads lazily on `load()` so peak in-flight memory stays at one segment.
pub(crate) struct TakenSegment {
    pub(crate) seg_ref: SegmentRef,
    pre_loaded: Option<(Bytes, SegmentAccounting)>,
}

impl TakenSegment {
    pub(crate) fn disk(seg: SealedSegment) -> Self {
        Self {
            seg_ref: SegmentRef::Disk(seg),
            pre_loaded: None,
        }
    }

    pub(super) fn memory(seg: MemorySegment, bytes: Bytes, accounting: SegmentAccounting) -> Self {
        Self {
            seg_ref: SegmentRef::Memory(seg),
            pre_loaded: Some((bytes, accounting)),
        }
    }

    /// Load the segment payload.
    /// - disk: reads the file (`Err(NotFound)` if it vanished between scan and load).
    /// - memory: zero-copy `Bytes`.
    pub(crate) fn load(self) -> io::Result<(SegmentRef, Payload, Option<SegmentAccounting>)> {
        match self.pre_loaded {
            Some((bytes, accounting)) => {
                Ok((self.seg_ref, Payload::from_bytes(bytes), Some(accounting)))
            }
            None => {
                // None means a disk segment, which always has a path. Error
                // instead of panic: this is off the worker's catch_unwind, so
                // a panic here would take telemetry down with it.
                let Some(path) = self.seg_ref.disk_path() else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "TakenSegment with no payload and no disk path (invariant violation)",
                    ));
                };
                let bytes = std::fs::read(path)?;
                Ok((self.seg_ref, Payload::from_vec(bytes), None))
            }
        }
    }
}

/// Per-cycle snapshot returned by `Fs::take_files`.
pub(crate) struct TakenFiles {
    pub(crate) segments: Vec<TakenSegment>,
    /// Segments still in the memory ring after this cycle's pop. `None` on disk.
    pub(crate) ring_depth: Option<u64>,
    /// Bytes still in the memory ring after this cycle's pop. `None` on disk.
    pub(crate) ring_bytes: Option<u64>,
    pub(crate) in_flight_count: u64,
    pub(crate) in_flight_bytes: u64,
    pub(crate) dropped_segments: u64,
}

/// Unified filesystem abstraction covering the writer↔worker seam.
pub(crate) enum Fs {
    Disk(DiskFs),
    Mem(MemFs),
}

impl Fs {
    /// Create a new active-segment write handle.
    pub(crate) fn create(&self, path: &Path) -> io::Result<ActiveHandle> {
        match self {
            Fs::Disk(d) => d.create_handle(path),
            Fs::Mem(m) => m.create_handle(path),
        }
    }

    pub(crate) fn disk(base_path: &Path) -> Arc<Self> {
        Arc::new(Fs::Disk(DiskFs::from_base_path(base_path)))
    }

    pub(crate) fn memory(max_total_size: u64) -> Arc<Self> {
        Arc::new(Fs::Mem(MemFs::with_capacity(max_total_size)))
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
            Fs::Disk(d) => d.seal_handle(active_handle, active_path, index),
            Fs::Mem(m) => m.seal_handle(active_handle, active_path, index),
        }
    }

    /// Remove a sealed segment.
    ///
    /// Disk: unlinks the file plus any extension-renamed siblings, drops the
    /// claim entry, bumps `dropped_segments` when `reason == Eviction`.
    /// Memory: no-op (bytes already left the ring on pop).
    pub(crate) fn remove_sealed(&self, seg: &SegmentRef, reason: RemoveReason) {
        match self {
            Fs::Disk(d) => d.remove_sealed_inner(seg, reason),
            Fs::Mem(m) => m.remove_sealed_inner(seg, reason),
        }
    }

    /// Discard an active-segment handle without sealing.
    pub(crate) fn remove_active(&self, path: &Path) -> io::Result<()> {
        match self {
            Fs::Disk(d) => d.remove_active_inner(path),
            Fs::Mem(m) => m.remove_active_inner(path),
        }
    }

    /// Return newly-visible sealed segments plus backpressure gauges.
    ///
    /// Each segment is dispensed at most once (claim-set dedup for disk,
    /// pop-once for memory). Memory mode pops at most one segment per call to
    /// bound peak in-flight memory to one segment regardless of backlog.
    pub(crate) fn take_files(&self) -> TakenFiles {
        match self {
            Fs::Disk(d) => d.take_files_inner(),
            Fs::Mem(m) => m.take_files_inner(),
        }
    }

    /// Wait for new segments to potentially appear.
    ///
    /// Disk: sleeps `poll_interval` or until stop fires.
    /// Memory: awaits the ring `Notify` or stop, with lost-wakeup protection.
    pub(crate) async fn wait_for_more(&self, stop: &CancellationToken, poll_interval: Duration) {
        match self {
            Fs::Disk(d) => d.wait_for_more_inner(stop, poll_interval).await,
            Fs::Mem(m) => m.wait_for_more_inner(stop, poll_interval).await,
        }
    }

    /// Returns `true` once `RotatingWriter::finalize` has run.
    pub(crate) fn writer_done(&self) -> bool {
        match self {
            Fs::Disk(d) => d.writer_done_inner(),
            Fs::Mem(m) => m.writer_done_inner(),
        }
    }

    /// Signal that the writer has sealed its final segment. Memory also
    /// pings `Notify` so a parked worker wakes.
    pub(crate) fn mark_writer_done(&self) {
        match self {
            Fs::Disk(d) => d.mark_writer_done_inner(),
            Fs::Mem(m) => m.mark_writer_done_inner(),
        }
    }

    /// Mark a previously dispensed segment as available for re-dispensing on
    /// the next `take_files`.
    ///
    /// Disk: drops the claim entry. Memory: no-op (bytes left the ring on
    /// pop and the segment is lost).
    pub(crate) fn release_claim(&self, seg: &SegmentRef) {
        match self {
            Fs::Disk(d) => d.release_claim(seg.index()),
            Fs::Mem(_) => {}
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
    fn accounting_drop_decrements() {
        let bytes = Arc::new(AtomicU64::new(1000));
        let count = Arc::new(AtomicU64::new(1));
        {
            let _acct = SegmentAccounting {
                in_flight_bytes: Arc::clone(&bytes),
                in_flight_count: Arc::clone(&count),
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

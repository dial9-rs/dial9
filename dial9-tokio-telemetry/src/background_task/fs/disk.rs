//! Disk-backed `Fs` variant.
//!
//! `DiskFs` wraps the real filesystem with a claim-set so the worker
//! dispenses each sealed file at most once per `DiskFs` instance, plus
//! eviction accounting for the writer's byte-budget shedding.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::background_task::sealed::{SealedSegment, SegmentRef, find_sealed_segments};
use crate::primitives::sync::Mutex;
use crate::primitives::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::rate_limit::rate_limited;

use super::{ActiveHandle, RemoveReason, TakenFiles, TakenSegment};

/// Disk-backed filesystem state.
///
/// Claim-set dedup: each sealed file is dispensed to the worker at most once
/// per `DiskFs` instance. `take_files` stats unclaimed files *outside* the
/// claim mutex (to avoid contention with the writer's `remove_sealed`), then
/// re-acquires once to batch-insert all new entries.
pub(crate) struct DiskFs {
    dir: PathBuf,
    stem: String,
    /// Claimed segment index → uncompressed size in bytes.
    claimed: Mutex<HashMap<u32, u64>>,
    dropped: AtomicU64,
    writer_done: AtomicBool,
}

impl DiskFs {
    pub(crate) fn from_base_path(base: &Path) -> Self {
        let dir = base
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf();
        let stem = base
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("trace")
            .to_string();
        Self {
            dir,
            stem,
            claimed: Mutex::new(HashMap::new()),
            dropped: AtomicU64::new(0),
            writer_done: AtomicBool::new(false),
        }
    }

    pub(super) fn create_handle(&self, path: &Path) -> io::Result<ActiveHandle> {
        match std::fs::File::create(path) {
            Ok(f) => Ok(ActiveHandle::Disk(f)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Parent directory missing. Recreate it once and retry. If
                // that still fails, propagate.
                if let Some(parent) = path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::File::create(path).map(ActiveHandle::Disk)
            }
            Err(e) => Err(e),
        }
    }

    pub(super) fn seal_handle(
        &self,
        active_handle: ActiveHandle,
        active_path: &Path,
        index: u32,
    ) -> io::Result<SegmentRef> {
        // File is flushed+closed when the handle is dropped.
        drop(active_handle);
        let sealed_path = strip_active_suffix(active_path);
        match std::fs::rename(active_path, &sealed_path) {
            Ok(()) => Ok(SegmentRef::Disk(SealedSegment {
                path: sealed_path,
                index,
            })),
            Err(e) => Err(e),
        }
    }

    pub(super) fn remove_sealed_inner(&self, seg: &SegmentRef, reason: RemoveReason) {
        let Some(path) = seg.disk_path() else {
            return;
        };
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Scan for extension-renamed siblings (e.g. `.gz` from WriteBack).
                if let Some(file_name) = path.file_name().and_then(|n| n.to_str())
                    && let Some(parent) = path.parent()
                    && let Ok(entries) = std::fs::read_dir(parent)
                {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        if let Some(name_str) = name.to_str()
                            && name_str.starts_with(file_name)
                            && name_str != file_name
                            && let Err(e2) = std::fs::remove_file(entry.path())
                        {
                            rate_limited!(Duration::from_secs(60), {
                                tracing::warn!(
                                    target: "dial9_worker",
                                    error = %e2,
                                    path = %entry.path().display(),
                                    "failed to evict renamed trace segment sibling"
                                );
                            });
                        }
                    }
                }
            }
            Err(e) => {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        target: "dial9_worker",
                        error = %e,
                        path = %path.display(),
                        "failed to remove sealed segment"
                    );
                });
            }
        }
        self.claimed.lock().unwrap().remove(&seg.index());
        if matches!(reason, RemoveReason::Eviction) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn remove_active_inner(&self, path: &Path) -> io::Result<()> {
        // Best-effort: a missing active file is expected (already sealed or
        // never created). Log anything else so silent FS failures (e.g.
        // permission) are observable instead of leaking active files.
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        target: "dial9_worker",
                        error = %e,
                        path = %path.display(),
                        "failed to remove active segment (best-effort)"
                    );
                });
                Ok(())
            }
        }
    }

    /// Reclaim a previously dispensed segment so the next scan re-dispenses it.
    pub(super) fn release_claim(&self, index: u32) {
        self.claimed.lock().unwrap().remove(&index);
    }

    pub(super) fn writer_done_inner(&self) -> bool {
        self.writer_done.load(Ordering::Acquire)
    }

    /// Signal that the writer has sealed its final segment. The disk seal
    /// (`std::fs::rename`) happens-before this `Release` store, so any worker
    /// thread observing `writer_done == true` will see the renamed file on its
    /// next `take_files` scan.
    pub(super) fn mark_writer_done_inner(&self) {
        self.writer_done.store(true, Ordering::Release);
    }

    pub(super) async fn wait_for_more_inner(
        &self,
        stop: &tokio_util::sync::CancellationToken,
        poll_interval: Duration,
    ) {
        tokio::select! {
            _ = stop.cancelled() => {}
            _ = tokio::time::sleep(poll_interval) => {}
        }
    }

    pub(super) fn take_files_inner(&self) -> TakenFiles {
        let on_disk = match find_sealed_segments(&self.dir, &self.stem) {
            Ok(s) => s,
            Err(e) => {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        target: "dial9_worker",
                        error = %e,
                        "failed to scan for sealed segments"
                    );
                });
                return empty_taken_files(self.dropped.load(Ordering::Relaxed));
            }
        };
        let on_disk_indices: HashSet<u32> = on_disk.iter().map(|s| s.index).collect();

        // Stat unclaimed files *outside* the claim mutex.
        let mut new_claims: Vec<(u32, u64)> = Vec::new();
        let mut new_segments: Vec<TakenSegment> = Vec::new();
        {
            let claimed = self.claimed.lock().unwrap();
            for seg in &on_disk {
                if claimed.contains_key(&seg.index) {
                    continue;
                }
                let size = match std::fs::metadata(&seg.path) {
                    Ok(m) => m.len(),
                    Err(e) => {
                        rate_limited!(Duration::from_secs(60), {
                            tracing::warn!(
                                target: "dial9_worker",
                                error = %e,
                                path = %seg.path.display(),
                                "failed to stat sealed segment; recording size 0 \
                                 (in_flight_bytes will undercount this segment)"
                            );
                        });
                        0
                    }
                };
                new_claims.push((seg.index, size));
                new_segments.push(TakenSegment::disk(seg.clone()));
            }
        }

        // Prune claims whose file is gone, add this cycle's claims, snapshot
        // the gauges.
        //
        // Gauges are best-effort: `claimed` is locked twice, so a racing
        // remove_sealed/release_claim shifts the counts. They feed backpressure
        // heuristics only, not correctness.
        let (in_flight_count, in_flight_bytes) = {
            let mut claimed = self.claimed.lock().unwrap();
            claimed.retain(|idx, _| on_disk_indices.contains(idx));
            for (idx, size) in new_claims {
                claimed.insert(idx, size);
            }
            (claimed.len() as u64, claimed.values().sum::<u64>())
        };

        TakenFiles {
            segments: new_segments,
            ring_depth: None,
            ring_bytes: None,
            in_flight_count,
            in_flight_bytes,
            dropped_segments: self.dropped.load(Ordering::Relaxed),
        }
    }
}

fn strip_active_suffix(path: &Path) -> PathBuf {
    let s = path.to_str().unwrap_or_default();
    if let Some(without) = s.strip_suffix(".active") {
        PathBuf::from(without)
    } else {
        path.to_path_buf()
    }
}

fn empty_taken_files(dropped_segments: u64) -> TakenFiles {
    TakenFiles {
        segments: vec![],
        // Used only by DiskFs's early-return on scan failure.
        ring_depth: None,
        ring_bytes: None,
        in_flight_count: 0,
        in_flight_bytes: 0,
        dropped_segments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background_task::fs::Fs;
    use assert2::check;

    #[test]
    fn disk_fs_claim_dedup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"seg0").unwrap();
        std::fs::write(dir.path().join("trace.1.bin"), b"seg1").unwrap();

        let base = dir.path().join("trace.bin");
        let fs = Fs::Disk(DiskFs::from_base_path(&base));

        let t1 = fs.take_files();
        check!(t1.segments.len() == 2);

        // Second scan returns nothing new
        let t2 = fs.take_files();
        check!(t2.segments.is_empty());
    }

    #[test]
    fn disk_fs_scan_prunes_claim_when_file_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.0.bin");
        std::fs::write(&path, b"seg0").unwrap();
        let base = dir.path().join("trace.bin");
        let fs = Fs::Disk(DiskFs::from_base_path(&base));

        let t1 = fs.take_files();
        check!(t1.segments.len() == 1);
        check!(t1.in_flight_count == 1);

        // Last-stage cleanup deletes the file out-of-band.
        std::fs::remove_file(&path).unwrap();

        let t2 = fs.take_files();
        check!(
            t2.segments.is_empty(),
            "vanished file must not be re-dispatched"
        );
        check!(t2.in_flight_count == 0, "stale claim must be pruned");
        check!(t2.in_flight_bytes == 0);
    }

    #[test]
    fn disk_fs_release_claim_redispatches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"seg0").unwrap();
        let base = dir.path().join("trace.bin");
        let disk = DiskFs::from_base_path(&base);

        let t1 = disk.take_files_inner();
        check!(t1.segments.len() == 1);

        let seg = &t1.segments[0].seg_ref;
        disk.release_claim(seg.index());

        let t2 = disk.take_files_inner();
        check!(
            t2.segments.len() == 1,
            "released claim should be re-dispensed"
        );
    }

    #[test]
    fn disk_fs_eviction_bumps_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"data").unwrap();
        let base = dir.path().join("trace.bin");
        let fs = Fs::Disk(DiskFs::from_base_path(&base));

        let t = fs.take_files();
        check!(t.segments.len() == 1);
        let seg = t.segments.into_iter().next().unwrap().seg_ref;

        check!(t.dropped_segments == 0);
        fs.remove_sealed(&seg, RemoveReason::Eviction);
        let t2 = fs.take_files();
        check!(t2.dropped_segments == 1);
    }

    #[test]
    fn disk_fs_terminal_does_not_bump_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"data").unwrap();
        let base = dir.path().join("trace.bin");
        let fs = Fs::Disk(DiskFs::from_base_path(&base));

        let t = fs.take_files();
        let seg = t.segments.into_iter().next().unwrap().seg_ref;
        fs.remove_sealed(&seg, RemoveReason::Terminal);
        let t2 = fs.take_files();
        check!(t2.dropped_segments == 0);
    }

    #[test]
    fn strip_active_suffix_removes_suffix() {
        let p = Path::new("/tmp/trace.0.bin.active");
        check!(strip_active_suffix(p) == PathBuf::from("/tmp/trace.0.bin"));
    }

    #[test]
    fn strip_active_suffix_no_suffix() {
        let p = Path::new("/tmp/trace.0.bin");
        check!(strip_active_suffix(p) == PathBuf::from("/tmp/trace.0.bin"));
    }
}

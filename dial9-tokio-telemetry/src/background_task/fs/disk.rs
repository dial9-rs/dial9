//! Disk-backed `Fs` variant.
//!
//! `DiskFs` wraps the real filesystem with a claim-set so the worker
//! dispenses each sealed file at most once per `DiskFs` instance, plus
//! eviction accounting for the writer's byte-budget shedding.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::background_task::sealed::{
    SealedSegment, SegmentArtifact, SegmentRef, find_sealed_segments, parse_segment_artifact,
};
use crate::primitives::sync::Mutex;
use crate::primitives::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::rate_limit::rate_limited;

use super::{ActiveHandle, DiscoveredArtifacts, RemoveReason, TakenFiles, TakenSegment};

/// Disk-backed filesystem state.
pub(crate) struct DiskFs {
    dir: PathBuf,
    stem: String,
    /// Claimed segment index -> uncompressed size in bytes. Dedup so each
    /// sealed file is dispensed at most once per `DiskFs` instance.
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

    pub(super) fn create_segment(&self, path: &Path) -> io::Result<ActiveHandle> {
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

    pub(super) fn seal(
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

    pub(super) fn remove_sealed(&self, seg: &SegmentRef, reason: RemoveReason) {
        if let Some(path) = seg.disk_path() {
            remove_segment_family(path);
        }
        self.claimed.lock().unwrap().remove(&seg.index());
        if matches!(reason, RemoveReason::Eviction) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn remove_active(&self, path: &Path) -> io::Result<()> {
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

    pub(super) fn writer_done(&self) -> bool {
        self.writer_done.load(Ordering::Acquire)
    }

    /// Signal that the writer has sealed its final segment. The disk seal
    /// (`std::fs::rename`) happens-before this `Release` store, so any worker
    /// thread observing `writer_done == true` will see the renamed file on its
    /// next `take_files` scan.
    pub(super) fn mark_writer_done(&self) {
        self.writer_done.store(true, Ordering::Release);
    }

    pub(super) async fn wait_for_more(
        &self,
        stop: &tokio_util::sync::CancellationToken,
        poll_interval: Duration,
    ) {
        tokio::select! {
            _ = stop.cancelled() => {}
            _ = tokio::time::sleep(poll_interval) => {}
        }
    }

    pub(super) fn take_files(&self) -> TakenFiles {
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
                return empty_taken_files(self.dropped.swap(0, Ordering::AcqRel));
            }
        };
        let on_disk_indices: HashSet<u32> = on_disk.iter().map(|s| s.index).collect();

        // Snapshot the claimed set under a brief lock, then stat candidates
        // outside it: metadata() syscalls must not hold the claim mutex, or
        // they contend with the writer's remove_sealed/release_claim. The
        // worker is the only caller of take_files, so no new claims appear
        // between this snapshot and the insert below.
        let already_claimed: HashSet<u32> = {
            let claimed = self.claimed.lock().unwrap();
            claimed.keys().copied().collect()
        };

        let mut new_claims: Vec<(u32, u64)> = Vec::new();
        let mut new_segments: Vec<TakenSegment> = Vec::new();
        for seg in &on_disk {
            if already_claimed.contains(&seg.index) {
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

        // Prune claims whose file is gone, add this cycle's claims, snapshot
        // the gauges.
        //
        // Gauges are best-effort: `claimed` is locked twice, so a racing
        // remove_sealed/release_claim shifts the counts. They feed backpressure
        // heuristics only, not correctness.
        let (in_flight_segments, in_flight_bytes) = {
            let mut claimed = self.claimed.lock().unwrap();
            claimed.retain(|idx, _| on_disk_indices.contains(idx));
            for (idx, size) in new_claims {
                claimed.insert(idx, size);
            }
            (claimed.len() as u64, claimed.values().sum::<u64>())
        };

        TakenFiles {
            segments: new_segments,
            queued_segments: None,
            queued_bytes: None,
            in_flight_segments,
            in_flight_bytes,
            in_flight_bytes_peak: None,
            segments_dropped: self.dropped.swap(0, Ordering::AcqRel),
        }
    }
}

impl DiskFs {
    /// Scan `self.dir` and seed `DiscoveredArtifacts`.
    /// Sums whole-family sizes (`.bin` + `.bin.gz` + future write-back suffixes) per index
    /// so the eviction budget covers post-processed artifacts and unlinks
    /// stale `.bin.active` orphans from dead writers.
    pub(super) fn discover_existing(&self) -> io::Result<DiscoveredArtifacts> {
        let mut retained_sizes: BTreeMap<u32, u64> = BTreeMap::new();

        if !self.dir.exists() {
            return Ok(DiscoveredArtifacts::default());
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if !metadata.is_file() {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            match parse_segment_artifact(file_name, &self.stem) {
                Some(SegmentArtifact::Retained { index }) => {
                    *retained_sizes.entry(index).or_default() += metadata.len();
                }
                Some(SegmentArtifact::Active) => {
                    tracing::warn!(
                        target: "dial9_worker",
                        path = %path.display(),
                        "discarding stale active trace segment from a previous writer"
                    );
                    match std::fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e),
                    }
                }
                None => {}
            }
        }

        let next_active_index = match retained_sizes.last_key_value() {
            Some((&idx, _)) => idx
                .checked_add(1)
                .ok_or_else(|| io::Error::other("trace segment index overflow"))?,
            None => 0,
        };
        let closed_files = retained_sizes
            .into_iter()
            .map(|(index, size)| {
                let path = self.dir.join(format!("{}.{}.bin", self.stem, index));
                (SegmentRef::Disk(SealedSegment { path, index }), size)
            })
            .collect();

        Ok(DiscoveredArtifacts {
            closed_files,
            next_active_index,
        })
    }
}

/// Unlink `path` plus any sibling whose name extends `{file_name}.`
/// (e.g. `.gz`).
fn remove_segment_family(path: &Path) {
    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    let entries = match std::fs::read_dir(parent) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(e) => {
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!(
                    target: "dial9_worker",
                    error = %e,
                    parent = %parent.display(),
                    "failed to scan parent for trace family eviction"
                );
            });
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let is_family = name_str == file_name
            || name_str
                .strip_prefix(file_name)
                .is_some_and(|s| s.starts_with('.'));
        if !is_family {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        target: "dial9_worker",
                        error = %e,
                        path = %entry.path().display(),
                        "failed to remove trace artifact"
                    );
                });
            }
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

fn empty_taken_files(segments_dropped: u64) -> TakenFiles {
    TakenFiles {
        segments: vec![],
        // Used only by DiskFs's early-return on scan failure.
        queued_segments: None,
        queued_bytes: None,
        in_flight_segments: 0,
        in_flight_bytes: 0,
        in_flight_bytes_peak: None,
        segments_dropped,
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
        check!(t1.in_flight_segments == 1);

        // Last-stage cleanup deletes the file out-of-band.
        std::fs::remove_file(&path).unwrap();

        let t2 = fs.take_files();
        check!(
            t2.segments.is_empty(),
            "vanished file must not be re-dispatched"
        );
        check!(t2.in_flight_segments == 0, "stale claim must be pruned");
        check!(t2.in_flight_bytes == 0);
    }

    #[test]
    fn disk_fs_release_claim_redispatches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"seg0").unwrap();
        let base = dir.path().join("trace.bin");
        let disk = DiskFs::from_base_path(&base);

        let t1 = disk.take_files();
        check!(t1.segments.len() == 1);

        let seg = &t1.segments[0].seg_ref;
        disk.release_claim(seg.index());

        let t2 = disk.take_files();
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

        check!(t.segments_dropped == 0);
        fs.remove_sealed(&seg, RemoveReason::Eviction);
        let t2 = fs.take_files();
        check!(t2.segments_dropped == 1);
        let t3 = fs.take_files();
        check!(t3.segments_dropped == 0);
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
        check!(t2.segments_dropped == 0);
    }

    #[test]
    fn discover_existing_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("trace.bin");
        let disk = DiskFs::from_base_path(&base);
        let d = disk.discover_existing().unwrap();
        check!(d.next_active_index == 0);
        check!(d.closed_files.is_empty());
    }

    #[test]
    fn discover_existing_sums_artifact_family_per_index() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.path().join("trace.0.bin.gz"), vec![0u8; 30]).unwrap();
        std::fs::write(dir.path().join("trace.2.bin"), vec![0u8; 50]).unwrap();
        let base = dir.path().join("trace.bin");
        let disk = DiskFs::from_base_path(&base);
        let d = disk.discover_existing().unwrap();
        check!(d.next_active_index == 3, "max(0,2)+1 = 3");
        let by_index: std::collections::HashMap<u32, u64> = d
            .closed_files
            .iter()
            .map(|(seg, size)| (seg.index(), *size))
            .collect();
        check!(by_index.get(&0) == Some(&130), ".bin + .bin.gz summed");
        check!(by_index.get(&2) == Some(&50));
    }

    #[test]
    fn discover_existing_discards_stale_active() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("trace.7.bin.active");
        std::fs::write(&stale, b"orphan").unwrap();
        let base = dir.path().join("trace.bin");
        let disk = DiskFs::from_base_path(&base);
        let _ = disk.discover_existing().unwrap();
        check!(!stale.exists(), "stale .active must be discarded");
    }

    #[test]
    fn discover_existing_ignores_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("other.0.bin"), b"x").unwrap();
        std::fs::write(dir.path().join("README"), b"x").unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"x").unwrap();
        let base = dir.path().join("trace.bin");
        let disk = DiskFs::from_base_path(&base);
        let d = disk.discover_existing().unwrap();
        check!(d.closed_files.len() == 1);
        check!(d.next_active_index == 1);
    }

    #[test]
    fn remove_segment_family_removes_bin_and_gz_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("trace.3.bin");
        let gz = dir.path().join("trace.3.bin.gz");
        let unrelated = dir.path().join("trace.4.bin");
        std::fs::write(&bin, b"x").unwrap();
        std::fs::write(&gz, b"x").unwrap();
        std::fs::write(&unrelated, b"x").unwrap();
        remove_segment_family(&bin);
        check!(!bin.exists());
        check!(!gz.exists());
        check!(unrelated.exists(), "sibling with different index untouched");
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

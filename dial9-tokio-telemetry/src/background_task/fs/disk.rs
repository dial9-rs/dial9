//! Disk-backed `Fs` variant.
//!
//! `DiskFs` is the **single owner of disk retention** (ADR-0003). It owns the
//! trace directory, an exact in-memory byte model per *segment family* (the
//! `.bin` plus any derived artifact such as `.bin.gz`), and all eviction. The
//! writer only notifies it on seal; write-back is routed through it; a
//! per-cycle directory scan folded into [`take_files`](DiskFs::take_files)
//! reconciles the model against reality.
//!
//! It also keeps a claim-set so the worker dispenses each sealed file at most
//! once per `DiskFs` instance.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::background_task::sealed::{
    SealedSegment, SegmentArtifact, SegmentRef, find_sealed_segments, parse_segment_artifact,
};
use crate::primitives::fs;
use crate::primitives::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::primitives::sync::{Arc, Mutex};
use crate::rate_limit::rate_limited;

use super::{ActiveHandle, DiscoveredArtifacts, RemoveReason, TakenFiles, TakenSegment};

/// True when `e` is a real `ENOSPC` (out-of-space) error, as opposed to
/// graceful over-budget eviction.
pub(crate) fn is_enospc(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ENOSPC)
}

/// Disk-backed filesystem state.
pub(crate) struct DiskFs {
    dir: PathBuf,
    stem: String,
    /// Retention budget. No on-disk family total may exceed this; eviction
    /// reserves `max_file_size` headroom for the active segment.
    max_total_size: u64,
    /// Reserved headroom for the (writer-owned, `DiskFs`-invisible) active
    /// segment. Eviction targets `max_total_size - max_file_size`.
    max_file_size: u64,
    /// Claimed segment index -> uncompressed size in bytes. Dedup so each
    /// sealed file is dispensed at most once per `DiskFs` instance. Locked
    /// independently of (never nested with) `family_sizes`.
    claimed: Mutex<HashMap<u32, u64>>,
    /// Exact bytes per segment family, keyed by index (ascending = oldest
    /// first). The fast path between scans; reconciled to the directory each
    /// `take_files`. Locked independently of (never nested with) `claimed`.
    family_sizes: Mutex<BTreeMap<u32, u64>>,
    /// Segments evicted since the last `take_files` swap (per-cycle delta).
    dropped: AtomicU64,
    /// Bytes evicted since the last `take_files` swap (per-cycle delta).
    bytes_evicted: AtomicU64,
    /// Last reconciled total bytes / family count, and the |model - scan|
    /// drift, surfaced as gauges by `take_files`.
    retained_bytes: AtomicU64,
    retained_segments: AtomicU64,
    reconcile_drift_bytes: AtomicU64,
    /// Set once when a real `ENOSPC` wipes our footprint and disables
    /// telemetry. Observed by the flush loop, which flips `shared.enabled`.
    disk_full: Arc<AtomicBool>,
    writer_done: AtomicBool,
}

impl DiskFs {
    pub(crate) fn new(base: &Path, max_total_size: u64, max_file_size: u64) -> Self {
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
            max_total_size,
            max_file_size,
            claimed: Mutex::new(HashMap::new()),
            family_sizes: Mutex::new(BTreeMap::new()),
            dropped: AtomicU64::new(0),
            bytes_evicted: AtomicU64::new(0),
            retained_bytes: AtomicU64::new(0),
            retained_segments: AtomicU64::new(0),
            reconcile_drift_bytes: AtomicU64::new(0),
            disk_full: Arc::new(AtomicBool::new(false)),
            writer_done: AtomicBool::new(false),
        }
    }

    pub(super) fn create_segment(&self, path: &Path) -> io::Result<ActiveHandle> {
        match fs::File::create(path) {
            Ok(f) => Ok(ActiveHandle::Disk(f)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Parent directory missing. Recreate it once and retry. If
                // that still fails, propagate.
                if let Some(parent) = path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    fs::create_dir_all(parent)?;
                }
                fs::File::create(path)
                    .inspect_err(|e| self.note_io_error(e))
                    .map(ActiveHandle::Disk)
            }
            Err(e) => {
                self.note_io_error(&e);
                Err(e)
            }
        }
    }

    /// Seal `active_handle` as segment `index`, recording its on-disk size in
    /// the family model.
    pub(super) fn seal(
        &self,
        active_handle: ActiveHandle,
        active_path: &Path,
        index: u32,
        sealed_size: u64,
    ) -> io::Result<SegmentRef> {
        // File is flushed+closed when the handle is dropped.
        drop(active_handle);
        let sealed_path = strip_active_suffix(active_path);
        match fs::rename(active_path, &sealed_path) {
            Ok(()) => {
                self.family_sizes.lock().unwrap().insert(index, sealed_size);
                Ok(SegmentRef::Disk(SealedSegment {
                    path: sealed_path,
                    index,
                }))
            }
            Err(e) => {
                self.note_io_error(&e);
                Err(e)
            }
        }
    }

    pub(super) fn remove_sealed(&self, seg: &SegmentRef, reason: RemoveReason) {
        let size = self.family_sizes.lock().unwrap().remove(&seg.index());
        if let Some(path) = seg.disk_path() {
            remove_segment_family(path);
        }
        self.claimed.lock().unwrap().remove(&seg.index());
        if matches!(reason, RemoveReason::Eviction) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.bytes_evicted
                .fetch_add(size.unwrap_or(0), Ordering::Relaxed);
        }
    }

    pub(super) fn remove_active(&self, path: &Path) -> io::Result<()> {
        // Best-effort: a missing active file is expected (already sealed or
        // never created). Log anything else so silent FS failures (e.g.
        // permission) are observable instead of leaking active files.
        match fs::remove_file(path) {
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

    /// Evict sealed families oldest-first until the on-disk total is within
    /// `max_total_size - max_file_size`, reserving one `max_file_size` slice
    /// for the active segment (which `DiskFs` cannot observe growing). This is
    /// the single eviction primitive both ADR-0003 triggers call — the worker
    /// run loop every cycle, and the writer thread immediately after each seal.
    /// Independent of writer state, so reclamation outlives `Finished`.
    ///
    /// May drop a segment currently being processed: the worker `load()`s bytes
    /// into memory before processing and tolerates a vanished file, so dropping
    /// the claim here is safe.
    pub(super) fn evict_to_budget(&self) {
        // `u64::MAX` is the unbounded sentinel (`single_file`): never evict.
        if self.max_total_size == u64::MAX {
            return;
        }
        let target = self.eviction_target();
        loop {
            // Pick the oldest victim under the model lock; do not hold it across
            // the unlink syscall.
            let victim = {
                let mut model = self.family_sizes.lock().unwrap();
                let total: u64 = model.values().sum();
                if total <= target {
                    break;
                }
                match model.iter().next().map(|(&idx, &sz)| (idx, sz)) {
                    Some((idx, sz)) => {
                        model.remove(&idx);
                        (idx, sz)
                    }
                    None => break,
                }
            };
            let (idx, size) = victim;
            let path = self.dir.join(format!("{}.{}.bin", self.stem, idx));
            remove_segment_family(&path);
            // Drop the claim so a since-evicted index is not re-dispensed for a
            // now-deleted file (the next scan would prune it anyway).
            self.claimed.lock().unwrap().remove(&idx);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.bytes_evicted.fetch_add(size, Ordering::Relaxed);
        }
    }

    /// Sealed-family budget: `max_total_size` minus headroom reserved for the
    /// active segment (which `DiskFs` cannot observe). ADR-0003 reserves one
    /// `max_file_size`; under the production default (`max_file_size ≤
    /// max_total_size / 4`) that leaves ~3/4 for sealed data. We additionally
    /// clamp the reserve to at most half the budget so a degenerate config
    /// (`max_file_size ≥ max_total_size`, e.g. size-rotation disabled with
    /// `u64::MAX`) does not zero out retention entirely.
    fn eviction_target(&self) -> u64 {
        let reserve = self.max_file_size.min(self.max_total_size / 2);
        self.max_total_size - reserve
    }

    /// Whether a real `ENOSPC` has wiped our footprint and disabled telemetry.
    pub(super) fn disk_full(&self) -> bool {
        self.disk_full.load(Ordering::Acquire)
    }

    /// Forward a real `ENOSPC` to [`on_enospc`](Self::on_enospc); ignore every
    /// other error (graceful over-budget eviction handles the rest).
    fn note_io_error(&self, e: &io::Error) {
        if is_enospc(e) {
            self.on_enospc();
        }
    }

    /// Terminal disk-full handler (ADR-0003 §6). Wipes **all** of dial9's own
    /// on-disk segment families (`{stem}.{index}.bin*`, nothing else on the
    /// volume) and latches the `disk_full` flag so the flush loop disables
    /// telemetry. Idempotent and logged once at `error!`.
    pub(super) fn on_enospc(&self) {
        if self.disk_full.swap(true, Ordering::AcqRel) {
            return; // already handled by the other thread
        }
        tracing::error!(
            target: "dial9_worker",
            dir = %self.dir.display(),
            "ENOSPC: wiping all dial9 trace segments and disabling telemetry for the process lifetime"
        );
        // Unlink every owned artifact. No model/claim lock held across syscalls.
        match fs::read_dir(&self.dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let Some(name_str) = name.to_str() else {
                        continue;
                    };
                    if parse_segment_artifact(name_str, &self.stem).is_some() {
                        match fs::remove_file(&entry.path()) {
                            Ok(()) => {}
                            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                            Err(e) => {
                                rate_limited!(Duration::from_secs(60), {
                                    tracing::warn!(
                                        target: "dial9_worker",
                                        error = %e,
                                        path = %entry.path().display(),
                                        "failed to remove trace artifact during ENOSPC wipe"
                                    );
                                });
                            }
                        }
                    }
                }
            }
            Err(e) => {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        target: "dial9_worker",
                        error = %e,
                        "failed to scan trace dir during ENOSPC wipe"
                    );
                });
            }
        }
        self.family_sizes.lock().unwrap().clear();
        self.claimed.lock().unwrap().clear();
    }

    /// Write `bytes` for segment `index` to `dest`, the single chokepoint for
    /// write-back (ADR-0003 §2). When `dest != original` (a derived artifact
    /// such as `.bin.gz`), the original `.bin` is removed once the new artifact
    /// is durable. Updates the family model so accounting reflects compression.
    ///
    /// Runs the blocking I/O on a `spawn_blocking` thread. A real `ENOSPC`
    /// triggers the terminal wipe before the error propagates.
    pub(super) async fn write_back(
        &self,
        index: u32,
        original: &Path,
        dest: &Path,
        bytes: crate::background_task::payload::Payload,
    ) -> io::Result<()> {
        let write_dest = dest.to_path_buf();
        let result = tokio::task::spawn_blocking(move || {
            use std::io::{BufWriter, Write};
            let mut f = BufWriter::new(std::fs::File::create(&write_dest)?);
            for chunk in bytes.chunks() {
                f.write_all(chunk)?;
            }
            f.flush()?;
            // Size of what we just wrote, for the family model.
            Ok::<u64, io::Error>(bytes.len() as u64)
        })
        .await;
        let dest_size = match result {
            Ok(Ok(size)) => size,
            Ok(Err(e)) => {
                self.note_io_error(&e);
                return Err(e);
            }
            Err(e) => return Err(io::Error::other(e)),
        };

        if dest != original {
            // Remove the original .bin now that the derived artifact exists.
            // If the writer already evicted it, clean up the dest we just wrote
            // so it does not leak on disk.
            match fs::remove_file(original) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let _ = fs::remove_file(dest);
                    self.family_sizes.lock().unwrap().remove(&index);
                    return Ok(());
                }
                Err(e) => {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(
                            target: "dial9_worker",
                            error = %e,
                            "failed to remove original segment after write-back"
                        );
                    });
                }
            }
        }
        // Family is now just the derived artifact (or the rewritten original).
        self.family_sizes.lock().unwrap().insert(index, dest_size);
        Ok(())
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
        // Reconcile the family-size model against the directory (ADR-0003 §2):
        // the scan is the authority, the model a fast cache it corrects.
        // `scan_family_sizes` does its own `read_dir`; tolerate failure by
        // skipping the reconcile (the model stays as-is until next cycle).
        if let Some(scan) = self.scan_family_sizes() {
            let scan_total: u64 = scan.values().sum();
            let scan_segments = scan.len() as u64;
            let drift = {
                let mut model = self.family_sizes.lock().unwrap();
                let model_total: u64 = model.values().sum();
                *model = scan;
                model_total.abs_diff(scan_total)
            };
            self.retained_bytes.store(scan_total, Ordering::Relaxed);
            self.retained_segments
                .store(scan_segments, Ordering::Relaxed);
            self.reconcile_drift_bytes.store(drift, Ordering::Relaxed);
        }

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
                return self.empty_taken_files();
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
            let size = match fs::metadata(&seg.path) {
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
            bytes_evicted: self.bytes_evicted.swap(0, Ordering::AcqRel),
            retained_bytes: Some(self.retained_bytes.load(Ordering::Relaxed)),
            retained_segments: Some(self.retained_segments.load(Ordering::Relaxed)),
            retention_budget_bytes: Some(self.max_total_size),
            reconcile_drift_bytes: Some(self.reconcile_drift_bytes.load(Ordering::Relaxed)),
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
        for entry in fs::read_dir(&self.dir)? {
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
                    match fs::remove_file(&path) {
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

        // Seed the family model so eviction immediately accounts for artifacts
        // left by prior writer lifetimes (crash-restart loops otherwise grow
        // the directory unbounded). The next `take_files` reconcile keeps it
        // honest.
        let retained_total: u64 = retained_sizes.values().sum();
        let retained_count = retained_sizes.len() as u64;
        *self.family_sizes.lock().unwrap() = retained_sizes;
        self.retained_bytes.store(retained_total, Ordering::Relaxed);
        self.retained_segments
            .store(retained_count, Ordering::Relaxed);

        Ok(DiscoveredArtifacts { next_active_index })
    }

    /// Sum whole-family sizes (`.bin` + `.bin.gz` + future write-back suffixes)
    /// per index. The authority `take_files` reconciles the in-memory model
    /// against. Returns `None` (skip the reconcile) if the directory cannot be
    /// scanned; does **not** mutate the directory (unlike `discover_existing`).
    fn scan_family_sizes(&self) -> Option<BTreeMap<u32, u64>> {
        let mut sizes: BTreeMap<u32, u64> = BTreeMap::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(_) => return None,
        };
        for entry in entries.flatten() {
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !metadata.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if let Some(SegmentArtifact::Retained { index }) =
                parse_segment_artifact(name_str, &self.stem)
            {
                *sizes.entry(index).or_default() += metadata.len();
            }
        }
        Some(sizes)
    }

    fn empty_taken_files(&self) -> TakenFiles {
        TakenFiles {
            segments: vec![],
            queued_segments: None,
            queued_bytes: None,
            in_flight_segments: 0,
            in_flight_bytes: 0,
            in_flight_bytes_peak: None,
            segments_dropped: self.dropped.swap(0, Ordering::AcqRel),
            bytes_evicted: self.bytes_evicted.swap(0, Ordering::AcqRel),
            retained_bytes: Some(self.retained_bytes.load(Ordering::Relaxed)),
            retained_segments: Some(self.retained_segments.load(Ordering::Relaxed)),
            retention_budget_bytes: Some(self.max_total_size),
            reconcile_drift_bytes: Some(self.reconcile_drift_bytes.load(Ordering::Relaxed)),
        }
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
    let entries = match fs::read_dir(parent) {
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
        match fs::remove_file(&entry.path()) {
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

#[cfg(test)]
impl DiskFs {
    /// Test constructor with an effectively unbounded budget (no eviction),
    /// for tests that exercise claim/scan behavior rather than retention.
    fn from_base_path(base: &Path) -> Self {
        Self::new(base, u64::MAX, u64::MAX)
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
        // "data" is 4 bytes; the family model (seeded by the first scan) reports
        // it as evicted.
        check!(t2.bytes_evicted == 4);
        let t3 = fs.take_files();
        check!(t3.segments_dropped == 0);
        check!(t3.bytes_evicted == 0);
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
        check!(disk.family_sizes.lock().unwrap().is_empty());
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
        let model = disk.family_sizes.lock().unwrap();
        check!(model.get(&0) == Some(&130), ".bin + .bin.gz summed");
        check!(model.get(&2) == Some(&50));
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
        check!(disk.family_sizes.lock().unwrap().len() == 1);
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

    // ── ADR-0003: DiskFs-owned retention ────────────────────────────────────

    fn write_seg(dir: &Path, index: u32, bytes: usize) {
        std::fs::write(dir.join(format!("trace.{index}.bin")), vec![0u8; bytes]).unwrap();
    }

    /// `evict_to_budget` drops oldest-first until the total is within
    /// `max_total_size - max_file_size` (the reserved active headroom).
    #[test]
    fn evict_to_budget_drops_oldest_to_reserved_target() {
        let dir = tempfile::tempdir().unwrap();
        // Four 100-byte families = 400 bytes on disk.
        for i in 0..4u32 {
            write_seg(dir.path(), i, 100);
        }
        let base = dir.path().join("trace.bin");
        // Budget 350, reserve 100 → target 250 → must keep ≤ 2 families.
        let disk = DiskFs::new(&base, 350, 100);
        // Seed the model from disk.
        let _ = disk.discover_existing().unwrap();

        disk.evict_to_budget();

        let remaining: Vec<u32> = (0..4)
            .filter(|i| dir.path().join(format!("trace.{i}.bin")).exists())
            .collect();
        // Oldest (0,1) evicted; newest (2,3) kept — 200 ≤ target 250.
        check!(remaining == vec![2, 3]);
        let model = disk.family_sizes.lock().unwrap();
        check!(model.values().sum::<u64>() <= 250);
    }

    /// A `u64::MAX` budget (e.g. `single_file`) never evicts.
    #[test]
    fn evict_to_budget_noop_under_unbounded_budget() {
        let dir = tempfile::tempdir().unwrap();
        write_seg(dir.path(), 0, 1_000_000);
        let base = dir.path().join("trace.bin");
        let disk = DiskFs::new(&base, u64::MAX, u64::MAX);
        let _ = disk.discover_existing().unwrap();
        disk.evict_to_budget();
        check!(dir.path().join("trace.0.bin").exists());
    }

    /// Eviction may drop a segment the worker has already claimed; the claim is
    /// removed too so it is not re-dispensed for a now-deleted file.
    #[test]
    fn evict_to_budget_drops_claimed_segment_and_claim() {
        let dir = tempfile::tempdir().unwrap();
        write_seg(dir.path(), 0, 100);
        let base = dir.path().join("trace.bin");
        let disk = DiskFs::new(&base, 100, 100); // target 0 → evict everything
        let t = disk.take_files();
        check!(t.segments.len() == 1, "segment 0 claimed");

        disk.evict_to_budget();
        check!(
            !dir.path().join("trace.0.bin").exists(),
            "claimed seg evicted"
        );
        check!(disk.claimed.lock().unwrap().is_empty(), "claim dropped");
    }

    /// The per-cycle scan reconciles the model when files change outside
    /// `DiskFs`, surfacing the drift and then re-converging.
    #[test]
    fn take_files_reconcile_surfaces_drift_then_reconverges() {
        let dir = tempfile::tempdir().unwrap();
        write_seg(dir.path(), 0, 100);
        let base = dir.path().join("trace.bin");
        let disk = DiskFs::new(&base, u64::MAX, u64::MAX);
        let _ = disk.discover_existing().unwrap(); // model: {0 -> 100}

        // External mutation: a .gz sibling appears (writer-back bypass / restart).
        std::fs::write(dir.path().join("trace.0.bin.gz"), vec![0u8; 40]).unwrap();
        let t1 = disk.take_files();
        check!(
            t1.reconcile_drift_bytes == Some(40),
            "model 100 vs scan 140"
        );
        check!(t1.retained_bytes == Some(140));

        // Next cycle: model now matches the directory, drift back to zero.
        let t2 = disk.take_files();
        check!(t2.reconcile_drift_bytes == Some(0));
        check!(t2.retained_bytes == Some(140));
    }

    /// Write-back replaces a `.bin` with a smaller `.bin.gz`; the model reflects
    /// the shrink (subtract original, add derived).
    #[tokio::test]
    async fn write_back_updates_family_model_on_shrink() {
        use crate::background_task::payload::Payload;
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("trace.0.bin");
        std::fs::write(&original, vec![0u8; 1000]).unwrap();
        let base = dir.path().join("trace.bin");
        let disk = DiskFs::new(&base, u64::MAX, u64::MAX);
        let _ = disk.discover_existing().unwrap(); // model: {0 -> 1000}

        let dest = dir.path().join("trace.0.bin.gz");
        disk.write_back(0, &original, &dest, Payload::from_vec(vec![0u8; 120]))
            .await
            .unwrap();

        check!(!original.exists(), "original .bin removed");
        check!(dest.exists());
        check!(disk.family_sizes.lock().unwrap().get(&0) == Some(&120));
    }

    /// A real `ENOSPC` wipes every `{stem}.{index}.bin*`, latches `disk_full`,
    /// clears the model + claims, and leaves unrelated files untouched.
    #[test]
    fn on_enospc_wipes_footprint_and_latches() {
        let dir = tempfile::tempdir().unwrap();
        write_seg(dir.path(), 0, 100);
        std::fs::write(dir.path().join("trace.1.bin.gz"), b"gz").unwrap();
        std::fs::write(dir.path().join("trace.2.bin.active"), b"active").unwrap();
        let unrelated = dir.path().join("keep.txt");
        std::fs::write(&unrelated, b"keep").unwrap();
        let base = dir.path().join("trace.bin");
        let disk = DiskFs::new(&base, u64::MAX, u64::MAX);
        let _ = disk.take_files(); // seed model + a claim on seg 0

        check!(!disk.disk_full());
        disk.on_enospc();

        check!(disk.disk_full(), "flag latched");
        check!(!dir.path().join("trace.0.bin").exists());
        check!(!dir.path().join("trace.1.bin.gz").exists());
        check!(!dir.path().join("trace.2.bin.active").exists());
        check!(unrelated.exists(), "unrelated file untouched");
        check!(disk.family_sizes.lock().unwrap().is_empty());
        check!(disk.claimed.lock().unwrap().is_empty());

        // Idempotent: a second call is a no-op and does not panic.
        disk.on_enospc();
        check!(disk.disk_full());
    }

    /// `create_segment`/`seal` translate a real `ENOSPC` into the terminal wipe.
    #[test]
    fn create_segment_enospc_triggers_wipe() {
        let dir = tempfile::tempdir().unwrap();
        write_seg(dir.path(), 0, 100);
        let base = dir.path().join("trace.bin");
        let disk = DiskFs::new(&base, u64::MAX, u64::MAX);
        let _ = disk.discover_existing().unwrap();

        let _fault = crate::primitives::fs::arm_enospc();
        let result = disk.create_segment(&dir.path().join("trace.1.bin.active"));
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected ENOSPC error from create_segment"),
        };
        check!(is_enospc(&err));
        drop(_fault);

        check!(disk.disk_full(), "ENOSPC on create wiped + latched");
        check!(!dir.path().join("trace.0.bin").exists());
    }
}

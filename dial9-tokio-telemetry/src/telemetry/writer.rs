use dial9_trace_format::encoder::{Encoder, RawEncoder};

use crate::background_task::fs::{ActiveHandle, Fs, RemoveReason};
use crate::background_task::sealed::SegmentRef;
use crate::primitives::fs;
use crate::rate_limit::rate_limited;
use crate::telemetry::collector::Batch;
use crate::telemetry::events::clock_pair;
use crate::telemetry::format::{ClockSyncEvent, SegmentMetadataEvent};
use std::collections::VecDeque;
use std::io::BufWriter;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use metrique_timesource::time_source;

mod mode_sealed {
    pub trait Sealed {}
}

/// Marker trait for `SegmentWriter`'s backend mode. Sealed: only [`Disk`]
/// and [`Memory`] implement it.
pub trait WriterMode: mode_sealed::Sealed + Send + 'static {
    /// Whether the writer mode is disk-backed.
    const IS_DISK: bool;
}

/// Disk-backed mode (default).
#[derive(Debug)]
#[non_exhaustive]
pub struct Disk;
/// In-memory mode.
#[derive(Debug)]
#[non_exhaustive]
pub struct Memory;

impl mode_sealed::Sealed for Disk {}
impl mode_sealed::Sealed for Memory {}
impl WriterMode for Disk {
    const IS_DISK: bool = true;
}
impl WriterMode for Memory {
    const IS_DISK: bool = false;
}

/// Alias for the disk-backed writer (the default mode).
pub type DiskWriter = SegmentWriter<Disk>;
/// Alias for the in-memory writer.
pub type InMemoryWriter = SegmentWriter<Memory>;

/// Segment-metadata key carrying the crates.io version of
/// `dial9-tokio-telemetry`. Populated by default, any user-supplied entry with
/// this key take precedence.
const DIAL9_VERSION_KEY: &str = "dial9.dial9-tokio-telemetry.version";

/// Compile-time value for `DIAL9_VERSION_KEY`.
const DIAL9_VERSION_VALUE: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone)]
struct SegmentMetadata {
    entries: Vec<(String, String)>,
}

impl Default for SegmentMetadata {
    fn default() -> Self {
        Self {
            entries: vec![(
                DIAL9_VERSION_KEY.to_string(),
                DIAL9_VERSION_VALUE.to_string(),
            )],
        }
    }
}

impl SegmentMetadata {
    /// Build segment metadata from user-supplied entries on top of the default
    /// `dial9.dial9-tokio-telemetry.version` key. User entries with the same key override the default.
    fn new(user_entries: Vec<(String, String)>) -> Self {
        let mut s = Self::default();
        s.merge(user_entries.into_iter());
        s
    }

    /// Merge incoming entries with existing ones. Incoming entries take priority
    /// on key conflict; existing entries with keys not in the incoming set are preserved.
    /// Returns `true` if the resulting entries differ from the previous state.
    fn merge(&mut self, entries: impl Iterator<Item = (String, String)>) -> bool {
        let mut merged: Vec<(String, String)> = entries.collect();
        for (k, v) in &self.entries {
            if !merged.iter().any(|(mk, _)| mk == k) {
                merged.push((k.clone(), v.clone()));
            }
        }
        if merged == self.entries {
            return false;
        }
        self.entries = merged;
        true
    }
}

/// Default rotation period: 1 minute.
const DEFAULT_ROTATION_PERIOD: Duration = Duration::from_secs(60);

/// Default maximum interval between thread-local buffer drains.
const DEFAULT_DRAIN_INTERVAL: Duration = Duration::from_secs(30);

const BYTES_PER_MIB: u64 = 1024 * 1024;

/// Hard cap on the builder-derived per-file size, regardless of the total
/// disk budget. Time-based rotation should fire first under normal load;
/// this cap keeps individual segments small enough to remain manageable.
const MAX_FILE_SIZE_CAP: u64 = 100 * BYTES_PER_MIB;

/// Default per-file rotation threshold derived from the total disk budget.
/// Picks a quarter of the budget so a single segment never dominates
/// retention, capped at 100 MiB.
fn derive_max_file_size(max_total_size: u64) -> u64 {
    (max_total_size / 4).min(MAX_FILE_SIZE_CAP)
}

/// A writer that rotates trace segments to bound resource usage and time.
/// Generic over backend: use [`DiskWriter`] (files) or [`InMemoryWriter`].
///
/// Rotation triggers when *either* condition is met:
/// - `max_file_size`: the active segment exceeds this many bytes
/// - `rotation_period`: this much monotonic time has elapsed since the writer
///   (or the previous rotation) started (default: 1 minute)
///
/// **Prefer time-based rotation.** Time-based rotation is coordinated with the
/// flush loop: thread-local buffers are drained before the segment is sealed,
/// so each segment contains events from a clean, non-overlapping time window.
/// Size-based rotation fires immediately when the threshold is crossed and does
/// not drain thread-local buffers, so segments may contain events that overlap
/// in time. Set `max_file_size` large enough that time-based rotation fires
/// first under normal conditions (e.g. 100 MB or more). Size-based rotation
/// then acts as a safety valve for unexpected data bursts. When using
/// [`DiskWriter::builder`] without specifying `max_file_size`, it
/// defaults to `min(100 MiB, max_total_size / 4)` on disk.
///
/// `max_total_size` is the retention budget across closed segments. The
/// oldest segments are dropped once the total exceeds this budget.
///
/// Disk segments are named `{base_path}.0.bin`, `{base_path}.1.bin`, etc.,
/// each a self-contained trace with its own header.
pub struct SegmentWriter<Mode: WriterMode = Disk> {
    base_path: PathBuf,
    max_file_size: u64,
    max_total_size: u64,
    /// How often to rotate based on monotonic time. `Duration::MAX` disables
    /// time-based rotation (used by `single_file()`).
    rotation_period: Duration,
    /// The next monotonic instant at which time-based rotation should fire,
    /// or `None` if time-based rotation is disabled.
    next_rotation_time: Option<Instant>,
    /// Tracks (seg_ref, size) of closed segments oldest-first for disk eviction.
    /// Always empty in memory mode (eviction handled by the memory backend).
    closed_files: VecDeque<(SegmentRef, u64)>,
    /// Path of the currently active (being-written) segment.
    /// Used as a HashMap key in memory mode; a real path in disk mode.
    active_path: PathBuf,
    state: WriterState,
    next_index: u32,
    /// Metadata written at the start of each segment. Updated by the flush
    /// thread to include runtime names alongside any user-provided entries.
    segment_metadata: SegmentMetadata,
    /// Events silently dropped because the writer was finished/stopped.
    dropped_events: usize,
    /// Whether any real (non-metadata) events have been written to the current segment.
    /// Reset on rotation; used by `finalize()` to avoid sealing empty segments.
    has_real_events: bool,
    /// How often the flush loop should drain thread-local buffers, independent
    /// of rotation. Defaults to `min(rotation_period, 30s)`.
    drain_interval: Duration,
    /// Next monotonic instant at which `should_drain()` returns true.
    next_drain_time: Instant,
    /// Unified filesystem/channel abstraction.
    fs: Arc<Fs>,
    boot_id: Option<String>,
    _namespace_lock: Option<std::fs::File>,
    _mode: PhantomData<Mode>,
}

impl<M: WriterMode> std::fmt::Debug for SegmentWriter<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentWriter")
            .field("base_path", &self.base_path)
            .field("max_file_size", &self.max_file_size)
            .field("max_total_size", &self.max_total_size)
            .finish_non_exhaustive()
    }
}

// the write side is obviously larger than the `Finished` size so clippy warns on this
// but we don't want to force going through a pointer every time we want to write.
#[allow(clippy::large_enum_variant)]
enum WriterState {
    /// Writer is open and events can be written
    Active {
        writer: RawEncoder<BufWriter<ActiveHandle>>,
        need_metadata: bool,
    },

    /// Writer has been finalized or stopped — no encoder, no fd, no writes.
    Finished,
}

#[bon::bon]
impl SegmentWriter<Disk> {
    /// Create a new rotating writer. For additional options like `segment_metadata`,
    /// use [`DiskWriter::builder()`].
    pub fn new(
        base_path: impl Into<PathBuf>,
        max_file_size: u64,
        max_total_size: u64,
    ) -> std::io::Result<Self> {
        Self::create(
            base_path,
            max_file_size,
            max_total_size,
            DEFAULT_ROTATION_PERIOD,
            SegmentMetadata::default(),
        )
    }

    /// Create a `DiskWriterBuilder` for advanced configuration.
    ///
    /// When `max_file_size` is omitted, it defaults to
    /// `min(100 MiB, max_total_size / 4)`.
    #[builder(builder_type = DiskWriterBuilder, finish_fn = build)]
    pub fn builder(
        base_path: impl Into<PathBuf>,
        /// Per-file rotation threshold in bytes. Defaults to
        /// `min(100 MiB, max_total_size / 4)` when not set.
        max_file_size: Option<u64>,
        max_total_size: u64,
        /// How often to rotate, measured in monotonic time since the writer
        /// (or the previous rotation) started. Defaults to 60 seconds.
        /// `Duration::MAX` disables time-based rotation.
        rotation_period: Option<Duration>,
        segment_metadata: Option<Vec<(String, String)>>,
    ) -> std::io::Result<Self> {
        Self::create(
            base_path,
            max_file_size.unwrap_or_else(|| derive_max_file_size(max_total_size)),
            max_total_size,
            rotation_period.unwrap_or(DEFAULT_ROTATION_PERIOD),
            segment_metadata
                .map(SegmentMetadata::new)
                .unwrap_or_default(),
        )
    }

    fn create(
        base_path: impl Into<PathBuf>,
        max_file_size: u64,
        max_total_size: u64,
        rotation_period: Duration,
        segment_metadata: SegmentMetadata,
    ) -> std::io::Result<Self> {
        if rotation_period == Duration::from_secs(0) {
            return Err(std::io::Error::other("Rotation period must not be zero"));
        }
        let base_path = base_path.into();
        if let Some(parent) = base_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let fs = Fs::new_disk(&base_path);
        let discovered = fs.discover_existing()?;
        let first_index = discovered.next_active_index;
        let next_index = first_index
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("trace segment index overflow"))?;
        let first_path = Self::active_path(&base_path, first_index);
        let handle = fs.create_segment(&first_path)?;
        let state = Self::prepare_segment(BufWriter::new(handle))?;
        let now = time_source().instant().as_std();
        let drain_interval = rotation_period.min(DEFAULT_DRAIN_INTERVAL);

        let mut writer = Self {
            base_path,
            max_file_size,
            max_total_size,
            rotation_period,
            next_rotation_time: Self::next_rotation_from(now, rotation_period),
            closed_files: discovered.closed_files,
            active_path: first_path,
            state,
            next_index,
            segment_metadata,
            dropped_events: 0,
            has_real_events: false,
            drain_interval,
            next_drain_time: now + drain_interval,
            fs,
            boot_id: None,
            _namespace_lock: None,
            _mode: PhantomData,
        };
        // Enforce the budget immediately so artifacts from prior writer
        // lifetimes don't push us over the cap before we even rotate once.
        writer.evict_oldest()?;
        Ok(writer)
    }

    pub(crate) fn set_namespace(&mut self, boot_id: String, lock: std::fs::File) {
        self.boot_id = Some(boot_id);
        self._namespace_lock = Some(lock);
    }

    /// Create a writer that writes to a single file with no rotation or eviction.
    /// The segment is written to `{stem}.0.bin.active` while active, then sealed
    /// to `{stem}.0.bin` on `finalize`. The background worker will symbolize
    /// and gzip it to `{stem}.0.bin.gz`.
    ///
    /// Note: This API does not allow the ability to provide custom segment metadata.
    /// Time-based rotation is disabled.
    pub fn single_file(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let fs = Fs::new_disk(&path);
        let active_path = Self::active_path(&path, 0);
        let handle = fs.create_segment(&active_path)?;
        let state = Self::prepare_segment(BufWriter::new(handle))?;
        let now = time_source().instant().as_std();

        Ok(Self {
            base_path: path,
            max_file_size: u64::MAX,
            max_total_size: u64::MAX,
            rotation_period: Duration::MAX,
            next_rotation_time: None,
            closed_files: VecDeque::new(),
            active_path,
            state,
            next_index: 1,
            segment_metadata: SegmentMetadata::default(),
            dropped_events: 0,
            has_real_events: false,
            drain_interval: DEFAULT_DRAIN_INTERVAL,
            next_drain_time: now + DEFAULT_DRAIN_INTERVAL,
            fs,
            boot_id: None,
            _namespace_lock: None,
            _mode: PhantomData,
        })
    }
}

/// Default segment size when no explicit segment size is provided.
/// Always at least 8 slots of burst headroom in the ring.
fn pick_segment_size(max_total_size: u64) -> u64 {
    const MIN_SLOTS: u64 = 8;
    (max_total_size / MIN_SLOTS).max(1)
}

#[bon::bon]
impl SegmentWriter<Memory> {
    /// Create an in-memory writer with a total byte budget. Segments live in process heap
    /// instead of files. Auto-picks a reasonable segment size,
    /// use [`builder`](Self::builder) for explicit control.
    ///
    /// Same rotation semantics as the disk path. Errors when
    /// `max_total_size == 0`.
    pub fn new(max_total_size: u64) -> std::io::Result<Self> {
        Self::create_in_memory(
            max_total_size,
            pick_segment_size(max_total_size),
            DEFAULT_ROTATION_PERIOD,
            SegmentMetadata::default(),
        )
    }

    /// Builder for in-memory writer configuration.
    #[builder(builder_type = InMemoryWriterBuilder, finish_fn = build)]
    pub fn builder(
        max_total_size: u64,
        /// Override the default segment size.
        max_segment_size: Option<u64>,
        /// Wall-clock rotation period.
        rotation_period: Option<Duration>,
        segment_metadata: Option<Vec<(String, String)>>,
    ) -> std::io::Result<Self> {
        let seg_size = max_segment_size.unwrap_or_else(|| pick_segment_size(max_total_size));
        Self::create_in_memory(
            max_total_size,
            seg_size,
            rotation_period.unwrap_or(DEFAULT_ROTATION_PERIOD),
            segment_metadata
                .map(SegmentMetadata::new)
                .unwrap_or_default(),
        )
    }

    fn create_in_memory(
        max_total_size: u64,
        max_segment_size: u64,
        rotation_period: Duration,
        segment_metadata: SegmentMetadata,
    ) -> std::io::Result<Self> {
        if max_total_size == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "max_total_size must be > 0",
            ));
        }
        if max_segment_size == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "max_segment_size must be > 0",
            ));
        }
        if rotation_period == Duration::from_secs(0) {
            return Err(std::io::Error::other("Rotation period must not be zero"));
        }
        // The active buffer and the worker's in-flight segment live outside the ring, so the ring needs
        // room for at least one sealed segment on top of that reserve.
        let min_total = (crate::background_task::fs::PIPELINE_RESERVE_SEGMENTS + 1)
            .saturating_mul(max_segment_size);
        if max_total_size < min_total {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "max_total_size ({max_total_size}) must be >= {min_total} \
                     ({} × max_segment_size: 1 active + 1 in-flight + 1 ring slot)",
                    crate::background_task::fs::PIPELINE_RESERVE_SEGMENTS + 1
                ),
            ));
        }
        let fs = Fs::new_in_memory(max_total_size, max_segment_size)?;
        // Dummy prefix, the memory backend ignores paths but `active_path`
        // still needs one.
        let base_path = PathBuf::from("mem");
        let active_path = Self::active_path(&base_path, 0);
        let handle = fs.create_segment(&active_path)?;
        let state = Self::prepare_segment(BufWriter::new(handle))?;
        let now = time_source().instant().as_std();
        // Drain at least as often as we rotate.
        let drain_interval = rotation_period.min(DEFAULT_DRAIN_INTERVAL);

        Ok(Self {
            base_path,
            max_file_size: max_segment_size,
            max_total_size,
            rotation_period,
            next_rotation_time: Self::next_rotation_from(now, rotation_period),
            closed_files: VecDeque::new(),
            active_path,
            state,
            next_index: 1,
            segment_metadata,
            dropped_events: 0,
            has_real_events: false,
            drain_interval,
            next_drain_time: now + drain_interval,
            fs,
            boot_id: None,
            _namespace_lock: None,
            _mode: PhantomData,
        })
    }
}

impl<M: WriterMode> SegmentWriter<M> {
    /// The base path used for trace segment files.
    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    /// Per-process boot identifier, if namespace isolation is active. This is
    /// the name of the [`trace_dir`](Self::trace_dir) subdirectory.
    pub fn boot_id(&self) -> Option<&str> {
        self.boot_id.as_deref()
    }

    /// Directory this writer's trace segments live in. When namespace
    /// isolation is active this is the per-process `{configured_dir}/{boot_id}/`
    /// subdirectory; otherwise it is the configured directory directly. Use
    /// this to locate the segment files on disk.
    pub fn trace_dir(&self) -> &Path {
        self.base_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
    }

    /// The path of the currently active (being-written) segment file.
    pub fn current_active_path(&self) -> &Path {
        &self.active_path
    }

    /// Create an encoder, write the file header, segment metadata, and a
    /// clock-sync anchor, then convert to a [`RawEncoder`] for the
    /// remainder of the file's lifetime.
    fn prepare_segment(writer: BufWriter<ActiveHandle>) -> std::io::Result<WriterState> {
        let mut encoder = Encoder::new_to(writer)?;
        let (mono, real) = clock_pair();
        encoder.write(&ClockSyncEvent {
            timestamp_ns: mono,
            realtime_ns: real,
        })?;
        Ok(WriterState::Active {
            writer: encoder.into_raw_encoder(),
            need_metadata: true,
        })
    }

    fn write_metadata_if_needed(&mut self) -> std::io::Result<()> {
        match &mut self.state {
            WriterState::Active {
                writer,
                need_metadata,
            } => {
                if *need_metadata {
                    Self::write_segment_metadata(writer, &self.segment_metadata.entries)?;
                }
                *need_metadata = false;
                Ok(())
            }
            WriterState::Finished => Ok(()),
        }
    }

    /// Write a `SegmentMetadataEvent` and a fresh `ClockSyncEvent` into
    /// the current active segment.
    fn write_segment_metadata(
        writer: &mut RawEncoder<BufWriter<ActiveHandle>>,
        entries: &[(String, String)],
    ) -> std::io::Result<()> {
        let mut enc = Encoder::new();
        let entries = entries.to_vec();
        let (mono, real) = clock_pair();
        enc.write(&SegmentMetadataEvent {
            timestamp_ns: mono,
            entries,
        })?;
        enc.write(&ClockSyncEvent {
            timestamp_ns: mono,
            realtime_ns: real,
        })?;
        writer.write_raw(&enc.finish())?;
        Ok(())
    }

    /// Path for a segment that is actively being written.
    fn active_path(base: &Path, index: u32) -> PathBuf {
        let stem = base.file_stem().unwrap_or_default().to_string_lossy();
        let parent = base.parent().unwrap_or(Path::new("."));
        parent.join(format!("{}.{}.bin.active", stem, index))
    }

    /// Compute the next rotation deadline as `now + period`, or `None` when
    /// `period == Duration::MAX` (time-based rotation disabled).
    fn next_rotation_from(now: Instant, period: Duration) -> Option<Instant> {
        (period != Duration::MAX).then(|| now + period)
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        if matches!(self.state, WriterState::Finished) {
            return Ok(());
        }

        // Advance timers up front. If anything below fails the flush loop must
        // NOT see should_drain() return true on the next 5ms tick — otherwise
        // it busy-spins re-attempting the same failing rotate.
        let now = time_source().instant().as_std();
        self.next_rotation_time = Self::next_rotation_from(now, self.rotation_period);
        self.next_drain_time = now + self.drain_interval;

        // Take ownership of the encoder (state is Finished until new segment opens).
        let WriterState::Active {
            writer: mut raw, ..
        } = std::mem::replace(&mut self.state, WriterState::Finished)
        else {
            return Ok(());
        };

        // Best-effort flush. If the underlying file is gone the buffered bytes
        // are already lost; proceed to rotate rather than erroring.
        let _ = raw.flush();
        let closed_size = raw.bytes_written();
        let current_index = self.next_index - 1;

        // Extract the ActiveHandle for sealing.
        let bw: BufWriter<ActiveHandle> = raw.into_inner();
        let handle: ActiveHandle = bw
            .into_inner()
            .unwrap_or_else(|e| e.into_inner().into_parts().0);

        // Seal the current segment. If `.active` was removed externally
        // (disk only: operator, log rotation, container teardown) abandon the
        // segment and start a fresh one.
        match self.fs.seal(handle, &self.active_path, current_index) {
            Ok(seg_ref) => {
                if M::IS_DISK {
                    self.closed_files.push_back((seg_ref, closed_size));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        "active trace file {} disappeared before sealing; \
                         abandoning segment and starting a fresh one",
                        self.active_path.display()
                    );
                });
            }
            Err(e) => {
                // state is already Finished from mem::replace above
                return Err(e);
            }
        }

        let new_path = Self::active_path(&self.base_path, self.next_index);
        self.next_index += 1;

        // Open the new active segment. The backend self-heals if a disk
        // parent directory was removed underneath us, any other failure
        // leaves state = Finished so the writer stops cleanly rather than
        // retrying every drain cycle.
        let handle: ActiveHandle = self.fs.create_segment(&new_path)?;

        self.state = match Self::prepare_segment(BufWriter::new(handle)) {
            Ok(s) => s,
            Err(e) => {
                let _ = self.fs.remove_active(&new_path);
                return Err(e);
            }
        };
        self.active_path = new_path;
        self.has_real_events = false;

        tracing::debug!(
            segment_index = self.next_index - 1,
            "rotated to new trace segment"
        );
        self.evict_oldest()?;
        Ok(())
    }

    /// Total size across all closed + active segments (disk mode only).
    /// Always returns 0 in memory mode, eviction is handled by the memory backend.
    fn total_size(&self) -> u64 {
        if !M::IS_DISK {
            return 0;
        }
        let closed: u64 = self.closed_files.iter().map(|(_, s)| s).sum();
        let active = match &self.state {
            WriterState::Active { writer, .. } => writer.bytes_written(),
            WriterState::Finished => 0,
        };
        closed + active
    }

    fn evict_oldest(&mut self) -> std::io::Result<()> {
        if !M::IS_DISK {
            return Ok(());
        }
        // Always keep at least the current file.
        while self.total_size() > self.max_total_size && !self.closed_files.is_empty() {
            if let Some((seg_ref, _size)) = self.closed_files.pop_front() {
                self.fs.remove_sealed(&seg_ref, RemoveReason::Eviction);
            }
        }
        // If even the current file alone exceeds total budget, stop writing.
        if self.total_size() > self.max_total_size {
            self.state = WriterState::Finished;
        }
        Ok(())
    }

    /// Rotate if the current file exceeds max_file_size.
    /// Called after writing a complete logical unit (def + event).
    fn maybe_rotate(&mut self) -> std::io::Result<()> {
        let WriterState::Active { writer: raw, .. } = &self.state else {
            return Ok(());
        };
        if raw.bytes_written() > self.max_file_size {
            self.rotate()?;
        }
        Ok(())
    }
}

impl<M: WriterMode> SegmentWriter<M> {
    pub(crate) fn fs_handle(&self) -> Option<Arc<Fs>> {
        Some(Arc::clone(&self.fs))
    }

    /// Flush buffered data to the underlying storage.
    pub fn flush(&mut self) -> std::io::Result<()> {
        if let WriterState::Active { writer: raw, .. } = &mut self.state {
            raw.flush()?;
        }
        Ok(())
    }

    pub(crate) fn segment_metadata(&self) -> &[(String, String)] {
        &self.segment_metadata.entries
    }

    /// Merge the segment metadata entries written into the next rotated segment.
    pub fn update_segment_metadata(&mut self, entries: Vec<(String, String)>) {
        if self.segment_metadata.merge(entries.into_iter()) {
            match &mut self.state {
                WriterState::Active { need_metadata, .. } => *need_metadata = true,
                WriterState::Finished => {}
            }
        }
    }

    pub(crate) fn write_current_segment_metadata(&mut self) -> std::io::Result<()> {
        self.write_metadata_if_needed()
    }

    pub(crate) fn should_drain(&self) -> bool {
        self.has_real_events && time_source().instant().as_std() >= self.next_drain_time
    }

    pub(crate) fn drained(&mut self) -> std::io::Result<bool> {
        if !self.has_real_events {
            return Ok(false);
        }
        let now = time_source().instant().as_std();
        if self
            .next_rotation_time
            .is_some_and(|deadline| now >= deadline)
        {
            self.rotate()?;
            return Ok(true);
        }
        // Periodic drain without rotation; advance the drain timer.
        self.next_drain_time = now + self.drain_interval;
        Ok(false)
    }

    /// Finalize the writer: flush, seal the active segment, and prevent further
    /// writes. Terminal — the writer is inert afterward.
    pub fn finalize(&mut self) -> std::io::Result<()> {
        if matches!(self.state, WriterState::Finished) {
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!("writer is already closed.");
            });
            self.fs.mark_writer_done();
            return Ok(());
        }
        // Best-effort flush: if the file is gone the bytes are already lost.
        let _ = self.flush();

        // Take ownership of the encoder (state -> Finished).
        let WriterState::Active { writer: raw, .. } =
            std::mem::replace(&mut self.state, WriterState::Finished)
        else {
            self.fs.mark_writer_done();
            return Ok(());
        };

        let bytes_written = raw.bytes_written();
        let bw: BufWriter<ActiveHandle> = raw.into_inner();
        let handle: ActiveHandle = bw
            .into_inner()
            .unwrap_or_else(|e| e.into_inner().into_parts().0);

        let current_index = self.next_index - 1;

        if self.has_real_events {
            match self.fs.seal(handle, &self.active_path, current_index) {
                Ok(seg_ref) => {
                    if M::IS_DISK {
                        self.closed_files.push_back((seg_ref, bytes_written));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(
                            "active trace file {} disappeared before finalize; \
                             dropping segment",
                            self.active_path.display()
                        );
                    });
                }
                Err(e) => {
                    self.fs.mark_writer_done();
                    return Err(e);
                }
            }
        } else {
            // No real events — just header + metadata. Remove instead of
            // sealing so the background worker doesn't upload an empty segment.
            tracing::debug!(
                "removing empty final segment {}",
                self.active_path.display()
            );
            if let Err(e) = self.fs.remove_active(&self.active_path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                self.fs.mark_writer_done();
                return Err(e);
            }
        }

        // Final sealed segment must count toward the eviction budget too,
        // otherwise finalize can leave the directory over `max_total_size`.
        // No-ops on memory mode (`!M::IS_DISK`).
        if let Err(e) = self.evict_oldest() {
            self.fs.mark_writer_done();
            return Err(e);
        }
        self.fs.mark_writer_done();
        Ok(())
    }

    /// Transcode an encoded batch into the active segment.
    pub fn write_encoded_batch(&mut self, batch: &Batch) -> std::io::Result<()> {
        self.write_metadata_if_needed()?;
        let WriterState::Active { writer: raw, .. } = &mut self.state else {
            self.dropped_events += batch.event_count as usize;
            return Ok(());
        };
        if batch.event_count > 0 {
            // Note: we do NOT advance next_rotation_time or next_drain_time
            // when the first event arrives in an empty segment, even if the
            // timers are stale. The drain state machine (Idle → EpochBumped →
            // drain) takes 3 flush cycles (~15ms) to complete, so by the time
            // drained() is called there will be multiple batches in the segment,
            // not a single event. Advancing the timers here would skip rotation
            // windows and produce fewer segments than expected.
            // Raw-copy the thread-local batch. Each batch is self-contained
            // (starts with its own header), so the next batch's header acts as
            // the reset frame for decoders.
            raw.write_raw(&batch.encoded_bytes)?;
            self.has_real_events = true;
            self.maybe_rotate()?;
        }
        Ok(())
    }
}

impl<M: WriterMode> Drop for SegmentWriter<M> {
    fn drop(&mut self) {
        if self.dropped_events > 0 {
            rate_limited!(Duration::from_secs(60), {
                tracing::info!(
                    target: "dial9_telemetry",
                    dropped_events = self.dropped_events,
                    "SegmentWriter dropped events after finalization"
                );
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::analysis_events::Dial9Event;
    use crate::telemetry::format;
    use crate::telemetry::format::WorkerParkEvent;
    use std::io::Read;
    use tempfile::TempDir;

    /// Encode a single park event into a self-contained batch (header + event),
    /// matching the format produced by ThreadLocalBuffer.
    fn test_batch() -> Batch {
        let mut enc = Encoder::new_to(Vec::new()).unwrap();
        enc.write_infallible(&WorkerParkEvent {
            timestamp_ns: 1000,
            worker_id: crate::telemetry::format::WorkerId::from(0usize),
            local_queue: 2,
            cpu_time_ns: 0,
            tid: 0,
        });
        Batch {
            encoded_bytes: enc.into_inner(),
            event_count: 1,
        }
    }

    fn rotating_file(base: &std::path::Path, i: u32) -> String {
        format!("{}.{}.bin", base.display(), i)
    }

    /// Read all non-metadata events from a trace file.
    fn read_trace_events(path: &str) -> Vec<Dial9Event> {
        let data = std::fs::read(path).unwrap();
        format::decode_events(&data)
            .unwrap()
            .into_iter()
            .filter(|e| {
                !matches!(
                    e,
                    Dial9Event::SegmentMetadataEvent(..) | Dial9Event::ClockSyncEvent(..)
                )
            })
            .collect()
    }

    /// Total size of all trace files (.bin and .active) in a directory.
    fn total_disk_usage(dir: &std::path::Path) -> u64 {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let p = e.path();
                p.extension()
                    .is_some_and(|ext| ext == "bin" || ext == "active")
            })
            .map(|e| e.metadata().unwrap().len())
            .sum()
    }

    /// Write one batch to a temp file and return the file size.
    /// This captures the actual overhead (header + schema + event) so tests
    /// don't depend on hardcoded format sizes.
    fn single_event_file_size() -> u64 {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("probe.bin");
        let mut w = DiskWriter::single_file(&path).unwrap();
        w.write_encoded_batch(&test_batch()).unwrap();
        w.flush().unwrap();
        std::fs::metadata(w.current_active_path()).unwrap().len()
    }

    #[test]
    fn test_writer_creation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_trace_v2.bin");
        let writer = DiskWriter::single_file(&path);
        assert!(writer.is_ok());
    }

    #[test]
    fn derive_max_file_size_caps_large_budgets_at_100_mib() {
        assert_eq!(
            derive_max_file_size(1024 * BYTES_PER_MIB),
            100 * BYTES_PER_MIB
        );
    }

    #[test]
    fn derive_max_file_size_uses_quarter_of_small_budgets() {
        assert_eq!(derive_max_file_size(64 * BYTES_PER_MIB), 16 * BYTES_PER_MIB);
    }

    #[test]
    fn builder_defaults_max_file_size_from_total_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("default_size.bin");
        let total = 64 * BYTES_PER_MIB;
        let writer = DiskWriter::builder()
            .base_path(&path)
            .max_total_size(total)
            .build()
            .expect("builder should succeed without max_file_size");
        assert_eq!(writer.max_file_size, derive_max_file_size(total));
    }

    #[test]
    fn builder_honors_explicit_max_file_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("explicit_size.bin");
        let writer = DiskWriter::builder()
            .base_path(&path)
            .max_file_size(7 * BYTES_PER_MIB)
            .max_total_size(64 * BYTES_PER_MIB)
            .build()
            .expect("builder should succeed");
        assert_eq!(writer.max_file_size, 7 * BYTES_PER_MIB);
    }

    #[test]
    fn test_write_event() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_event_v2.bin");
        let mut writer = DiskWriter::single_file(&path).unwrap();

        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();

        let metadata = std::fs::metadata(writer.current_active_path()).unwrap();
        assert!(
            metadata.len() > 0,
            "file should not be empty after writing an event"
        );
    }

    #[test]
    fn test_write_batch_sizes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_batch_v2.bin");
        let mut writer = DiskWriter::single_file(&path).unwrap();

        let one_event_size = single_event_file_size();

        for _ in 0..2 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.flush().unwrap();

        let metadata = std::fs::metadata(writer.current_active_path()).unwrap();
        // Two events should be larger than one event
        assert!(metadata.len() > one_event_size);
    }

    #[test]
    fn test_binary_format_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_format_v2.bin");
        let writer = DiskWriter::single_file(&path).unwrap();
        let active = writer.current_active_path().to_owned();
        drop(writer);

        let mut file = std::fs::File::open(&active).unwrap();
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic).unwrap();
        assert_eq!(&magic, b"TRC\0");
    }

    #[test]
    fn test_rotating_writer_creation() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::new(&base, 1024, 4096).unwrap();
        writer.finalize().unwrap();

        // No real events were written, so finalize removes the empty segment.
        assert!(
            !dir.path().join("trace.0.bin").exists(),
            "empty segment should not be sealed"
        );
        assert!(
            !dir.path().join("trace.0.bin.active").exists(),
            "active file should be removed"
        );
    }

    #[test]
    fn test_rotating_writer_rotation() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // Set max_file_size to fit ~1 event so rotation triggers quickly
        let one_event = single_event_file_size();
        let mut writer = DiskWriter::new(&base, one_event, 100_000).unwrap();

        for _ in 0..3 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.finalize().unwrap();

        // All 3 events should be readable across rotated files
        let total: usize = (0..10)
            .map(|i| {
                let f = rotating_file(&base, i);
                if std::path::Path::new(&f).exists() {
                    read_trace_events(&f).len()
                } else {
                    0
                }
            })
            .sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn test_rotating_writer_eviction() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let max_file_size = one_event;
        let max_total_size = max_file_size * 3;
        let mut writer = DiskWriter::new(&base, max_file_size, max_total_size).unwrap();

        for _ in 0..10 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.finalize().unwrap();

        // Key invariant: total disk usage stays within budget
        assert!(total_disk_usage(dir.path()) <= max_total_size);

        // Oldest files should be evicted
        assert!(!std::path::Path::new(&rotating_file(&base, 0)).exists());
    }

    #[test]
    fn test_rotating_writer_stops_when_over_budget() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        // Small file size to force rotation, total budget fits ~1 file
        let max_file_size = one_event;
        let max_total_size = one_event + 5;
        let mut writer = DiskWriter::new(&base, max_file_size, max_total_size).unwrap();

        for _ in 0..100 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.finalize().unwrap();

        // Should have stopped writing — total events across all files < 100
        let total: usize = (0..100)
            .map(|i| {
                let f = rotating_file(&base, i);
                if std::path::Path::new(&f).exists() {
                    read_trace_events(&f).len()
                } else {
                    0
                }
            })
            .sum();
        assert!(
            total < 100,
            "should have stopped writing, got {total} events"
        );
    }

    /// Bug: write_encoded_batch sets stopped=true when total_size slightly exceeds
    /// max_total_size, without attempting eviction. This happens right after
    /// rotate() + evict_oldest() brings total_size just under budget, then the
    /// first batch in the new file pushes it a few bytes over. The writer
    /// permanently stops even though eviction could free space.
    ///
    /// Reproduces the stress test failure: 64-worker runtime with 1MB segments
    /// and 100MB budget stops producing segments after ~100 rotations.
    #[test]
    fn test_writer_stops_on_tiny_overshoot_after_eviction() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // Use max_file_size that doesn't evenly divide by batch size,
        // so files end up slightly under max_file_size (with leftover bytes).
        // Over 100 files, these leftovers accumulate and push total_size
        // past max_total_size after eviction.
        let max_file_size = 200;
        let num_files = 100u64;
        let max_total_size = max_file_size * num_files;
        let mut writer = DiskWriter::new(&base, max_file_size, max_total_size).unwrap();

        // Write many batches. The batch size doesn't divide evenly into
        // (max_file_size - header), so each file wastes a few bytes. After
        // 100 rotations, total_size drifts above max_total_size.
        for i in 0..5000 {
            writer.write_encoded_batch(&test_batch()).unwrap();
            if matches!(writer.state, WriterState::Finished) {
                panic!(
                    "Writer stopped at batch {i}! total_size={}, max_total_size={}, \
                     closed_files={}. \
                     write_encoded_batch should try eviction before stopping.",
                    writer.total_size(),
                    max_total_size,
                    writer.closed_files.len()
                );
            }
        }
    }

    #[test]
    fn test_rotating_writer_file_naming() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = DiskWriter::new(&base, one_event, 100_000).unwrap();

        for _ in 0..5 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.finalize().unwrap();

        // Should have created multiple files with sequential naming
        assert!(
            std::path::Path::new(&rotating_file(&base, 0)).exists(),
            "File 0 should exist"
        );
        // All events should be readable
        let total: usize = (0..10)
            .map(|i| {
                let f = rotating_file(&base, i);
                if std::path::Path::new(&f).exists() {
                    read_trace_events(&f).len()
                } else {
                    0
                }
            })
            .sum();
        assert_eq!(total, 5);
    }

    #[test]
    fn test_write_batch_across_rotation_boundary() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = DiskWriter::new(&base, one_event, 100_000).unwrap();

        for _ in 0..3 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.finalize().unwrap();

        // All 3 events should be readable across the rotated files.
        let total: usize = (0..10)
            .map(|i| {
                let f = rotating_file(&base, i);
                if std::path::Path::new(&f).exists() {
                    read_trace_events(&f).len()
                } else {
                    0
                }
            })
            .sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn test_rotated_files_have_valid_headers() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = DiskWriter::new(&base, one_event, 100_000).unwrap();

        for _ in 0..3 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.finalize().unwrap();

        // Each rotated file must be a self-contained, readable trace.
        let total: usize = (0..10)
            .map(|i| {
                let f = rotating_file(&base, i);
                if std::path::Path::new(&f).exists() {
                    read_trace_events(&f).len() // panics if corrupt
                } else {
                    0
                }
            })
            .sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn test_flush_after_stop() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // Total budget smaller than one file — stops immediately
        let mut writer = DiskWriter::new(&base, 10_000, 50).unwrap();

        for _ in 0..5 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        // Repeated flush after stop should not error
        assert!(writer.flush().is_ok());
        assert!(writer.flush().is_ok());
    }

    #[test]
    fn test_mixed_event_sizes() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = DiskWriter::new(&base, one_event, 100_000).unwrap();

        for _ in 0..3 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.finalize().unwrap();

        // All events should be readable across files.
        let mut total = 0;
        for i in 0..10 {
            let f = rotating_file(&base, i);
            if std::path::Path::new(&f).exists() {
                total += read_trace_events(&f).len();
            }
        }
        assert_eq!(total, 3);
    }

    #[test]
    fn test_event_exactly_on_max_file_size_boundary() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        // Exactly fits one event file — second event triggers rotation
        let mut writer = DiskWriter::new(&base, one_event, 100_000).unwrap();

        for _ in 0..2 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.finalize().unwrap();

        // Both events readable across files
        let total: usize = (0..10)
            .map(|i| {
                let f = rotating_file(&base, i);
                if std::path::Path::new(&f).exists() {
                    read_trace_events(&f).len()
                } else {
                    0
                }
            })
            .sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn test_active_suffix_while_writing() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::new(&base, 1024, 100000).unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();

        // Current file should have .active suffix
        let active = dir.path().join("trace.0.bin.active");
        assert!(active.exists(), "active file should exist while writing");
        let sealed = dir.path().join("trace.0.bin");
        assert!(!sealed.exists(), "sealed file should not exist yet");
    }

    #[test]
    fn test_rotation_seals_previous_file() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = DiskWriter::new(&base, one_event, 100_000).unwrap();

        // Write 2 events — triggers rotation after first
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();

        // First file should be sealed (.bin), second should be active
        assert!(
            dir.path().join("trace.0.bin").exists(),
            "rotated file should be sealed"
        );
        assert!(
            !dir.path().join("trace.0.bin.active").exists(),
            "rotated file should not be active"
        );
        assert!(
            dir.path().join("trace.1.bin.active").exists(),
            "current file should be active"
        );
        assert!(
            !dir.path().join("trace.1.bin").exists(),
            "current file should not be sealed"
        );
    }

    #[test]
    fn test_finalize_renames_current_file() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::new(&base, 1024, 100000).unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.finalize().unwrap();

        assert!(
            dir.path().join("trace.0.bin").exists(),
            "file should be sealed after finalize()"
        );
        assert!(
            !dir.path().join("trace.0.bin.active").exists(),
            "active file should be gone after finalize()"
        );
    }

    #[test]
    fn test_finalize_removes_empty_segment_after_rotation() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // Small max_file_size so one event triggers rotation.
        let mut writer = DiskWriter::new(&base, 1, 100_000).unwrap();
        // Write an event — this fills segment 0 and triggers rotation to segment 1.
        writer.write_encoded_batch(&test_batch()).unwrap();
        // Segment 0 is sealed, segment 1 is active with only header + metadata.
        assert!(dir.path().join("trace.0.bin").exists());
        assert!(dir.path().join("trace.1.bin.active").exists());

        // Finalize should remove the empty segment 1 instead of sealing it.
        writer.finalize().unwrap();
        assert!(
            !dir.path().join("trace.1.bin").exists(),
            "empty segment should not be sealed"
        );
        assert!(
            !dir.path().join("trace.1.bin.active").exists(),
            "empty active file should be removed"
        );
        // Segment 0 should still exist.
        assert!(dir.path().join("trace.0.bin").exists());
    }

    #[test]
    fn test_single_file_no_active_suffix() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        let mut writer = DiskWriter::single_file(&path).unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        // single_file seals to test.0.bin after finalize, no leftover .active
        assert!(dir.path().join("test.0.bin").exists());
        assert!(!dir.path().join("test.0.bin.active").exists());
    }

    #[test]
    fn test_single_file_sealed_segment_discoverable_by_worker() {
        use crate::background_task::sealed::find_sealed_segments;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("trace.bin");
        let mut writer = DiskWriter::single_file(&path).unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let segments = find_sealed_segments(dir.path(), "trace").unwrap();
        assert_eq!(
            segments.len(),
            1,
            "worker should find exactly one sealed segment"
        );
        assert_eq!(segments[0].path, dir.path().join("trace.0.bin"));
    }

    #[test]
    fn test_segment_metadata_roundtrip() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(100_000)
            .max_total_size(100_000)
            .segment_metadata(vec![
                ("service".into(), "checkout-api".into()),
                ("host".into(), "i-0abc123".into()),
            ])
            .build()
            .unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let all_events =
            format::decode_events(&std::fs::read(format!("{}.0.bin", base.display())).unwrap())
                .unwrap();
        let metadata: Vec<_> = all_events
            .iter()
            .filter_map(|e| match e {
                Dial9Event::SegmentMetadataEvent(meta) => Some(meta.entries.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(metadata.len(), 1);
        assert!(
            metadata[0].get("service").map(String::as_str) == Some("checkout-api"),
            "missing service entry: {:?}",
            metadata[0]
        );
        assert!(
            metadata[0].get("host").map(String::as_str) == Some("i-0abc123"),
            "missing host entry: {:?}",
            metadata[0]
        );
        assert_eq!(
            metadata[0].get(DIAL9_VERSION_KEY).map(String::as_str),
            Some(DIAL9_VERSION_VALUE),
            "missing built-in dial9.dial9-tokio-telemetry.version: {:?}",
            metadata[0]
        );
    }

    #[test]
    fn test_segment_metadata_written_in_every_rotated_file() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(one_event)
            .max_total_size(100_000)
            .segment_metadata(vec![("k".into(), "v".into())])
            .build()
            .unwrap();

        for _ in 0..5 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
            .collect();
        files.sort();
        assert!(files.len() >= 2, "expected at least 2 files from rotation");

        for file in &files {
            let all_events = format::decode_events(&std::fs::read(file).unwrap()).unwrap();
            let has_metadata = all_events.iter().any(|e| match e {
                Dial9Event::SegmentMetadataEvent(meta) => {
                    meta.entries.get("k").map(String::as_str) == Some("v")
                }
                _ => false,
            });
            assert!(has_metadata, "{}: expected SegmentMetadata", file.display());
        }
    }

    #[test]
    fn test_dynamic_metadata_merged_on_rotation() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(one_event)
            .max_total_size(100_000)
            .segment_metadata(vec![("service".into(), "myapp".into())])
            .build()
            .unwrap();

        // Simulate the flush thread merging static + runtime→worker entries.
        let mut merged = writer.segment_metadata().to_vec();
        merged.push(("runtime.main".into(), "0,1,2,3".into()));
        writer.update_segment_metadata(merged);

        // Write enough events to trigger rotation — rotated segments should
        // contain both static and dynamic metadata.
        for _ in 0..4 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
            .collect();
        files.sort();
        assert!(files.len() >= 2, "expected at least 2 files from rotation");

        // First segment was constructed before update_dynamic_metadata, so
        // it only has static metadata. Rotated segments have both.
        for file in &files[1..] {
            let all_events = format::decode_events(&std::fs::read(file).unwrap()).unwrap();
            let meta: Vec<_> = all_events
                .iter()
                .filter_map(|e| match e {
                    Dial9Event::SegmentMetadataEvent(meta) => Some(meta.entries.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                meta.len(),
                1,
                "{}: expected 1 metadata event",
                file.display()
            );
            assert!(
                meta[0].get("service").map(String::as_str) == Some("myapp"),
                "{}: missing static metadata",
                file.display()
            );
            assert!(
                meta[0].get("runtime.main").map(String::as_str) == Some("0,1,2,3"),
                "{}: missing dynamic runtime worker metadata",
                file.display()
            );
        }
    }

    #[test]
    fn test_segment_metadata_empty_entries() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("trace.bin");
        let mut writer = DiskWriter::single_file(&path).unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();

        let all_events =
            format::decode_events(&std::fs::read(writer.current_active_path()).unwrap()).unwrap();
        let park_count = all_events
            .iter()
            .filter(|e| matches!(e, Dial9Event::WorkerParkEvent(..)))
            .count();
        assert_eq!(park_count, 1);
        // Metadata should be present and carry only the built-in dial9.dial9-tokio-telemetry.version entry
        // (no user-supplied entries via single_file()).
        let metadata: Vec<_> = all_events
            .iter()
            .filter_map(|e| match e {
                Dial9Event::SegmentMetadataEvent(meta) => Some(&meta.entries),
                _ => None,
            })
            .collect();
        assert_eq!(metadata.len(), 1);
        assert_eq!(
            metadata[0].get(DIAL9_VERSION_KEY).map(String::as_str),
            Some(DIAL9_VERSION_VALUE)
        );
    }

    /// When the background worker has renamed a sealed `.bin` to `.bin.gz`,
    /// eviction should clean up the `.gz` variant instead of silently leaking it.
    #[test]
    fn test_eviction_removes_gz_variant() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let max_file_size = one_event;
        // Budget fits many files so segment 0 is not immediately evicted.
        let max_total_size = max_file_size * 100;
        let mut writer = DiskWriter::new(&base, max_file_size, max_total_size).unwrap();

        // Write two batches: the first fills segment 0, the second triggers
        // rotation (sealing segment 0 as trace.0.bin) and starts segment 1.
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        // Segment 0 is now sealed as trace.0.bin.

        // Simulate the background worker renaming trace.0.bin → trace.0.bin.gz.
        let seg0 = dir.path().join("trace.0.bin");
        let seg0_gz = dir.path().join("trace.0.bin.gz");
        assert!(seg0.exists(), "trace.0.bin should exist after rotation");
        std::fs::rename(&seg0, &seg0_gz).unwrap();

        // Now shrink the budget so the next rotation triggers eviction of
        // segment 0 (which has been renamed to .bin.gz on disk).
        writer.max_total_size = max_file_size;
        for _ in 0..3 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.finalize().unwrap();

        // The .bin.gz file should have been cleaned up by eviction.
        assert!(!seg0_gz.exists(), "trace.0.bin.gz should have been evicted");
    }

    /// Eviction must never drop below the most-recent segment, even when that
    /// single segment alone exceeds `max_total_size`. In that case it retains
    /// the segment on disk (so on-disk usage legitimately exceeds the budget)
    /// and signals "stop writing" by transitioning to `Finished`.
    ///
    /// This is the floor that makes an end-to-end `on-disk bytes <=
    /// max_total_size` assertion unsound — see `tests/writeback_no_leaked_gz.rs`.
    #[test]
    fn test_eviction_keeps_most_recent_segment_when_over_budget() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        // No rotation (huge per-file size) so the single active segment is the
        // only one; a budget smaller than one segment forces the floor.
        let max_file_size = u64::MAX;
        let max_total_size = one_event / 2;
        assert!(
            max_total_size < one_event,
            "test setup: budget must be smaller than a single segment"
        );
        let mut writer = DiskWriter::new(&base, max_file_size, max_total_size).unwrap();

        writer.write_encoded_batch(&test_batch()).unwrap();
        // The lone active segment already exceeds the total budget.
        assert!(
            writer.total_size() > max_total_size,
            "single segment ({}) should exceed budget ({max_total_size})",
            writer.total_size()
        );

        // Eviction has no closed segments to drop and must NOT delete the
        // current (most-recent) segment. It signals "stop" instead.
        writer.evict_oldest().unwrap();

        assert!(
            matches!(writer.state, WriterState::Finished),
            "writer should stop once even the most-recent segment exceeds budget"
        );
        // The most-recent segment is retained on disk despite exceeding the
        // budget — eviction never drops below one segment.
        assert!(
            std::path::Path::new(&writer.current_active_path()).exists(),
            "the most-recent segment must not be evicted"
        );
        assert!(
            total_disk_usage(dir.path()) > max_total_size,
            "retained segment is expected to push on-disk usage over the budget"
        );
    }

    // ---- Time-based rotation tests ----

    #[tokio::test(start_paused = true)]
    async fn test_time_rotation_triggers_on_expired_boundary() {
        use metrique_timesource::{TimeSource, tokio::set_time_source_for_current_runtime};
        let _guard = set_time_source_for_current_runtime(TimeSource::tokio(std::time::UNIX_EPOCH));

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(u64::MAX)
            .max_total_size(100_000)
            .rotation_period(Duration::from_secs(60))
            .build()
            .unwrap();

        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        let initial_index = writer.next_index;

        // Advance past the 60s boundary
        tokio::time::advance(Duration::from_secs(61)).await;

        // Time-based rotation is now driven by drained(), not write_encoded_batch.
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.drained().unwrap();

        assert!(
            writer.next_index > initial_index,
            "expected time-based rotation to trigger"
        );
        writer.finalize().unwrap();

        let total: usize = (0..10)
            .map(|i| {
                let f = rotating_file(&base, i);
                if std::path::Path::new(&f).exists() {
                    read_trace_events(&f).len()
                } else {
                    0
                }
            })
            .sum();
        assert_eq!(total, 2);
    }

    /// The first rotation must happen exactly `rotation_period` after the writer
    /// is created, not earlier due to wall-clock alignment. Starting at a non-aligned
    /// wall-clock time (UNIX_EPOCH + 22s) with a 60s period and advancing 50s must
    /// NOT rotate. So only 50s of monotonic time have elapsed since the writer started.
    #[tokio::test(start_paused = true)]
    async fn test_first_rotation_uses_monotonic_period_not_wallclock_alignment() {
        use metrique_timesource::{TimeSource, tokio::set_time_source_for_current_runtime};
        let start_wall = std::time::UNIX_EPOCH + Duration::from_secs(22);
        let _guard = set_time_source_for_current_runtime(TimeSource::tokio(start_wall));

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(u64::MAX)
            .max_total_size(100_000)
            .rotation_period(Duration::from_secs(60))
            .build()
            .unwrap();

        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        let initial_index = writer.next_index;

        // 50s of monotonic time have elapsed under the 60s period, so no rotation.
        // On the old wall-clock-aligned implementation this would advance past the
        // 60s wall-clock boundary (22s + 50s = 72s ≥ 60s) and incorrectly rotate.
        tokio::time::advance(Duration::from_secs(50)).await;

        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.drained().unwrap();

        assert_eq!(
            writer.next_index, initial_index,
            "rotation must not fire before one full rotation_period of monotonic time has elapsed",
        );

        // after the period DOES elapse, rotation fires.
        tokio::time::advance(Duration::from_secs(11)).await;
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.drained().unwrap();
        assert!(
            writer.next_index > initial_index,
            "rotation should fire once a full rotation_period of monotonic time has elapsed",
        );

        writer.finalize().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn test_time_rotation_skips_when_no_real_events() {
        use metrique_timesource::{TimeSource, tokio::set_time_source_for_current_runtime};
        let _guard = set_time_source_for_current_runtime(TimeSource::tokio(std::time::UNIX_EPOCH));

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(u64::MAX)
            .max_total_size(100_000)
            .rotation_period(Duration::from_secs(60))
            .build()
            .unwrap();

        // Advance past the boundary without writing any events
        tokio::time::advance(Duration::from_secs(120)).await;

        let empty_batch = Batch {
            encoded_bytes: vec![],
            event_count: 0,
        };
        writer.write_encoded_batch(&empty_batch).unwrap();

        assert_eq!(
            writer.next_index, 1,
            "should not rotate when no real events exist"
        );
        writer.finalize().unwrap();
    }

    #[test]
    fn test_size_rotation_still_works_with_time_disabled() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(one_event)
            .max_total_size(100_000)
            .rotation_period(std::time::Duration::MAX)
            .build()
            .unwrap();

        for _ in 0..3 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.finalize().unwrap();

        let total: usize = (0..10)
            .map(|i| {
                let f = rotating_file(&base, i);
                if std::path::Path::new(&f).exists() {
                    read_trace_events(&f).len()
                } else {
                    0
                }
            })
            .sum();
        assert_eq!(total, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn test_time_rotation_respects_eviction_budget() {
        use metrique_timesource::{TimeSource, tokio::set_time_source_for_current_runtime};
        let _guard = set_time_source_for_current_runtime(TimeSource::tokio(std::time::UNIX_EPOCH));

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(u64::MAX)
            .max_total_size(one_event * 3)
            .rotation_period(Duration::from_secs(60))
            .build()
            .unwrap();

        writer.write_encoded_batch(&test_batch()).unwrap();
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(61)).await;
            writer.write_encoded_batch(&test_batch()).unwrap();
            writer.drained().unwrap();
        }
        writer.finalize().unwrap();

        assert!(
            total_disk_usage(dir.path()) <= one_event * 3,
            "disk usage should stay within budget"
        );
    }

    #[test]
    fn test_builder_rotation_period_default() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(1024)
            .max_total_size(100_000)
            .build()
            .unwrap();
        assert_eq!(writer.rotation_period, DEFAULT_ROTATION_PERIOD);
    }

    #[test]
    fn test_new_uses_default_rotation_period() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let writer = DiskWriter::new(&base, 1024, 100_000).unwrap();
        assert_eq!(writer.rotation_period, DEFAULT_ROTATION_PERIOD);
    }

    #[tokio::test(start_paused = true)]
    async fn test_finalize_after_time_rotation() {
        use metrique_timesource::{TimeSource, tokio::set_time_source_for_current_runtime};
        let _guard = set_time_source_for_current_runtime(TimeSource::tokio(std::time::UNIX_EPOCH));

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(u64::MAX)
            .max_total_size(100_000)
            .rotation_period(Duration::from_secs(60))
            .build()
            .unwrap();

        writer.write_encoded_batch(&test_batch()).unwrap();
        tokio::time::advance(Duration::from_secs(61)).await;
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.drained().unwrap();
        writer.finalize().unwrap();

        let total: usize = (0..10)
            .map(|i| {
                let f = rotating_file(&base, i);
                if std::path::Path::new(&f).exists() {
                    read_trace_events(&f).len()
                } else {
                    0
                }
            })
            .sum();
        assert_eq!(total, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn test_stale_boundary_does_not_rotate_first_event() {
        use metrique_timesource::{TimeSource, tokio::set_time_source_for_current_runtime};
        let _guard = set_time_source_for_current_runtime(TimeSource::tokio(std::time::UNIX_EPOCH));

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(u64::MAX)
            .max_total_size(100_000)
            .rotation_period(Duration::from_secs(60))
            .build()
            .unwrap();

        // Advance well past the boundary with no events
        tokio::time::advance(Duration::from_secs(300)).await;

        // First event after the gap — should NOT trigger rotation
        writer.write_encoded_batch(&test_batch()).unwrap();
        assert_eq!(
            writer.next_index, 1,
            "first event after idle gap should not trigger immediate rotation"
        );

        // Second event shortly after — still within the new boundary
        writer.write_encoded_batch(&test_batch()).unwrap();
        assert_eq!(
            writer.next_index, 1,
            "second event should still be in the same segment"
        );

        writer.finalize().unwrap();

        let events = read_trace_events(&rotating_file(&base, 0));
        assert_eq!(events.len(), 2, "both events should be in segment 0");
    }

    #[test]
    fn test_clock_sync_precedes_first_data_event() {
        use crate::background_task::sealed::LEGACY_EPOCH_NS_FLOOR;

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(100_000)
            .max_total_size(100_000)
            .build()
            .unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let data = std::fs::read(rotating_file(&base, 0)).unwrap();
        let all = format::decode_events(&data).unwrap();

        // ClockSync must precede the first data event so a streaming
        // decoder never sees a data timestamp without an anchor.
        let first_data_idx = all
            .iter()
            .position(|e| {
                !matches!(
                    e,
                    Dial9Event::SegmentMetadataEvent(..) | Dial9Event::ClockSyncEvent(..)
                )
            })
            .expect("expected at least one data event");
        let first_clock_sync_idx = all
            .iter()
            .position(|e| matches!(e, Dial9Event::ClockSyncEvent(..)))
            .expect("expected a ClockSyncEvent in the file");
        assert!(first_clock_sync_idx < first_data_idx);

        match &all[first_clock_sync_idx] {
            Dial9Event::ClockSyncEvent(e) => {
                assert!(e.realtime_ns >= LEGACY_EPOCH_NS_FLOOR);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_segment_metadata_timestamp_is_monotonic_scale() {
        use crate::background_task::sealed::LEGACY_EPOCH_NS_FLOOR;

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(100_000)
            .max_total_size(100_000)
            .build()
            .unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let data = std::fs::read(rotating_file(&base, 0)).unwrap();
        let all = format::decode_events(&data).unwrap();

        // SegmentMetadata.timestamp_ns should remain monotonic-scale,
        // not epoch wall-clock.
        let seg_ts = all
            .iter()
            .find_map(|e| match e {
                Dial9Event::SegmentMetadataEvent(m) => Some(m.timestamp_ns),
                _ => None,
            })
            .expect("SegmentMetadata");
        assert!(
            seg_ts < LEGACY_EPOCH_NS_FLOOR,
            "SegmentMetadata.timestamp_nanos ({seg_ts}) should be monotonic-scale"
        );
    }

    #[test]
    fn test_clock_sync_written_in_every_rotated_file() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(one_event)
            .max_total_size(100_000)
            .build()
            .unwrap();

        for _ in 0..5 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
            .collect();
        files.sort();
        assert!(files.len() >= 2, "expected at least 2 files from rotation");

        for file in &files {
            let all = format::decode_events(&std::fs::read(file).unwrap()).unwrap();
            let has_clock_sync = all
                .iter()
                .any(|e| matches!(e, Dial9Event::ClockSyncEvent(..)));
            assert!(
                has_clock_sync,
                "{}: expected ClockSyncEvent",
                file.display()
            );
        }
    }

    /// A hand-built legacy-shaped buffer (SegmentMetadata + WorkerPark,
    /// no ClockSyncEvent) must still round-trip through the decoder.
    #[test]
    fn test_legacy_trace_without_clock_sync_still_decodes() {
        let mut enc = Encoder::new_to(Vec::new()).unwrap();
        enc.write(&SegmentMetadataEvent {
            timestamp_ns: 1,
            entries: vec![("k".into(), "v".into())],
        })
        .unwrap();
        enc.write_infallible(&WorkerParkEvent {
            timestamp_ns: 1000,
            worker_id: crate::telemetry::format::WorkerId::from(0usize),
            local_queue: 0,
            cpu_time_ns: 0,
            tid: 0,
        });
        let buf = enc.into_inner();

        let all = format::decode_events(&buf).unwrap();
        assert!(
            all.iter()
                .any(|e| matches!(e, Dial9Event::WorkerParkEvent(..))),
            "expected WorkerPark to decode"
        );
        assert!(
            !all.iter()
                .any(|e| matches!(e, Dial9Event::ClockSyncEvent(..))),
            "legacy trace must not contain ClockSync"
        );
    }

    #[test]
    fn test_clock_sync_offset_recovers_wall_clock_for_recent_event() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(100_000)
            .max_total_size(100_000)
            .build()
            .unwrap();

        // Use a real monotonic reading so reconstruction lands near now.
        let park_ts = crate::telemetry::events::clock_monotonic_ns();
        let mut enc = Encoder::new_to(Vec::new()).unwrap();
        enc.write_infallible(&WorkerParkEvent {
            timestamp_ns: park_ts,
            worker_id: crate::telemetry::format::WorkerId::from(0usize),
            local_queue: 0,
            cpu_time_ns: 0,
            tid: 0,
        });
        writer
            .write_encoded_batch(&Batch {
                encoded_bytes: enc.into_inner(),
                event_count: 1,
            })
            .unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let all = format::decode_events(&std::fs::read(rotating_file(&base, 0)).unwrap()).unwrap();

        let (sync_mono, sync_real) = all
            .iter()
            .find_map(|e| match e {
                Dial9Event::ClockSyncEvent(c) => Some((c.timestamp_ns, c.realtime_ns)),
                _ => None,
            })
            .expect("ClockSync");
        let park_from_file = all
            .iter()
            .find_map(|e| match e {
                Dial9Event::WorkerParkEvent(p) => Some(p.timestamp_ns),
                _ => None,
            })
            .expect("WorkerPark");

        let offset = sync_real as i128 - sync_mono as i128;
        let reconstructed_wall_ns = park_from_file as i128 + offset;
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i128;
        let diff = (reconstructed_wall_ns - now_ns).abs();
        assert!(
            diff < 5_000_000_000,
            "reconstructed wall clock {reconstructed_wall_ns} diverges from now {now_ns} by {diff}ns"
        );
    }

    /// S3-style metadata set via `update_segment_metadata` before any events
    /// are written must appear in the segment's SegmentMetadata event.
    #[test]
    fn test_update_segment_metadata_appears_in_trace() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::new(&base, 100_000, 100_000).unwrap();

        // Simulate TelemetryCore::new setting S3 metadata
        writer.update_segment_metadata(vec![
            ("bucket".into(), "my-bucket".into()),
            ("service_name".into(), "my-svc".into()),
        ]);

        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let all = format::decode_events(&std::fs::read(rotating_file(&base, 0)).unwrap()).unwrap();
        let metadata: Vec<_> = all
            .iter()
            .filter_map(|e| match e {
                Dial9Event::SegmentMetadataEvent(meta) => Some(meta.entries.clone()),
                _ => None,
            })
            .collect();
        assert!(!metadata.is_empty(), "expected SegmentMetadata event");
        assert!(
            metadata.last().unwrap().get("bucket").map(String::as_str) == Some("my-bucket"),
            "S3 metadata should be in segment"
        );
        assert!(
            metadata
                .last()
                .unwrap()
                .get("service_name")
                .map(String::as_str)
                == Some("my-svc"),
            "S3 metadata should be in segment"
        );
    }

    /// Simulates the flush loop pattern: S3 metadata is set once, then
    /// runtime entries are merged repeatedly. S3 metadata must survive.
    #[test]
    fn test_merge_preserves_s3_metadata_across_runtime_updates() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = DiskWriter::new(&base, one_event, 100_000).unwrap();

        // Step 1: S3 metadata set (like TelemetryCore::new)
        writer.update_segment_metadata(vec![
            ("bucket".into(), "my-bucket".into()),
            ("service_name".into(), "my-svc".into()),
        ]);

        // Step 2: flush loop merges only runtime entries — S3 metadata
        // set in step 1 must be preserved by the merge logic.
        writer.update_segment_metadata(vec![("runtime.main".into(), "0,1".into())]);

        // Write enough to trigger rotation
        for _ in 0..4 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
            .collect();
        files.sort();
        assert!(files.len() >= 2, "expected rotation");

        // Rotated segments should contain both S3 and runtime metadata
        for file in &files[1..] {
            let all = format::decode_events(&std::fs::read(file).unwrap()).unwrap();
            let meta: Vec<_> = all
                .iter()
                .filter_map(|e| match e {
                    Dial9Event::SegmentMetadataEvent(meta) => Some(meta.entries.clone()),
                    _ => None,
                })
                .collect();
            let last = meta.last().expect("expected SegmentMetadata");
            assert!(
                last.get("bucket").map(String::as_str) == Some("my-bucket"),
                "{}: S3 metadata lost after merge",
                file.display()
            );
            assert!(
                last.get("runtime.main").map(String::as_str) == Some("0,1"),
                "{}: runtime metadata missing",
                file.display()
            );
        }
    }

    /// Repeated calls to `update_segment_metadata` with identical entries
    /// should not set `need_metadata`, avoiding redundant writes.
    #[test]
    fn test_update_segment_metadata_no_op_when_unchanged() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::new(&base, 100_000, 100_000).unwrap();

        let entries = vec![("k".into(), "v".into())];
        writer.update_segment_metadata(entries.clone());
        // First batch writes metadata
        writer.write_encoded_batch(&test_batch()).unwrap();

        // Same entries again — should be a no-op
        writer.update_segment_metadata(entries.clone());
        // Second batch should NOT write another metadata event
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let all = format::decode_events(&std::fs::read(rotating_file(&base, 0)).unwrap()).unwrap();
        let metadata_count = all
            .iter()
            .filter(|e| matches!(e, Dial9Event::SegmentMetadataEvent(..)))
            .count();
        assert_eq!(
            metadata_count, 1,
            "identical update_segment_metadata should not trigger another write"
        );
    }

    /// The crates.io version of `dial9-tokio-telemetry` is embedded in every
    /// segment's metadata under `dial9.dial9-tokio-telemetry.version`. Regression test for
    /// https://github.com/dial9-rs/dial9/issues/423.
    #[test]
    fn test_dial9_version_in_segment_metadata() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("trace.bin");
        let mut writer = DiskWriter::single_file(&path).unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let sealed = dir.path().join("trace.0.bin");
        let all = format::decode_events(&std::fs::read(&sealed).unwrap()).unwrap();
        let version_value = all.iter().find_map(|e| match e {
            Dial9Event::SegmentMetadataEvent(meta) => meta.entries.get(DIAL9_VERSION_KEY).cloned(),
            _ => None,
        });
        assert_eq!(
            version_value.as_deref(),
            Some(env!("CARGO_PKG_VERSION")),
            "expected dial9.dial9-tokio-telemetry.version entry matching CARGO_PKG_VERSION"
        );
    }

    /// User-supplied `dial9.dial9-tokio-telemetry.version` entries win over the built-in default,
    /// both at builder time and via `update_segment_metadata`.
    #[test]
    fn test_dial9_version_user_override_wins() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(100_000)
            .max_total_size(100_000)
            .segment_metadata(vec![(DIAL9_VERSION_KEY.into(), "builder-override".into())])
            .build()
            .unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        // Rotate and then runtime-override on the next segment.
        writer.rotate().unwrap();
        writer.update_segment_metadata(vec![(DIAL9_VERSION_KEY.into(), "runtime-override".into())]);
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let read_version = |idx: u32| -> String {
            let all =
                format::decode_events(&std::fs::read(rotating_file(&base, idx)).unwrap()).unwrap();
            all.iter()
                .find_map(|e| match e {
                    Dial9Event::SegmentMetadataEvent(meta) => {
                        meta.entries.get(DIAL9_VERSION_KEY).cloned()
                    }
                    _ => None,
                })
                .expect("expected dial9.dial9-tokio-telemetry.version entry")
        };
        assert_eq!(read_version(0), "builder-override");
        assert_eq!(read_version(1), "runtime-override");
    }

    /// Regression test for https://github.com/dial9-rs/dial9/issues/386
    ///
    /// If the `.active` file is removed externally (e.g. by an operator,
    /// log-rotation tool, or container teardown) the flush loop calls
    /// `drained()` → `rotate()` → `fs::rename(.active, .bin)` which fails
    /// with `NotFound`. Without recovery, `next_drain_time` is never
    /// advanced, so `should_drain()` returns true on every subsequent
    /// 5ms tick and the flush thread busy-loops.
    ///
    /// `drained()` must recover by abandoning the missing segment, opening a
    /// fresh one, and advancing the drain/rotation timers.
    #[tokio::test(start_paused = true)]
    async fn test_drained_recovers_when_active_file_deleted() {
        use metrique_timesource::{TimeSource, tokio::set_time_source_for_current_runtime};
        let _guard = set_time_source_for_current_runtime(TimeSource::tokio(std::time::UNIX_EPOCH));

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(u64::MAX)
            .max_total_size(100_000)
            .rotation_period(Duration::from_secs(60))
            .build()
            .unwrap();

        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();

        // Simulate external deletion of the .active file.
        let active_path = writer.current_active_path().to_owned();
        assert!(active_path.exists());
        std::fs::remove_file(&active_path).unwrap();

        // Cross the rotation boundary so drained() will try to rotate.
        tokio::time::advance(Duration::from_secs(61)).await;

        assert!(writer.should_drain(), "should_drain should fire");

        // drained() must succeed despite the missing .active file. Returning
        // an error here is what causes the flush thread to busy-loop because
        // the timers are never advanced.
        writer
            .drained()
            .expect("drained() must recover from missing .active file");

        // After recovery, should_drain() must return false — otherwise the
        // flush thread would spin calling drained() every 5ms.
        assert!(
            !writer.should_drain(),
            "should_drain must return false after recovery (otherwise flush loop spins)"
        );

        // The writer must still be usable: a fresh active file exists and
        // subsequent writes succeed.
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        assert!(
            writer.current_active_path().exists(),
            "writer must have a fresh active file after recovery"
        );

        writer.finalize().unwrap();
    }

    /// Companion to `test_drained_recovers_when_active_file_deleted` covering
    /// the more realistic case where the entire trace directory has been
    /// removed (e.g. `rm -rf /var/log/dial9/`). Both the rename AND the
    /// `File::create` for the new segment fail with `NotFound`. `drained()`
    /// must still advance timers so `should_drain()` stops firing — the
    /// writer can transition to `Finished`, but the flush loop must NOT
    /// busy-spin.
    #[tokio::test(start_paused = true)]
    async fn test_drained_recovers_when_parent_dir_deleted() {
        use metrique_timesource::{TimeSource, tokio::set_time_source_for_current_runtime};
        let _guard = set_time_source_for_current_runtime(TimeSource::tokio(std::time::UNIX_EPOCH));

        let dir = TempDir::new().unwrap();
        let trace_dir = dir.path().join("traces");
        std::fs::create_dir_all(&trace_dir).unwrap();
        let base = trace_dir.join("trace");
        let mut writer = DiskWriter::builder()
            .base_path(&base)
            .max_file_size(u64::MAX)
            .max_total_size(100_000)
            .rotation_period(Duration::from_secs(60))
            .build()
            .unwrap();

        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();

        std::fs::remove_dir_all(&trace_dir).unwrap();
        assert!(!writer.current_active_path().exists());

        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(writer.should_drain());

        // `drained()` may surface the underlying error, but the critical
        // invariant is that `should_drain()` must NOT fire on the next tick —
        // otherwise the flush thread busy-loops.
        let _ = writer.drained();
        assert!(
            !writer.should_drain(),
            "should_drain must return false after a failed rotation \
             (otherwise the flush loop spins on every 5ms tick)"
        );

        // Subsequent drained() calls must not re-fire either.
        tokio::time::advance(Duration::from_millis(5)).await;
        let _ = writer.drained();
        assert!(!writer.should_drain());
    }

    /// Across a process restart, retained `.bin`/`.bin.gz` artifacts from the
    /// previous lifetime must count toward `max_total_size`. Without this, a
    /// crash-restart loop grows the trace directory unbounded.
    #[test]
    fn test_restart_seeds_closed_files_and_evicts() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // Lifetime 1: write a few sealed segments.
        let one_event = single_event_file_size();
        {
            let mut w = DiskWriter::new(&base, one_event, 100_000).unwrap();
            for _ in 0..4 {
                w.write_encoded_batch(&test_batch()).unwrap();
            }
            w.finalize().unwrap();
        }
        let bin_count_before = (0..20)
            .filter(|i| std::path::Path::new(&rotating_file(&base, *i)).exists())
            .count();
        assert!(
            bin_count_before >= 2,
            "lifetime 1 should leave multiple sealed segments"
        );

        // Lifetime 2: shrink the budget so existing artifacts must be evicted.
        let new_budget = one_event + 1; // fits ~1 retained segment + the new active one
        let writer = DiskWriter::new(&base, one_event, new_budget).unwrap();
        // Discovery + immediate evict_oldest should have shed older segments.
        assert!(
            total_disk_usage(dir.path()) <= new_budget,
            "disk usage exceeds shrunk budget after restart: {}",
            total_disk_usage(dir.path())
        );
        // Next active index must not collide with retained segments.
        let next_active_path = writer.current_active_path();
        assert!(next_active_path.exists());
        assert!(
            next_active_path
                .to_str()
                .is_some_and(|s| s.ends_with(".bin.active"))
        );
    }

    /// Stale `.active` files from a dead writer can't be processed by the
    /// worker — they must be cleaned up on startup so the next writer doesn't
    /// trip over orphaned indices.
    #[test]
    fn test_restart_discards_stale_active_files() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // Simulate an orphan from a previous, crashed writer.
        let orphan = dir.path().join("trace.99.bin.active");
        std::fs::write(&orphan, b"orphaned").unwrap();

        let _w = DiskWriter::new(&base, 1024, 100_000).unwrap();
        assert!(
            !orphan.exists(),
            "stale .active should be discarded on construction"
        );
    }

    /// `.bin.gz` write-back siblings must count toward the eviction budget so
    /// post-processing doesn't push retention past the cap.
    #[test]
    fn test_restart_counts_gz_siblings_toward_budget() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // Simulate a previous lifetime where WriteBack produced a .bin.gz.
        let bin = dir.path().join("trace.0.bin");
        let gz = dir.path().join("trace.0.bin.gz");
        std::fs::write(&bin, vec![0u8; 4096]).unwrap();
        std::fs::write(&gz, vec![0u8; 1024]).unwrap();

        // Budget too small for both. Restart must evict the whole family.
        let _w = DiskWriter::new(&base, 100_000, 100).unwrap();
        assert!(!bin.exists(), ".bin should be evicted under restart budget");
        assert!(!gz.exists(), ".bin.gz must be evicted with its .bin family");
    }

    /// finalize() must run eviction so the final sealed segment counts toward
    /// the budget. Without it, finalize can leave the directory over cap.
    #[test]
    fn test_finalize_evicts_to_budget() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let max_total_size = one_event * 2;
        let mut writer = DiskWriter::new(&base, one_event, max_total_size).unwrap();

        for _ in 0..10 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.finalize().unwrap();

        assert!(
            total_disk_usage(dir.path()) <= max_total_size,
            "finalize must leave disk usage within budget"
        );
    }

    #[test]
    fn in_memory_builder_wires_custom_options() {
        use crate::telemetry::InMemoryWriter;
        let writer = InMemoryWriter::builder()
            .max_total_size(8 * 1024 * 1024)
            .max_segment_size(64 * 1024)
            .rotation_period(Duration::from_secs(30))
            .segment_metadata(vec![("svc".into(), "test".into())])
            .build()
            .unwrap();
        assert_eq!(writer.max_file_size, 64 * 1024);
        assert_eq!(writer.rotation_period, Duration::from_secs(30));
        assert!(
            writer
                .segment_metadata
                .entries
                .iter()
                .any(|(k, v)| k == "svc" && v == "test")
        );
    }

    #[test]
    fn in_memory_rejects_zero_total_size() {
        let err = InMemoryWriter::new(0).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn in_memory_builder_enforces_3x_segment_min_total_size() {
        let seg: u64 = 2048;
        // Below the boundary: rejected (no room for even one ring slot).
        let err = InMemoryWriter::builder()
            .max_total_size(3 * seg - 1)
            .max_segment_size(seg)
            .build()
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        // At boundary: accepted (1 active + 1 in-flight + 1 ring slot).
        InMemoryWriter::builder()
            .max_total_size(3 * seg)
            .max_segment_size(seg)
            .build()
            .expect("3× segment must be accepted");
    }

    /// Drive a memory writer through the real worker pipeline and return the
    /// captured per-segment payloads. Exercises the full seam: write ->
    /// Fs::Mem seal -> ring -> finalize (mark_writer_done) -> WorkerLoop::run
    /// drain-to-empty -> processor.
    async fn run_mem_e2e(mut writer: InMemoryWriter, events: usize) -> Vec<Vec<u8>> {
        use crate::background_task::WorkerLoop;
        use crate::background_task::testutil::CapturingProcessor;

        let fs = writer.fs_handle().expect("memory writer exposes its Fs");
        for _ in 0..events {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        // Seals the active segment onto the ring and signals writer_done.
        writer.finalize().unwrap();

        let (capture, captured) = CapturingProcessor::new();
        // stop is never cancelled: the loop exits via writer_done only.
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            fs,
            Duration::from_millis(5),
            vec![Box::new(capture)],
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        worker.run().await;

        let segments = captured.lock().unwrap();
        segments.clone()
    }

    /// Count decoded payload events across `segments`, dropping the per-segment
    /// metadata/clock-sync framing the writer emits.
    fn count_payload_events(segments: &[Vec<u8>]) -> usize {
        use crate::telemetry::analysis_events::Dial9Event;

        crate::background_task::testutil::decode_captured(segments)
            .into_iter()
            .filter(|e| {
                !matches!(
                    e,
                    Dial9Event::SegmentMetadataEvent(..) | Dial9Event::ClockSyncEvent(..)
                )
            })
            .count()
    }

    #[tokio::test]
    async fn mem_writer_e2e_delivers_all_events() {
        const EVENTS: usize = 25;

        let segments = run_mem_e2e(InMemoryWriter::new(1 << 20).unwrap(), EVENTS).await;

        assert!(!segments.is_empty(), "worker captured no segments");
        assert_eq!(
            count_payload_events(&segments),
            EVENTS,
            "every written event must reach the processor"
        );
    }

    /// Same as `mem_writer_e2e_delivers_all_events`, but a tiny `max_segment_size` forces several rotations so the
    /// worker delivers multiple sealed segments.
    #[tokio::test]
    async fn mem_writer_e2e_delivers_all_events_across_rotations() {
        const EVENTS: usize = 60;

        // Huge ring (nothing evicts) + tiny segments (rotate every few batches).
        let writer = InMemoryWriter::builder()
            .max_total_size(16 * 1024 * 1024)
            .max_segment_size(256)
            .build()
            .unwrap();
        let segments = run_mem_e2e(writer, EVENTS).await;

        assert!(
            segments.len() >= 2,
            "tiny segments must force rotation, got {} segment(s)",
            segments.len()
        );
        assert_eq!(
            count_payload_events(&segments),
            EVENTS,
            "every event across all rotated segments must reach the processor"
        );
    }
}

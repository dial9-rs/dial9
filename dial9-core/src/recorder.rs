//! The recorder builder.
//!
//! [`recorder`] assembles a [`Recorder`](crate::recording::Recorder): it
//! builds the shared bus, registers your [`Source`](crate::source::Source)s,
//! spawns the flush thread (and, with the `pipeline` feature, the background
//! worker), and starts recording.
//!
//! ```no_run
//! use dial9_core::buffer::DiskBuffer;
//! let recorder = dial9_core::recorder::recorder(DiskBuffer::single_file("/tmp/trace.bin")?)
//!     .build();
//! // record events through `recorder.handle()`.
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! This wraps low-level `Recorder::start`, which expects pre-built shared
//! recording state with sources already registered. The Tokio integration
//! reuses the same builder.

use crate::buffer::{BufferMode, Disk, SegmentWriter};
use crate::clock;
use crate::handle::Dial9Handle;
use crate::primitives::sync::Arc;
use crate::recording::{Recorder, RecordingStartHook};
use crate::shared_state::SharedState;
use crate::source::Source;

/// A reusable per-thread hook: run on each recording thread, returning a
/// teardown closure. Reusable (`Fn`) because both the flush thread and the
/// background worker each need a fresh `FnOnce`.
type RecordingThreadHook = Arc<dyn Fn() -> Box<dyn FnOnce() + Send> + Send + Sync>;

fn noop_thread_hook() -> RecordingThreadHook {
    Arc::new(|| Box::new(|| {}) as Box<dyn FnOnce() + Send>)
}

/// Merge `entries` into `existing`: on a key collision the incoming value wins.
/// Matches the writer's segment-metadata merge, so builder-side metadata
/// accumulates across the core and tokio layers.
pub(crate) fn merge_segment_metadata(
    existing: &mut Vec<(String, String)>,
    entries: impl IntoIterator<Item = (String, String)>,
) {
    let incoming: Vec<(String, String)> = entries.into_iter().collect();
    existing.retain(|(k, _)| !incoming.iter().any(|(ik, _)| ik == k));
    existing.extend(incoming);
}

/// Begin building a recorder backed by `writer`.
///
/// Register data sources with [`RecorderBuilder::source`], then
/// [`build`](RecorderBuilder::build), which starts recording.
pub fn recorder<M: BufferMode>(writer: SegmentWriter<M>) -> RecorderBuilder<M> {
    builder_with(Some(writer))
}

/// Begin building a recorder backed by `writer`, or a writer-free disabled one
/// when the writer could not be created.
///
/// Configure it exactly like [`recorder`]: register sources, set a pipeline,
/// then [`build`](RecorderBuilder::build). When the writer failed, that config
/// is kept but inert, and `build` yields the same recorder
/// [`recorder_disabled`] does.
pub fn recorder_or_disabled<M: BufferMode>(
    writer: std::io::Result<SegmentWriter<M>>,
) -> RecorderBuilder<M> {
    match writer {
        Ok(writer) => builder_with(Some(writer)),
        Err(e) => {
            tracing::error!(
                target: "dial9_telemetry",
                "dial9: trace writer setup failed; running without telemetry: {e}"
            );
            builder_with(None)
        }
    }
}

fn builder_with<M: BufferMode>(writer: Option<SegmentWriter<M>>) -> RecorderBuilder<M> {
    RecorderBuilder {
        writer,
        sources: Vec::new(),
        recording_start_hooks: Vec::new(),
        segment_metadata: Vec::new(),
        metrics_sink: None,
        thread_init: noop_thread_hook(),
        #[cfg(feature = "pipeline")]
        pipeline: None,
        #[cfg(feature = "pipeline")]
        terminal_processor: None,
        #[cfg(feature = "pipeline")]
        worker_poll_interval: None,
        #[cfg(feature = "pipeline")]
        trigger: None,
        #[cfg(feature = "pipeline")]
        pending_dump_trigger: None,
        enabled: true,
    }
}

/// A recorder with recording permanently disabled: no flush thread, no sources,
/// and [`handle`](crate::recording::Recorder::handle) returns a disabled handle.
///
/// Attaching Tokio to it is a no-op and `enable`/`graceful_shutdown` do nothing,
/// so it is the "telemetry off" fallback for `#[dial9::main]` and
/// `recorder_or_disabled`: application code runs unchanged, recording nothing.
pub fn recorder_disabled() -> crate::recording::Recorder {
    crate::recording::Recorder::new(crate::handle::Dial9Handle::disabled(), None)
}

/// Assemble dial9's default pipeline: source-requested stages
/// (for example symbolization), then compression, then a terminal stage —
/// uploader if configured, otherwise disk write-back.
///
/// Returns empty when there is no source stage and no uploader; in that case
/// no worker is spawned.
#[cfg(feature = "pipeline")]
fn default_pipeline(
    source_stages: Vec<Box<dyn crate::pipeline::SegmentProcessor>>,
    terminal: Option<Box<dyn crate::pipeline::SegmentProcessor>>,
    is_disk: bool,
) -> Vec<Box<dyn crate::pipeline::SegmentProcessor>> {
    if source_stages.is_empty() && terminal.is_none() {
        return Vec::new();
    }
    let mut processors = source_stages;
    processors.push(Box::new(crate::worker::processors::GzipCompressor));
    match terminal {
        Some(terminal) => processors.push(terminal),
        None if is_disk => processors.push(Box::new(
            crate::worker::processors::WriteBackProcessor::default(),
        )),
        None => {}
    }
    processors
}

/// Builder for a runtime-agnostic [`Recorder`]. See [`recorder`].
#[must_use = "call `.build()` to start recording"]
pub struct RecorderBuilder<M: BufferMode = Disk> {
    /// `None` when the writer could not be created (see
    /// [`recorder_or_disabled`]); `build` then yields a disabled recorder.
    writer: Option<SegmentWriter<M>>,
    sources: Vec<Box<dyn Source>>,
    recording_start_hooks: Vec<RecordingStartHook>,
    segment_metadata: Vec<(String, String)>,
    metrics_sink: Option<metrique::writer::BoxEntrySink>,
    thread_init: RecordingThreadHook,
    /// The segment-processing pipeline. `Some` runs exactly these processors,
    /// `None` assembles dial9's default from the registered sources at build.
    #[cfg(feature = "pipeline")]
    pipeline: Option<Vec<Box<dyn crate::pipeline::SegmentProcessor>>>,
    /// Final stage of the default pipeline, replacing write-back (the S3
    /// uploader sets it). Applied at build, so it does not depend on the order
    /// the builder was called in.
    #[cfg(feature = "pipeline")]
    terminal_processor: Option<Box<dyn crate::pipeline::SegmentProcessor>>,
    #[cfg(feature = "pipeline")]
    worker_poll_interval: Option<std::time::Duration>,
    #[cfg(feature = "pipeline")]
    trigger: Option<crate::dump::DumpRx>,
    /// Dump-trigger sender, installed on the shared state at build so
    /// `Dial9Handle::dump_trigger` can reach it.
    #[cfg(feature = "pipeline")]
    pending_dump_trigger: Option<crate::dump::DumpTrigger>,
    /// Whether [`build`](RecorderBuilder::build) starts recording. See
    /// [`paused`](RecorderBuilder::paused).
    enabled: bool,
}

impl<M: BufferMode> std::fmt::Debug for RecorderBuilder<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecorderBuilder")
            .field("sources", &self.sources.len())
            .finish_non_exhaustive()
    }
}

impl<M: BufferMode> RecorderBuilder<M> {
    /// Register a [`Source`] drained by the flush thread each cycle.
    pub fn source(mut self, source: impl Source + 'static) -> Self {
        self.sources.push(Box::new(source));
        self
    }

    /// Names of the registered sources, in registration order.
    pub fn source_names(&self) -> impl Iterator<Item = &str> + '_ {
        self.sources.iter().map(|s| s.name())
    }

    /// The writer's per-process namespace boot id, or `None` before
    /// [`set_namespace`](SegmentWriter::set_namespace) has run.
    pub fn writer_boot_id(&self) -> Option<&str> {
        self.writer.as_ref()?.boot_id()
    }

    /// Static metadata written into every rotated segment header. Merged across
    /// calls (and across the tokio layer); on a key collision the later value wins.
    ///
    /// # Examples
    ///
    /// ```
    /// use dial9_core::buffer::MemoryBuffer;
    /// use dial9_core::recorder::recorder;
    ///
    /// let recorder = recorder(MemoryBuffer::new(1024 * 1024)?)
    ///     .segment_metadata([("service".into(), "checkout".into())])
    ///     .segment_metadata([("environment".into(), "production".into())])
    ///     .segment_metadata([("service".into(), "payments".into())])
    ///     .build();
    ///
    /// // Metadata is merged across calls. The third call overrides `service`
    /// // from the first, so its value is `payments`.
    /// # recorder.graceful_shutdown(std::time::Duration::ZERO);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn segment_metadata(mut self, entries: impl IntoIterator<Item = (String, String)>) -> Self {
        merge_segment_metadata(&mut self.segment_metadata, entries);
        self
    }

    /// Metrics sink for the flush (and, with `pipeline`, worker) threads.
    /// Defaults to discarding flush metrics.
    pub fn metrics_sink(mut self, sink: metrique::writer::BoxEntrySink) -> Self {
        self.metrics_sink = Some(sink);
        self
    }

    /// Start the recorder and begin recording.
    ///
    /// Chain [`paused`](Self::paused) beforehand to build without recording, then
    /// start it later with [`Recorder::enable`].
    ///
    /// Yields a disabled recorder when the writer could not be created (see
    /// [`recorder_or_disabled`]); the sources and pipeline configured on the way
    /// here are never started.
    pub fn build(self) -> Recorder {
        #[allow(unused_mut)]
        let Some(mut writer) = self.writer else {
            return recorder_disabled();
        };

        let shared = Arc::new(SharedState::new(clock::clock_monotonic_ns()));

        // Install the on-demand dump trigger so `Dial9Handle::dump_trigger` can
        // reach it (paired with the `trigger` receiver handed to the worker).
        #[cfg(feature = "pipeline")]
        if let Some(trigger) = self.pending_dump_trigger {
            shared.set_dump_trigger(trigger);
        }

        // Sync any `boot_id` metadata to the writer's per-process namespace, so a
        // trace's identity matches its on-disk `{boot_id}/` directory (and its
        // S3 keys).
        let mut segment_metadata = self.segment_metadata;
        if let Some(boot_id) = writer.boot_id().map(str::to_owned) {
            for (key, value) in &mut segment_metadata {
                if key == "boot_id" {
                    *value = boot_id.clone();
                }
            }
        }
        if !segment_metadata.is_empty() {
            writer.update_segment_metadata(segment_metadata);
        }

        #[allow(unused_mut)]
        let mut sources = self.sources;

        // Collect the stages the sources ask for before they move into the
        // shared state; only the default pipeline uses them.
        #[cfg(feature = "pipeline")]
        let processors = match self.pipeline {
            Some(processors) => processors,
            None => {
                let source_stages = sources
                    .iter_mut()
                    .filter_map(|source| source.segment_processor())
                    .collect();
                default_pipeline(source_stages, self.terminal_processor, M::IS_DISK)
            }
        };

        for source in sources {
            shared.push_source(source);
        }

        // The worker borrows `&writer`, so it must be spawned before the writer
        // moves into `Recorder::start`.
        #[cfg(feature = "pipeline")]
        let worker = if processors.is_empty() {
            None
        } else {
            let poll = self
                .worker_poll_interval
                .unwrap_or(crate::worker::DEFAULT_POLL_INTERVAL);
            let metrics = self
                .metrics_sink
                .clone()
                .unwrap_or_else(metrique::writer::sink::DevNullSink::boxed);
            let config = crate::worker::BackgroundTaskConfig::builder()
                .maybe_trace_dir(M::IS_DISK.then(|| writer.trace_dir().to_path_buf()))
                .maybe_trace_stem(M::IS_DISK.then(|| writer.trace_stem().to_string()))
                .poll_interval(poll)
                .processors(processors)
                .metrics_sink(metrics)
                .maybe_trigger(self.trigger)
                .build();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let hook = self.thread_init.clone();
            crate::worker::spawn(&writer, config, rx, move || hook())
                .map(|wt| crate::recording::WorkerHandle::new(tx, wt))
        };

        let hook = self.thread_init.clone();
        #[allow(unused_mut)]
        let mut recorder = Recorder::start(shared, writer, self.metrics_sink, move || hook());

        #[cfg(feature = "pipeline")]
        if let Some(worker) = worker {
            recorder.attach_worker(worker);
        }

        recorder.set_recording_start_hooks(self.recording_start_hooks);

        if self.enabled {
            recorder.enable();
        }
        recorder
    }

    /// Build without recording. [`Recorder::enable`] starts it later.
    ///
    /// Use for a recorder that should exist but stay quiet until something turns
    /// it on; for permanently-off telemetry prefer [`recorder_disabled`], which
    /// allocates no writer at all.
    pub fn paused(mut self) -> Self {
        self.enabled = false;
        self
    }
}

// TODO(tokio-as-source): now that tokio attaches to a built `Recorder`, this
// trait has a single implementor. Fold it into inherent `RecorderBuilder`
// methods once the `RecorderPerfExt` blanket impl can be reworked.
/// A builder that can register [`Source`]s.
///
/// Implemented by [`RecorderBuilder`]; the `.with_*()` perf-source sugar is
/// built on top of it.
pub trait RecorderSourceExt: Sized {
    /// Register a [`Source`] with the underlying recording recorder.
    fn source(self, source: impl Source + 'static) -> Self;

    /// Register a hook run once, with the live [`Dial9Handle`], when the recorder
    /// starts recording.
    fn on_recording_start(self, hook: impl FnOnce(&Dial9Handle) + Send + 'static) -> Self;

    /// Register a hook run on each of dial9's own threads (the flush thread and,
    /// with `pipeline`, the background worker) before it starts, returning a
    /// teardown run when it stops. Defaults to a no-op.
    fn on_recording_thread_start<F, T>(self, hook: F) -> Self
    where
        F: Fn() -> T + Send + Sync + 'static,
        T: FnOnce() + Send + 'static;

    /// Register a callback that dial9 invokes on the flush thread at the config's
    /// interval to emit custom events. Sugar for [`source`](Self::source) with a
    /// [`CustomEventsSource`](crate::custom_events::CustomEventsSource). Not
    /// tokio-coupled — works on the plain recorder and the tokio builder.
    fn with_custom_events<F>(
        self,
        config: crate::custom_events::CustomEventsConfig,
        callback: F,
    ) -> Self
    where
        F: for<'a> FnMut(&mut crate::custom_events::CustomEventsContext<'a>) + Send + 'static,
    {
        self.source(crate::custom_events::CustomEventsSource::new(
            config, callback,
        ))
    }
}

impl<M: BufferMode> RecorderSourceExt for RecorderBuilder<M> {
    fn source(mut self, source: impl Source + 'static) -> Self {
        self.sources.push(Box::new(source));
        self
    }

    fn on_recording_start(mut self, hook: impl FnOnce(&Dial9Handle) + Send + 'static) -> Self {
        self.recording_start_hooks.push(Box::new(hook));
        self
    }

    fn on_recording_thread_start<F, T>(mut self, hook: F) -> Self
    where
        F: Fn() -> T + Send + Sync + 'static,
        T: FnOnce() + Send + 'static,
    {
        self.thread_init = Arc::new(move || Box::new(hook()) as Box<dyn FnOnce() + Send>);
        self
    }
}

#[cfg(feature = "pipeline")]
impl<M: BufferMode> RecorderBuilder<M> {
    /// Append a segment processor (compress, symbolize, upload, write-back),
    /// replacing dial9's default pipeline with your own stages.
    pub fn pipe(mut self, processor: impl crate::pipeline::SegmentProcessor + 'static) -> Self {
        self.pipeline
            .get_or_insert_default()
            .push(Box::new(processor));
        self
    }

    /// Set the full processor pipeline at once, replacing dial9's default and
    /// anything added with [`pipe`](Self::pipe). Use this when you already have
    /// a built list, or `pipe` to append incrementally.
    pub fn processors(
        mut self,
        processors: Vec<Box<dyn crate::pipeline::SegmentProcessor>>,
    ) -> Self {
        self.pipeline = Some(processors);
        self
    }

    /// Replace write-back as the last stage of the default pipeline, so sealed
    /// segments are shipped elsewhere instead of written back to disk. This
    /// also makes processing meaningful even with no other stage, so the worker
    /// still runs.
    ///
    /// Ignored when a custom pipeline is set. The S3 uploader is wired up this
    /// way.
    pub fn terminal_processor(
        mut self,
        processor: impl crate::pipeline::SegmentProcessor + 'static,
    ) -> Self {
        self.terminal_processor = Some(Box::new(processor));
        self
    }

    /// How often the background worker polls for sealed segments.
    pub fn worker_poll_interval(mut self, interval: std::time::Duration) -> Self {
        self.worker_poll_interval = Some(interval);
        self
    }

    /// Trigger receiver switching the worker into on-demand dump mode; see
    /// [`crate::dump`]. `None` keeps continuous mode.
    pub fn trigger(mut self, trigger: crate::dump::DumpRx) -> Self {
        self.trigger = Some(trigger);
        self
    }

    /// Enable on-demand dump mode: the background worker runs the pipeline only
    /// when a dump is requested through the
    /// [`DumpTrigger`](crate::dump::DumpTrigger), reachable from any recording
    /// thread via [`Dial9Handle::dump_trigger`](crate::handle::Dial9Handle::dump_trigger).
    /// Pass `|_| {}` for the default, or `|t| { t.debounce(window); }` to
    /// coalesce bursts.
    pub fn with_dump_trigger<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(&mut crate::dump::DumpTriggerConfig),
    {
        let mut config = crate::dump::DumpTriggerConfig::new();
        configure(&mut config);
        let (mut trigger, rx) = crate::dump::channel();
        if let Some(window) = config.debounce_window() {
            trigger = trigger.with_debounce(window);
        }
        self.trigger = Some(rx);
        self.pending_dump_trigger = Some(trigger);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{DiskBuffer, MemoryBuffer};
    use crate::source::FlushContext;
    use dial9_trace_format::TraceEvent;
    use dial9_trace_format::decoder::Decoder;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    #[derive(Debug, serde::Deserialize, TraceEvent)]
    struct TestEvent {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        value: u64,
    }

    /// A `Source` that emits one `TestEvent` on its first flush.
    struct OnceSource {
        emitted: bool,
        value: u64,
    }

    impl Source for OnceSource {
        fn flush(&mut self, ctx: &FlushContext<'_>) {
            if !self.emitted {
                self.emitted = true;
                ctx.record_event(&TestEvent {
                    timestamp_ns: clock::clock_monotonic_ns(),
                    value: self.value,
                });
            }
        }
        fn name(&self) -> &'static str {
            "once"
        }
    }

    fn sealed_segment(dir: &Path) -> PathBuf {
        std::fs::read_dir(dir)
            .expect("trace dir readable")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.ends_with(".bin") && !name.ends_with(".active")
            })
            .expect("a sealed .bin segment")
    }

    fn decoded_test_values(bytes: &[u8]) -> Vec<u64> {
        let mut decoder = Decoder::new(bytes).expect("valid trace header");
        let mut values = Vec::new();
        decoder
            .for_each_event(|raw| {
                if raw.name == "TestEvent" {
                    let event: TestEvent = raw.deserialize().expect("TestEvent decodes");
                    values.push(event.value);
                }
            })
            .expect("decode events");
        values
    }

    /// A registered `Source` records to a real trace file without an async
    /// runtime. The final flush on `graceful_shutdown` runs the source.
    #[test]
    fn records_source_events_to_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).expect("writer");

        let recorder = recorder(writer)
            .segment_metadata([("service".to_string(), "recorder-test".to_string())])
            .source(OnceSource {
                emitted: false,
                value: 7,
            })
            .build();
        recorder.graceful_shutdown(Duration::ZERO);

        let bytes = std::fs::read(sealed_segment(dir.path())).expect("read segment");
        assert!(
            decoded_test_values(&bytes).contains(&7),
            "the source's event should round-trip through the trace file"
        );
    }

    /// `build()` starts recording.
    #[test]
    fn build_starts_recording() {
        let writer = MemoryBuffer::new(1 << 20).expect("writer");
        let recorder = recorder(writer).build();
        assert!(
            recorder.shared().expect("live recorder").is_enabled(),
            "build() must start recording"
        );
    }

    /// `paused()` builds a live recorder that is not yet recording.
    #[test]
    fn paused_build_waits_for_enable() {
        let writer = MemoryBuffer::new(1 << 20).expect("writer");
        let recorder = recorder(writer).paused().build();
        assert!(
            !recorder.shared().expect("live recorder").is_enabled(),
            "paused() must leave recording off"
        );
        // The handle tells the truth: connected but paused reports disabled.
        assert!(
            !recorder.handle().is_enabled(),
            "a paused handle must report disabled"
        );
        assert!(
            recorder.handle().shared().is_some(),
            "a paused handle is still connected"
        );
        recorder.enable();
        assert!(
            recorder.shared().expect("live recorder").is_enabled(),
            "recording on after enable()"
        );
        assert!(
            recorder.handle().is_enabled(),
            "handle enabled after enable()"
        );
    }

    /// `on_recording_start` hooks run once, with the handle, when recording
    /// starts — at `build()`, or at `enable()` when the build was paused.
    #[test]
    fn on_recording_start_runs_once_when_recording_starts() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let runs = StdArc::new(AtomicUsize::new(0));
        let runs_hook = StdArc::clone(&runs);
        let live = recorder(MemoryBuffer::new(1 << 20).expect("writer"))
            .on_recording_start(move |_handle| {
                runs_hook.fetch_add(1, Ordering::SeqCst);
            })
            .build();
        assert_eq!(runs.load(Ordering::SeqCst), 1, "hook runs at build");
        live.enable();
        assert_eq!(runs.load(Ordering::SeqCst), 1, "hook runs at most once");

        let paused_runs = StdArc::new(AtomicUsize::new(0));
        let paused_hook = StdArc::clone(&paused_runs);
        let paused = recorder(MemoryBuffer::new(1 << 20).expect("writer"))
            .on_recording_start(move |_handle| {
                paused_hook.fetch_add(1, Ordering::SeqCst);
            })
            .paused()
            .build();
        assert_eq!(
            paused_runs.load(Ordering::SeqCst),
            0,
            "paused build must not run the hook yet"
        );
        paused.enable();
        assert_eq!(paused_runs.load(Ordering::SeqCst), 1, "hook runs on enable");
    }

    /// Pipeline: `.pipe()` spawns the background worker for a runtime-agnostic
    /// recorder, and it processes the sealed segment on shutdown.
    #[cfg(feature = "pipeline")]
    #[test]
    fn pipe_runs_the_background_worker() {
        use crate::pipeline::{ProcessError, SegmentData, SegmentProcessor};
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug)]
        struct CountingProcessor(StdArc<AtomicUsize>);
        impl SegmentProcessor for CountingProcessor {
            fn name(&self) -> &'static str {
                "Counting"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(data) })
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).expect("writer");
        let processed = StdArc::new(AtomicUsize::new(0));

        let recorder = recorder(writer)
            .source(OnceSource {
                emitted: false,
                value: 11,
            })
            .pipe(CountingProcessor(StdArc::clone(&processed)))
            .build();
        recorder.graceful_shutdown(Duration::from_secs(5));

        assert!(
            processed.load(Ordering::SeqCst) >= 1,
            "the background worker should process the sealed segment"
        );
    }

    #[test]
    fn segment_metadata_merges_last_key_wins() {
        let mut md = vec![
            ("service".to_string(), "checkout".to_string()),
            ("region".to_string(), "us-east-1".to_string()),
        ];
        super::merge_segment_metadata(
            &mut md,
            [
                ("region".to_string(), "eu-west-1".to_string()),
                ("bucket".to_string(), "traces".to_string()),
            ],
        );

        assert!(md.contains(&("service".to_string(), "checkout".to_string())));
        assert!(md.contains(&("region".to_string(), "eu-west-1".to_string())));
        assert!(md.contains(&("bucket".to_string(), "traces".to_string())));
        assert!(
            !md.iter().any(|(k, v)| k == "region" && v == "us-east-1"),
            "the colliding key's old value must be gone"
        );
    }
    /// Default pipeline behavior: source-requested stages run automatically,
    /// then compression and write-back.
    #[cfg(feature = "pipeline")]
    #[test]
    fn source_stage_joins_the_default_pipeline() {
        use crate::pipeline::{ProcessError, SegmentData, SegmentProcessor};
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct MarkerStage(StdArc<AtomicUsize>);
        impl SegmentProcessor for MarkerStage {
            fn name(&self) -> &'static str {
                "Marker"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(data) })
            }
        }

        /// A source that contributes a pipeline stage, as the CPU profiler does.
        struct StagedSource(StdArc<AtomicUsize>);
        impl Source for StagedSource {
            fn flush(&mut self, _ctx: &FlushContext<'_>) {}
            fn name(&self) -> &'static str {
                "staged"
            }
            fn segment_processor(&mut self) -> Option<Box<dyn SegmentProcessor>> {
                Some(Box::new(MarkerStage(StdArc::clone(&self.0))))
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).expect("writer");
        let ran = StdArc::new(AtomicUsize::new(0));

        let recorder = recorder(writer)
            .source(StagedSource(StdArc::clone(&ran)))
            .source(OnceSource {
                emitted: false,
                value: 3,
            })
            .build();
        recorder.graceful_shutdown(Duration::from_secs(5));

        assert!(
            ran.load(Ordering::SeqCst) >= 1,
            "the source's stage should run in the default pipeline"
        );
        let gzipped = std::fs::read_dir(dir.path())
            .expect("trace dir readable")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .any(|p| p.to_string_lossy().ends_with(".bin.gz"));
        assert!(gzipped, "the default pipeline compresses and writes back");
    }

    /// With no stages and no uploader, the worker does not spawn.
    #[cfg(feature = "pipeline")]
    #[test]
    fn default_pipeline_is_empty_without_stages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).expect("writer");

        let recorder = recorder(writer)
            .source(OnceSource {
                emitted: false,
                value: 5,
            })
            .build();
        recorder.graceful_shutdown(Duration::from_secs(5));

        let compressed = std::fs::read_dir(dir.path())
            .expect("trace dir readable")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .any(|p| p.to_string_lossy().ends_with(".gz"));
        assert!(
            !compressed,
            "no stages means no worker, so nothing is gzipped"
        );
        // The trace itself is still readable, straight off the writer.
        let bytes = std::fs::read(sealed_segment(dir.path())).expect("read segment");
        assert_eq!(decoded_test_values(&bytes), vec![5]);
    }

    /// A terminal stage ships segments elsewhere, so the worker should run even
    /// when no source contributes a stage. It replaces write-back.
    #[cfg(feature = "pipeline")]
    #[test]
    fn terminal_processor_replaces_write_back() {
        use crate::pipeline::{ProcessError, SegmentData, SegmentProcessor};
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Uploader(StdArc<AtomicUsize>);
        impl SegmentProcessor for Uploader {
            fn name(&self) -> &'static str {
                "Uploader"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(data) })
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).expect("writer");
        let uploaded = StdArc::new(AtomicUsize::new(0));

        let recorder = recorder(writer)
            .source(OnceSource {
                emitted: false,
                value: 9,
            })
            .terminal_processor(Uploader(StdArc::clone(&uploaded)))
            .build();
        recorder.graceful_shutdown(Duration::from_secs(5));

        assert!(
            uploaded.load(Ordering::SeqCst) >= 1,
            "the terminal stage runs on its own"
        );
        let written_back = std::fs::read_dir(dir.path())
            .expect("trace dir readable")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .any(|p| p.to_string_lossy().ends_with(".bin.gz"));
        assert!(!written_back, "the terminal stage takes write-back's place");
    }

    #[test]
    fn recording_thread_hook_runs_on_dial9_threads() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        fn build_and_shut_down<M: BufferMode + Send + 'static>(
            writer: crate::buffer::SegmentWriter<M>,
        ) -> (usize, usize) {
            let started = StdArc::new(AtomicUsize::new(0));
            let stopped = StdArc::new(AtomicUsize::new(0));
            let (s, t) = (StdArc::clone(&started), StdArc::clone(&stopped));

            let recorder =
                RecorderSourceExt::on_recording_thread_start(recorder(writer), move || {
                    s.fetch_add(1, Ordering::SeqCst);
                    let t = StdArc::clone(&t);
                    move || {
                        t.fetch_add(1, Ordering::SeqCst);
                    }
                })
                .build();
            recorder.graceful_shutdown(Duration::from_secs(5));

            (
                started.load(Ordering::SeqCst),
                stopped.load(Ordering::SeqCst),
            )
        }

        let (started, stopped) = build_and_shut_down(MemoryBuffer::new(64 * 1024).unwrap());
        assert!(
            started >= 1,
            "the hook should run on the flush thread, ran {started} times"
        );
        assert_eq!(
            started, stopped,
            "every thread that ran the hook should run its teardown"
        );
    }
}

//! Recording-session builder.
//!
//! [`recorder`] assembles a [`CoreSession`](crate::session::CoreSession): it
//! builds the shared bus, registers your [`Source`](crate::source::Source)s,
//! spawns the flush thread (and, with the `pipeline` feature, the background
//! worker), and starts recording.
//!
//! ```no_run
//! use dial9_core::writer::DiskWriter;
//! let session = dial9_core::recorder::recorder(DiskWriter::single_file("/tmp/trace.bin")?)
//!     .build_and_start();
//! // record events through `session.handle()`.
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! This is the assembler layered above the low-level [`CoreSession::start`](crate::session::CoreSession::start),
//! which requires a pre-built [`SharedState`](crate::shared_state::SharedState) with sources
//! already registered. The Tokio integration builds on the same builder internally.

use crate::clock;
use crate::primitives::sync::Arc;
use crate::session::CoreSession;
use crate::shared_state::SharedState;
use crate::source::Source;
use crate::writer::{Disk, SegmentWriter, WriterMode};

/// A reusable per-thread hook: run on each recording thread, returning a
/// teardown closure. Reusable (`Fn`) because both the flush thread and the
/// background worker each need a fresh `FnOnce`.
type RecordingThreadHook = Arc<dyn Fn() -> Box<dyn FnOnce() + Send> + Send + Sync>;

fn noop_thread_hook() -> RecordingThreadHook {
    Arc::new(|| Box::new(|| {}) as Box<dyn FnOnce() + Send>)
}

/// Begin a recording session backed by `writer`.
///
/// Register data sources with [`RecorderBuilder::source`], then
/// [`build`](RecorderBuilder::build) (session starts disabled) or
/// [`build_and_start`](RecorderBuilder::build_and_start).
pub fn recorder<M: WriterMode>(writer: SegmentWriter<M>) -> RecorderBuilder<M> {
    #[cfg(feature = "pipeline")]
    let worker_trace_path = M::IS_DISK.then(|| writer.base_path().to_path_buf());
    RecorderBuilder {
        writer,
        sources: Vec::new(),
        segment_metadata: Vec::new(),
        metrics_sink: None,
        thread_init: noop_thread_hook(),
        #[cfg(feature = "pipeline")]
        processors: Vec::new(),
        #[cfg(feature = "pipeline")]
        worker_poll_interval: None,
        #[cfg(feature = "pipeline")]
        trigger: None,
        #[cfg(feature = "pipeline")]
        worker_trace_path,
    }
}

/// Builder for a runtime-agnostic [`CoreSession`]. See [`recorder`].
#[must_use = "call `.build()` (or `.build_and_start()`) to start the session"]
pub struct RecorderBuilder<M: WriterMode = Disk> {
    writer: SegmentWriter<M>,
    sources: Vec<Box<dyn Source>>,
    segment_metadata: Vec<(String, String)>,
    metrics_sink: Option<metrique::writer::BoxEntrySink>,
    thread_init: RecordingThreadHook,
    #[cfg(feature = "pipeline")]
    processors: Vec<Box<dyn crate::pipeline::SegmentProcessor>>,
    #[cfg(feature = "pipeline")]
    worker_poll_interval: Option<std::time::Duration>,
    #[cfg(feature = "pipeline")]
    trigger: Option<crate::dump::DumpRx>,
    #[cfg(feature = "pipeline")]
    worker_trace_path: Option<std::path::PathBuf>,
}

impl<M: WriterMode> std::fmt::Debug for RecorderBuilder<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecorderBuilder")
            .field("sources", &self.sources.len())
            .finish_non_exhaustive()
    }
}

impl<M: WriterMode> RecorderBuilder<M> {
    /// Register a [`Source`] drained by the flush thread each cycle.
    pub fn source(mut self, source: impl Source + 'static) -> Self {
        self.sources.push(Box::new(source));
        self
    }

    /// Static metadata written into every rotated segment header. Later calls
    /// replace earlier ones.
    pub fn segment_metadata(mut self, entries: impl IntoIterator<Item = (String, String)>) -> Self {
        self.segment_metadata = entries.into_iter().collect();
        self
    }

    /// Metrics sink for the flush (and, with `pipeline`, worker) threads.
    /// Defaults to discarding flush metrics.
    pub fn metrics_sink(mut self, sink: metrique::writer::BoxEntrySink) -> Self {
        self.metrics_sink = Some(sink);
        self
    }

    /// Hook run once on every recording thread (flush thread and background
    /// worker) before it starts, returning a teardown run when it stops. Use it
    /// to register/unregister the thread with a profiler. Defaults to a no-op.
    pub fn on_recording_thread_start<F, T>(mut self, hook: F) -> Self
    where
        F: Fn() -> T + Send + Sync + 'static,
        T: FnOnce() + Send + 'static,
    {
        self.thread_init = Arc::new(move || Box::new(hook()) as Box<dyn FnOnce() + Send>);
        self
    }

    /// Start the session. Recording begins **disabled**; call
    /// [`CoreSession::enable`] (or use [`build_and_start`](Self::build_and_start)).
    pub fn build(self) -> CoreSession {
        let shared = Arc::new(SharedState::new(clock::clock_monotonic_ns()));

        #[allow(unused_mut)]
        let mut writer = self.writer;
        if !self.segment_metadata.is_empty() {
            writer.update_segment_metadata(self.segment_metadata);
        }

        for source in self.sources {
            shared.push_source(source);
        }

        // The worker borrows `&writer`, so it must be spawned before the writer
        // moves into `CoreSession::start`.
        #[cfg(feature = "pipeline")]
        let worker = if self.processors.is_empty() {
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
                .maybe_trace_path(self.worker_trace_path)
                .poll_interval(poll)
                .processors(self.processors)
                .metrics_sink(metrics)
                .maybe_trigger(self.trigger)
                .build();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let hook = self.thread_init.clone();
            crate::worker::spawn(&writer, config, rx, move || hook())
                .map(|wt| crate::session::WorkerHandle::new(tx, wt))
        };

        let hook = self.thread_init.clone();
        #[allow(unused_mut)]
        let mut session = CoreSession::start(shared, writer, self.metrics_sink, move || hook());

        #[cfg(feature = "pipeline")]
        if let Some(worker) = worker {
            session.attach_worker(worker);
        }

        session
    }

    /// Start the session and immediately begin recording.
    pub fn build_and_start(self) -> CoreSession {
        let session = self.build();
        session.enable();
        session
    }
}

#[cfg(feature = "pipeline")]
impl<M: WriterMode> RecorderBuilder<M> {
    /// Append a segment processor (compress, symbolize, upload, write-back).
    /// The background worker is spawned only when at least one processor is set
    /// and the writer is filesystem-backed.
    pub fn pipe(mut self, processor: impl crate::pipeline::SegmentProcessor + 'static) -> Self {
        self.processors.push(Box::new(processor));
        self
    }

    /// Set the full processor pipeline at once, replacing any added with
    /// [`pipe`](Self::pipe). Use this when you already have a built list,
    /// or `pipe` to append incrementally.
    pub fn processors(
        mut self,
        processors: Vec<Box<dyn crate::pipeline::SegmentProcessor>>,
    ) -> Self {
        self.processors = processors;
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

    /// Override the trace path the worker reports (dir/stem for its logs).
    /// Defaults to the writer's base path on disk, `None` for in-memory.
    pub fn worker_trace_path(mut self, path: impl Into<Option<std::path::PathBuf>>) -> Self {
        self.worker_trace_path = path.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FlushContext;
    use crate::writer::{DiskWriter, InMemoryWriter};
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

    /// Stories 1 + 7: a registered `Source` records to a real trace file with
    /// no async runtime. The final flush on `graceful_shutdown` runs the source.
    #[test]
    fn records_source_events_to_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = DiskWriter::single_file(dir.path().join("trace.bin")).expect("writer");

        let session = recorder(writer)
            .segment_metadata([("service".to_string(), "recorder-test".to_string())])
            .source(OnceSource {
                emitted: false,
                value: 7,
            })
            .build_and_start();
        session
            .graceful_shutdown(Duration::ZERO)
            .expect("graceful shutdown");

        let bytes = std::fs::read(sealed_segment(dir.path())).expect("read segment");
        assert!(
            decoded_test_values(&bytes).contains(&7),
            "the source's event should round-trip through the trace file"
        );
    }

    /// The session is born with recording off; `build()` (without `_and_start`)
    /// leaves it disabled until `enable()`.
    #[test]
    fn build_starts_disabled() {
        let writer = InMemoryWriter::new(1 << 20).expect("writer");
        let session = recorder(writer).build();
        assert!(
            !session.shared().expect("live session").is_enabled(),
            "recording must be off before enable()"
        );
        session.enable();
        assert!(
            session.shared().expect("live session").is_enabled(),
            "recording on after enable()"
        );
    }

    /// Pipeline: `.pipe()` spawns the background worker for a runtime-agnostic
    /// session, and it processes the sealed segment on shutdown.
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
        let writer = DiskWriter::single_file(dir.path().join("trace.bin")).expect("writer");
        let processed = StdArc::new(AtomicUsize::new(0));

        let session = recorder(writer)
            .source(OnceSource {
                emitted: false,
                value: 11,
            })
            .pipe(CountingProcessor(StdArc::clone(&processed)))
            .build_and_start();
        session
            .graceful_shutdown(Duration::from_secs(5))
            .expect("graceful shutdown drains the worker");

        assert!(
            processed.load(Ordering::SeqCst) >= 1,
            "the background worker should process the sealed segment"
        );
    }
}

#![cfg(all(feature = "pipeline", feature = "test-util"))]

use dial9_core::buffer::{Memory, MemoryBuffer};
use dial9_core::pipeline::{DisableRecording, ProcessError, SegmentData, SegmentProcessor};
use dial9_core::recorder::{RecorderBuilder, recorder};
use dial9_core::source::{FlushContext, Source};
use std::error::Error;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

struct FailingProcessor {
    error: Option<io::Error>,
    initialized: Arc<AtomicUsize>,
}

impl SegmentProcessor for FailingProcessor {
    fn name(&self) -> &'static str {
        "failing-initializer"
    }

    fn initialize(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        self.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(Err(self
            .error
            .take()
            .expect("initialized only once"))))
    }

    fn process(
        &mut self,
        _data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        panic!("failed processor must not process segments")
    }
}

struct WorkerGate {
    release: mpsc::Sender<()>,
    stopped: mpsc::Receiver<()>,
    initialized: Arc<AtomicUsize>,
}

impl WorkerGate {
    fn finish(self) {
        self.release.send(()).expect("worker is waiting");
        self.stopped
            .recv_timeout(TIMEOUT)
            .expect("worker teardown completes");
        assert_eq!(self.initialized.load(Ordering::SeqCst), 1);
    }
}

fn failing_recorder(error: io::Error) -> (RecorderBuilder<Memory>, WorkerGate) {
    let (release_tx, release_rx) = mpsc::channel();
    let (stopped_tx, stopped_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let initialized = Arc::new(AtomicUsize::new(0));
    let builder = recorder(MemoryBuffer::new(1 << 20).expect("writer"))
        .pipe(FailingProcessor {
            error: Some(error),
            initialized: initialized.clone(),
        })
        .on_recording_thread_start(move || {
            let is_worker = std::thread::current().name() == Some("dial9-worker");
            if is_worker {
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(TIMEOUT)
                    .expect("test releases the worker");
            }
            let stopped_tx = stopped_tx.clone();
            move || {
                if is_worker {
                    stopped_tx.send(()).expect("test waits for worker teardown");
                }
            }
        });
    (
        builder,
        WorkerGate {
            release: release_tx,
            stopped: stopped_rx,
            initialized,
        },
    )
}

fn permanent_failure() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        DisableRecording::new(io::Error::other("destination authorization failed")),
    )
}

#[test]
fn failure_before_initial_enable_cannot_be_reenabled() {
    let (builder, worker) = failing_recorder(permanent_failure());
    let recorder = builder.paused().build();
    worker.finish();

    let handle = recorder.handle();
    assert!(handle.is_connected());
    assert!(!handle.is_stopped());
    handle.enable();
    assert!(!handle.is_enabled());
    recorder.enable();
    assert!(!handle.is_enabled());
    handle.disable();
    recorder.enable();
    assert!(!handle.is_enabled());
    handle.record_event_with(|| -> dial9_core::format::ClockSyncEvent {
        panic!("permanently disabled recording must not construct events")
    });
}

#[test]
fn paused_failure_refuses_enable_without_running_start_hooks() {
    let (builder, worker) = failing_recorder(permanent_failure());
    let starts = Arc::new(AtomicUsize::new(0));
    let hook_starts = starts.clone();
    let recorder = builder
        .on_recording_start(move |_| {
            hook_starts.fetch_add(1, Ordering::SeqCst);
        })
        .paused()
        .build();
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    worker.finish();

    recorder.enable();
    recorder.disable();
    recorder.enable();
    assert!(!recorder.handle().is_enabled());
    assert_eq!(starts.load(Ordering::SeqCst), 0);
}

#[test]
fn initialization_failure_disables_already_enabled_recording() {
    let (builder, worker) = failing_recorder(permanent_failure());
    let recorder = builder.build();
    assert!(recorder.handle().is_enabled());
    worker.finish();

    assert!(!recorder.handle().is_enabled());
    recorder.enable();
    assert!(!recorder.handle().is_enabled());
}

#[test]
fn nested_io_errors_do_not_hide_permanent_failure() {
    for depth in 1..=3 {
        let mut error = permanent_failure();
        for _ in 0..depth {
            error = io::Error::other(error);
        }
        let (builder, worker) = failing_recorder(error);
        let recorder = builder.build();
        worker.finish();

        assert!(!recorder.handle().is_enabled(), "I/O wrapper depth {depth}");
        recorder.enable();
        assert!(!recorder.handle().is_enabled(), "I/O wrapper depth {depth}");
    }
}

#[derive(Debug)]
struct InitializationContext(io::Error);

impl std::fmt::Display for InitializationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "destination setup failed: {}", self.0)
    }
}

impl Error for InitializationContext {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

#[test]
fn error_sources_and_io_payloads_are_both_checked_for_permanent_failure() {
    let error = io::Error::other(InitializationContext(io::Error::other(permanent_failure())));
    let (builder, worker) = failing_recorder(error);
    let recorder = builder.build();
    worker.finish();

    assert!(!recorder.handle().is_enabled());
    recorder.enable();
    assert!(!recorder.handle().is_enabled());
}

struct NoopSource;

impl Source for NoopSource {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn flush(&mut self, _ctx: &FlushContext<'_>) {}
}

#[test]
fn late_initialization_failure_preserves_stopped_handle_after_drop() {
    let (builder, worker) = failing_recorder(permanent_failure());
    let recorder = builder.build();
    let handle = recorder.handle().clone();
    drop(recorder);
    assert!(handle.is_stopped());

    worker.finish();
    assert!(handle.is_stopped());
    handle.enable();
    assert!(!handle.is_enabled());
    assert!(
        handle
            .with_source_or_insert::<NoopSource, ()>(
                || panic!("stopped recorder must reject new sources"),
                |_| panic!("stopped recorder must reject source access"),
            )
            .is_none()
    );
}

#[test]
fn ordinary_initialization_failure_allows_enable_and_start_hooks_once() {
    let (builder, worker) = failing_recorder(io::Error::other("optional pipeline failed"));
    let starts = Arc::new(AtomicUsize::new(0));
    let hook_starts = starts.clone();
    let recorder = builder
        .on_recording_start(move |handle| {
            assert!(handle.is_enabled());
            hook_starts.fetch_add(1, Ordering::SeqCst);
        })
        .paused()
        .build();
    worker.finish();

    recorder.handle().enable();
    assert!(recorder.handle().is_enabled());
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    recorder.enable();
    recorder.enable();
    recorder.disable();
    recorder.enable();
    assert!(recorder.handle().is_enabled());
    assert_eq!(starts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn initialization_error_preserves_processor_and_io_context() {
    let source = io::Error::from_raw_os_error(libc::EACCES);
    let message = source.to_string();
    let error = dial9_core::test_util::run_pipeline_continuous(
        Vec::new(),
        vec![Box::new(FailingProcessor {
            error: Some(source),
            initialized: Arc::new(AtomicUsize::new(0)),
        })],
        Duration::from_secs(1),
    )
    .await
    .expect_err("initialization fails");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        error.to_string(),
        format!("processor failing-initializer initialization failed: {message}")
    );
    let source = error
        .get_ref()
        .expect("processor context")
        .source()
        .expect("original I/O error")
        .downcast_ref::<io::Error>()
        .expect("I/O error retains its type");
    assert_eq!(source.raw_os_error(), Some(libc::EACCES));
}

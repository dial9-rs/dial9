//! Processor order and pipeline lifecycle, published for
//! [`Dial9Handle::pipeline_status`](crate::handle::Dial9Handle::pipeline_status).

use crate::pipeline::{PipelineProcessor, PipelineStatus, StopCause};
// Shuttle-aware, so the exit guard racing shutdown is explored.
use crate::primitives::sync::atomic::{AtomicUsize, Ordering};
use crate::primitives::sync::{Arc, Mutex};

/// `phase` before the first `initialize()`.
const NOT_STARTED: usize = usize::MAX - 2;
const RUNNING: usize = usize::MAX - 1;
const STOPPED: usize = usize::MAX;

/// What a handle reads about the pipeline, fixed when the recorder is built.
#[derive(Debug, Default)]
pub(crate) struct PipelineShared {
    /// On-demand dump trigger, when built with `with_dump_trigger`. Reached by
    /// application code through
    /// [`Dial9Handle::dump_trigger`](crate::handle::Dial9Handle::dump_trigger).
    pub(crate) dump_trigger: Option<crate::dump::DumpTrigger>,
    /// Processor order and pipeline state, when a pipeline worker runs.
    pub(crate) state: Option<Arc<PipelineState>>,
}

/// Written by the worker, read by any handle.
#[derive(Debug)]
pub(crate) struct PipelineState {
    /// Processors in pipeline order, fixed at build. Shared with every status,
    /// so reading one doesn't allocate.
    processors: std::sync::Arc<[PipelineProcessor]>,
    /// Index into `processors` of the processor initializing, or one of the
    /// constants above.
    phase: AtomicUsize,
    /// Why the worker stopped. Recorded before `phase` becomes `STOPPED`, so a
    /// reader that sees `STOPPED` finds a cause.
    cause: Mutex<Option<StopCause>>,
}

impl PipelineState {
    pub(crate) fn new(processors: Vec<&'static str>) -> Self {
        Self {
            processors: processors.into_iter().map(PipelineProcessor::new).collect(),
            phase: AtomicUsize::new(NOT_STARTED),
            cause: Mutex::new(None),
        }
    }

    // Release/Acquire keeps the status consistent with the processors: a reader that
    // sees `Running` also sees what their `initialize()` built, such as the S3
    // stage's client.
    pub(crate) fn set_initializing(&self, processor: usize) {
        debug_assert!(
            processor < self.processors.len(),
            "processor index out of range"
        );
        self.phase.store(processor, Ordering::Release);
    }

    pub(crate) fn set_running(&self) {
        self.phase.store(RUNNING, Ordering::Release);
    }

    pub(crate) fn status(&self) -> PipelineStatus {
        let processors = std::sync::Arc::clone(&self.processors);
        match self.phase.load(Ordering::Acquire) {
            STOPPED => PipelineStatus::Stopped {
                processors,
                cause: self
                    .cause
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("the stop guard records a cause before publishing STOPPED"),
            },
            RUNNING => PipelineStatus::Running { processors },
            i => PipelineStatus::Initializing {
                processor: self.processors.get(i).cloned(),
                processors,
            },
        }
    }
}

/// Marks the worker [`Stopped`](PipelineStatus::Stopped) when dropped, so every
/// exit path is covered: normal exit, initialization error, panic, drain
/// timeout.
pub(crate) struct StoppedOnDrop(pub(crate) Arc<PipelineState>);

impl StoppedOnDrop {
    /// Record why the worker stopped; the first cause recorded wins.
    pub(crate) fn record(&self, cause: StopCause) {
        self.0.cause.lock().unwrap().get_or_insert(cause);
    }

    /// Record that the processor being initialized failed.
    pub(crate) fn record_initialize_failed(&self) {
        let phase = self.0.phase.load(Ordering::Acquire);
        let processor = self
            .0
            .processors
            .get(phase)
            .cloned()
            .expect("an initialize() error comes from the processor `phase` points at");
        self.record(StopCause::InitializeFailed { processor });
    }
}

impl Drop for StoppedOnDrop {
    fn drop(&mut self) {
        // Every exit records its cause first, so only a panic unwinding past
        // the guard leaves none.
        self.record(StopCause::Panicked);
        self.0.phase.store(STOPPED, Ordering::Release);
    }
}

#[cfg(all(test, not(shuttle)))]
mod tests {
    use super::{PipelineState, StoppedOnDrop};
    use crate::buffer::MemoryBuffer;
    use crate::pipeline::{
        PipelineProcessor, PipelineStatus, ProcessError, SegmentData, SegmentProcessor, StopCause,
    };
    use crate::recorder::recorder;
    use crate::recording::Recorder;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    type ProcessFuture<'a> =
        Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + 'a>>;
    type InitFuture<'a> = Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + 'a>>;

    const INIT_DELAY: Duration = Duration::from_millis(200);

    /// Passes segments through; `initialize()` hangs, fails, or returns.
    struct Stage {
        name: &'static str,
        init: Init,
    }

    #[derive(Clone, Copy)]
    enum Init {
        Ok,
        Hang,
        Fail,
        /// Returns after a virtual delay in a paused-time test.
        SlowOk,
        /// Fails after a virtual delay in a paused-time test.
        SlowFail,
    }

    impl SegmentProcessor for Stage {
        fn name(&self) -> &'static str {
            self.name
        }
        fn initialize(&mut self) -> InitFuture<'_> {
            match self.init {
                Init::Ok => Box::pin(std::future::ready(Ok(()))),
                Init::Hang => Box::pin(std::future::pending()),
                Init::Fail => Box::pin(std::future::ready(Err(std::io::Error::other(
                    "test stage failed to initialize",
                )))),
                Init::SlowOk => Box::pin(async {
                    tokio::time::sleep(INIT_DELAY).await;
                    Ok(())
                }),
                Init::SlowFail => Box::pin(async {
                    tokio::time::sleep(INIT_DELAY).await;
                    Err(std::io::Error::other("test stage failed to initialize"))
                }),
            }
        }
        fn process(&mut self, data: SegmentData) -> ProcessFuture<'_> {
            Box::pin(std::future::ready(Ok(data)))
        }
    }

    fn build(stages: &[(&'static str, Init)]) -> Recorder {
        let processors = stages
            .iter()
            .map(|&(name, init)| Box::new(Stage { name, init }) as Box<dyn SegmentProcessor>)
            .collect();
        recorder(MemoryBuffer::new(1 << 20).unwrap())
            .processors(processors)
            .build()
    }

    fn wait_for(rec: &Recorder, want: impl Fn(&PipelineStatus) -> bool) -> PipelineStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = rec.handle().pipeline_status();
            if want(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "unexpected status: {status:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn processors_in_pipeline_order_then_running_then_stopped() {
        let rec = build(&[("First", Init::Ok), ("Second", Init::Ok)]);
        let status = rec.handle().pipeline_status();
        let names: Vec<_> = status
            .processors()
            .iter()
            .map(PipelineProcessor::name)
            .collect();
        assert_eq!(names, ["First", "Second"]);
        let running = wait_for(&rec, |s| matches!(s, PipelineStatus::Running { .. }));
        let handle = rec.handle().clone();
        rec.graceful_shutdown(Duration::from_secs(5));
        let stopped = handle.pipeline_status();
        assert!(matches!(
            stopped,
            PipelineStatus::Stopped {
                cause: StopCause::Exited,
                ..
            }
        ));
        assert_eq!(stopped.processors(), running.processors());
        // Earlier snapshots don't change when the live pipeline changes.
        assert!(matches!(running, PipelineStatus::Running { .. }));
    }

    #[test]
    fn names_the_processor_whose_initialize_hangs() {
        let rec = build(&[("First", Init::Ok), ("Hanging", Init::Hang)]);
        wait_for(&rec, |s| {
            matches!(s, PipelineStatus::Initializing {
                processor: Some(processor), ..
            } if processor.name() == "Hanging")
        });
        let handle = rec.handle().clone();
        // The drain timeout drops the hung `initialize()`.
        rec.graceful_shutdown(Duration::from_millis(10));
        assert!(matches!(
            handle.pipeline_status(),
            PipelineStatus::Stopped {
                cause: StopCause::DrainTimedOut,
                ..
            }
        ));
    }

    #[test]
    fn failed_initialize_is_stopped() {
        let rec = build(&[("First", Init::Ok), ("Failing", Init::Fail)]);
        wait_for(&rec, |s| {
            matches!(s, PipelineStatus::Stopped {
                cause: StopCause::InitializeFailed { processor }, ..
            } if processor.name() == "Failing")
        });
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    #[test]
    fn panicking_thread_start_hook_is_stopped() {
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .processors(vec![Box::new(Stage {
                name: "Stage",
                init: Init::Ok,
            }) as Box<dyn SegmentProcessor>])
            .on_recording_thread_start(|| {
                if std::thread::current().name() == Some("dial9-worker") {
                    panic!("worker thread hook");
                }
                || {}
            })
            .build();
        wait_for(&rec, |s| {
            matches!(
                s,
                PipelineStatus::Stopped {
                    cause: StopCause::Panicked,
                    ..
                }
            )
        });
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    #[test]
    fn no_pipeline_and_disconnected_have_distinct_statuses() {
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap()).build();
        assert_eq!(
            rec.handle().pipeline_status(),
            PipelineStatus::NotConfigured
        );
        assert!(rec.handle().pipeline_status().processors().is_empty());
        rec.graceful_shutdown(Duration::from_secs(5));
        assert_eq!(
            crate::handle::Dial9Handle::disabled().pipeline_status(),
            PipelineStatus::Disconnected,
        );
        assert!(
            crate::handle::Dial9Handle::disabled()
                .pipeline_status()
                .processors()
                .is_empty()
        );
    }

    #[test]
    fn paused_recording_still_reports_the_configured_pipeline() {
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .processors(vec![Box::new(Stage {
                name: "Pending",
                init: Init::Hang,
            })])
            .paused()
            .build();
        assert!(!rec.handle().is_enabled());
        assert!(rec.handle().is_connected());
        wait_for(&rec, |s| {
            matches!(s, PipelineStatus::Initializing {
                processor: Some(processor), ..
            } if processor.name() == "Pending")
        });
        rec.graceful_shutdown(Duration::ZERO);
    }

    /// A panic unwinding past the guard is reported as `Panicked`.
    #[test]
    fn a_panic_past_the_guard_is_panicked() {
        let state = Arc::new(PipelineState::new(vec!["Stage"]));
        let guarded = Arc::clone(&state);
        let result = std::panic::catch_unwind(move || {
            let _stopped = StoppedOnDrop(guarded);
            panic!("worker panicked");
        });
        assert!(result.is_err());
        assert!(matches!(
            state.status(),
            PipelineStatus::Stopped {
                cause: StopCause::Panicked,
                ..
            }
        ));
    }

    /// A processor still in `initialize()` when shutdown starts finishes during
    /// the drain, and the worker exits normally.
    #[tokio::test(start_paused = true)]
    async fn initialize_finishing_during_the_drain_is_exited() {
        initialize_during_drain(Init::SlowOk, StopCause::Exited).await;
    }

    /// A processor whose `initialize()` fails during the drain is named.
    #[tokio::test(start_paused = true)]
    async fn initialize_failing_during_the_drain_is_initialize_failed() {
        initialize_during_drain(
            Init::SlowFail,
            StopCause::InitializeFailed {
                processor: PipelineProcessor::new("Slow"),
            },
        )
        .await;
    }

    async fn initialize_during_drain(init: Init, cause: StopCause) {
        let state = Arc::new(PipelineState::new(vec!["Slow"]));
        let mut config = super::super::BackgroundTaskConfig::builder()
            .processors(vec![
                Box::new(Stage { name: "Slow", init }) as Box<dyn SegmentProcessor>
            ])
            .build();
        config.set_state(state.clone());
        let fs = crate::fs::Fs::new_in_memory(1 << 20, 4096).unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let worker = super::super::run_background_task_inner(config, shutdown_rx, fs);
        tokio::pin!(worker);

        // Poll initialization until its timer is pending, without allowing
        // paused time to advance automatically.
        assert!(poll_once(worker.as_mut()).await.is_pending());
        let initializing = state.status();
        assert!(matches!(
            &initializing,
            PipelineStatus::Initializing { processor: Some(processor), .. }
                if processor.name() == "Slow"
        ));

        shutdown_tx.send(Duration::from_secs(5)).unwrap();
        // Initialization is still pending, so the ready shutdown message
        // enters the drain arm before we advance the initializer's timer.
        assert!(poll_once(worker.as_mut()).await.is_pending());
        assert_eq!(state.status(), initializing);
        tokio::time::advance(INIT_DELAY).await;
        // Completing in one poll proves this result finishes during the
        // drain, without silently advancing to its five-second deadline.
        assert!(poll_once(worker.as_mut()).await.is_ready());
        match state.status() {
            PipelineStatus::Stopped { cause: actual, .. } => assert_eq!(actual, cause),
            other => panic!("pipeline didn't stop after initialization: {other:?}"),
        }
    }

    async fn poll_once<F: Future>(mut future: Pin<&mut F>) -> std::task::Poll<F::Output> {
        std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx))).await
    }
}

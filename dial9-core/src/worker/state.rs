//! Stage order and worker lifecycle, published for
//! [`Dial9Handle::pipeline_status`](crate::handle::Dial9Handle::pipeline_status).

use crate::pipeline::{PipelineStage, PipelineStatus, StopCause, WorkerState};
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
    /// Stage order and worker state, when a pipeline worker runs.
    pub(crate) state: Option<Arc<PipelineState>>,
}

/// Written by the worker, read by any handle.
#[derive(Debug)]
pub(crate) struct PipelineState {
    /// Stages in pipeline order, fixed at build. Shared with every status,
    /// so reading one doesn't allocate.
    stages: std::sync::Arc<[PipelineStage]>,
    /// Index into `stages` of the stage initializing, or one of the
    /// constants above.
    phase: AtomicUsize,
    /// Why the worker stopped. Recorded before `phase` becomes `STOPPED`.
    cause: Mutex<Option<StopCause>>,
}

impl PipelineState {
    pub(crate) fn new(stages: Vec<&'static str>) -> Self {
        Self {
            stages: stages.into_iter().map(PipelineStage::new).collect(),
            phase: AtomicUsize::new(NOT_STARTED),
            cause: Mutex::new(None),
        }
    }

    // Release/Acquire: a reader that sees `RUNNING` and then reads what a
    // stage's `initialize()` stored (e.g. the S3 client slot) sees the stored
    // value. The stage's own lock doesn't order that read after this one.
    pub(crate) fn set_initializing(&self, stage: usize) {
        debug_assert!(stage < self.stages.len(), "stage index out of range");
        self.phase.store(stage, Ordering::Release);
    }

    pub(crate) fn set_running(&self) {
        self.phase.store(RUNNING, Ordering::Release);
    }

    pub(crate) fn status(&self) -> PipelineStatus {
        let worker = match self.phase.load(Ordering::Acquire) {
            STOPPED => WorkerState::Stopped {
                cause: self
                    .cause
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("the stop guard records a cause before publishing STOPPED"),
            },
            RUNNING => WorkerState::Running,
            i => WorkerState::Initializing {
                stage: self.stages.get(i).cloned(),
            },
        };
        PipelineStatus {
            stages: std::sync::Arc::clone(&self.stages),
            worker,
        }
    }
}

/// Marks the worker [`Stopped`](WorkerState::Stopped) when dropped, so every
/// exit path is covered: normal exit, initialization error, panic, drain
/// timeout. An exit that recorded no cause is a panic before
/// `run_background_task_inner` got to record one.
pub(crate) struct StoppedOnDrop(pub(crate) Arc<PipelineState>);

impl StoppedOnDrop {
    /// Record why the worker stopped; the first cause recorded wins.
    pub(crate) fn record(&self, cause: StopCause) {
        self.0.cause.lock().unwrap().get_or_insert(cause);
    }

    /// Record that the stage being initialized failed.
    pub(crate) fn record_initialize_failed(&self) {
        let phase = self.0.phase.load(Ordering::Acquire);
        let stage = self.0.stages.get(phase).cloned();
        self.record(StopCause::InitializeFailed { stage });
    }
}

impl Drop for StoppedOnDrop {
    fn drop(&mut self) {
        self.record(StopCause::Panicked);
        self.0.phase.store(STOPPED, Ordering::Release);
    }
}

#[cfg(all(test, not(shuttle)))]
mod tests {
    use crate::buffer::MemoryBuffer;
    use crate::pipeline::{
        PipelineStage, ProcessError, SegmentData, SegmentProcessor, StopCause, WorkerState,
    };
    use crate::recorder::recorder;
    use crate::recording::Recorder;
    use std::future::Future;
    use std::pin::Pin;
    use std::time::{Duration, Instant};

    type ProcessFuture<'a> =
        Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + 'a>>;
    type InitFuture<'a> = Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + 'a>>;

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
    }

    impl SegmentProcessor for Stage {
        fn name(&self) -> &'static str {
            self.name
        }
        fn initialize(&mut self) -> InitFuture<'_> {
            match self.init {
                Init::Ok => Box::pin(std::future::ready(Ok(()))),
                Init::Hang => Box::pin(std::future::pending()),
                Init::Fail => Box::pin(std::future::ready(Err(std::io::Error::other("no")))),
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

    fn wait_for(rec: &Recorder, want: WorkerState) -> WorkerState {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = rec
                .handle()
                .pipeline_status()
                .unwrap()
                .worker_state()
                .clone();
            if state == want || Instant::now() > deadline {
                return state;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn stages_in_pipeline_order_then_running_then_stopped() {
        let rec = build(&[("First", Init::Ok), ("Second", Init::Ok)]);
        let status = rec.handle().pipeline_status().unwrap();
        let names: Vec<_> = status.stages().iter().map(PipelineStage::name).collect();
        assert_eq!(names, ["First", "Second"]);
        assert_eq!(wait_for(&rec, WorkerState::Running), WorkerState::Running);
        let handle = rec.handle().clone();
        rec.graceful_shutdown(Duration::from_secs(5));
        assert_eq!(
            handle.pipeline_status().unwrap().worker_state(),
            &WorkerState::Stopped {
                cause: StopCause::Exited
            }
        );
    }

    #[test]
    fn names_the_stage_whose_initialize_hangs() {
        let rec = build(&[("First", Init::Ok), ("Hanging", Init::Hang)]);
        let want = WorkerState::Initializing {
            stage: Some(PipelineStage::new("Hanging")),
        };
        assert_eq!(wait_for(&rec, want.clone()), want);
        let handle = rec.handle().clone();
        // The drain timeout drops the hung `initialize()`.
        rec.graceful_shutdown(Duration::from_millis(10));
        assert_eq!(
            handle.pipeline_status().unwrap().worker_state(),
            &WorkerState::Stopped {
                cause: StopCause::DrainTimedOut
            }
        );
    }

    #[test]
    fn failed_initialize_is_stopped() {
        let rec = build(&[("First", Init::Ok), ("Failing", Init::Fail)]);
        let want = WorkerState::Stopped {
            cause: StopCause::InitializeFailed {
                stage: Some(PipelineStage::new("Failing")),
            },
        };
        assert_eq!(wait_for(&rec, want.clone()), want);
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
        let want = WorkerState::Stopped {
            cause: StopCause::Panicked,
        };
        assert_eq!(wait_for(&rec, want.clone()), want);
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    #[test]
    fn no_pipeline_and_disconnected_have_no_status() {
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap()).build();
        assert!(rec.handle().pipeline_status().is_none());
        rec.graceful_shutdown(Duration::from_secs(5));
        assert!(
            crate::handle::Dial9Handle::disabled()
                .pipeline_status()
                .is_none()
        );
    }
}

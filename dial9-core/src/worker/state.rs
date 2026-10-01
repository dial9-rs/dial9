//! Stage order and worker lifecycle, published for
//! [`Dial9Handle::pipeline_status`](crate::handle::Dial9Handle::pipeline_status).

use crate::pipeline::{PipelineStatus, WorkerState};
// Shuttle-aware, so the exit guard racing shutdown is explored.
use crate::primitives::sync::Arc;
use crate::primitives::sync::atomic::{AtomicUsize, Ordering};

/// `phase` before the first `initialize()`.
const NOT_STARTED: usize = usize::MAX - 2;
const RUNNING: usize = usize::MAX - 1;
const STOPPED: usize = usize::MAX;

/// Written by the worker, read by any handle.
#[derive(Debug)]
pub(crate) struct PipelineState {
    /// Stage names in pipeline order, fixed at build.
    stages: Vec<&'static str>,
    /// Index into `stages` of the stage initializing, or one of the
    /// constants above.
    phase: AtomicUsize,
}

impl PipelineState {
    pub(crate) fn new(stages: Vec<&'static str>) -> Self {
        Self {
            stages,
            phase: AtomicUsize::new(NOT_STARTED),
        }
    }

    pub(crate) fn set_initializing(&self, stage: usize) {
        self.phase.store(stage, Ordering::Release);
    }

    pub(crate) fn set_running(&self) {
        self.phase.store(RUNNING, Ordering::Release);
    }

    pub(crate) fn status(&self) -> PipelineStatus {
        let worker = match self.phase.load(Ordering::Acquire) {
            STOPPED => WorkerState::Stopped,
            RUNNING => WorkerState::Running,
            i => WorkerState::Initializing {
                stage: self.stages.get(i).copied(),
            },
        };
        PipelineStatus {
            stages: self.stages.clone(),
            worker,
        }
    }
}

/// Marks the worker [`Stopped`](WorkerState::Stopped) when dropped, so every
/// exit path is covered: normal exit, initialization error, panic, drain
/// timeout.
pub(crate) struct StoppedOnDrop(pub(crate) Arc<PipelineState>);

impl Drop for StoppedOnDrop {
    fn drop(&mut self) {
        self.0.phase.store(STOPPED, Ordering::Release);
    }
}

#[cfg(all(test, not(shuttle)))]
mod tests {
    use crate::buffer::MemoryBuffer;
    use crate::pipeline::{ProcessError, SegmentData, SegmentProcessor, WorkerState};
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
            let state = rec.handle().pipeline_status().unwrap().worker();
            if state == want || Instant::now() > deadline {
                return state;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn stages_in_pipeline_order_then_running_then_stopped() {
        let rec = build(&[("First", Init::Ok), ("Second", Init::Ok)]);
        assert_eq!(
            rec.handle().pipeline_status().unwrap().stages(),
            ["First", "Second"]
        );
        assert_eq!(wait_for(&rec, WorkerState::Running), WorkerState::Running);
        let handle = rec.handle().clone();
        rec.graceful_shutdown(Duration::from_secs(5));
        assert_eq!(
            handle.pipeline_status().unwrap().worker(),
            WorkerState::Stopped
        );
    }

    #[test]
    fn names_the_stage_whose_initialize_hangs() {
        let rec = build(&[("First", Init::Ok), ("Hanging", Init::Hang)]);
        let want = WorkerState::Initializing {
            stage: Some("Hanging"),
        };
        assert_eq!(wait_for(&rec, want.clone()), want);
        rec.graceful_shutdown(Duration::from_millis(10));
    }

    #[test]
    fn failed_initialize_is_stopped() {
        let rec = build(&[("Failing", Init::Fail)]);
        assert_eq!(wait_for(&rec, WorkerState::Stopped), WorkerState::Stopped);
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
        assert_eq!(wait_for(&rec, WorkerState::Stopped), WorkerState::Stopped);
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

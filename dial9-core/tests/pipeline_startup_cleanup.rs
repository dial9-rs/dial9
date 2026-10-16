#![cfg(feature = "pipeline")]
use dial9_core::buffer::MemoryBuffer;
use dial9_core::pipeline::{
    PipelineStatus, ProcessError, SegmentData, SegmentProcessor, StopCause,
};
use dial9_core::recorder::recorder;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, mpsc};
use std::time::Duration;

struct GatedDrop {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
impl SegmentProcessor for GatedDrop {
    fn name(&self) -> &'static str {
        "GatedDrop"
    }
    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(std::future::ready(Ok(data)))
    }
}
impl Drop for GatedDrop {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        self.release.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}

#[test]
fn stopped_is_not_published_before_processor_cleanup_on_start_hook_panic() {
    let (panic_tx, panic_rx) = mpsc::channel();
    let panic_rx = Mutex::new(panic_rx);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
        .processors(vec![Box::new(GatedDrop {
            entered: entered_tx,
            release: release_rx,
        })])
        .on_recording_thread_start(move || {
            if std::thread::current().name() == Some("dial9-worker") {
                panic_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                panic!("start hook panic");
            }
            || {}
        })
        .build();
    let handle = rec.handle().clone();
    panic_tx.send(()).unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let during_drop = handle.pipeline_status();
    release_tx.send(()).unwrap();
    rec.graceful_shutdown(Duration::from_secs(5));
    assert!(matches!(
        handle.pipeline_status(),
        PipelineStatus::Stopped {
            cause: StopCause::Panicked { .. },
            ..
        },
    ));
    assert!(
        !matches!(during_drop, PipelineStatus::Stopped { .. }),
        "Stopped must wait for processor cleanup, got {during_drop:?}"
    );
}

//! Shuttle coverage for the `WorkerLoop` segment handoff.
//!
//! Drives `WorkerLoop` directly via `shuttle::future::block_on` rather than
//! through `run_background_task`'s real Tokio runtime: `run()`/
//! `run_continuous()` depend on `tokio::select!`/`tokio::time::sleep`, which
//! shuttle does not model.
//! The test-only `process_open_segments()` helper avoids all of that as long as
//! the processor pipeline itself never takes a retryable-failure path (the
//! only place `process_segments` awaits a real `tokio::time::sleep`).

use super::*;
use crate::pipeline::ProcessError;
use crate::primitives::sync::atomic::{AtomicUsize, Ordering};
use std::future::Future;
use std::io::Write;
use std::pin::Pin;

struct CountingProcessor(Arc<AtomicUsize>);

impl SegmentProcessor for CountingProcessor {
    fn name(&self) -> &'static str {
        "CountingProcessor"
    }

    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(data)
        })
    }
}

const WRITERS: usize = 3;
const SEGMENTS_PER_WRITER: u32 = 2;

/// Multiple writer threads seal segments into an in-memory `Fs` concurrently
/// with a worker thread draining+processing them (no real Tokio runtime —
/// see module doc). Every sealed segment must reach the processor exactly
/// once.
fn scenario_concurrent_seal_and_process() {
    let fs = Fs::new_in_memory(1 << 20, 4096).unwrap();
    let processed = Arc::new(AtomicUsize::new(0));

    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let fs = fs.clone();
            crate::primitives::thread::spawn(move || {
                for i in 0..SEGMENTS_PER_WRITER {
                    let index = w as u32 * SEGMENTS_PER_WRITER + i;
                    let mut h = fs.create_segment(Path::new("trace")).unwrap();
                    h.write_all(b"x").unwrap();
                    fs.seal(h, Path::new("trace"), index).unwrap();
                }
            })
        })
        .collect();

    let worker_fs = fs.clone();
    let worker_processed = processed.clone();
    let worker = crate::primitives::thread::spawn(move || {
        shuttle::future::block_on(async move {
            let stop = tokio_util::sync::CancellationToken::new();
            let mut worker = WorkerLoop::new(
                worker_fs.clone(),
                Duration::from_millis(1),
                vec![Box::new(CountingProcessor(worker_processed))],
                stop,
                metrique::writer::sink::DevNullSink::boxed(),
                None,
            )
            .await
            .expect("initialize worker");

            loop {
                let found = worker.process_open_segments().await;
                if found {
                    continue;
                }
                if !worker_fs.writer_done() {
                    shuttle::thread::yield_now();
                    continue;
                }
                // `found` may predate this writer_done() observation (it
                // came from a scan taken before we knew the writer was
                // done). Once writer_done() is true, nothing more will ever
                // be sealed, so re-scan and use *that* result — mirroring
                // run_continuous's drain-to-empty loop, which scans
                // again after observing writer_done rather than reusing a
                // stale result.
                if !worker.process_open_segments().await {
                    return;
                }
            }
        });
    });

    for w in writers {
        w.join().unwrap();
    }
    fs.mark_writer_done();
    worker.join().unwrap();

    assert_eq!(
        processed.load(Ordering::Relaxed),
        WRITERS * SEGMENTS_PER_WRITER as usize,
        "every sealed segment must be processed exactly once"
    );
}

#[test]
fn shuttle_worker_handoff_pct() {
    shuttle::check_pct(scenario_concurrent_seal_and_process, 2_000, 3);
}

#[test]
fn shuttle_worker_handoff_determinism() {
    shuttle::check_uncontrolled_nondeterminism(scenario_concurrent_seal_and_process, 2_000);
}

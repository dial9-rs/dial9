//! Shuttle coverage for the `WorkerLoop` segment handoff.
//!
//! Drives `WorkerLoop` directly via `shuttle::future::block_on` rather than
//! through `run_background_task`'s real Tokio runtime.
//!
//! `shuttle_worker_handoff` drives the real `run()`/`run_continuous()` path
//! (not the test-only `process_open_segments()` shortcut): this works for
//! the in-memory `Fs` backend because `wait_for_more`'s in-memory branch
//! only awaits `Fs::wait_for_wakeup()` (a plain `tokio::sync::Notify`, no
//! reactor/timer needed) rather than `tokio::time::sleep` -- that real timer
//! is only on the disk-backend branch, which this module never uses.
//!
//! `shuttle_dump_resolves_exactly_once` drives the real `run_triggered()`,
//! including its outer `select!` on stop/`sleep_until`/`wait_for_more` --
//! `primitives::time` and `shuttle_select!` make both shuttle-safe. The
//! processor pipeline itself never takes a retryable-failure path in either
//! scenario, which is the only other place `process_segments` awaits a real
//! `tokio::time::sleep`.

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

/// Fixed seal time every `spawn_writers` segment gets pinned to, so segment
/// metadata stays reproducible under shuttle's determinism replay instead
/// of reading real wall-clock time.
const FIXED_EPOCH_SECS_FOR_TEST: u64 = 1_000;

/// Spawns `WRITERS` threads, each sealing `SEGMENTS_PER_WRITER` segments
/// into `fs` with globally unique indices. Shared by every scenario in this
/// module that needs writer threads racing a worker/trigger. Takes the
/// `Arc<Fs>` by value (callers pass a clone) rather than by reference, so
/// each iteration's `fs.clone()` unambiguously bumps the `Arc` refcount.
fn spawn_writers(fs: Arc<Fs>) -> Vec<crate::primitives::thread::JoinHandle<()>> {
    (0..WRITERS)
        .map(|w| {
            let fs = fs.clone();
            crate::primitives::thread::spawn(move || {
                for i in 0..SEGMENTS_PER_WRITER {
                    let index = w as u32 * SEGMENTS_PER_WRITER + i;
                    let mut h = fs.create_segment(Path::new("trace")).unwrap();
                    h.write_all(b"x").unwrap();
                    fs.seal(h, Path::new("trace"), index).unwrap();
                    fs.set_seal_secs_for_test(index, FIXED_EPOCH_SECS_FOR_TEST);
                }
            })
        })
        .collect()
}

crate::shuttle_test! {
    num_iters = 2_000, depth = 3;
    // Multiple writer threads seal segments into an in-memory `Fs`
    // concurrently with a worker thread draining+processing them
    fn shuttle_worker_handoff() {
        let fs = Fs::new_in_memory(1 << 20, 4096).unwrap();
        let processed = Arc::new(AtomicUsize::new(0));

        let writers = spawn_writers(fs.clone());

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

                worker.run().await;
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
}

crate::shuttle_test! {
    num_iters = 500, depth = 3;
    // Drives the real `run_triggered()`, including its outer `select!` on
    // stop/`sleep_until`/`wait_for_more` -- `primitives::time` and
    // `shuttle_select!` make both shuttle-safe (see module doc). Multiple
    // writers seal segments concurrently with one on-demand dump request; it
    // must resolve exactly once regardless of whether any segments landed in
    // its window first.
    //
    // `dump_current_data` has an unbounded lookback (no window to pin), so
    // this verifies the resolve-exactly-once race itself, not any
    // window-matching behavior.
    fn shuttle_dump_resolves_exactly_once() {
        let fs = Fs::new_in_memory(1 << 20, 4096).unwrap();
        let processed = Arc::new(AtomicUsize::new(0));

        let writers = spawn_writers(fs.clone());

        let (trigger, rx) = crate::dump::channel();

        // Sole `DumpTrigger` handle -- moved (not cloned) into this thread,
        // so the channel closes deterministically once it drops, right
        // after this await resolves. The worker's `recv()` loop below
        // relies on that to know no more requests are ever coming.
        let trigger_handle = crate::primitives::thread::spawn(move || {
            shuttle::future::block_on(async move {
                let receipt = trigger.dump_current_data().await;
                assert!(
                    receipt.is_ok(),
                    "an on-demand dump request must resolve successfully: {receipt:?}"
                );
            });
        });

        let stop = tokio_util::sync::CancellationToken::new();
        let worker_fs = fs.clone();
        let worker_processed = processed.clone();
        let worker_stop = stop.clone();
        let worker = crate::primitives::thread::spawn(move || {
            shuttle::future::block_on(async move {
                let mut worker = WorkerLoop::new(
                    worker_fs.clone(),
                    Duration::from_millis(1),
                    vec![Box::new(CountingProcessor(worker_processed))],
                    worker_stop,
                    metrique::writer::sink::DevNullSink::boxed(),
                    Some(rx),
                )
                .await
                .expect("initialize worker");

                let rx = worker.trigger.take().expect("triggered mode");
                worker.run_triggered(rx).await;
            });
        });

        for w in writers {
            w.join().unwrap();
        }
        // Must join before marking the writer done: `run_triggered` checks
        // `writer_done()` at the top of its loop and rejects any
        // not-yet-registered request with `WorkerStopped`.
        trigger_handle.join().unwrap();
        fs.mark_writer_done();
        // `run_triggered` only notices `writer_done()` while idle if `stop`
        // is also cancelled -- mirror real callers by doing both.
        stop.cancel();
        worker.join().unwrap();

        assert!(
            processed.load(Ordering::Relaxed) <= WRITERS * SEGMENTS_PER_WRITER as usize,
            "dump must not double-dispatch a segment to the processor"
        );
    }
}

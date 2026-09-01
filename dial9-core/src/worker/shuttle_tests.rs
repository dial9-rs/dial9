//! Shuttle coverage for the `WorkerLoop` segment handoff.
//!
//! `shuttle_worker_handoff`/`shuttle_dump_resolves_exactly_once`/
//! `shuttle_dump_time_range_resolves_via_deadline` drive `WorkerLoop`
//! directly via `shuttle::future::block_on` rather than through
//! `run_background_task`'s real Tokio runtime.
//!
//! `shuttle_worker_handoff` drives the real `run()`/`run_continuous()` path
//! (not the test-only `process_open_segments()` shortcut): safe for the
//! in-memory `Fs` backend since `wait_for_more` only awaits a plain
//! `Notify` there, not `tokio::time::sleep` (disk-backend only).
//!
//! `shuttle_dump_resolves_exactly_once` drives the real `run_triggered()`,
//! including its `select!` on stop/`sleep_until`/`wait_for_more`.
//! `primitives::time`/`shuttle_select!` make both shuttle-safe. Neither
//! scenario's processor pipeline takes a retryable-failure path, the only
//! other place `process_segments` awaits a real `tokio::time::sleep`.
//!
//! `shuttle_background_task_contains_init_panic` drives
//! `run_background_task_inner` directly, covering the panic-containment
//! `catch_unwind`/shutdown race the `WorkerLoop`-level scenarios above
//! never reach. That function's drain-timeout race calls real
//! `tokio::time::timeout` (needs a live reactor), so it stays covered only
//! by `worker/tests.rs`'s real-runtime tests.

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

/// A minimal, valid trace segment header. An unparseable payload makes
/// `process_segments` fall back to `SystemTime::now()`, a real-clock read
/// shuttle can't control.
fn segment_bytes_with_epoch(epoch_secs: u64) -> Vec<u8> {
    use dial9_trace_format::encoder::Encoder;
    let mut enc = Encoder::new_to(Vec::new()).unwrap();
    enc.write_infallible(&crate::format::ClockSyncEvent {
        timestamp_ns: 1,
        realtime_ns: epoch_secs * 1_000_000_000,
    });
    enc.into_inner()
}

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
                    h.write_all(&segment_bytes_with_epoch(FIXED_EPOCH_SECS_FOR_TEST))
                        .unwrap();
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
    // Drives the real `run_triggered()`, including its `select!` on
    // stop/`sleep_until`/`wait_for_more` (see module doc). Multiple writers
    // seal segments concurrently with one on-demand dump request; it must
    // resolve exactly once regardless of window timing (`dump_current_data`
    // has unbounded lookback, so this verifies the race itself, not
    // window-matching).
    fn shuttle_dump_resolves_exactly_once() {
        let fs = Fs::new_in_memory(1 << 20, 4096).unwrap();
        let processed = Arc::new(AtomicUsize::new(0));

        let writers = spawn_writers(fs.clone());

        let (trigger, rx) = crate::dump::channel();

        // Sole `DumpTrigger` handle, moved (not cloned) into this thread, so
        // the channel closes deterministically once it drops, right after
        // this await resolves. The worker's `recv()` loop below relies on
        // that to know no more requests are ever coming.
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
        // is also cancelled. Mirror real callers by doing both.
        stop.cancel();
        worker.join().unwrap();

        assert!(
            processed.load(Ordering::Relaxed) <= WRITERS * SEGMENTS_PER_WRITER as usize,
            "dump must not double-dispatch a segment to the processor"
        );
    }
}

/// Must be nonzero so `ActiveDump::register` sets a real `deadline`. The
/// value itself doesn't matter, shuttle's `sleep_until` ignores it.
const LOOKFORWARD: Duration = Duration::from_millis(1);

// `determinism` SIGBUSes here because shuttle's
// default 60KB coroutine stack is too small for this call depth.
// Config issue, to be fixed crate-wide in a follow-up. Bumping
// it locally confirmed the SIGBUS goes away, but also surfaces an
// unresolved shuttle-internal panic during replay, so `determinism` stays
// `#[ignore]`d below until both are addressed. `pct` is unaffected.
mod shuttle_dump_time_range_resolves_via_deadline {
    use super::*;

    // Only scenario using a real deadline: every other one uses
    // `dump_current_data` (deadline always `None`), never exercising
    // `sleep_until`.
    //
    // `Duration::MAX` lookback matches every segment's fixed test
    // `seal_secs` regardless of real time (`dump_time_range` has no
    // `Lookback::Unbounded` to request instead).
    //
    // Joining `trigger_handle` before signaling shutdown guarantees `due()`
    // resolved the dump via the deadline.
    fn shuttle_dump_time_range_resolves_via_deadline() {
        let fs = Fs::new_in_memory(1 << 20, 4096).unwrap();
        let processed = Arc::new(AtomicUsize::new(0));

        let writers = spawn_writers(fs.clone());

        let (trigger, rx) = crate::dump::channel();

        let trigger_handle = crate::primitives::thread::spawn(move || {
            shuttle::future::block_on(async move {
                let receipt = trigger.dump_time_range(Duration::MAX, LOOKFORWARD).await;
                assert!(
                    receipt.is_ok(),
                    "a windowed dump request must resolve successfully: {receipt:?}"
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
        // Join order matters here too. See the module comment above.
        trigger_handle.join().unwrap();
        fs.mark_writer_done();
        stop.cancel();
        worker.join().unwrap();

        assert!(
            processed.load(Ordering::Relaxed) <= WRITERS * SEGMENTS_PER_WRITER as usize,
            "dump must not double-dispatch a segment to the processor"
        );
    }

    #[test]
    fn pct() {
        // Drain any count left over from an earlier test in this binary.
        crate::primitives::time::take_yield_pending_polls();

        shuttle::check_pct(shuttle_dump_time_range_resolves_via_deadline, 500, 3);

        // Batch-level: whether a given schedule reaches the wait depends on the interleaving,
        // but across 500 iterations at least one must have. Without this, the scenario passes
        // just as happily when `deadline` is never set at all and `sleep_until` is never awaited.
        //
        // Failing here means this test lost coverage of its own scenario, not
        // that WorkerLoop/ActiveDump regressed.
        assert!(
            crate::primitives::time::take_yield_pending_polls() > 0,
            "no sleep/sleep_until await ever suspended: this scenario never \
             exercised the deadline path it exists to cover"
        );
    }

    #[test]
    #[ignore = "SIGBUSes due to shuttle's default 60KB stack size."]
    fn determinism() {
        shuttle::check_uncontrolled_nondeterminism(
            shuttle_dump_time_range_resolves_via_deadline,
            500,
        );
    }
}

/// A processor whose `initialize()` panics instead of returning an error.
struct PanickingInitializer;

impl SegmentProcessor for PanickingInitializer {
    fn name(&self) -> &'static str {
        "PanickingInitializer"
    }

    fn initialize(&mut self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>> {
        Box::pin(async { panic!("PanickingInitializer: simulated init panic") })
    }

    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(async move { Ok(data) })
    }
}

crate::shuttle_test! {
    num_iters = 500, determinism_only;
    // A panic escaping `WorkerLoop::new` must be caught by
    // `run_background_task_inner`'s top-level `catch_unwind`, distinct from
    // the per-segment panics `process_segments` already catches internally.
    // No `pct`: single task, nothing to interleave against; still needs
    // shuttle's harness for `shuttle_select!`.
    fn shuttle_background_task_contains_init_panic() {
        let fs = Fs::new_in_memory(1 << 20, 4096).unwrap();
        let config = BackgroundTaskConfig::builder()
            .processors(vec![Box::new(PanickingInitializer) as Box<dyn SegmentProcessor>])
            .build();
        let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let worker = crate::primitives::thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                shuttle::future::block_on(run_background_task_inner(config, shutdown_rx, fs));
            }))
        });

        assert!(
            worker.join().unwrap().is_ok(),
            "a panic inside WorkerLoop::new/run must be caught by \
             run_background_task_inner's top-level catch_unwind, not \
             propagate to its caller"
        );
    }
}

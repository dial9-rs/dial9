//! Shuttle round-trip + error-injection tests for the core pipeline:
//! writer -> flush thread -> sealed segment -> drain, plus the flush loop's
//! rate-limited error handling under fs faults. No tokio, no telemetry sources.

use crate::buffer::{DiskBuffer, MemoryBuffer};
use crate::clock::clock_monotonic_ns;
use crate::primitives::fs;
use crate::primitives::sync::atomic::{AtomicU64, Ordering};
use crate::primitives::sync::{Arc, Mutex};
use crate::recording::Recorder;
use crate::shared_state::SharedState;
use crate::source::{FlushContext, Source};
use dial9_trace_format::TraceEvent;
use shuttle::rand::Rng;
use std::collections::HashMap;

// ── Event definition ────────────────────────────────────────────────

/// Custom event for round-trip validation. Each event carries a
/// per-thread monotonic `seq`, a `thread_id`, and a `timestamp_ns`
/// that is mostly monotonic with occasional backward jumps.
#[derive(TraceEvent, Clone, Debug, serde::Deserialize)]
struct ValidationEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    thread_id: u64,
    seq: u64,
    id: u64,
}

/// Generate a timestamp that is mostly monotonic with occasional backward
/// jumps, driven by shuttle's deterministic RNG.
fn next_timestamp(prev: &mut u64) -> u64 {
    let mut rng = shuttle::rand::thread_rng();
    if rng.gen_range(0u32..5) == 0 {
        *prev = prev.saturating_sub(rng.gen_range(1u64..=100));
    } else {
        *prev += rng.gen_range(1u64..=1000);
    }
    *prev
}

// ── Decoding ────────────────────────────────────────────────────────

fn decode_validation_events(data: &[u8]) -> Vec<ValidationEvent> {
    use dial9_trace_format::decoder::Decoder;
    let Some(mut dec) = Decoder::new(data) else {
        assert!(data.is_empty(), "failed to non-empty segment!");
        return vec![];
    };
    let mut out = Vec::new();
    dec.for_each_event(|ev| {
        if ev.name == "ValidationEvent" {
            if let Ok(decoded) = ev.deserialize::<ValidationEvent>() {
                out.push(decoded);
            }
        }
    })
    .expect("decode failed");
    out
}

// ── Invariants ──────────────────────────────────────────────────────

/// All emitted event IDs appear exactly once in the decoded output.
fn check_all_events_present(expected: &[ValidationEvent], decoded: &[ValidationEvent]) {
    let mut exp_ids: Vec<u64> = expected.iter().map(|e| e.id).collect();
    let mut dec_ids: Vec<u64> = decoded.iter().map(|e| e.id).collect();
    exp_ids.sort();
    dec_ids.sort();
    assert_eq!(
        exp_ids,
        dec_ids,
        "event ids mismatch: expected {} events, got {}",
        exp_ids.len(),
        dec_ids.len()
    );
}

/// Every event's timestamp round-trips exactly.
fn check_timestamps_roundtrip(expected: &[ValidationEvent], decoded: &[ValidationEvent]) {
    let exp_by_id: HashMap<u64, u64> = expected.iter().map(|e| (e.id, e.timestamp_ns)).collect();
    for ev in decoded {
        let exp_ts = exp_by_id[&ev.id];
        assert_eq!(
            exp_ts, ev.timestamp_ns,
            "timestamp mismatch for event id {}: expected {exp_ts}, got {}",
            ev.id, ev.timestamp_ns
        );
    }
}

// ── Mock Source ────────────────────────────────────────────────────

/// A Source that accumulates events from worker threads and emits them
/// during flush. Exercises the Source flush path under shuttle.
struct MockSource {
    pending: Arc<Mutex<Vec<ValidationEvent>>>,
}

impl MockSource {
    fn new(pending: Arc<Mutex<Vec<ValidationEvent>>>) -> Self {
        Self { pending }
    }
}

impl Source for MockSource {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        let events: Vec<_> = self.pending.lock().unwrap().drain(..).collect();
        for ev in &events {
            ctx.record_event(ev);
        }
    }

    fn name(&self) -> &'static str {
        "mock"
    }

    // TODO: exercise on_worker_thread_start/on_thread_stop once shuttle
    // tests include a Tokio runtime.
}

/// A Source whose `flush` always panics. Used to check whether the flush
/// loop contains a panicking source.
struct PanickingSource;

impl Source for PanickingSource {
    fn flush(&mut self, _ctx: &FlushContext<'_>) {
        panic!("PanickingSource intentionally panics for shuttle coverage");
    }

    fn name(&self) -> &'static str {
        "panicking"
    }
}

// ── Test body ───────────────────────────────────────────────────────

crate::shuttle_test! {
    num_iters = 10_000, depth = 3;
    fn test_core_pipeline() {
        let _ts_guard = metrique_timesource::set_time_source(metrique_timesource::TimeSource::custom(
            metrique_timesource::fakes::StaticTimeSource::at_time(std::time::UNIX_EPOCH),
        ));

        let num_threads = 3;
        let next_id = Arc::new(AtomicU64::new(0));

        // Small segments force frequent rotation: the 100 MiB budget is far above the test's data,
        // so the ring never evicts before we drain it.
        let writer = MemoryBuffer::builder()
            .max_total_size(100 * 1024 * 1024)
            .max_segment_size(256)
            .build()
            .unwrap();
        let fs = writer.fs_handle().expect("in-memory writer exposes its fs");

        // Mock source: worker threads push events here, flush thread drains them.
        let source_pending: Arc<Mutex<Vec<ValidationEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let shared = Arc::new(SharedState::new(clock_monotonic_ns()));
        shared.push_source(Box::new(MockSource::new(source_pending.clone())));
        let mut recorder = Recorder::start(shared, writer, None, || || {});
        recorder.handle().enable();
        let handle = recorder.handle().clone();

        let expected: Arc<Mutex<Vec<ValidationEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let writers: Vec<_> = (0..num_threads)
            .map(|thread_id| {
                let h = handle.clone();
                let next_id = next_id.clone();
                let expected = expected.clone();
                let source_pending = source_pending.clone();
                let thread_id = thread_id as u64;
                crate::primitives::thread::spawn(move || {
                    let mut rng = shuttle::rand::thread_rng();
                    let count = rng.gen_range(3u64..=10);
                    let mut ts = rng.gen_range(1000u64..2000);
                    for seq in 0..count {
                        let id = next_id.fetch_add(1, Ordering::Relaxed);
                        let timestamp_ns = next_timestamp(&mut ts);
                        let ev = ValidationEvent {
                            timestamp_ns,
                            thread_id,
                            seq,
                            id,
                        };
                        expected.lock().unwrap().push(ev.clone());
                        // Randomly choose: emit via handle (TL buffer path)
                        // or via mock source (flush-thread path).
                        if rng.gen_range(0u32..2) == 0 {
                            h.record_event(ev);
                        } else {
                            source_pending.lock().unwrap().push(ev);
                        }
                    }
                })
            })
            .collect();

        for w in writers {
            w.join().unwrap();
        }
        // Final flush + seal the last segment, then join the flush thread.
        recorder.stop_flush_thread();

        // Drain the in-memory ring (memory pops one sealed segment per call).
        let mut all_decoded: Vec<ValidationEvent> = Vec::new();
        loop {
            let taken = fs.take_files();
            if taken.segments.is_empty() {
                break;
            }
            for seg in taken.segments {
                let (_seg_ref, payload, _accounting) = seg.load().unwrap();
                all_decoded.extend(decode_validation_events(&payload.into_vec()));
            }
        }
        let expected = expected.lock().unwrap();

        // Run all invariants.
        check_all_events_present(&expected, &all_decoded);
        check_timestamps_roundtrip(&expected, &all_decoded);
    }
}

// ── Panic injection ─────────────────────────────────────────────────

// `flush_sources` has no `catch_unwind`, so the panic propagates out of the
// flush thread's main loop and kills it.
crate::shuttle_test! {
    num_iters = 500, depth = 3, should_panic,
    expect_panic = "PanickingSource intentionally panics for shuttle coverage",
    replay = "91011be187b1dcc7fc8f9dbc0100000058555515";
    fn test_source_panic_does_not_wedge_pipeline() {
        let _ts_guard = metrique_timesource::set_time_source(metrique_timesource::TimeSource::custom(
            metrique_timesource::fakes::StaticTimeSource::at_time(std::time::UNIX_EPOCH),
        ));

        let writer = MemoryBuffer::builder()
            .max_total_size(100 * 1024 * 1024)
            .max_segment_size(256)
            .build()
            .unwrap();
        let fs = writer.fs_handle().expect("in-memory writer exposes its fs");

        let source_pending: Arc<Mutex<Vec<ValidationEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::new(SharedState::new(clock_monotonic_ns()));
        shared.push_source(Box::new(PanickingSource));
        shared.push_source(Box::new(MockSource::new(source_pending.clone())));
        let mut recorder = Recorder::start(shared, writer, None, || || {});
        recorder.handle().enable();

        let healthy_source_event = ValidationEvent {
            timestamp_ns: 1,
            thread_id: 0,
            seq: 0,
            id: 0,
        };
        source_pending
            .lock()
            .unwrap()
            .push(healthy_source_event.clone());

        recorder.stop_flush_thread();

        let mut all_decoded: Vec<ValidationEvent> = Vec::new();
        loop {
            let taken = fs.take_files();
            if taken.segments.is_empty() {
                break;
            }
            for seg in taken.segments {
                let (_seg_ref, payload, _accounting) = seg.load().unwrap();
                all_decoded.extend(decode_validation_events(&payload.into_vec()));
            }
        }

        assert!(
            all_decoded.iter().any(|e| e.id == healthy_source_event.id),
            "healthy source's event must survive a sibling source's panic"
        );
    }
}

// Companion to the scenario above: a TL-buffer write hits the same
// unguarded-flush gap. Kept separate because the live buffer left behind at
// panic time is what makes `determinism` crash (16/30 with the write vs
// 0/30 without) -- hence `flaky_sigabrt_determinism_only`.
crate::shuttle_test! {
    num_iters = 500, depth = 3, should_panic, flaky_sigabrt_determinism_only,
    expect_panic = "PanickingSource intentionally panics for shuttle coverage",
    replay = "910124ca81ffb4a781e6ae0a000000006055555555";
    fn test_source_panic_does_not_lose_tl_buffer_write() {
        let _ts_guard = metrique_timesource::set_time_source(metrique_timesource::TimeSource::custom(
            metrique_timesource::fakes::StaticTimeSource::at_time(std::time::UNIX_EPOCH),
        ));

        let writer = MemoryBuffer::builder()
            .max_total_size(100 * 1024 * 1024)
            .max_segment_size(256)
            .build()
            .unwrap();
        let fs = writer.fs_handle().expect("in-memory writer exposes its fs");

        let shared = Arc::new(SharedState::new(clock_monotonic_ns()));
        shared.push_source(Box::new(PanickingSource));
        let mut recorder = Recorder::start(shared, writer, None, || || {});
        recorder.handle().enable();
        let handle = recorder.handle().clone();

        let tl_buffer_event = ValidationEvent {
            timestamp_ns: 2,
            thread_id: 0,
            seq: 1,
            id: 1,
        };
        handle.record_event(tl_buffer_event.clone());

        recorder.stop_flush_thread();

        let mut all_decoded: Vec<ValidationEvent> = Vec::new();
        loop {
            let taken = fs.take_files();
            if taken.segments.is_empty() {
                break;
            }
            for seg in taken.segments {
                let (_seg_ref, payload, _accounting) = seg.load().unwrap();
                all_decoded.extend(decode_validation_events(&payload.into_vec()));
            }
        }

        assert!(
            all_decoded.iter().any(|e| e.id == tl_buffer_event.id),
            "TL-buffer event must survive a sibling source's panic"
        );
    }
}

// ── Error injection ─────────────────────────────────────────────────
//
// Companion to the round-trip pipeline test. `primitives::fs` is armed
// with a fault policy so the writer's real I/O points (write/flush/rename/
// remove) fail, exercising the flush loop's error paths.

use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicU64 as StdAtomicU64, Ordering as StdOrdering};

// ── Counting subscriber ─────────────────────────────────────────────
//
// A minimal `tracing::Subscriber` that increments a shared counter
// on every WARN or ERROR event. We use `tracing::subscriber::with_default`
// to scope it to a single test invocation. We deliberately avoid adding a
// `tracing-subscriber` dependency just for this `_shuttle` test.

struct CountingSubscriber {
    warn_or_error_count: StdArc<StdAtomicU64>,
}

impl tracing::Subscriber for CountingSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        matches!(
            *metadata.level(),
            tracing::Level::WARN | tracing::Level::ERROR
        )
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        // We never actually use spans; return a fixed non-zero id.
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let level = *event.metadata().level();
        if level == tracing::Level::WARN || level == tracing::Level::ERROR {
            self.warn_or_error_count.fetch_add(1, StdOrdering::Relaxed);
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

// ── Erroring-pipeline test body ─────────────────────────────────────

/// Drive the pipeline with the fs armed to `fault`, returning the
/// number of WARN/ERROR events the flush loop emitted.
fn run_erroring_pipeline(fault: fs::FaultPolicy) -> u64 {
    let _ts_guard = metrique_timesource::set_time_source(metrique_timesource::TimeSource::custom(
        metrique_timesource::fakes::StaticTimeSource::at_time(std::time::UNIX_EPOCH),
    ));

    let warn_count = StdArc::new(StdAtomicU64::new(0));
    let subscriber = CountingSubscriber {
        warn_or_error_count: warn_count.clone(),
    };

    tracing::subscriber::with_default(subscriber, || {
        let num_threads = 3;
        let next_id = Arc::new(AtomicU64::new(0));

        let dir = tempfile::tempdir().unwrap();
        let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();
        let _fault = fs::set_fault(fault);
        let shared = Arc::new(SharedState::new(clock_monotonic_ns()));
        let mut recorder = Recorder::start(shared, writer, None, || || {});
        recorder.handle().enable();
        let handle = recorder.handle().clone();

        let writers: Vec<_> = (0..num_threads)
            .map(|thread_id| {
                let h = handle.clone();
                let next_id = next_id.clone();
                let thread_id = thread_id as u64;
                crate::primitives::thread::spawn(move || {
                    let mut rng = shuttle::rand::thread_rng();
                    let count = rng.gen_range(3u64..=10);
                    let mut ts = rng.gen_range(1000u64..2000);
                    for seq in 0..count {
                        let id = next_id.fetch_add(1, Ordering::Relaxed);
                        let timestamp_ns = next_timestamp(&mut ts);
                        let ev = ValidationEvent {
                            timestamp_ns,
                            thread_id,
                            seq,
                            id,
                        };
                        // Errors are expected; whether the event is
                        // recorded or dropped is not asserted here.
                        h.record_event(ev);
                    }
                })
            })
            .collect();

        for w in writers {
            w.join().unwrap();
        }
        // The shutdown/finalize path runs a final flush + seal, which should
        // also be rate-limited if it logs on error.
        recorder.stop_flush_thread();
    });

    warn_count.load(StdOrdering::Relaxed)
}

// Pins `primitives::fs::FAULT`'s real `std::thread_local!`: a fault armed
// on this thread must stay visible to a spawned thread too. No `pct`: the
// spawning thread parks on `.join()` immediately, no interleaving to explore.
crate::shuttle_test! {
    default, determinism_only;
    fn fs_fault_visible_across_threads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fault_probe");
        std::fs::write(&path, b"x").unwrap();

        let _fault = fs::set_fault(fs::FaultPolicy::FailAll);
        let observed_fault =
            crate::primitives::thread::spawn(move || fs::remove_file(&path).is_err())
                .join()
                .unwrap();

        assert!(
            observed_fault,
            "fault armed on the test thread was not observed on a spawned thread"
        );
    }
}

crate::shuttle_test! {
    num_iters = 10_000, depth = 3;
    fn test_core_erroring_pipeline() {
        let total = run_erroring_pipeline(fs::FaultPolicy::FailAll);
        assert!(
            total <= 10,
            "rate limiting failed under persistent writer errors: \
             observed {total} WARN/ERROR events, expected <= 10. \
             A `rate_limited!` wrapper has likely been removed from a tight loop."
        );
    }
}

crate::shuttle_test! {
    num_iters = 10_000, depth = 3;
    fn test_core_probabilistic_fs_faults() {
        let total = run_erroring_pipeline(fs::FaultPolicy::FailProb(0.5));
        assert!(
            total <= 10,
            "rate limiting failed under probabilistic fs faults: observed {total} \
             WARN/ERROR events, expected <= 10."
        );
    }
}

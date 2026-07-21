//! The `recorder(writer).with_tokio(..).build()` path: a Tokio-integrated
//! runtime built from the core recorder, producing poll instrumentation.
#![cfg(feature = "tokio")]

use dial9::{DiskBuffer, recorder};
use dial9::{RecorderBuilderTokioExt, RecorderSourceExt};
use dial9_trace_format::decoder::Decoder;
use std::time::Duration;

#[test]
fn recorder_with_tokio_records_poll_events() {
    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();

    let traced = recorder(writer)
        .with_tokio(|t| {
            t.worker_threads(2);
        })
        .with_runtime_name("main")
        .with_task_tracking(true)
        .build()
        .expect("build traced runtime");

    traced.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..50 {
            handles.push(tokio::spawn(async {
                for _ in 0..5 {
                    tokio::task::yield_now().await;
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    traced.graceful_shutdown(Duration::from_secs(1));

    let bytes = std::fs::read(dir.path().join("trace.0.bin")).expect("sealed segment");
    let mut decoder = Decoder::new(&bytes).expect("valid trace");
    let mut poll_starts = 0u32;
    let mut poll_ends = 0u32;
    decoder
        .for_each_event(|ev| match ev.name {
            "PollStartEvent" => poll_starts += 1,
            "PollEndEvent" => poll_ends += 1,
            _ => {}
        })
        .expect("decode events");

    assert!(
        poll_starts > 0,
        "expected poll events from the with_tokio runtime, got 0"
    );
    assert_eq!(
        poll_starts, poll_ends,
        "PollStart ({poll_starts}) != PollEnd ({poll_ends})"
    );
}

/// A disabled recorder still yields a working plain runtime.
#[test]
fn recorder_with_tokio_disabled_runs_plainly() {
    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();

    let traced = recorder(writer)
        .with_tokio(|_| {})
        .enabled(false)
        .build()
        .expect("build disabled runtime");

    let out = traced.block_on(async { 1 + 1 });
    assert_eq!(out, 2);
    traced.graceful_shutdown(Duration::from_secs(1));
}

/// On-demand dump mode on the `with_tokio` path: the trigger must be reachable
/// via the ambient handle. Regression: `build_traced` has to stash the tx.
#[test]
fn recorder_with_tokio_dump_trigger_reachable() {
    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();

    let traced = recorder(writer)
        .with_tokio(|_| {})
        .with_dump_trigger(|_| {})
        .build()
        .expect("build traced runtime");

    traced.block_on(async {
        assert!(
            dial9::core::current_handle().dump_trigger().is_some(),
            "dump_trigger should be reachable via the ambient handle in on-demand mode"
        );
    });

    traced.graceful_shutdown(Duration::from_secs(1));
}

/// The builder is `TryInto<TracedRuntime>`, so `#[dial9::main]` (which calls
/// `TracedRuntime::new(config)`) accepts it directly.
#[test]
fn traced_recorder_is_macro_compatible() {
    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();
    let builder = recorder(writer).with_tokio(|_| {});
    let traced = dial9::TracedRuntime::try_new(builder).expect("try_into TracedRuntime");
    traced.graceful_shutdown(Duration::from_secs(1));
}

/// `recorder_or_disabled` runs the Tokio configurator on the downgrade path
/// (writer setup failed), matching the old `build_or_disabled` behavior.
#[test]
fn recorder_or_disabled_runs_tokio_config_on_writer_failure() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A base path whose parent is a regular file: writer creation fails, so the
    // recorder downgrades to a plain runtime.
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();
    let writer = DiskBuffer::builder()
        .base_path(blocker.join("traces"))
        .max_total_size(4 * 1024 * 1024)
        .build();

    let ran = Arc::new(AtomicUsize::new(0));
    let ran_in_closure = Arc::clone(&ran);
    let traced = dial9::recorder_or_disabled(writer, move |b| {
        ran_in_closure.fetch_add(1, Ordering::SeqCst);
        b.worker_threads(1);
    })
    .build()
    .expect("downgrade builds a plain runtime");

    assert!(
        !traced.is_enabled(),
        "writer failure must downgrade to a disabled runtime"
    );
    assert_eq!(traced.block_on(async { 7u32 }), 7);
    assert!(
        ran.load(Ordering::SeqCst) >= 1,
        "the Tokio configurator must run on the downgrade path"
    );
}

/// `recorder_or_disabled` is generic over writer mode: a valid in-memory writer
/// builds an enabled, memory-backed runtime just like disk does.
#[test]
fn recorder_or_disabled_accepts_in_memory_writer() {
    use dial9::MemoryBuffer;

    let writer = MemoryBuffer::new(4 * 1024 * 1024);
    let traced = dial9::recorder_or_disabled(writer, |b| {
        b.worker_threads(1);
    })
    .build()
    .expect("in-memory recorder builds");

    assert!(
        traced.is_enabled(),
        "a valid in-memory writer must stay enabled"
    );
    assert_eq!(traced.block_on(async { 5u32 }), 5);
    traced.graceful_shutdown(Duration::from_secs(1));
}

/// The downgrade path is generic too: an in-memory writer failure falls back to
/// a disabled runtime the same way disk does, with the configurator preserved.
#[test]
fn recorder_or_disabled_downgrades_on_in_memory_writer_failure() {
    use dial9::MemoryBuffer;

    let writer: std::io::Result<MemoryBuffer> =
        Err(std::io::Error::other("simulated in-memory writer failure"));

    let traced = dial9::recorder_or_disabled(writer, |b| {
        b.worker_threads(1);
    })
    .build()
    .expect("downgrade builds a plain runtime");

    assert!(
        !traced.is_enabled(),
        "in-memory writer failure must downgrade to a disabled runtime"
    );
    assert_eq!(traced.block_on(async { 9u32 }), 9);
    traced.graceful_shutdown(Duration::from_secs(1));
}

#[test]
fn source_registered_after_with_tokio_is_recorded() {
    use dial9::core::{FlushContext, Source, clock_monotonic_ns};

    #[derive(Debug, serde::Deserialize, dial9_trace_format::TraceEvent)]
    struct MarkerEvent {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        value: u64,
    }

    struct MarkerSource {
        emitted: bool,
    }
    impl Source for MarkerSource {
        fn flush(&mut self, ctx: &FlushContext<'_>) {
            if !self.emitted {
                self.emitted = true;
                ctx.record_event(&MarkerEvent {
                    timestamp_ns: clock_monotonic_ns(),
                    value: 4242,
                });
            }
        }
        fn name(&self) -> &'static str {
            "marker"
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();

    let traced = recorder(writer)
        .with_tokio(|t| {
            t.worker_threads(1);
        })
        .with_task_tracking(true)
        // The point of the test: `.source(..)` chained after `.with_tokio(..)`.
        .source(MarkerSource { emitted: false })
        .build()
        .expect("build traced runtime");

    traced.block_on(async {
        tokio::task::yield_now().await;
    });
    traced.graceful_shutdown(Duration::from_secs(1));

    let bytes = std::fs::read(dir.path().join("trace.0.bin")).expect("sealed segment");
    let mut decoder = Decoder::new(&bytes).expect("valid trace");
    let mut markers = Vec::new();
    decoder
        .for_each_event(|ev| {
            if ev.name == "MarkerEvent" {
                let m: MarkerEvent = ev.deserialize().expect("MarkerEvent decodes");
                markers.push(m.value);
            }
        })
        .expect("decode events");

    assert!(
        markers.contains(&4242),
        "a source added after with_tokio should still record; got {markers:?}"
    );
}

/// `on_recording_start` hooks forward through the tokio builder and fire when the
/// runtime's recorder enables.
#[test]
fn on_recording_start_fires_on_tokio_path() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();

    let ran = Arc::new(AtomicBool::new(false));
    let ran_hook = Arc::clone(&ran);
    let traced = recorder(writer)
        .with_tokio(|_| {})
        .on_recording_start(move |_handle| ran_hook.store(true, Ordering::SeqCst))
        .build()
        .expect("build traced runtime");

    assert!(
        ran.load(Ordering::SeqCst),
        "on_recording_start should fire when the tokio recorder enables at build"
    );
    traced.graceful_shutdown(Duration::from_secs(1));
}

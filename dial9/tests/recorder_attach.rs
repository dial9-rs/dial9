//! Usage of `recorder(writer).<sources>.build()` + `attach_tokio_runtime(..)`: a Tokio
//! runtime instrumented against a recorder, producing poll instrumentation.
#![cfg(feature = "tokio")]

use dial9::{
    AttachedRuntime, DiskBuffer, Recorder, RecorderSourceExt, RecorderTokioExt, TokioAttachOptions,
    recorder,
};
use dial9_trace_format::decoder::Decoder;
use std::time::Duration;

/// Attach a multi-thread runtime with `workers` threads and the given options.
fn attach(recorder: Recorder, workers: usize, options: TokioAttachOptions) -> AttachedRuntime {
    recorder
        .attach_tokio_runtime_with(options, |t| {
            t.worker_threads(workers);
        })
        .expect("build tokio runtime")
}

#[test]
fn attach_runtime_records_poll_events() {
    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();

    let recorder = recorder(writer).build();
    let (recorder, runtime) = attach(
        recorder,
        2,
        TokioAttachOptions::builder()
            .runtime_name("main")
            .task_tracking_enabled(true)
            .build(),
    );

    runtime.block_on(async {
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

    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(1));

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
        "expected poll events from the attached runtime, got 0"
    );
    assert_eq!(
        poll_starts, poll_ends,
        "PollStart ({poll_starts}) != PollEnd ({poll_ends})"
    );
}

/// A disabled recorder still yields a working plain runtime.
#[test]
fn disabled_recorder_runs_plainly() {
    let recorder = dial9::recorder_disabled();
    let (recorder, runtime) = attach(recorder, 1, TokioAttachOptions::default());

    let out = runtime.block_on(async { 1 + 1 });
    assert_eq!(out, 2);
    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(1));
}

/// On-demand dump mode: the trigger must be reachable via the ambient handle.
#[test]
fn dump_trigger_reachable_from_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();

    let recorder = recorder(writer).with_dump_trigger(|_| {}).build();
    let (recorder, runtime) = attach(recorder, 1, TokioAttachOptions::default());

    runtime.block_on(async {
        assert!(
            dial9::core::current_handle().dump_trigger().is_some(),
            "dump_trigger should be reachable via the ambient handle in on-demand mode"
        );
    });

    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(1));
}

/// `recorder_or_disabled` downgrades to a disabled recorder on writer failure.
#[test]
fn recorder_or_disabled_downgrades_on_writer_failure() {
    // A base path whose parent is a regular file: writer creation fails, so the
    // recorder downgrades.
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();
    let writer = DiskBuffer::builder()
        .base_path(blocker.join("traces"))
        .max_total_size(4 * 1024 * 1024)
        .build();

    let recorder = dial9::recorder_or_disabled(writer).build();
    let (recorder, runtime) = attach(recorder, 1, TokioAttachOptions::default());

    assert!(
        !recorder.handle().is_enabled(),
        "writer failure must downgrade to a disabled recorder"
    );
    assert_eq!(runtime.block_on(async { 7u32 }), 7);
    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(1));
}

/// The downgrade composes with builder configuration: sources and a custom
/// pipeline can be chained onto a writer that failed to open, and `build` still
/// yields a disabled recorder instead of panicking.
#[test]
fn recorder_or_disabled_keeps_builder_config_on_writer_failure() {
    use dial9::RecorderPipelineExt;

    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();
    let writer = DiskBuffer::builder()
        .base_path(blocker.join("traces"))
        .max_total_size(4 * 1024 * 1024)
        .build();

    let recorder = dial9::recorder_or_disabled(writer)
        .with_custom_pipeline(|p| p.gzip().write_back())
        .segment_metadata([("service".to_string(), "demo".to_string())])
        .build();

    assert!(
        !recorder.handle().is_enabled(),
        "a failed writer must downgrade even with a pipeline configured"
    );

    let (recorder, runtime) = attach(recorder, 1, TokioAttachOptions::default());
    assert_eq!(runtime.block_on(async { 11u32 }), 11);
    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(1));
}

/// `recorder_or_disabled` is generic over writer mode: a valid in-memory writer
/// builds an enabled, memory-backed recorder just like disk does.
#[test]
fn recorder_or_disabled_accepts_in_memory_writer() {
    use dial9::MemoryBuffer;

    let writer = MemoryBuffer::new(4 * 1024 * 1024);
    let recorder = dial9::recorder_or_disabled(writer).build();
    let (recorder, runtime) = attach(recorder, 1, TokioAttachOptions::default());

    assert!(
        recorder.handle().is_enabled(),
        "a valid in-memory writer must stay enabled"
    );
    assert_eq!(runtime.block_on(async { 5u32 }), 5);
    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(1));
}

/// The downgrade path is generic too: an in-memory writer failure falls back to
/// a disabled recorder the same way disk does.
#[test]
fn recorder_or_disabled_downgrades_on_in_memory_writer_failure() {
    use dial9::MemoryBuffer;

    let writer: std::io::Result<MemoryBuffer> =
        Err(std::io::Error::other("simulated in-memory writer failure"));

    let recorder = dial9::recorder_or_disabled(writer).build();
    let (recorder, runtime) = attach(recorder, 1, TokioAttachOptions::default());

    assert!(
        !recorder.handle().is_enabled(),
        "in-memory writer failure must downgrade to a disabled recorder"
    );
    assert_eq!(runtime.block_on(async { 9u32 }), 9);
    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(1));
}

#[test]
fn source_on_recorder_is_recorded() {
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

    let recorder = recorder(writer)
        .source(MarkerSource { emitted: false })
        .build();
    let (recorder, runtime) = attach(
        recorder,
        1,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    );

    runtime.block_on(async {
        tokio::task::yield_now().await;
    });
    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(1));

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
        "a source on the recorder should record; got {markers:?}"
    );
}

/// `on_recording_start` hooks fire when the recorder enables.
#[test]
fn on_recording_start_fires_at_enable() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();

    let ran = Arc::new(AtomicBool::new(false));
    let ran_hook = Arc::clone(&ran);
    let recorder = recorder(writer)
        .on_recording_start(move |_handle| ran_hook.store(true, Ordering::SeqCst))
        .build();
    let (recorder, runtime) = attach(recorder, 1, TokioAttachOptions::default());

    assert!(
        ran.load(Ordering::SeqCst),
        "on_recording_start should fire when the recorder enables"
    );
    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(1));
}

/// Thread-per-core: one recorder, one cloned `Dial9Handle` per thread, one
/// `current_thread` runtime built on each. Exercises `Dial9HandleTokioExt`
/// from several threads at once.
#[test]
fn shared_recorder_attaches_from_many_threads() {
    use dial9::Dial9HandleTokioExt;

    const CORES: usize = 4;

    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();
    let recorder = recorder(writer).build();

    let mut threads = Vec::with_capacity(CORES);
    for core in 0..CORES {
        let handle = recorder.handle().clone();
        threads.push(std::thread::spawn(move || {
            let mut builder = tokio::runtime::Builder::new_current_thread();
            builder.enable_all();

            let runtime = handle
                .attach_tokio_runtime(
                    builder,
                    TokioAttachOptions::builder()
                        .runtime_name(format!("core-{core}"))
                        .build(),
                )
                .expect("build core runtime");

            runtime.block_on(async {
                dial9::spawn(async {
                    for _ in 0..5 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            });
        }));
    }
    for t in threads {
        t.join().unwrap();
    }

    recorder.graceful_shutdown(Duration::from_secs(1));

    #[derive(serde::Deserialize)]
    struct WorkerIdField {
        worker_id: u64,
    }
    const UNKNOWN_WORKER_ID: u64 = 255;

    let bytes = std::fs::read(dir.path().join("trace.0.bin")).expect("sealed segment");
    let mut decoder = Decoder::new(&bytes).expect("valid trace");
    let mut poll_starts = 0u32;
    let mut poll_ends = 0u32;
    let mut worker_ids = std::collections::BTreeSet::new();
    decoder
        .for_each_event(|ev| match ev.name {
            "PollStartEvent" => {
                poll_starts += 1;
                if let Ok(fields) = ev.deserialize::<WorkerIdField>()
                    && fields.worker_id != UNKNOWN_WORKER_ID
                {
                    worker_ids.insert(fields.worker_id);
                }
            }
            "PollEndEvent" => poll_ends += 1,
            _ => {}
        })
        .expect("decode events");

    // Each core runs one task doing 5 `yield_now().await` calls: an initial
    // poll plus one re-poll per yield, so at least 6 polls per core.
    assert!(
        poll_starts >= CORES as u32 * 6,
        "expected at least {} poll events from {CORES} core runtimes, got {poll_starts}",
        CORES * 6
    );
    assert!(
        worker_ids.len() >= CORES,
        "expected poll events from {CORES} distinct worker IDs, got {worker_ids:?}"
    );
    assert_eq!(
        poll_starts, poll_ends,
        "PollStart ({poll_starts}) != PollEnd ({poll_ends})"
    );
}

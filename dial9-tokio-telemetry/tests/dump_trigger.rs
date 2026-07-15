//! Builder wiring for on-trigger pipeline runs.

mod common;

use common::{fast_sealing_writer, wait_for_sealed_segment};
use dial9_tokio_telemetry::background_task::s3::S3Config;
use dial9_tokio_telemetry::dump::DumpError;
use dial9_tokio_telemetry::telemetry::{
    DiskBuffer, MemoryBuffer, RecorderBuilderTokioExt, recorder,
};

/// `with_dump_trigger` is available in every pipeline state (compile check).
#[allow(dead_code)]
fn with_dump_trigger_compiles_in_all_pipeline_states() {
    let _unset = recorder(MemoryBuffer::new(4096).unwrap())
        .with_tokio(|_| {})
        .with_dump_trigger(|_| {});

    let s3_config = S3Config::builder()
        .bucket("bucket")
        .service_name("service")
        .build();
    let _s3 = recorder(MemoryBuffer::new(4096).unwrap())
        .with_tokio(|_| {})
        .with_s3_uploader(s3_config)
        .with_dump_trigger(|_| {});

    let _custom = recorder(DiskBuffer::single_file("throwaway").unwrap())
        .with_tokio(|_| {})
        .with_custom_pipeline(|p| p.gzip().write_back())
        .with_dump_trigger(|_| {});
}

/// A trigger without a configured pipeline never spawns the worker; the
/// receiver is dropped and every dump resolves `WorkerStopped`.
#[test]
fn trigger_without_pipeline_resolves_worker_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let writer = DiskBuffer::single_file(&trace_path).unwrap();

    let traced = recorder(writer)
        .with_tokio(|t| {
            t.worker_threads(1);
        })
        .with_dump_trigger(|_| {})
        .build()
        .unwrap();

    let trigger = traced
        .record_handle()
        .dump_trigger()
        .expect("trigger wired");

    let err = traced
        .runtime()
        .block_on(async { trigger.dump_current_data().await })
        .expect_err("no worker, dump must fail");
    assert!(matches!(err, DumpError::WorkerStopped));

    drop(traced);
}

/// Two `dump_current_data()` calls fired concurrently both succeed with
/// distinct dump ids and at least one captures the ring. Per-dump fan-out (a
/// segment captured by every overlapping window) is covered by the worker
/// unit tests; this pins the end-to-end answer to "what happens if two dumps
/// are triggered at once": they run independently, no coordination.
#[test]
fn concurrent_dumps_both_resolve_with_distinct_ids() {
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();

    let writer = fast_sealing_writer(dir.path());

    let traced = recorder(writer)
        .worker_poll_interval(Duration::from_millis(50))
        .with_tokio(|t| {
            t.worker_threads(2);
        })
        .with_custom_pipeline(|p| p.gzip().write_back())
        .with_dump_trigger(|_| {})
        .build()
        .unwrap();

    let trigger = traced
        .record_handle()
        .dump_trigger()
        .expect("trigger wired");

    // A triggered worker parks until a dump is requested, so a confirmed-sealed
    // segment persists in the ring for the concurrent dumps to capture.
    wait_for_sealed_segment(traced.runtime(), dir.path());

    let (first, second) = traced.runtime().block_on(async {
        // Fire two dumps concurrently.
        tokio::join!(
            trigger.dump_current_data().with_metadata("reason", "a"),
            trigger.dump_current_data().with_metadata("reason", "b"),
        )
    });

    let first = first.expect("first dump resolves");
    let second = second.expect("second dump resolves");
    assert_ne!(
        first.dump_id, second.dump_id,
        "concurrent dumps get distinct ids"
    );
    assert!(
        first.segments_processed + second.segments_processed > 0,
        "at least one concurrent dump captured the ring"
    );

    drop(traced);
}

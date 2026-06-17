//! Builder wiring for on-trigger pipeline runs.

use dial9_tokio_telemetry::background_task::s3::S3Config;
use dial9_tokio_telemetry::dump::DumpError;
use dial9_tokio_telemetry::telemetry::{Disk, DiskWriter, TracedRuntime};

/// `with_dump_trigger` is available in every pipeline state (compile check).
#[allow(dead_code)]
fn with_dump_trigger_compiles_in_all_pipeline_states() {
    let _unset = TracedRuntime::builder().with_dump_trigger(|_| {});

    let s3_config = S3Config::builder()
        .bucket("bucket")
        .service_name("service")
        .build();
    let _s3 = TracedRuntime::builder()
        .with_s3_uploader::<Disk>(s3_config)
        .with_dump_trigger(|_| {});

    let _custom = TracedRuntime::builder()
        .with_custom_pipeline::<_, Disk>(|p| p.gzip().write_back())
        .with_dump_trigger(|_| {});
}

/// A trigger without a configured pipeline never spawns the worker; the
/// receiver is dropped and every dump resolves `WorkerStopped`.
#[test]
fn trigger_without_pipeline_resolves_worker_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let writer = DiskWriter::single_file(&trace_path).unwrap();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_dump_trigger(|_| {})
        .build_and_start(builder, writer)
        .unwrap();

    let trigger = guard.handle().dump_trigger().expect("trigger wired");

    let err = runtime
        .block_on(async { trigger.dump_current_data().await })
        .expect_err("no worker, dump must fail");
    assert!(matches!(err, DumpError::WorkerStopped));

    drop(runtime);
    drop(guard);
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
    let trace_path = dir.path().join("trace.bin");

    // Small segments so the workload seals a few into the ring quickly.
    let writer = DiskWriter::new(&trace_path, 512, 50 * 1024).unwrap();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_custom_pipeline::<_, Disk>(|p| p.gzip().write_back())
        .with_dump_trigger(|_| {})
        .with_worker_poll_interval(Duration::from_millis(50))
        .build_and_start(builder, writer)
        .unwrap();

    let trigger = guard.handle().dump_trigger().expect("trigger wired");

    // Count sealed segments in the ring (`*.bin`; the active file is `.active`).
    let count_sealed = || {
        std::fs::read_dir(dir.path())
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".bin"))
                    .count()
            })
            .unwrap_or(0)
    };

    let (first, second) = runtime.block_on(async {
        // Generate trace data; the pipeline stays parked until we dump.
        let mut handles = Vec::new();
        for _ in 0..200 {
            handles.push(tokio::spawn(async { tokio::task::yield_now().await }));
        }
        for h in handles {
            let _ = h.await;
        }
        // Wait until at least one segment has sealed into the ring.
        for _ in 0..200 {
            if count_sealed() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
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

    drop(runtime);
    drop(guard);
}

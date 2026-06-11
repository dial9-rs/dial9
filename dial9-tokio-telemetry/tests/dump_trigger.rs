//! Builder wiring for on-trigger pipeline runs.

use dial9_tokio_telemetry::background_task::s3::S3Config;
use dial9_tokio_telemetry::dump::{self, DumpError};
use dial9_tokio_telemetry::telemetry::{Disk, DiskWriter, TracedRuntime};

/// `with_trigger` is available in every pipeline state (compile check).
#[allow(dead_code)]
fn with_trigger_compiles_in_all_pipeline_states() {
    let (_control, rx) = dump::trigger();
    let _unset = TracedRuntime::builder().with_trigger(rx);

    let (_control, rx) = dump::trigger();
    let s3_config = S3Config::builder()
        .bucket("bucket")
        .service_name("service")
        .build();
    let _s3 = TracedRuntime::builder()
        .with_s3_uploader::<Disk>(s3_config)
        .with_trigger(rx);

    let (_control, rx) = dump::trigger();
    let _custom = TracedRuntime::builder()
        .with_custom_pipeline::<_, Disk>(|p| p.gzip().write_back())
        .with_trigger(rx);
}

/// A trigger without a configured pipeline never spawns the worker; the
/// receiver is dropped and every dump resolves `WorkerStopped`.
#[test]
fn trigger_without_pipeline_resolves_worker_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let (control, rx) = dump::trigger();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let writer = DiskWriter::single_file(&trace_path).unwrap();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_trigger(rx)
        .build_and_start(builder, writer)
        .unwrap();

    let err = runtime
        .block_on(async { control.dump_current_data().await })
        .expect_err("no worker, dump must fail");
    assert!(matches!(err, DumpError::WorkerStopped));

    drop(runtime);
    drop(guard);
}

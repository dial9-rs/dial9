//! Builder wiring for on-trigger pipeline runs.

mod common;

use common::{fast_sealing_writer, wait_for_sealed_segment};
use dial9_destinations_s3::S3Config;
use dial9_tokio_telemetry::dump::DumpError;
use dial9_tokio_telemetry::telemetry::{
    DiskBuffer, MemoryBuffer, RecorderPipelineExt, TokioAttachOptions, recorder,
};

/// `with_dump_trigger` is available in every pipeline state (compile check).
#[allow(dead_code)]
fn with_dump_trigger_compiles_in_all_pipeline_states() {
    let _unset = recorder(MemoryBuffer::new(4096).unwrap()).with_dump_trigger(|_| {});

    let s3_config = S3Config::builder()
        .bucket("bucket")
        .service_name("service")
        .build();
    let _s3 = recorder(MemoryBuffer::new(4096).unwrap())
        .with_s3_uploader(s3_config)
        .with_dump_trigger(|_| {});

    let _custom = recorder(DiskBuffer::single_file("throwaway").unwrap())
        .with_custom_pipeline(|p| p.gzip().write_back())
        .with_dump_trigger(|_| {});
}

/// A current-data trigger without processing stages still checkpoints and
/// resolves. Its internal retain stage leaves the raw segment on disk.
#[test]
fn trigger_without_pipeline_retains_raw_segment() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let writer = DiskBuffer::single_file(&trace_path).unwrap();

    let recorder = recorder(writer).with_dump_trigger(|_| {}).build();
    let rt = common::attach(&recorder, 1, TokioAttachOptions::default());

    let trigger = recorder.handle().dump_trigger().expect("trigger wired");

    let receipt = rt
        .block_on(async { trigger.dump_current_data().await })
        .expect("checkpoint-only dump should succeed");
    assert!(receipt.manifest_key.is_none());

    drop(rt);
    drop(recorder);
}

/// Concurrent current-data dumps never build an unbounded checkpoint backlog.
/// Depending on whether the flush thread consumes the first command before the
/// second is submitted, both may succeed or one may report a busy checkpoint.
#[test]
fn concurrent_current_data_dumps_are_bounded() {
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();

    let writer = fast_sealing_writer(dir.path());

    let recorder = recorder(writer)
        .worker_poll_interval(Duration::from_millis(50))
        .with_custom_pipeline(|p| p.gzip().write_back())
        .with_dump_trigger(|_| {})
        .build();
    let rt = common::attach(&recorder, 2, TokioAttachOptions::default());

    let trigger = recorder.handle().dump_trigger().expect("trigger wired");

    // A triggered worker parks until a dump is requested, so a confirmed-sealed
    // segment persists in the ring for the concurrent dumps to capture.
    wait_for_sealed_segment(&rt, dir.path());

    let (first, second) = rt.block_on(async {
        // Fire two dumps concurrently.
        tokio::join!(
            trigger.dump_current_data().with_metadata("reason", "a"),
            trigger.dump_current_data().with_metadata("reason", "b"),
        )
    });

    let outcomes = [first, second];
    let successful: Vec<_> = outcomes
        .iter()
        .filter_map(|outcome| outcome.as_ref().ok())
        .collect();
    assert!(
        !successful.is_empty(),
        "the single available control slot accepts at least one dump"
    );
    if successful.len() == 2 {
        assert_ne!(
            successful[0].dump_id, successful[1].dump_id,
            "accepted concurrent dumps get distinct ids"
        );
    }
    for outcome in outcomes {
        if let Err(error) = outcome {
            assert!(
                matches!(
                    error,
                    DumpError::Checkpoint(ref error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                ),
                "a rejected concurrent dump reports bounded backpressure: {error}"
            );
        }
    }

    drop(rt);
    drop(recorder);
}

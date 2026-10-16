use super::S3FallbackProcessor;
use crate::background_task::{GzipCompressor, S3PipelineUploader, SegmentProcessor};
use dial9_core::dump::DumpError;
use dial9_destinations_s3::S3Config;
use rstest::rstest;
use std::time::{Duration, SystemTime};

#[path = "../../../tests/fake_s3/mod.rs"]
mod fake_s3;

#[rstest]
#[case::fallible_preset(true)]
#[case::infallible_preset(false)]
#[tokio::test(start_paused = true)]
async fn s3_presets_preserve_dump_deadline(#[case] fallible: bool) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("test-bucket")).unwrap();
    let client = fake_s3::fake_s3_client_always_failing(directory.path());
    let client = aws_sdk_s3::Client::from_conf(
        client
            .config()
            .to_builder()
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::standard().with_max_attempts(1))
            .build(),
    );
    let config = S3Config::builder()
        .bucket("test-bucket")
        .service_name("deadline-test")
        .region("us-east-1")
        .build();
    let uploader = S3PipelineUploader::new(config, Some(client));
    let terminal: Box<dyn SegmentProcessor> = if fallible {
        Box::new(S3FallbackProcessor {
            uploader,
            write_back: None,
        })
    } else {
        Box::new(uploader)
    };
    // The real worker and test share the clock started paused by the attribute.
    let pipeline =
        dial9_core::test_util::spawn_triggered_pipeline(vec![Box::new(GzipCompressor), terminal]);
    pipeline.seal(
        0,
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    );
    let lookforward = Duration::from_millis(500);
    let started = tokio::time::Instant::now();
    let result = pipeline
        .trigger
        .dump_time_range(Duration::from_secs(3600), lookforward)
        .await;
    let elapsed = started.elapsed();
    pipeline.shutdown().await;
    assert!(
        matches!(result, Err(DumpError::PipelineUnavailable)),
        "an unavailable S3 stage must respect the dump deadline: {result:?}"
    );
    assert!(
        elapsed <= lookforward + Duration::from_millis(1),
        "dump exceeded its paused-time deadline: {elapsed:?}"
    );
}

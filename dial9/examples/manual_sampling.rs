//! Sampler-owned preparation and coordination around the application entry point.
use dial9::{Dial9HandleTokioExt, MemoryBuffer, RecorderS3ClientExt, TokioAttachOptions};
use dial9_sampling::{Sampling, SamplingStatus, StartupFailure, s3::S3SamplingStore};
use std::{io, time::Duration};

fn run_application(status: SamplingStatus, bucket: &str) -> io::Result<()> {
    let seconds = std::env::var("DIAL9_SMOKE_SECONDS")
        .unwrap_or_else(|_| "30".into())
        .parse::<u64>()
        .map_err(io::Error::other)?;
    let SamplingStatus { enabled, .. } = status;
    let recorder = if enabled {
        let s3 = dial9::s3::S3Config::builder()
            .bucket(bucket)
            .service_name("dial9-sampling-smoke")
            .prefix("dial9-traces/sampling-smoke")
            .build();
        let writer = MemoryBuffer::builder()
            .max_total_size(16 * 1024 * 1024)
            .max_segment_size(1024 * 1024)
            .rotation_period(Duration::from_secs(5))
            .build()?;
        dial9::recorder(writer)
            // Upload initialization and credentials belong to the recorder's worker runtime.
            .with_s3_uploader_client_future(s3, async {
                let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
                aws_sdk_s3::Client::new(&config)
            })
            .build()
    } else {
        dial9::recorder_disabled()
    };
    status.register_recorder(&recorder);
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(2);
    let runtime = recorder.handle().attach_tokio_runtime(
        builder,
        TokioAttachOptions::builder()
            .runtime_name("sampling-smoke")
            .task_tracking_enabled(true)
            .build(),
    )?;
    dial9::block_on(&runtime, async move {
        dial9::spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
        })
        .await
        .expect("workload task");
        tokio::time::sleep(Duration::from_secs(seconds)).await;
    });
    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(5));
    Ok(())
}

fn main() -> io::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let bucket = std::env::var("DIAL9_S3_BUCKET").map_err(io::Error::other)?;
    let scope = std::env::var("DIAL9_SAMPLING_SCOPE").map_err(io::Error::other)?;
    let target = std::env::var("DIAL9_SAMPLING_TARGET")
        .unwrap_or_else(|_| "2".into())
        .parse::<u32>()
        .map_err(io::Error::other)?;
    Sampling::builder()
        .scope(scope)
        .add_dimension("architecture", std::env::consts::ARCH)
        .exact_target(target)
        .startup_timeout(Duration::from_secs(5))
        .if_failed(StartupFailure::disabled())
        .build()
        .with_backend(async {
            // Initialization and refresh tasks use the sampler's long-lived runtime.
            let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            Ok(S3SamplingStore::builder()
                .client(aws_sdk_s3::Client::new(&config))
                .bucket(&bucket)
                .build())
        })
        .start_application(|status| run_application(status, &bucket))
}

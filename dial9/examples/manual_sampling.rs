//! Standalone sampler with ordinary local ownership and two independent S3 clients.
use dial9::{Dial9HandleTokioExt, MemoryBuffer, RecorderS3ClientExt, TokioAttachOptions};
use dial9_sampling::{Sampling, StartupFailure, s3::S3SamplingStore};
use std::{io, time::Duration};

async fn configure() -> io::Result<dial9::AttachedRuntime> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let bucket = std::env::var("DIAL9_S3_BUCKET").map_err(io::Error::other)?;
    let scope = std::env::var("DIAL9_SAMPLING_SCOPE").map_err(io::Error::other)?;
    let target = std::env::var("DIAL9_SAMPLING_TARGET")
        .unwrap_or_else(|_| "2".into())
        .parse::<u32>()
        .map_err(io::Error::other)?;
    let sampling = Sampling::builder()
        .scope(scope)
        .add_dimension("architecture", std::env::consts::ARCH)
        .exact_target(target)
        .startup_timeout(Duration::from_secs(5))
        .if_failed(StartupFailure::disabled())
        .build();
    // Client 1 is initialized and maintained on the sampler-owned runtime.
    let sampler = sampling
        .prepare(async {
            let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            Ok(S3SamplingStore::builder()
                .client(aws_sdk_s3::Client::new(&config))
                .bucket(bucket.clone())
                .prefix("_dial9_sampling/sampling-smoke/")
                .build())
        })
        .await;
    tracing::info!(?sampler, "manual sampling startup");
    let recorder = if sampler.enabled() {
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
            // Client 2 belongs to the recorder worker runtime.
            .with_s3_uploader_client_future(s3, async {
                let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
                aws_sdk_s3::Client::new(&config)
            })
            .build()
    } else {
        dial9::recorder_disabled()
    };
    let stopped = sampler.stopped();
    let recorder = recorder.attach_object(sampler);
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(2);
    let runtime = recorder.handle().attach_tokio_runtime(
        builder,
        TokioAttachOptions::builder()
            .runtime_name("sampling-smoke")
            .task_tracking_enabled(true)
            .build(),
    )?;
    let handle = recorder.handle().clone();
    runtime.spawn(async move {
        stopped.await;
        handle.disable();
    });
    Ok((recorder, runtime))
}

fn main() -> io::Result<()> {
    let (recorder, runtime) = {
        // The bootstrap executor can disappear as soon as configuration completes.
        let bootstrap = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        bootstrap.block_on(configure())?
    };
    runtime.block_on(async {
        dial9::spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
        })
        .await
        .expect("workload task");
    });
    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(5));
    Ok(())
}

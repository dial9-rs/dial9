use dial9_core::pipeline::{DisableRecording, SegmentProcessor};
use dial9_destinations_s3::{FailureBehavior, S3Config, S3PipelineUploader};
use std::cell::Cell;
use std::error::Error;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc, oneshot};

#[tokio::test]
async fn cancelled_initialization_resumes_and_caches_each_outcome() {
    let client = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .build(),
    );
    for outcome in [
        Ok(client),
        Err(FailureBehavior::DisableS3(
            io::Error::new(io::ErrorKind::PermissionDenied, "credentials denied").into(),
        )),
        Err(FailureBehavior::DisableDial9(
            io::Error::new(io::ErrorKind::PermissionDenied, "credentials denied").into(),
        )),
    ] {
        let disable_dial9 = matches!(outcome, Err(FailureBehavior::DisableDial9(_)));
        let disable_s3 = matches!(outcome, Err(FailureBehavior::DisableS3(_)));
        let (started_tx, started_rx) = oneshot::channel();
        let (complete_tx, complete_rx) = oneshot::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let future_calls = calls.clone();
        let mut uploader = S3PipelineUploader::new(
            S3Config::builder()
                .bucket("unused")
                .service_name("test")
                .region("us-east-1")
                .build(),
            None,
        )
        .with_fallible_client_future(async move {
            let not_sync = Cell::new(0);
            future_calls.fetch_add(1, Ordering::SeqCst);
            started_tx.send(()).unwrap();
            let result = complete_rx.await.unwrap();
            not_sync.set(1);
            result
        });

        let mut initialization = uploader.initialize();
        tokio::select! {
            biased;
            result = &mut initialization => {
                panic!("constructor completed before being released: {result:?}");
            }
            result = started_rx => result.unwrap(),
        }
        drop(initialization);
        assert!(!uploader.is_disabled());
        complete_tx.send(outcome).unwrap();

        for _ in 0..2 {
            let result = uploader.initialize().await;
            if disable_dial9 {
                let error = result.unwrap_err();
                let marker = error
                    .get_ref()
                    .unwrap()
                    .downcast_ref::<DisableRecording>()
                    .unwrap();
                let original = marker
                    .source()
                    .unwrap()
                    .source()
                    .unwrap()
                    .downcast_ref::<io::Error>()
                    .unwrap();
                assert_eq!(original.kind(), io::ErrorKind::PermissionDenied);
                assert_eq!(original.to_string(), "credentials denied");
                assert!(error.to_string().contains("credentials denied"));
            } else {
                result.unwrap();
            }
            assert!(
                std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(uploader.wait_until_live().as_mut().poll(cx).is_ready())
                })
                .await
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(uploader.is_disabled(), disable_s3);
        assert_eq!(
            uploader.name(),
            if uploader.is_disabled() {
                "S3Disabled"
            } else {
                "S3Upload"
            }
        );
    }
}

#[test]
fn both_failure_policies_preserve_the_original_error() {
    for failure in [
        FailureBehavior::DisableDial9(io::Error::other("credential refresh unavailable").into()),
        FailureBehavior::DisableS3(io::Error::other("credential refresh unavailable").into()),
    ] {
        assert!(
            failure
                .to_string()
                .contains("credential refresh unavailable")
        );
        let original = failure
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert_eq!(original.to_string(), "credential refresh unavailable");
    }
}

struct RegionGate<S> {
    inner: S,
    started: mpsc::UnboundedSender<()>,
    permits: Arc<Semaphore>,
    uploads: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl<S: s3s::S3 + Send + Sync> s3s::S3 for RegionGate<S> {
    async fn head_bucket(
        &self,
        _request: s3s::S3Request<s3s::dto::HeadBucketInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadBucketOutput>> {
        self.started.send(()).unwrap();
        self.permits.acquire().await.unwrap().forget();
        Ok(s3s::S3Response::new(s3s::dto::HeadBucketOutput {
            bucket_region: Some("eu-west-1".into()),
            ..Default::default()
        }))
    }

    async fn put_object(
        &self,
        request: s3s::S3Request<s3s::dto::PutObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
        if request.region.as_ref().map(|region| region.as_str()) != Some("eu-west-1") {
            return Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::PermanentRedirect,
                "client was not corrected to the discovered region",
            ));
        }
        let response = self.inner.put_object(request).await?;
        self.uploads.fetch_add(1, Ordering::SeqCst);
        Ok(response)
    }
}

#[tokio::test]
async fn cancellation_during_region_discovery_retains_the_committed_client() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("test-bucket")).unwrap();
    let (started, mut requests) = mpsc::unbounded_channel();
    let permits = Arc::new(Semaphore::new(0));
    let uploads = Arc::new(AtomicUsize::new(0));
    let mut service_builder = s3s::service::S3ServiceBuilder::new(RegionGate {
        inner: s3s_fs::FileSystem::new(directory.path()).unwrap(),
        started,
        permits: permits.clone(),
        uploads: uploads.clone(),
    });
    service_builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
    let service = service_builder.build();
    let client = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .http_client(s3s_aws::Client::from(service))
            .force_path_style(true)
            .build(),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let future_calls = calls.clone();
    let mut uploader = S3PipelineUploader::new(
        S3Config::builder()
            .bucket("test-bucket")
            .service_name("test")
            .build(),
        None,
    )
    .with_fallible_client_future(async move {
        future_calls.fetch_add(1, Ordering::SeqCst);
        Ok(client)
    });

    let mut initialization = uploader.initialize();
    tokio::select! {
        result = &mut initialization => panic!("HeadBucket must stay gated: {result:?}"),
        request = requests.recv() => request.unwrap(),
    }
    drop(initialization);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    permits.add_permits(1);
    uploader.initialize().await.unwrap();
    requests
        .try_recv()
        .expect("region discovery retried after cancellation");
    uploader.initialize().await.unwrap();
    assert!(
        requests.try_recv().is_err(),
        "ready initialization must not query the region again"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let mut encoder = dial9_trace_format::encoder::Encoder::new_to(Vec::new()).unwrap();
    encoder.write_infallible(&dial9_core::format::ClockSyncEvent {
        timestamp_ns: 1,
        realtime_ns: 1_000_000_000,
    });
    tokio::time::timeout(
        Duration::from_secs(10),
        dial9_core::test_util::run_pipeline_continuous(
            vec![encoder.into_inner()],
            vec![Box::new(uploader)],
            Duration::from_millis(10),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        uploads.load(Ordering::SeqCst),
        1,
        "the retained client must upload in the discovered region"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

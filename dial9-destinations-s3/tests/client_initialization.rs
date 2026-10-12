use dial9_core::pipeline::{DisableRecording, SegmentProcessor};
use dial9_destinations_s3::{FailureBehavior, S3Config, S3PipelineUploader};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::oneshot;

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
        Err(FailureBehavior::DisableS3),
        Err(FailureBehavior::DisableDial9),
    ] {
        let behavior = outcome.as_ref().err().copied();
        let (started_tx, started_rx) = oneshot::channel();
        let (complete_tx, complete_rx) = oneshot::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let future_calls = calls.clone();
        let mut uploader = S3PipelineUploader::from_client_future()
            .config(
                S3Config::builder()
                    .bucket("unused")
                    .service_name("test")
                    .region("us-east-1")
                    .build(),
            )
            .client_future(async move {
                let not_sync = Cell::new(0);
                future_calls.fetch_add(1, Ordering::SeqCst);
                started_tx.send(()).unwrap();
                let result = complete_rx.await.unwrap();
                not_sync.set(1);
                result
            })
            .build();

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
            match behavior {
                Some(FailureBehavior::DisableDial9) => assert!(
                    result
                        .unwrap_err()
                        .get_ref()
                        .unwrap()
                        .is::<DisableRecording>()
                ),
                _ => result.unwrap(),
            }
            assert!(
                std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(uploader.wait_until_live().as_mut().poll(cx).is_ready())
                })
                .await
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            uploader.is_disabled(),
            behavior == Some(FailureBehavior::DisableS3)
        );
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

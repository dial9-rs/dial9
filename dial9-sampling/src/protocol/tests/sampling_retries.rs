#![cfg(feature = "s3")]
use crate::protocol::*;
use crate::s3::S3SamplingStore;
use aws_smithy_runtime_api::client::{
    http::{HttpClient, HttpConnector, HttpConnectorFuture, SharedHttpConnector, http_client_fn},
    orchestrator::HttpRequest,
    result::ConnectorError,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Debug, Clone)]
struct LoseSlotResponse {
    inner: SharedHttpConnector,
    armed: Arc<AtomicBool>,
    attempts: Arc<AtomicUsize>,
}
impl HttpConnector for LoseSlotResponse {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let slot = request.method() == "PUT" && request.uri().contains("/slots/");
        let inner = self.inner.clone();
        let armed = self.armed.clone();
        let attempts = self.attempts.clone();
        HttpConnectorFuture::new_boxed(Box::pin(async move {
            if slot {
                attempts.fetch_add(1, Ordering::SeqCst);
            }
            // Perform the actual conditional write before losing its response.
            let response = inner.call(request).await?;
            if slot && response.status().is_success() && armed.swap(false, Ordering::SeqCst) {
                return Err(ConnectorError::io(Box::new(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "response lost after slot PUT committed",
                ))));
            }
            Ok(response)
        }))
    }
}

#[rstest::rstest]
#[case(1)]
#[case(2)]
#[tokio::test]
async fn committed_claim_with_lost_response_is_reconciled(#[case] target: u32) {
    let root = tempfile::tempdir().unwrap();
    let fs = s3s_fs::FileSystem::new(root.path()).unwrap();
    let mut service = s3s::service::S3ServiceBuilder::new(fs);
    service.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
    let base_client: s3s_aws::Client = service.build().into();
    let armed = Arc::new(AtomicBool::new(true));
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let http = http_client_fn(move |settings, components| {
        SharedHttpConnector::new(LoseSlotResponse {
            inner: base_client.http_connector(settings, components),
            armed: armed.clone(),
            attempts: count.clone(),
        })
    });
    let client = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(http)
            // The adapter must override even a supplied retry-enabled client.
            .retry_config(
                aws_sdk_s3::config::retry::RetryConfig::standard()
                    .with_initial_backoff(Duration::from_millis(1))
                    .with_max_attempts(3),
            )
            .force_path_style(true)
            .build(),
    );
    client
        .create_bucket()
        .bucket("traces")
        .send()
        .await
        .unwrap();
    let store = Arc::new(
        S3SamplingStore::builder()
            .client(client)
            .bucket("traces")
            .prefix("coord/")
            .build(),
    );
    let policy = Config::builder()
        .scope("retry-reconciliation")
        .exact_target(target)
        .if_failed(StartupFailure::disabled())
        .build();
    // This is an outcome test using real asynchronous filesystem I/O, not an
    // elapsed-time assertion. The budget is a watchdog for the local service.
    let node = policy
        .prepare_with_timeout(store.clone(), Duration::from_secs(30))
        .await;
    assert!(
        matches!(node.snapshot().reason(), DecisionReason::Admitted { .. }),
        "{:?}",
        node.snapshot()
    );
    assert!(node.snapshot().coordinated());
    let allocations = store
        .list(
            ListRequest::builder()
                .prefix(format!("{}slots/", policy.pool_prefix()))
                .page_size(100)
                .build(),
        )
        .await
        .unwrap();
    assert!(allocations.continuation().is_none());
    assert_eq!(allocations.objects().len(), 1);
    // One lost claim plus one owner-fencing PUT; no hidden SDK PUT retry.
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let key = allocations.objects()[0].key().to_string();
    let before = store.get(key.clone()).await.unwrap().unwrap();
    let lease: serde_json::Value = serde_json::from_slice(before.body()).unwrap();
    assert!(lease["owner"].is_string());

    node.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(30))
        .await;
    let after = store.get(key).await.unwrap().unwrap();
    let released: serde_json::Value = serde_json::from_slice(after.body()).unwrap();
    assert!(released["owner"].is_null());
}

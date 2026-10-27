//! Signed native SDK requests against an in-process filesystem-backed S3 service.

use super::*;
use crate::s3::S3SamplingStore;
use aws_smithy_runtime_api::client::{
    http::{HttpClient, HttpConnector, HttpConnectorFuture, SharedHttpConnector, http_client_fn},
    orchestrator::HttpRequest,
    result::ConnectorError,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Debug, Clone)]
struct LoseRosterResponse {
    inner: SharedHttpConnector,
    armed: Arc<AtomicBool>,
    attempts: Arc<AtomicUsize>,
    unconditional: Arc<AtomicUsize>,
}
impl HttpConnector for LoseRosterResponse {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let roster = request.method() == "PUT" && request.uri().contains("/roster.json");
        if roster
            && request.headers().get("if-match").is_none()
            && request.headers().get("if-none-match").is_none()
        {
            self.unconditional.fetch_add(1, Ordering::SeqCst);
        }
        let inner = self.inner.clone();
        let armed = self.armed.clone();
        let attempts = self.attempts.clone();
        HttpConnectorFuture::new_boxed(Box::pin(async move {
            if roster {
                attempts.fetch_add(1, Ordering::SeqCst);
            }
            // Commit the real conditional write before losing its response.
            let response = inner.call(request).await?;
            if roster && response.status().is_success() && armed.swap(false, Ordering::SeqCst) {
                return Err(ConnectorError::io(Box::new(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "response lost after roster PUT committed",
                ))));
            }
            Ok(response)
        }))
    }
}

struct Harness {
    _root: tempfile::TempDir,
    store: Arc<S3SamplingStore>,
    attempts: Arc<AtomicUsize>,
    unconditional: Arc<AtomicUsize>,
}
impl Harness {
    async fn new(lose_ack: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let fs = s3s_fs::FileSystem::new(root.path()).unwrap();
        let mut service = s3s::service::S3ServiceBuilder::new(fs);
        service.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        let base_client: s3s_aws::Client = service.build().into();
        let armed = Arc::new(AtomicBool::new(lose_ack));
        let attempts = Arc::new(AtomicUsize::new(0));
        let unconditional = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let unguarded = unconditional.clone();
        let http = http_client_fn(move |settings, components| {
            SharedHttpConnector::new(LoseRosterResponse {
                inner: base_client.http_connector(settings, components),
                armed: armed.clone(),
                attempts: count.clone(),
                unconditional: unguarded.clone(),
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
                // The adapter must override even a retry-enabled SDK client.
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
        Self {
            _root: root,
            store: Arc::new(
                S3SamplingStore::builder()
                    .client(client)
                    .bucket("traces")
                    .prefix("coord/")
                    .build(),
            ),
            attempts,
            unconditional,
        }
    }
}

fn config(scope: &str, target: u32) -> Config {
    Config::builder()
        .scope(scope)
        .exact_target(target)
        .host_id("stable-host")
        .if_failed(super::super::StartupFailure::disabled())
        .build()
}
fn worker(config: Config, store: Arc<S3SamplingStore>) -> Worker {
    let (signal, _) = watch::channel(true);
    Worker::new(
        config,
        store,
        Arc::new(WallClock),
        None,
        signal,
        Arc::new(Mutex::new(SamplingSnapshot {
            recording: true,
            reason: DecisionReason::MissingRoster,
            coordinated: false,
            estimated_population: None,
        })),
    )
}

// These assert outcomes of real asynchronous filesystem I/O, not elapsed time.
// The existing local service cannot run under the deterministic Shuttle executor.
#[rstest::rstest]
#[case(1)]
#[case(15)]
#[tokio::test]
async fn committed_roster_claim_with_lost_response_requires_readback(#[case] target: u32) {
    let harness = Harness::new(true).await;
    let policy = config("retry-reconciliation", target);
    let worker = worker(policy.clone(), harness.store.clone());
    let roster = Roster::initial(
        &policy,
        &worker.incarnation,
        millis(worker.clock.now()).unwrap(),
    );
    let error = worker.replace(&roster, None).await.err().unwrap();
    assert!(matches!(
        error.kind(),
        StoreErrorKind::WriteOutcomeUnknown { .. }
    ));
    assert!(worker.leader.is_none());
    assert_eq!(harness.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(harness.unconditional.load(Ordering::SeqCst), 0);
    let committed = worker.read_roster().await.unwrap().unwrap();
    assert_eq!(committed.roster.holder, worker.incarnation);
    assert_eq!(committed.roster.revision, roster.revision);
    // An explicit stale retry would fail its condition. It cannot be folded
    // into the earlier uncertain call or treated as proof that no write landed.
    assert!(worker.replace(&roster, None).await.unwrap().is_none());
    assert_eq!(harness.attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn takeover_fences_stale_roster_publication_on_native_s3() {
    let harness = Harness::new(false).await;
    let policy = config("takeover-fencing", 15);
    let worker = worker(policy.clone(), harness.store.clone());
    let now = millis(worker.clock.now()).unwrap();
    let mut initial = Roster::initial(&policy, &worker.incarnation, now);
    let first = worker.replace(&initial, None).await.unwrap().unwrap();
    let mut successor = initial.clone();
    let next_holder = nonce();
    successor.mutate(&policy, &next_holder, now).unwrap();
    let takeover = worker
        .replace(&successor, Some(&first.version))
        .await
        .unwrap()
        .unwrap();
    initial.mutate(&policy, &worker.incarnation, now).unwrap();
    assert!(
        worker
            .replace(&initial, Some(&first.version))
            .await
            .unwrap()
            .is_none()
    );
    let current = worker.read_roster().await.unwrap().unwrap();
    assert_eq!(current.roster.holder, next_holder);
    assert_eq!(current.version, takeover.version);
    assert_eq!(harness.attempts.load(Ordering::SeqCst), 3);
    assert_eq!(harness.unconditional.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn membership_census_and_old_cleanup_isolate_process_incarnations() {
    let harness = Harness::new(false).await;
    let policy = config("restart-membership", 15);
    let mut old = worker(policy.clone(), harness.store.clone());
    let mut replacement = worker(policy.clone(), harness.store.clone());
    assert_eq!(old.rank, replacement.rank);
    assert_ne!(old.incarnation, replacement.incarnation);
    let bucket = old.bucket().unwrap();
    assert!(bucket > 0);
    old.write_member(bucket - 1).await.unwrap();
    old.write_member(bucket).await.unwrap();
    replacement.write_member(bucket - 1).await.unwrap();
    replacement.write_member(bucket).await.unwrap();
    // Leave the old active key present to exercise carrier-priority dedup.
    old.state = MemberState::Carrier;
    old.write_member(bucket).await.unwrap();
    let members = census(harness.store.as_ref(), &policy, bucket)
        .await
        .unwrap();
    assert_eq!(members.len(), 2);
    assert!(
        members
            .iter()
            .any(|member| member.boundary.incarnation == old.incarnation
                && member.state == MemberState::Carrier)
    );
    assert!(members.iter().any(
        |member| member.boundary.incarnation == replacement.incarnation
            && member.state == MemberState::Active
    ));
    old.cleanup().await;
    let remaining = census(harness.store.as_ref(), &policy, bucket)
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].boundary.incarnation, replacement.incarnation);
    assert_eq!(remaining[0].state, MemberState::Active);
    assert_eq!(harness.attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn old_settled_cutoff_is_usable_at_startup_on_native_s3() {
    let harness = Harness::new(false).await;
    let policy = config("old-cutoff-continuity", 15);
    let worker = worker(policy.clone(), harness.store.clone());
    let mut roster = Roster::initial(&policy, &worker.incarnation, 1000);
    roster.cutoff_settled = true;
    roster.settled = true;
    roster.arm_cutoff = Cutoff::threshold(policy.tracked_limit());
    worker.replace(&roster, None).await.unwrap().unwrap();
    let prepared = prepare_using(
        policy.clone(),
        harness.store.clone(),
        Duration::from_secs(30),
        None,
        Arc::new(WallClock),
    )
    .await;
    let snapshot = prepared.snapshot();
    assert!(snapshot.recording());
    assert_eq!(snapshot.reason(), &DecisionReason::Roster);
    assert!(snapshot.coordinated);
    prepared
        .into_session()
        .unwrap()
        .shutdown(Duration::from_secs(30))
        .await;
    assert_eq!(harness.unconditional.load(Ordering::SeqCst), 0);
}

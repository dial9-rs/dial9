#![cfg(not(shuttle))]
use crate::backend::{
    ListPage, ListRequest, PutOutcome, PutRequest, SamplingStore, StoreFuture, StoredObject,
};
use crate::protocol::{
    simulation::{self, MemoryStore},
    *,
};
use std::{sync::Arc, time::Duration};

fn policy(target: u32) -> Config {
    Config::builder()
        .scope("test/prod")
        .add_dimension("az", "a/b")
        .exact_target(target)
        .if_failed(StartupFailure::disabled())
        .build()
}
async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn concurrent_startups_fill_exact_quota_without_rotating() {
    let store = Arc::new(MemoryStore::new());
    let fleet = simulation::prepare_fleet(policy(15), store.clone(), 200).await;
    assert_eq!(
        fleet
            .iter()
            .filter(|node| node.snapshot().recording())
            .count(),
        15
    );
    let original: Vec<_> = fleet
        .iter()
        .map(|node| node.snapshot().recording())
        .collect();
    for _ in 0..6 {
        tokio::time::advance(Duration::from_secs(60)).await;
        settle().await;
        assert_eq!(
            fleet
                .iter()
                .map(|node| node.snapshot().recording())
                .collect::<Vec<_>>(),
            original
        );
    }
}

#[tokio::test(start_paused = true)]
async fn full_pool_does_not_invoke_probability_fallback() {
    let store = Arc::new(MemoryStore::new());
    let policy = Config::builder()
        .scope("full")
        .exact_target(1)
        .if_failed(StartupFailure::enable_with_fixed_probability(1.0))
        .build();
    let first = policy.prepare(store.clone()).await;
    let second = policy.prepare(store).await;
    assert!(first.snapshot().recording());
    assert!(!second.snapshot().recording());
    assert!(matches!(
        second.snapshot().reason(),
        DecisionReason::Full { .. }
    ));
}

#[rstest::rstest]
#[case(0.0, false)]
#[case(1.0, true)]
#[tokio::test(start_paused = true)]
async fn startup_timeout_is_bounded_and_applies_fallback_once(
    #[case] probability: f64,
    #[case] expected: bool,
) {
    let store = Arc::new(MemoryStore::new());
    store.set_latency(Duration::from_secs(5));
    let policy = Config::builder()
        .scope("timeout")
        .exact_target(1)
        .if_failed(StartupFailure::enable_with_fixed_probability(probability))
        .build();
    let before = tokio::time::Instant::now();
    let prepared = policy.prepare(store.clone()).await;
    assert_eq!(
        tokio::time::Instant::now() - before,
        DEFAULT_PREPARE_TIMEOUT
    );
    assert_eq!(prepared.snapshot().recording(), expected);
    store.set_latency(Duration::ZERO);
    tokio::time::advance(Duration::from_secs(600)).await;
    settle().await;
    assert_eq!(prepared.snapshot().recording(), expected);
    assert!(!prepared.snapshot().coordinated());
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test(start_paused = true)]
async fn lease_loss_is_sticky_and_old_cleanup_cannot_release_replacement(#[case] strict: bool) {
    let store = Arc::new(MemoryStore::new());
    let policy = Config::builder()
        .scope("outage")
        .exact_target(1)
        .lease_loss(if strict {
            LeaseLoss::stop_profiling()
        } else {
            LeaseLoss::keep_profiling()
        })
        .if_failed(StartupFailure::disabled())
        .build();
    let original = simulation::prepare(&policy, store.clone()).await;
    assert!(original.snapshot().recording());
    store.set_unavailable(true);
    tokio::time::advance(Duration::from_secs(306)).await;
    settle().await;
    assert_eq!(original.snapshot().recording(), !strict);
    assert!(!original.snapshot().coordinated());
    store.set_unavailable(false);
    let replacement = simulation::prepare(&policy, store.clone()).await;
    assert!(replacement.snapshot().coordinated());
    assert_eq!(original.snapshot().recording(), !strict);
    original
        .into_session()
        .unwrap()
        .shutdown(DEFAULT_PREPARE_TIMEOUT)
        .await;
    assert!(replacement.snapshot().coordinated());
    assert!(!policy.prepare(store).await.snapshot().recording());
}

#[tokio::test(start_paused = true)]
async fn graceful_exit_releases_slot_for_short_lived_process() {
    let store = Arc::new(MemoryStore::new());
    let policy = policy(1);
    for _ in 0..30 {
        let node = simulation::prepare(&policy, store.clone()).await;
        assert!(node.snapshot().recording());
        tokio::time::advance(Duration::from_secs(30)).await;
        node.into_session()
            .unwrap()
            .shutdown(DEFAULT_PREPARE_TIMEOUT)
            .await;
    }
}

#[tokio::test(start_paused = true)]
async fn unknown_claim_acknowledgement_is_fenced_without_second_allocation() {
    let store = Arc::new(MemoryStore::new());
    let policy = policy(2);
    // Establish policy first, so the lost acknowledgement applies to a claim.
    let seed = policy.prepare(store.clone()).await;
    seed.into_session()
        .unwrap()
        .shutdown(DEFAULT_PREPARE_TIMEOUT)
        .await;
    store.lose_write_acknowledgements(1);
    let node = policy.prepare(store.clone()).await;
    assert!(node.snapshot().recording());
    let owned = store
        .objects()
        .iter()
        .filter(|(key, obj)| {
            key.contains("/slots/")
                && serde_json::from_slice::<serde_json::Value>(obj.body()).unwrap()["owner"]
                    .is_string()
        })
        .count();
    assert_eq!(owned, 1);
}

#[tokio::test(start_paused = true)]
async fn labels_are_canonical_and_lease_mismatch_fails_closed() {
    let store = Arc::new(MemoryStore::new());
    let a = Config::builder()
        .scope("groups")
        .exact_target(1)
        .add_dimension("az", "x")
        .add_dimension("type", "y")
        .if_failed(StartupFailure::disabled())
        .build();
    let b = Config::builder()
        .scope("groups")
        .exact_target(1)
        .add_dimension("type", "y")
        .add_dimension("az", "x")
        .if_failed(StartupFailure::disabled())
        .build();
    assert_eq!(a.pool_prefix(), b.pool_prefix());
    let first = a.prepare(store.clone()).await;
    assert!(first.snapshot().recording());
    assert!(!b.prepare(store.clone()).await.snapshot().recording());
    let mismatch = Config::builder()
        .scope("groups")
        .exact_target(1)
        .lease_duration(Duration::from_secs(400))
        .add_dimension("type", "y")
        .add_dimension("az", "x")
        .if_failed(StartupFailure::enable_with_fixed_probability(1.0))
        .build()
        .prepare(store)
        .await;
    assert!(matches!(
        mismatch.snapshot().reason(),
        DecisionReason::IncompatiblePolicy { .. }
    ));
    assert!(!mismatch.snapshot().recording());
}

#[tokio::test(start_paused = true)]
async fn invalid_configuration_and_zero_target_never_contact_store() {
    let store = Arc::new(MemoryStore::new());
    let zero = Config::builder()
        .scope("zero")
        .exact_target(0)
        .if_failed(StartupFailure::enable_with_fixed_probability(1.0))
        .build();
    assert!(!zero.prepare(store.clone()).await.snapshot().recording());
    let invalid = Config::builder()
        .scope("bad")
        .exact_target(1)
        .add_dimension("az", "a")
        .add_dimension("az", "b")
        .if_failed(StartupFailure::disabled())
        .build();
    assert!(!invalid.prepare(store.clone()).await.snapshot().recording());
    assert_eq!(store.operation_count(), 0);
}

/// Object storage can finish a PUT even after the caller cancels its future.
#[derive(Debug)]
struct DelayedCommit {
    inner: Arc<MemoryStore>,
    delay_claim: std::sync::atomic::AtomicBool,
}
impl SamplingStore for DelayedCommit {
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        self.inner.get(key)
    }
    fn list(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        self.inner.list(request)
    }
    fn delete(&self, key: String) -> StoreFuture<'_, ()> {
        self.inner.delete(key)
    }
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
        if request.key().contains("/slots/")
            && self
                .delay_claim
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let inner = self.inner.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let result = inner.put(request).await;
                let _ = tx.send(result);
            });
            Box::pin(async move { rx.await.expect("server operation survived cancellation") })
        } else {
            self.inner.put(request)
        }
    }
}
#[tokio::test(start_paused = true)]
async fn late_claim_after_timeout_cannot_enable_or_take_another_slot() {
    let store = Arc::new(DelayedCommit {
        inner: Arc::new(MemoryStore::new()),
        delay_claim: true.into(),
    });
    let policy = policy(1);
    let timed_out = policy.prepare(store.clone()).await;
    assert!(!timed_out.snapshot().recording());
    assert!(matches!(
        timed_out.snapshot().reason(),
        DecisionReason::Timeout { .. }
    ));
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert!(!timed_out.snapshot().recording());
    assert!(!policy.prepare(store.clone()).await.snapshot().recording());
    timed_out
        .into_session()
        .unwrap()
        .shutdown(DEFAULT_PREPARE_TIMEOUT)
        .await;
    assert!(policy.prepare(store).await.snapshot().recording());
}
#[tokio::test(start_paused = true)]
async fn sparse_unsampled_membership_converges_to_bounded_bottom_k() {
    let store = Arc::new(MemoryStore::new());
    let policy = Config::builder()
        .scope("sparse")
        .exact_target(2)
        .membership_sample_size(8)
        .if_failed(StartupFailure::disabled())
        .build();
    let fleet = simulation::prepare_fleet(policy, store.clone(), 200).await;
    for _ in 0..3 {
        tokio::time::advance(Duration::from_secs(60)).await;
        settle().await;
    }
    let members = store
        .objects()
        .keys()
        .filter(|key| key.contains("/members/"))
        .count();
    assert!(
        members <= 8,
        "bottom-k membership must withdraw higher ranks: {members}"
    );
    assert_eq!(
        fleet
            .iter()
            .filter(|node| node.snapshot().recording())
            .count(),
        2
    );
    assert!(
        fleet
            .iter()
            .any(|node| node.snapshot().estimated_population().is_some())
    );
}

#[rstest::rstest]
#[case(5)]
#[case(50)]
#[tokio::test(start_paused = true)]
async fn concurrent_admission_with_storage_latency_remains_bounded(#[case] latency_ms: u64) {
    let store = Arc::new(MemoryStore::new());
    store.set_latency(Duration::from_millis(latency_ms));
    let before = tokio::time::Instant::now();
    let fleet = simulation::prepare_fleet(policy(15), store, 100).await;
    assert!(tokio::time::Instant::now() - before <= DEFAULT_PREPARE_TIMEOUT);
    let admitted = fleet
        .iter()
        .filter(|node| node.snapshot().recording())
        .count();
    assert!(admitted > 0 && admitted <= 15);
    assert!(
        fleet
            .iter()
            .all(|node| !node.snapshot().recording() || node.snapshot().coordinated())
    );
}

#[tokio::test(start_paused = true)]
async fn caller_target_changes_share_slots_without_stopping_existing_profilers() {
    let store = Arc::new(MemoryStore::new());
    let first = policy(1).prepare(store.clone()).await;
    let second = policy(2).prepare(store.clone()).await;
    assert!(first.snapshot().recording());
    assert!(second.snapshot().recording());
    assert_eq!(policy(1).pool_prefix(), policy(2).pool_prefix());
    assert!(
        !policy(1)
            .prepare(store.clone())
            .await
            .snapshot()
            .recording()
    );
    assert!(
        !policy(2)
            .prepare(store.clone())
            .await
            .snapshot()
            .recording()
    );
    second
        .into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
    assert!(first.snapshot().recording());
    assert!(policy(2).prepare(store).await.snapshot().recording());
}

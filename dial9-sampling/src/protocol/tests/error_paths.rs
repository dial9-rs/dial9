#![cfg(not(shuttle))]
use crate::protocol::{
    simulation::{self, MemoryStore},
    *,
};
use bytes::Bytes;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[rstest::rstest]
#[case("scope")]
#[case("dimension-name")]
#[case("dimension-value")]
#[case("probability-nan")]
#[case("probability-infinity")]
#[case("probability-negative")]
#[case("probability-too-large")]
#[case("renewal-zero")]
#[case("lease-too-short")]
#[case("lease-too-long")]
#[case("membership-too-small")]
#[case("membership-too-large")]
#[case("target-too-large")]
#[tokio::test(start_paused = true)]
async fn invalid_configuration_never_reaches_storage(#[case] case: &str) {
    macro_rules! base {
        () => {
            Config::builder().scope("invalid").exact_target(1)
        };
    }
    let policy = match case {
        "scope" => Config::builder()
            .scope(" ")
            .exact_target(1)
            .if_failed(StartupFailure::disabled())
            .build(),
        "dimension-name" => base!()
            .add_dimension(" ", "value")
            .if_failed(StartupFailure::disabled())
            .build(),
        "dimension-value" => base!()
            .add_dimension("name", " ")
            .if_failed(StartupFailure::disabled())
            .build(),
        "probability-nan" => base!()
            .if_failed(StartupFailure::enable_with_fixed_probability(f64::NAN))
            .build(),
        "probability-infinity" => base!()
            .if_failed(StartupFailure::enable_with_fixed_probability(f64::INFINITY))
            .build(),
        "probability-negative" => base!()
            .if_failed(StartupFailure::enable_with_fixed_probability(-0.1))
            .build(),
        "probability-too-large" => base!()
            .if_failed(StartupFailure::enable_with_fixed_probability(1.1))
            .build(),
        "renewal-zero" => base!()
            .renewal_interval(Duration::ZERO)
            .if_failed(StartupFailure::disabled())
            .build(),
        "lease-too-short" => base!()
            .lease_duration(Duration::from_secs(60))
            .if_failed(StartupFailure::disabled())
            .build(),
        "lease-too-long" => base!()
            .lease_duration(Duration::from_secs(86401))
            .if_failed(StartupFailure::disabled())
            .build(),
        "membership-too-small" => base!()
            .membership_sample_size(1)
            .if_failed(StartupFailure::disabled())
            .build(),
        "membership-too-large" => base!()
            .membership_sample_size(1001)
            .if_failed(StartupFailure::disabled())
            .build(),
        "target-too-large" => Config::builder()
            .scope("invalid")
            .exact_target(10001)
            .if_failed(StartupFailure::disabled())
            .build(),
        _ => panic!("unknown test case"),
    };
    let store = Arc::new(MemoryStore::new());
    let node = policy.prepare(store.clone()).await;
    assert!(matches!(
        node.snapshot().reason(),
        DecisionReason::InvalidConfiguration { .. }
    ));
    assert!(!node.snapshot().recording());
    assert_eq!(store.operation_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn overflowing_deadline_fails_closed_without_storage() {
    let store = Arc::new(MemoryStore::new());
    let node = Config::builder()
        .scope("overflow")
        .exact_target(1)
        .if_failed(StartupFailure::enable_with_fixed_probability(1.0))
        .build()
        .prepare_with_timeout(store.clone(), Duration::MAX)
        .await;
    assert!(matches!(
        node.snapshot().reason(),
        DecisionReason::InvalidConfiguration { .. }
    ));
    assert!(!node.snapshot().recording());
    assert_eq!(store.operation_count(), 0);
}

#[rstest::rstest]
#[case("policy.json")]
#[case("slots/00000000.json")]
#[tokio::test(start_paused = true)]
async fn malformed_shared_records_never_invoke_enable_fallback(#[case] suffix: &str) {
    let store = Arc::new(MemoryStore::new());
    let policy = Config::builder()
        .scope("malformed")
        .exact_target(1)
        .if_failed(StartupFailure::enable_with_fixed_probability(1.0))
        .build();
    policy
        .prepare(store.clone())
        .await
        .into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
    store
        .put(
            PutRequest::builder()
                .key(format!("{}{suffix}", policy.pool_prefix()))
                .body(Bytes::from_static(b"malformed JSON"))
                .condition(WriteCondition::unconditional())
                .build(),
        )
        .await
        .unwrap();
    let node = policy.prepare(store).await;
    assert!(matches!(
        node.snapshot().reason(),
        DecisionReason::InvalidConfiguration { .. }
    ));
    assert!(!node.snapshot().recording());
}

#[derive(Debug)]
struct FailingStore(StoreErrorKind);
impl SamplingStore for FailingStore {
    fn get(&self, _: String) -> StoreFuture<'_, Option<StoredObject>> {
        Box::pin(async { Err(StoreError::new(self.0.clone(), "injected GET failure")) })
    }
    fn put(&self, _: PutRequest) -> StoreFuture<'_, PutOutcome> {
        panic!("GET should fail first")
    }
    fn list(&self, _: ListRequest) -> StoreFuture<'_, ListPage> {
        Box::pin(async { Err(StoreError::new(self.0.clone(), "injected LIST failure")) })
    }
    fn delete(&self, _: String) -> StoreFuture<'_, ()> {
        Box::pin(async { Err(StoreError::new(self.0.clone(), "injected DELETE failure")) })
    }
}
#[rstest::rstest]
#[case(StoreErrorKind::unauthorized(), true)]
#[case(StoreErrorKind::unavailable(), true)]
#[case(StoreErrorKind::unsupported(), false)]
#[case(StoreErrorKind::invalid_response(), false)]
#[tokio::test(start_paused = true)]
async fn store_errors_apply_only_the_requested_fallback(
    #[case] kind: StoreErrorKind,
    #[case] enabled: bool,
) {
    let policy = Config::builder()
        .scope("errors")
        .exact_target(1)
        .if_failed(StartupFailure::enable_with_fixed_probability(1.0))
        .build();
    let node = policy.prepare(Arc::new(FailingStore(kind))).await;
    assert_eq!(node.snapshot().recording(), enabled);
    assert!(!node.snapshot().coordinated());
}

#[tokio::test(start_paused = true)]
async fn renewal_retains_ownership_with_200ms_per_operation() {
    let store = Arc::new(MemoryStore::new());
    let policy = Config::builder()
        .scope("renewal-latency")
        .exact_target(1)
        .lease_loss(LeaseLoss::stop_profiling())
        .if_failed(StartupFailure::disabled())
        .build();
    let node = simulation::prepare(&policy, store.clone()).await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(node.snapshot().coordinated());
    store.set_latency(Duration::from_millis(200));
    tokio::time::sleep(Duration::from_secs(306)).await;
    assert!(node.snapshot().recording());
    assert!(node.snapshot().coordinated());
}

#[derive(Debug)]
struct CountSlotReads {
    inner: Arc<MemoryStore>,
    reads: AtomicUsize,
}
impl SamplingStore for CountSlotReads {
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        if key.contains("/slots/") {
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.get(key)
    }
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
        self.inner.put(request)
    }
    fn list(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        self.inner.list(request)
    }
    fn delete(&self, key: String) -> StoreFuture<'_, ()> {
        self.inner.delete(key)
    }
}
#[tokio::test(start_paused = true)]
async fn full_pool_shutdown_does_not_scan_unowned_slots() {
    let store = Arc::new(CountSlotReads {
        inner: Arc::new(MemoryStore::new()),
        reads: AtomicUsize::new(0),
    });
    let policy = Config::builder()
        .scope("full-cleanup")
        .exact_target(1)
        .if_failed(StartupFailure::disabled())
        .build();
    let owner = policy.prepare(store.clone()).await;
    let denied = policy.prepare(store.clone()).await;
    assert!(matches!(
        denied.snapshot().reason(),
        DecisionReason::Full { .. }
    ));
    tokio::time::sleep(Duration::from_millis(1)).await;
    let before = store.reads.load(Ordering::SeqCst);
    denied
        .into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
    assert_eq!(before, store.reads.load(Ordering::SeqCst));
    assert!(owner.snapshot().coordinated());
    owner
        .into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
}

#[rstest::rstest]
#[case(0)]
#[case(1)]
#[case(2)]
#[case(1000)]
#[case(usize::MAX)]
#[tokio::test(start_paused = true)]
async fn memory_list_paginates_only_the_requested_prefix(#[case] page_size: usize) {
    let store = MemoryStore::new();
    for key in [
        "before/a", "pool/a", "pool/b", "pool/c", "pool/d", "z/after",
    ] {
        store
            .put(PutRequest::builder().key(key).body(Bytes::new()).build())
            .await
            .unwrap();
    }
    let mut continuation = Some(ContinuationToken::new("before/a"));
    let mut keys = Vec::new();
    loop {
        let page = store
            .list(
                ListRequest::builder()
                    .prefix("pool/")
                    .page_size(page_size)
                    .maybe_continuation(continuation)
                    .build(),
            )
            .await
            .unwrap();
        keys.extend(page.objects().iter().map(|o| o.key().to_string()));
        continuation = page.continuation().cloned();
        if continuation.is_none() {
            break;
        }
    }
    assert_eq!(keys, ["pool/a", "pool/b", "pool/c", "pool/d"]);
}

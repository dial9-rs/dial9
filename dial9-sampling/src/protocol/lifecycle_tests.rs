use super::{engine, simulation::MemoryStore, *};
use bytes::Bytes;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

const DAY: Duration = Duration::from_secs(86400);

async fn prepare(
    policy: &Config,
    store: Arc<dyn SamplingStore>,
    clock: Arc<MemoryStore>,
) -> PreparedSampling {
    engine::prepare_using(policy.clone(), store, DEFAULT_PREPARE_TIMEOUT, None, clock).await
}

// Model age-based expiration of the current object. A queued key is evaluated
// against its latest modification time when collection runs, rather than
// deleting a replacement solely because an earlier version was eligible.
async fn expire_queued(store: &MemoryStore, keys: Vec<String>, cutoff: SystemTime) {
    for key in keys {
        if store
            .get(key.clone())
            .await
            .unwrap()
            .is_some_and(|object| object.modified_at() <= cutoff)
        {
            store.delete(key).await.unwrap();
        }
    }
}

#[tokio::test(start_paused = true)]
async fn live_coordination_survives_trace_retention_and_abandoned_records_expire() {
    let store = Arc::new(MemoryStore::new());
    let live = Config::builder()
        .scope("live")
        .exact_target(1)
        .lease_duration(Duration::from_secs(6 * 3600))
        .renewal_interval(Duration::from_secs(3 * 3600))
        .max_clock_skew(Duration::ZERO)
        .lease_loss(LeaseLoss::stop_profiling())
        .if_failed(StartupFailure::disabled())
        .build();
    let old = Config::builder()
        .scope("retired")
        .exact_target(1)
        .if_failed(StartupFailure::disabled())
        .build();
    let node = prepare(&live, store.clone(), store.clone()).await;
    prepare(&old, store.clone(), store.clone())
        .await
        .into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
    store
        .put(
            PutRequest::builder()
                .key(format!(
                    "{}members/0000000000000000/crashed",
                    old.pool_prefix()
                ))
                .body(Bytes::from_static(b"{}"))
                .build(),
        )
        .await
        .unwrap();
    let origin = engine::Clock::now(store.as_ref());
    for day in 1..=28 {
        tokio::time::sleep(DAY).await;
        if day >= 14 {
            expire_queued(
                &store,
                store.objects().into_keys().collect(),
                origin + DAY * (day - 14),
            )
            .await;
        }
        assert!(node.snapshot().recording());
        assert!(node.snapshot().coordinated());
        let policy = store
            .get(format!("{}policy.json", live.pool_prefix()))
            .await
            .unwrap()
            .unwrap();
        assert!(policy.modified_at() >= origin + DAY * day - Duration::from_secs(3 * 3600));
    }
    assert!(
        store
            .objects()
            .keys()
            .all(|key| !key.starts_with(&old.pool_prefix()))
    );
    node.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
}

#[tokio::test(start_paused = true)]
async fn admission_refreshes_aged_policy_before_delayed_expiration() {
    let store = Arc::new(MemoryStore::new());
    let policy = Config::builder()
        .scope("restarted")
        .exact_target(1)
        .if_failed(StartupFailure::disabled())
        .build();
    prepare(&policy, store.clone(), store.clone())
        .await
        .into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
    let key = format!("{}policy.json", policy.pool_prefix());
    let old = store.get(key.clone()).await.unwrap().unwrap();
    let queued = store.objects().into_keys().collect();
    tokio::time::sleep(DAY * 15).await;
    let node = prepare(&policy, store.clone(), store.clone()).await;
    let refreshed = store.get(key).await.unwrap().unwrap();
    // Body-based ETags remain identical despite a successful refresh. Age,
    // rather than the token alone, determines lifecycle eligibility.
    assert_eq!(old.version().as_str(), refreshed.version().as_str());
    assert!(refreshed.modified_at() > old.modified_at());
    expire_queued(&store, queued, old.modified_at() + DAY).await;
    assert!(node.snapshot().recording());
    assert!(node.snapshot().coordinated());
    assert_eq!(
        store
            .objects()
            .keys()
            .filter(|key| key.contains("/slots/"))
            .count(),
        1
    );
    node.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
}

#[derive(Debug)]
struct PolicyFailure {
    inner: Arc<MemoryStore>,
    fail: AtomicBool,
    conflict: bool,
}
impl SamplingStore for PolicyFailure {
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        self.inner.get(key)
    }
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
        if self.fail.load(Ordering::SeqCst) && request.key().ends_with("policy.json") {
            Box::pin(async {
                if self.conflict {
                    Ok(PutOutcome::retryable_conflict())
                } else {
                    Err(StoreError::new(
                        StoreErrorKind::unavailable(),
                        "policy refresh failed",
                    ))
                }
            })
        } else {
            self.inner.put(request)
        }
    }
    fn list(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        self.inner.list(request)
    }
    fn delete(&self, key: String) -> StoreFuture<'_, ()> {
        self.inner.delete(key)
    }
}

#[rstest::rstest]
#[case(false, false)]
#[case(false, true)]
#[case(true, false)]
#[case(true, true)]
#[tokio::test(start_paused = true)]
async fn policy_failure_does_not_extend_a_lease_or_restart_profiling(
    #[case] stop: bool,
    #[case] conflict: bool,
) {
    let clock = Arc::new(MemoryStore::new());
    let store = Arc::new(PolicyFailure {
        inner: clock.clone(),
        fail: AtomicBool::new(false),
        conflict,
    });
    let policy = Config::builder()
        .scope("failure")
        .exact_target(1)
        .lease_duration(Duration::from_secs(10))
        .renewal_interval(Duration::from_secs(1))
        .max_clock_skew(Duration::ZERO)
        .lease_loss(if stop {
            LeaseLoss::stop_profiling()
        } else {
            LeaseLoss::keep_profiling()
        })
        .if_failed(StartupFailure::disabled())
        .build();
    let node = prepare(&policy, store.clone(), clock.clone()).await;
    assert!(node.snapshot().recording());
    store.fail.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert!(!node.snapshot().coordinated());
    assert_eq!(node.snapshot().recording(), !stop);
    store.fail.store(false, Ordering::SeqCst);
    let replacement = prepare(&policy, store, clock).await;
    assert!(replacement.snapshot().recording());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(node.snapshot().recording(), !stop);
    assert!(!node.snapshot().coordinated());
    node.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
    assert!(replacement.snapshot().coordinated());
    replacement
        .into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
}

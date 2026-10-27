#![cfg(not(shuttle))]
use crate::protocol::{
    simulation::{self, MemoryStore},
    *,
};
use bytes::Bytes;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Debug)]
struct PanicOnRenewal {
    inner: Arc<MemoryStore>,
    armed: AtomicBool,
}
impl SamplingStore for PanicOnRenewal {
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        Box::pin(async move {
            assert!(
                !(key.contains("/slots/") && self.armed.load(Ordering::SeqCst)),
                "injected coordinator panic"
            );
            self.inner.get(key).await
        })
    }
    fn put(&self, r: PutRequest) -> StoreFuture<'_, PutOutcome> {
        self.inner.put(r)
    }
    fn list(&self, r: ListRequest) -> StoreFuture<'_, ListPage> {
        self.inner.list(r)
    }
    fn delete(&self, k: String) -> StoreFuture<'_, ()> {
        self.inner.delete(k)
    }
}
#[tokio::test(start_paused = true)]
async fn strict_coordinator_panic_disables_recording() {
    let inner = Arc::new(MemoryStore::new());
    let store = Arc::new(PanicOnRenewal {
        inner: inner.clone(),
        armed: false.into(),
    });
    let policy = Config::builder()
        .scope("panic")
        .exact_target(1)
        .lease_loss(LeaseLoss::stop_profiling())
        .if_failed(StartupFailure::disabled())
        .build();
    let original = policy.prepare(store.clone()).await;
    let changes = original.into_session().unwrap();
    let watch = changes.recording_changes();
    tokio::time::sleep(Duration::from_millis(1)).await;
    store.armed.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(306)).await;
    assert!(!changes.snapshot().recording());
    assert!(!*watch.borrow());
    assert!(!changes.snapshot().coordinated());

    let replacement = simulation::prepare(&policy, inner).await;
    assert!(replacement.snapshot().recording());
}

#[tokio::test(start_paused = true)]
async fn failed_membership_refresh_invalidates_diagnostics() {
    let store = Arc::new(MemoryStore::new());
    let policy = Config::builder()
        .scope("stale-estimate")
        .exact_target(1)
        .membership_sample_size(8)
        .if_failed(StartupFailure::disabled())
        .build();
    let fleet = simulation::prepare_fleet(policy, store.clone(), 100).await;
    tokio::time::sleep(Duration::from_secs(183)).await;
    let node = fleet
        .iter()
        .find(|n| {
            n.snapshot().membership_registered() && n.snapshot().estimated_population().is_some()
        })
        .unwrap();
    assert!(node.snapshot().population_updated_at().is_some());
    store.set_unavailable(true);
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert!(node.snapshot().estimated_population().is_none());
    assert!(node.snapshot().population_updated_at().is_none());
    assert!(!node.snapshot().membership_registered());
}

#[derive(Debug)]
struct ReleaseRace {
    inner: Arc<MemoryStore>,
    release: AtomicBool,
}
impl SamplingStore for ReleaseRace {
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        Box::pin(async move {
            let Some(object) = self.inner.get(key).await? else {
                return Ok(None);
            };
            if object.body().as_ref() == br#"{"owner":"old","nonce":"seed"}"# {
                return Ok(Some(
                    StoredObject::builder()
                        .body(object.body().clone())
                        .version(object.version().clone())
                        .modified_at(std::time::UNIX_EPOCH)
                        .build(),
                ));
            }
            Ok(Some(object))
        })
    }
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
        Box::pin(async move {
            if request.key().contains("/slots/") && self.release.swap(false, Ordering::SeqCst) {
                let old = self.inner.get(request.key().into()).await?.unwrap();
                self.inner
                    .put(
                        PutRequest::builder()
                            .key(request.key())
                            .body(Bytes::from_static(br#"{"owner":null,"nonce":"released"}"#))
                            .condition(WriteCondition::matching(old.version().clone()))
                            .build(),
                    )
                    .await?;
            }
            self.inner.put(request).await
        })
    }
    fn list(&self, r: ListRequest) -> StoreFuture<'_, ListPage> {
        self.inner.list(r)
    }
    fn delete(&self, k: String) -> StoreFuture<'_, ()> {
        self.inner.delete(k)
    }
}

#[tokio::test(start_paused = true)]
async fn claim_racing_release_retries_vacant_slot() {
    let inner = Arc::new(MemoryStore::new());
    let policy = Config::builder()
        .scope("release-race")
        .exact_target(1)
        .if_failed(StartupFailure::disabled())
        .build();
    policy
        .prepare(inner.clone())
        .await
        .into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
    let key = format!("{}slots/00000000.json", policy.pool_prefix());
    inner
        .put(
            PutRequest::builder()
                .key(&key)
                .body(Bytes::from_static(br#"{"owner":"old","nonce":"seed"}"#))
                .condition(WriteCondition::unconditional())
                .build(),
        )
        .await
        .unwrap();
    let store = Arc::new(ReleaseRace {
        inner: inner.clone(),
        release: true.into(),
    });
    let node = policy.prepare(store).await;
    assert!(matches!(
        node.snapshot().reason(),
        DecisionReason::Admitted { .. }
    ));
    let slot: serde_json::Value =
        serde_json::from_slice(inner.get(key).await.unwrap().unwrap().body()).unwrap();
    assert!(slot["owner"].is_string());
}

#[tokio::test(start_paused = true)]
async fn admitted_high_slot_is_released_directly() {
    let store = Arc::new(MemoryStore::new());
    let policy = Config::builder()
        .scope("cleanup-latency-review")
        .exact_target(15)
        .if_failed(StartupFailure::disabled())
        .build();
    // Initialize policy, then release the initial owner with zero store latency.
    let seed = simulation::prepare(&policy, store.clone()).await;
    seed.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(5))
        .await;
    let prefix = policy.pool_prefix();
    for slot in 0..14 {
        store
            .put(
                PutRequest::builder()
                    .key(format!("{prefix}slots/{slot:08}.json"))
                    .condition(WriteCondition::unconditional())
                    .body(Bytes::from(
                        serde_json::to_vec(&serde_json::json!({
                            "owner": format!("busy-{slot}"), "nonce": format!("seed-{slot}")
                        }))
                        .unwrap(),
                    ))
                    .build(),
            )
            .await
            .unwrap();
    }
    // The only available allocation is slot 14.
    let node = simulation::prepare(&policy, store.clone()).await;
    assert!(matches!(
        node.snapshot().reason(),
        DecisionReason::Admitted { .. }
    ));
    tokio::time::sleep(Duration::from_millis(1)).await;
    let key = format!("{prefix}slots/00000014.json");
    let before: serde_json::Value = serde_json::from_slice(store.objects()[&key].body()).unwrap();
    assert!(before["owner"].is_string());
    store.set_latency(Duration::from_millis(50));
    let started = tokio::time::Instant::now();
    node.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(5))
        .await;
    let elapsed = started.elapsed();
    store.set_latency(Duration::ZERO);
    let after: serde_json::Value = serde_json::from_slice(store.objects()[&key].body()).unwrap();
    assert!(after["owner"].is_null());
    assert!(
        elapsed <= Duration::from_millis(250),
        "direct cleanup should need no slot scan: {elapsed:?}"
    );
    let replacement = simulation::prepare(&policy, store.clone()).await;
    assert!(matches!(
        replacement.snapshot().reason(),
        DecisionReason::Admitted { .. }
    ));
}

#[rstest::rstest]
#[case(0)]
#[case(100)]
#[tokio::test(start_paused = true)]
async fn expired_prefix_scan_makes_progress(#[case] latency_ms: u64) {
    let store = Arc::new(MemoryStore::new());
    let policy = Config::builder()
        .scope("stale")
        .exact_target(1)
        .membership_sample_size(8)
        .if_failed(StartupFailure::disabled())
        .build();
    let prefix = format!("{}members/", policy.pool_prefix());
    for rank in 0..80 {
        store
            .put(
                PutRequest::builder()
                    .key(format!("{prefix}{rank:016x}/dead-{rank}"))
                    .body(Bytes::from_static(b"dead"))
                    .build(),
            )
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_secs(306)).await;
    let fleet = simulation::prepare_fleet(policy, store.clone(), 20).await;
    store.set_latency(Duration::from_millis(latency_ms));
    tokio::time::sleep(Duration::from_millis(1)).await;
    let mut estimated = vec![false; fleet.len()];
    let mut registered = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(61)).await;
        for (seen, node) in estimated.iter_mut().zip(&fleet) {
            *seen |= node.snapshot().estimated_population().is_some();
            registered |= node.snapshot().membership_registered();
        }
    }
    assert!(registered);
    assert!(estimated.iter().all(|seen| *seen));
    assert!(
        store
            .objects()
            .keys()
            .filter(|k| k.contains("/members/"))
            .count()
            > 80
    );
}

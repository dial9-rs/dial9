#![cfg(not(shuttle))]
use crate::protocol::{simulation::MemoryStore, *};
use std::{sync::Arc, time::Duration};
#[derive(Debug)]
struct CapacityLimited {
    inner: Arc<MemoryStore>,
    permits: tokio::sync::Semaphore,
}
impl CapacityLimited {
    async fn enter(&self) -> tokio::sync::SemaphorePermit<'_> {
        let permit = self.permits.acquire().await.unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
        permit
    }
}
impl SamplingStore for CapacityLimited {
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        Box::pin(async move {
            let _permit = self.enter().await;
            self.inner.get(key).await
        })
    }
    fn put(&self, r: PutRequest) -> StoreFuture<'_, PutOutcome> {
        Box::pin(async move {
            let _permit = self.enter().await;
            self.inner.put(r).await
        })
    }
    fn list(&self, r: ListRequest) -> StoreFuture<'_, ListPage> {
        Box::pin(async move {
            let _permit = self.enter().await;
            self.inner.list(r).await
        })
    }
    fn delete(&self, k: String) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            let _permit = self.enter().await;
            self.inner.delete(k).await
        })
    }
}
#[tokio::test(start_paused = true)]
#[ignore = "100k bounded-capacity qualification; run explicitly"]
async fn cold_100k_fleet_can_all_time_out_on_a_capacity_limited_store() {
    let store = Arc::new(CapacityLimited {
        inner: Arc::new(MemoryStore::new()),
        permits: tokio::sync::Semaphore::new(8),
    });
    let policy = Config::builder()
        .scope("capacity")
        .exact_target(1)
        .if_failed(StartupFailure::disabled())
        .build();
    let mut tasks = Vec::new();
    for _ in 0..100_000 {
        let store = store.clone();
        let policy = policy.clone();
        tasks.push(tokio::spawn(async move {
            let start = tokio::time::Instant::now();
            let node = policy.prepare(store).await;
            (start.elapsed(), node)
        }));
    }
    let mut nodes = Vec::new();
    for t in tasks {
        nodes.push(t.await.unwrap());
    }
    assert!(nodes.iter().all(|(_, n)| !n.snapshot().recording()));
    assert!(nodes.iter().all(|(t, _)| *t <= DEFAULT_PREPARE_TIMEOUT));
    let timeouts = nodes
        .iter()
        .filter(|(_, n)| matches!(n.snapshot().reason(), DecisionReason::Timeout { .. }))
        .count();
    assert_eq!(timeouts, 100_000);
    println!(
        "FIFO storage model, 8 permits/1ms: 100k cold startups all decide off within 500ms; this is an overload counterexample, not an S3 throughput prediction"
    );
}

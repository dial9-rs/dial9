//! In-memory backend with body-based ETags, latency and failure injection.
//! Uses the production engine, including its conditional write protocol.
use super::*;
use sha2::{Digest, Sha256};
#[cfg(all(test, shuttle))]
use shuttle::sync::Mutex;
#[cfg(all(test, not(shuttle)))]
use std::sync::Arc;
#[cfg(not(all(test, shuttle)))]
use std::sync::Mutex;
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

#[derive(Debug, Default)]
/// Atomic in-memory storage with injectable latency, partitions and lost acknowledgements.
pub struct MemoryStore {
    objects: Mutex<BTreeMap<String, StoredObject>>,
    unavailable: AtomicBool,
    latency_ms: AtomicU64,
    lost_writes: AtomicU64,
    operations: AtomicU64,
    clock_origin: Mutex<Option<(SystemTime, tokio::time::Instant)>>,
}
impl MemoryStore {
    fn now(&self) -> SystemTime {
        let mut origin = self.clock_origin.lock().expect("clock lock");
        let (wall, instant) =
            origin.get_or_insert_with(|| (SystemTime::now(), tokio::time::Instant::now()));
        *wall + instant.elapsed()
    }
    /// Constructs empty, available storage with zero operation latency.
    pub fn new() -> Self {
        Self::default()
    }
    /// Makes all subsequent operations fail while enabled.
    #[cfg(any(not(shuttle), feature = "simulation"))]
    pub fn set_unavailable(&self, value: bool) {
        self.unavailable.store(value, Ordering::SeqCst);
    }
    /// Sets latency per operation. Tokio virtual time can control it on a paused executor.
    #[cfg(any(not(shuttle), feature = "simulation"))]
    pub fn set_latency(&self, value: Duration) {
        self.latency_ms
            .store(value.as_millis() as u64, Ordering::SeqCst);
    }
    /// Commit this many upcoming writes, but return an unknown-outcome error.
    pub fn lose_write_acknowledgements(&self, count: u64) {
        self.lost_writes.store(count, Ordering::SeqCst);
    }
    /// Returns the number of attempted storage operations.
    #[cfg(any(not(shuttle), feature = "simulation"))]
    pub fn operation_count(&self) -> u64 {
        self.operations.load(Ordering::Relaxed)
    }
    /// Clones the current objects and their versions for simulation assertions.
    pub fn objects(&self) -> BTreeMap<String, StoredObject> {
        self.objects.lock().expect("store lock").clone()
    }
    async fn before(&self) -> Result<(), StoreError> {
        self.operations.fetch_add(1, Ordering::Relaxed);
        let latency = self.latency_ms.load(Ordering::SeqCst);
        if latency > 0 {
            tokio::time::sleep(Duration::from_millis(latency)).await;
        }
        if self.unavailable.load(Ordering::SeqCst) {
            Err(StoreError::new(
                StoreErrorKind::Unavailable {},
                "simulated partition",
            ))
        } else {
            Ok(())
        }
    }
}
impl SamplingStore for MemoryStore {
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        Box::pin(async move {
            self.before().await?;
            Ok(self.objects.lock().expect("store lock").get(&key).cloned())
        })
    }
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
        Box::pin(async move {
            self.before().await?;
            let mut objects = self.objects.lock().expect("store lock");
            let existing = objects.get(request.key());
            let allowed = match request.condition() {
                WriteCondition::Unconditional {} => true,
                WriteCondition::Absent {} => existing.is_none(),
                WriteCondition::Matches { version } => {
                    existing.is_some_and(|object| object.version() == version)
                }
            };
            if !allowed {
                return Ok(PutOutcome::precondition_failed());
            }
            let version = VersionToken::new(format!("{:x}", Sha256::digest(request.body())));
            objects.insert(
                request.key().into(),
                StoredObject::builder()
                    .body(request.body().clone())
                    .version(version.clone())
                    .modified_at(self.now())
                    .build(),
            );
            if self
                .lost_writes
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(StoreError::new(
                    StoreErrorKind::WriteOutcomeUnknown {},
                    "committed with lost acknowledgement",
                ));
            }
            Ok(PutOutcome::stored(version))
        })
    }
    fn list(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        Box::pin(async move {
            self.before().await?;
            let objects = self.objects.lock().expect("store lock");
            use std::ops::Bound::{Excluded, Included, Unbounded};
            let lower = match request.continuation() {
                Some(token) if token.as_str() >= request.prefix() => Excluded(token.as_str()),
                _ => Included(request.prefix()),
            };
            let size = request.page_size().max(1);
            let mut page: Vec<_> = objects
                .range::<str, _>((lower, Unbounded))
                .take_while(|(key, _)| key.starts_with(request.prefix()))
                .take(size.saturating_add(1))
                .map(|(key, object)| {
                    ListedObject::builder()
                        .key(key.as_str())
                        .modified_at(object.modified_at())
                        .build()
                })
                .collect();
            let continuation = if page.len() > size {
                page.truncate(size);
                page.last().map(|last| ContinuationToken::new(last.key()))
            } else {
                None
            };
            Ok(ListPage::builder()
                .objects(page)
                .maybe_continuation(continuation)
                .build())
        })
    }
    fn delete(&self, key: String) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            self.before().await?;
            self.objects.lock().expect("store lock").remove(&key);
            Ok(())
        })
    }
}
impl crate::protocol::engine::Clock for MemoryStore {
    fn now(&self) -> SystemTime {
        self.now()
    }
}

/// Prepares a startup choice with the default 500 ms budget; maintenance requires the caller executor to remain alive.
#[cfg(all(test, not(shuttle)))]
pub async fn prepare(policy: &Config, store: Arc<MemoryStore>) -> PreparedSampling {
    crate::protocol::engine::prepare_using(
        policy.clone(),
        store.clone(),
        DEFAULT_PREPARE_TIMEOUT,
        None,
        store,
    )
    .await
}

/// Establishes a fleet using the exact same preparation API as production.
#[cfg(all(test, not(shuttle)))]
pub async fn prepare_fleet(
    policy: Config,
    store: Arc<MemoryStore>,
    count: usize,
) -> Vec<PreparedSampling> {
    let mut tasks = Vec::with_capacity(count);
    for _ in 0..count {
        let policy = policy.clone();
        let store = store.clone();
        tasks.push(tokio::spawn(async move { prepare(&policy, store).await }));
    }
    let mut fleet = Vec::with_capacity(count);
    for task in tasks {
        fleet.push(task.await.expect("simulation task panicked"));
    }
    fleet
}

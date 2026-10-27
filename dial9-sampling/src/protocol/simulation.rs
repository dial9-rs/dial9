//! Fleet simulations using the production conditional-write engine.
#[cfg(not(shuttle))]
use super::{Config, DEFAULT_PREPARE_TIMEOUT, PreparedSampling};
pub(crate) use crate::test_store::MemoryStore;
#[cfg(not(shuttle))]
use std::sync::Arc;
use std::time::SystemTime;

impl crate::protocol::engine::Clock for MemoryStore {
    fn now(&self) -> SystemTime {
        self.now()
    }
}

/// Prepares a startup choice with the default 500 ms budget; maintenance requires the caller executor to remain alive.
#[cfg(all(test, not(shuttle)))]
pub(crate) async fn prepare(policy: &Config, store: Arc<MemoryStore>) -> PreparedSampling {
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
pub(crate) async fn prepare_fleet(
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

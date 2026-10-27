//! Test clock and fleet driver exercise the production protocol.
pub(crate) use crate::test_store::MemoryStore;
use std::time::SystemTime;
impl super::engine::Clock for MemoryStore {
    fn now(&self) -> SystemTime {
        self.now()
    }
}
#[cfg(not(shuttle))]
pub(crate) async fn prepare(
    policy: &super::Config,
    store: std::sync::Arc<MemoryStore>,
) -> super::PreparedSampling {
    super::engine::prepare_using(
        policy.clone(),
        store.clone(),
        super::DEFAULT_PREPARE_TIMEOUT,
        None,
        store,
    )
    .await
}

use super::*;
use crate::test_store::MemoryStore;

#[rstest::rstest]
#[case(None, 2000, 0, StartupFailure::disabled(), false)]
#[case(Some(500), 5000, 0, StartupFailure::disabled(), false)]
#[case(Some(500), 5000, 0, StartupFailure::assume_population(1), true)]
#[case(Some(500), 400, 150, StartupFailure::disabled(), false)]
#[tokio::test(start_paused = true)]
async fn startup_budget_covers_initialization_and_get_without_revisiting_fallback(
    #[case] timeout_ms: Option<u64>,
    #[case] initialization_ms: u64,
    #[case] storage_ms: u64,
    #[case] fallback: StartupFailure,
    #[case] expected: bool,
) {
    let store = Arc::new(MemoryStore::new());
    store.set_latency(Duration::from_millis(storage_ms));
    let sampling = Sampling::builder()
        .add_dimension("test", "timeout")
        .num_target_profilers(1)
        .startup_timeout(Duration::from_millis(timeout_ms.unwrap_or(1000)))
        .if_failed(fallback)
        .build();
    let before = tokio::time::Instant::now();
    // Exercise the shared admission deadline on a paused executor; the public
    // synchronous entry point owns a multi-thread runtime that cannot be paused.
    let prepared = Sampling::admit(
        &sampling.policy(),
        async {
            tokio::time::sleep(Duration::from_millis(initialization_ms)).await;
            Ok(store.clone())
        },
        sampling.startup_timeout,
    )
    .await;
    assert_eq!(
        before.elapsed(),
        Duration::from_millis(timeout_ms.unwrap_or(1000))
    );
    assert_eq!(prepared.enabled, expected);
    assert_eq!(prepared.reason, protocol::DecisionReason::Timeout);
    if let Some(session) = prepared.into_session() {
        session.shutdown(Duration::from_secs(1)).await;
    }
}

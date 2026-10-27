use super::*;
use crate::protocol::simulation::MemoryStore;
use std::{io, sync::Arc, time::Duration};

fn policy() -> Sampling {
    Sampling::builder()
        .scope("manual/prod")
        .add_dimension("az", "a")
        .exact_target(1)
        .if_failed(StartupFailure::disabled())
        .build()
}

#[tokio::test(start_paused = true)]
async fn guard_owns_capacity_until_explicit_shutdown() {
    let store = Arc::new(MemoryStore::new());
    let first = policy()
        .prepare_on_executor(async { Ok(store.clone()) })
        .await;
    assert!(first.enabled());
    let full = policy()
        .prepare_on_executor(async { Ok(store.clone()) })
        .await;
    assert!(!full.enabled());
    full.stopped().await;
    first.shutdown(Duration::from_secs(1)).await;
    let replacement = policy().prepare_on_executor(async { Ok(store) }).await;
    assert!(replacement.enabled());
    replacement.shutdown(Duration::from_secs(1)).await;
}

#[rstest::rstest]
#[case(0.0, false)]
#[case(1.0, true)]
#[tokio::test(start_paused = true)]
async fn deadline_includes_backend_initialization_and_never_revisits_fallback(
    #[case] probability: f64,
    #[case] expected: bool,
) {
    let policy = Sampling::builder()
        .scope("timeout")
        .exact_target(1)
        .if_failed(StartupFailure::enable_with_fixed_probability(probability))
        .build();
    let before = tokio::time::Instant::now();
    let session = policy
        .prepare_on_executor(async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(MemoryStore::new())
        })
        .await;
    assert_eq!(before.elapsed(), Duration::from_millis(500));
    assert_eq!(session.enabled(), expected);
    tokio::time::advance(Duration::from_secs(600)).await;
    assert_eq!(session.enabled(), expected);
    if expected {
        assert!(
            tokio::time::timeout(Duration::from_secs(1), session.stopped())
                .await
                .is_err()
        );
    }
    session.shutdown(Duration::ZERO).await;
}

#[rstest::rstest]
#[case(0.0, false)]
#[case(1.0, true)]
#[tokio::test(start_paused = true)]
async fn backend_construction_error_uses_explicit_failure_choice(
    #[case] probability: f64,
    #[case] expected: bool,
) {
    let session = Sampling::builder()
        .scope("constructor-failure")
        .exact_target(1)
        .if_failed(StartupFailure::enable_with_fixed_probability(probability))
        .build()
        .prepare_on_executor(async {
            Err::<MemoryStore, _>(io::Error::other("credentials unavailable"))
        })
        .await;
    assert_eq!(session.enabled(), expected);
    session.shutdown(Duration::ZERO).await;
}

#[rstest::rstest]
#[case("valid", 0, Duration::from_millis(500))]
#[case("", 1, Duration::from_millis(500))]
#[case("valid", 1, Duration::MAX)]
#[tokio::test(start_paused = true)]
async fn disabled_or_invalid_configuration_never_polls_backend(
    #[case] scope: &str,
    #[case] target: u32,
    #[case] timeout: Duration,
) {
    let session = Sampling::builder()
        .scope(scope)
        .exact_target(target)
        .startup_timeout(timeout)
        .if_failed(StartupFailure::enable_with_fixed_probability(1.0))
        .build()
        .prepare_on_executor(async {
            panic!("backend must not be polled");
            #[allow(unreachable_code)]
            Ok(MemoryStore::new())
        })
        .await;
    assert!(!session.enabled());
    session.shutdown(Duration::ZERO).await;
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test(start_paused = true)]
async fn lease_loss_stop_is_owned_and_opt_in(#[case] strict: bool) {
    let store = Arc::new(MemoryStore::new());
    let session = Sampling::builder()
        .scope("outage")
        .exact_target(1)
        .if_failed(StartupFailure::disabled())
        .stop_on_lease_loss(strict)
        .build()
        .prepare_on_executor(async { Ok(store.clone()) })
        .await;
    assert!(session.enabled());
    let stopped = tokio::spawn(session.stopped());
    store.set_unavailable(true);
    tokio::time::advance(Duration::from_secs(306)).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert_eq!(stopped.is_finished(), strict);
    store.set_unavailable(false);
    tokio::time::advance(Duration::from_secs(600)).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(session.enabled(), "the startup choice is immutable");
    if strict {
        stopped.await.unwrap();
    } else {
        stopped.abort();
    }
    session.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn backend_initialization_and_storage_share_one_deadline() {
    let store = Arc::new(MemoryStore::new());
    store.set_latency(Duration::from_millis(150));
    let before = tokio::time::Instant::now();
    let session = policy()
        .prepare_on_executor(async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            Ok(store.clone())
        })
        .await;
    assert_eq!(before.elapsed(), Duration::from_millis(500));
    assert!(!session.enabled());
    store.set_latency(Duration::ZERO);
    session.shutdown(Duration::from_secs(1)).await;
    let next = policy().prepare_on_executor(async { Ok(store) }).await;
    assert!(next.enabled());
    next.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn storage_failure_fallback_remains_on_in_optional_stop_mode() {
    let store = Arc::new(MemoryStore::new());
    store.set_unavailable(true);
    let session = Sampling::builder()
        .scope("fallback-stop")
        .exact_target(1)
        .stop_on_lease_loss(true)
        .if_failed(StartupFailure::enable_with_fixed_probability(1.0))
        .build()
        .prepare_on_executor(async { Ok(store.clone()) })
        .await;
    assert!(session.enabled());
    store.set_unavailable(false);
    assert!(
        tokio::time::timeout(Duration::from_secs(600), session.stopped())
            .await
            .is_err()
    );
    assert!(session.enabled());
    session.shutdown(Duration::from_secs(1)).await;
}

#[rstest::rstest]
#[case(0.0, false)]
#[case(1.0, true)]
#[tokio::test(start_paused = true)]
async fn runtime_creation_failure_uses_required_failure_policy(
    #[case] probability: f64,
    #[case] expected: bool,
) {
    let sampler = Sampling::builder()
        .scope("runtime-error")
        .exact_target(1)
        .if_failed(StartupFailure::enable_with_fixed_probability(probability))
        .build()
        .prepare_with_runtime(
            async {
                panic!("factory must not run if runtime creation fails");
                #[allow(unreachable_code)]
                Ok(MemoryStore::new())
            },
            || Err(io::Error::other("thread creation failed")),
        )
        .await;
    assert_eq!(sampler.enabled(), expected);
    sampler.shutdown(Duration::ZERO).await;
}

#[tokio::test(start_paused = true)]
async fn exhausted_startup_budget_never_polls_backend() {
    let sampler = Sampling::builder()
        .scope("runtime-budget")
        .exact_target(1)
        .startup_timeout(Duration::ZERO)
        .if_failed(StartupFailure::disabled())
        .build()
        .prepare(async {
            panic!("an exhausted budget must not poll the factory");
            #[allow(unreachable_code)]
            Ok(MemoryStore::new())
        })
        .await;
    assert!(!sampler.enabled());
    sampler.shutdown(Duration::ZERO).await;
}

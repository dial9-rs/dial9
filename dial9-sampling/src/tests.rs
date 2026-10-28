use super::*;

struct TestSession {
    status: SamplingStatus,
    maintenance: Option<protocol::SamplingSession>,
    #[cfg(feature = "dial9")]
    registration: Arc<registration::RecorderRegistrations>,
}

impl TestSession {
    fn enabled(&self) -> bool {
        self.status.enabled
    }

    fn stopped(&self) -> impl Future<Output = ()> + Send + 'static {
        self.status.stopped()
    }

    async fn shutdown(self, timeout: Duration) {
        #[cfg(feature = "dial9")]
        self.registration.stop();
        if let Some(session) = self.maintenance {
            session.shutdown(timeout).await;
        }
    }
}

impl Sampling {
    // Paused-time tests drive admission on their test executor; public callers
    // exercise the owned runtime through start_application.
    async fn prepare_on_executor<S: SamplingStore>(
        &self,
        backend: impl Future<Output = io::Result<S>>,
    ) -> TestSession {
        let policy = self.policy();
        let prepared = match self.preflight(&policy) {
            Some(prepared) => prepared,
            None => Self::admit(&policy, backend, self.startup_timeout).await,
        };
        let enabled = prepared.enabled;
        let reason = prepared.reason.clone();
        let maintenance = prepared.into_session();
        #[allow(unused_mut)]
        let mut status = policy.status(enabled, &reason, maintenance.as_ref());
        #[cfg(feature = "dial9")]
        let registration = {
            let registration = Arc::new(registration::RecorderRegistrations::new(Some(
                tokio::runtime::Handle::current(),
            )));
            status.registration = Arc::downgrade(&registration);
            registration
        };
        TestSession {
            status,
            maintenance,
            #[cfg(feature = "dial9")]
            registration,
        }
    }
}
use crate::test_store::MemoryStore;
use std::{io, sync::Arc, time::Duration};

#[rstest::rstest]
#[case(Duration::from_millis(100))]
#[case(Duration::from_secs(3))]
#[tokio::test(start_paused = true)]
async fn cold_disabled_startup_never_enables_after_roster_settles(#[case] poll_interval: Duration) {
    let store = Arc::new(MemoryStore::new());
    let sampling = Sampling::builder()
        .scope("cold")
        .target(1)
        .poll_interval(poll_interval)
        .bucket_width(Duration::from_secs(30))
        .max_clock_skew(Duration::ZERO)
        .if_failed(StartupFailure::disabled())
        .build();
    let session = sampling
        .prepare_on_executor(async { Ok(store.clone()) })
        .await;
    assert!(!session.enabled());
    session.stopped().await;
    for _ in 0..400 {
        tokio::time::advance(Duration::from_millis(50)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }
    let census = sampling
        .population_at()
        .store(store.as_ref())
        // This helper exercises the public admission path's wall clock;
        // MemoryStore's modification times advance with paused Tokio time.
        .at(std::time::SystemTime::now())
        .max_age(Duration::from_secs(10))
        .call()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        census.no_support, 1,
        "cold disabled cohort cannot be reconstructed"
    );
    let future_startup = sampling
        .prepare_on_executor(async { Ok(store.clone()) })
        .await;
    assert!(
        future_startup.enabled(),
        "a settled small-pool roster enables future startups"
    );
    assert!(!session.enabled(), "an unarmed process never enables later");
    future_startup.shutdown(Duration::from_secs(1)).await;
    session.shutdown(Duration::from_secs(1)).await;
}

#[cfg(feature = "dial9")]
#[rstest::rstest]
#[case(StartupFailure::assume_population(1), true)]
#[case(StartupFailure::disabled(), false)]
fn fallback_recorder_controls_and_cleanup(#[case] failure: StartupFailure, #[case] enabled: bool) {
    let sampling = Sampling::builder()
        .scope("fallback-weight")
        .target(1)
        .startup_timeout(Duration::ZERO)
        .if_failed(failure)
        .build();
    let (status, recorder) = sampling
        .with_backend(async {
            panic!("zero budget must not poll the backend");
            #[allow(unreachable_code)]
            Ok(MemoryStore::new())
        })
        .start_application(|status| {
            assert_eq!(status.enabled, enabled);
            assert_eq!(status.initial_expansion_weight, enabled.then_some(1.0));
            assert!(status.changes.is_none());
            let recorder = dial9_core::recorder::recorder(
                dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap(),
            )
            .build();
            status.register_recorder(&recorder);
            assert_eq!(recorder.handle().is_enabled(), enabled);
            recorder.enable();
            assert!(recorder.handle().is_enabled());
            (status, recorder)
        });
    assert!(!recorder.handle().is_enabled());
    assert_eq!(
        status
            .weights
            .as_ref()
            .map(trace_weights::TraceWeights::current_expansion_weight),
        enabled.then_some(Some(0.0))
    );
    recorder.enable();
    assert!(
        recorder.handle().is_enabled(),
        "normal enable remains available"
    );
}

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
    let mut sampling = Sampling::builder()
        .scope("timeout")
        .target(1)
        .if_failed(fallback)
        .build();
    if let Some(timeout) = timeout_ms {
        sampling.startup_timeout = Duration::from_millis(timeout);
    }
    let before = tokio::time::Instant::now();
    let session = sampling
        .prepare_on_executor(async {
            tokio::time::sleep(Duration::from_millis(initialization_ms)).await;
            Ok(store.clone())
        })
        .await;
    assert_eq!(
        before.elapsed(),
        Duration::from_millis(timeout_ms.unwrap_or(1000))
    );
    assert_eq!(session.enabled(), expected);
    store.set_latency(Duration::ZERO);
    tokio::time::advance(Duration::from_secs(600)).await;
    assert_eq!(session.enabled(), expected);
    if expected {
        assert!(
            tokio::time::timeout(Duration::from_secs(1), session.stopped())
                .await
                .is_err()
        );
    }
    session.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn storage_outage_preserves_active_state() {
    let store = Arc::new(MemoryStore::new());
    let session = Sampling::builder()
        .scope("outage")
        .target(1)
        .if_failed(StartupFailure::assume_population(1))
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
    assert!(!stopped.is_finished());
    store.set_unavailable(false);
    tokio::time::advance(Duration::from_secs(600)).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(session.enabled(), "the startup choice is immutable");
    assert!(!stopped.is_finished());
    stopped.abort();
    session.shutdown(Duration::from_secs(1)).await;
}

#[cfg(feature = "dial9")]
#[tokio::test(start_paused = true)]
async fn registered_recorder_survives_outage_then_stops_at_cleanup_after_status_drop() {
    let store = Arc::new(MemoryStore::new());
    let TestSession {
        status,
        maintenance,
        registration,
    } = Sampling::builder()
        .scope("registered-outage")
        .target(1)
        .if_failed(StartupFailure::assume_population(1))
        .build()
        .prepare_on_executor(async { Ok(store.clone()) })
        .await;
    assert!(status.enabled);
    let recorder =
        dial9_core::recorder::recorder(dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap())
            .build();
    status.register_recorder(&recorder);
    drop(status);
    store.set_unavailable(true);
    tokio::time::advance(Duration::from_secs(306)).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(recorder.handle().is_enabled());
    store.set_unavailable(false);
    tokio::time::advance(Duration::from_secs(600)).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(recorder.handle().is_enabled());
    registration.stop();
    assert!(!recorder.handle().is_enabled());
    maintenance.unwrap().shutdown(Duration::from_secs(1)).await;
}

#[cfg(feature = "dial9")]
#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test(start_paused = true)]
async fn automatic_recorder_stop_survives_status_drop(#[case] maintenance_ended: bool) {
    let (recording, changes) = tokio::sync::watch::channel(true);
    let registration = Arc::new(registration::RecorderRegistrations::new(Some(
        tokio::runtime::Handle::current(),
    )));
    let status = SamplingStatus {
        enabled: true,
        initial_expansion_weight: Some(1.0),
        changes: Some(changes),
        registration: Arc::downgrade(&registration),
        weights: None,
    };
    let recorder =
        dial9_core::recorder::recorder(dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap())
            .build();
    status.register_recorder(&recorder);
    drop(status);
    assert!(recorder.handle().is_enabled());
    if maintenance_ended {
        drop(recording);
    } else {
        recording.send(false).unwrap();
    }
    registration.wait_for_stops().await;
    assert!(!recorder.handle().is_enabled());
    recorder.enable();
    assert!(
        recorder.handle().is_enabled(),
        "normal enable remains available"
    );
    registration.stop();
    assert!(
        !recorder.handle().is_enabled(),
        "cleanup disables recording again"
    );
}

#[tokio::test(start_paused = true)]
async fn storage_failure_fallback_participates_without_enabling_again() {
    let store = Arc::new(MemoryStore::new());
    store.set_unavailable(true);
    let session = Sampling::builder()
        .scope("fallback-stop")
        .target(1)
        .if_failed(StartupFailure::assume_population(1))
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
#[case(StartupFailure::disabled(), false)]
#[case(StartupFailure::assume_population(1), true)]
#[test]
fn runtime_creation_failure_uses_required_failure_policy(
    #[case] fallback: StartupFailure,
    #[case] expected: bool,
) {
    let enabled = Sampling::builder()
        .scope("runtime-error")
        .target(1)
        .if_failed(fallback)
        .build()
        .with_backend(async {
            panic!("factory must not run if runtime creation fails");
            #[allow(unreachable_code)]
            Ok(MemoryStore::new())
        })
        .start_application_with_runtime(
            |SamplingStatus { enabled, .. }| enabled,
            || Err(io::Error::other("thread creation failed")),
        );
    assert_eq!(enabled, expected);
}

#[test]
fn exhausted_startup_budget_never_polls_backend() {
    let enabled = Sampling::builder()
        .scope("runtime-budget")
        .target(1)
        .startup_timeout(Duration::ZERO)
        .if_failed(StartupFailure::disabled())
        .build()
        .with_backend(async {
            panic!("an exhausted budget must not poll the factory");
            #[allow(unreachable_code)]
            Ok(MemoryStore::new())
        })
        .start_application(|SamplingStatus { enabled, .. }| enabled);
    assert!(!enabled);
}

#[rstest::rstest]
#[case("valid", 0, 0, Duration::from_millis(1000))]
#[case("valid", 1, u64::MAX, Duration::from_millis(1000))]
#[case("", 1, 0, Duration::from_millis(1000))]
#[case("valid", 1, 0, Duration::MAX)]
#[test]
fn zero_untracked_or_invalid_startup_builds_no_runtime_and_polls_no_backend(
    #[case] scope: &str,
    #[case] target: u32,
    #[case] minimum_population: u64,
    #[case] timeout: Duration,
) {
    let sampling = Sampling::builder()
        .scope(scope)
        .host_id("untracked-process")
        .target(target)
        .minimum_population(minimum_population)
        .startup_timeout(timeout)
        .if_failed(StartupFailure::assume_population(1))
        .build();
    let enabled = sampling
        .with_backend(async {
            panic!("preflight must not initialize the backend");
            #[allow(unreachable_code)]
            Ok(MemoryStore::new())
        })
        .start_application_with_runtime(
            |SamplingStatus { enabled, .. }| enabled,
            || panic!("preflight must not construct the runtime"),
        );
    assert!(!enabled);
}

use super::*;
use crate::{
    StartupFailure,
    backend::{PutOutcome, PutRequest},
    test_store::MemoryStore,
};
use bytes::Bytes;

fn sampling() -> Sampling {
    Sampling::builder()
        .add_dimension("environment", "historical")
        .target(15)
        .if_failed(StartupFailure::disabled())
        .build()
}

fn observation(
    sampling: &Sampling,
    completed_ms: u64,
    tracked: u64,
    slice_bits: u32,
) -> CensusObservation {
    CensusObservation {
        schema: 2,
        pool: sampling.pool_hash(),
        started_ms: completed_ms - 1,
        completed_ms,
        bucket: completed_ms / 300_000,
        bucket_ms: 300_000,
        skew_ms: 0,
        timestamp_uncertainty_ms: 0,
        reporting_epoch: "bootstrap".into(),
        reporting_cutoff: (slice_bits > 0).then(|| 1u64 << (64 - slice_bits)),
        observed: tracked,
        tracked,
        admitted: tracked / 2,
        carriers: 0,
        unarmed: tracked - tracked / 2,
        no_support: 0,
    }
}

async fn put(store: &MemoryStore, observation: &CensusObservation) {
    let outcome = store
        .put(
            PutRequest::builder()
                .key(observation.key())
                .body(Bytes::from(serde_json::to_vec(observation).unwrap()))
                .build(),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, PutOutcome::Stored { .. }));
}

#[rstest::rstest]
#[case(0, 7, None)]
#[case(29, 7, None)]
#[case(32, 7, Some((32.0_f64 * (1.0 - 1.0/128.0)).sqrt() * 128.0))]
#[case(0, 0, Some(0.0))]
#[case(10, 0, Some(0.0))]
#[tokio::test(start_paused = true)]
async fn history_estimate_distinguishes_sparse_zero_and_full_census(
    #[case] count: u64,
    #[case] bits: u32,
    #[case] standard_error: Option<f64>,
) {
    let store = MemoryStore::new();
    let sampling = sampling();
    put(&store, &observation(&sampling, 1_000, count, bits)).await;
    let estimate = sampling
        .population_at()
        .store(&store)
        .at(UNIX_EPOCH + Duration::from_millis(1_050))
        .max_age(Duration::from_secs(1))
        .call()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        estimate.population,
        count as f64 * 2.0_f64.powi(bits as i32)
    );
    assert_eq!(estimate.gaussian_standard_error, standard_error);
    assert_eq!(estimate.age, Duration::from_millis(50));
    assert_eq!(estimate.admitted + estimate.unarmed, count);
}

#[tokio::test(start_paused = true)]
async fn historical_lookup_never_uses_future_or_stale_censuses_and_crosses_days() {
    let store = MemoryStore::new();
    let sampling = sampling();
    for (time, count) in [(DAY_MS - 50, 32), (DAY_MS + 10, 64), (DAY_MS + 500, 128)] {
        put(&store, &observation(&sampling, time, count, 1)).await;
    }
    let at = |ms| UNIX_EPOCH + Duration::from_millis(ms);
    let estimate = sampling
        .population_at()
        .store(&store)
        .at(at(DAY_MS + 5))
        .max_age(Duration::from_secs(1))
        .call()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(estimate.tracked, 32);
    let estimate = sampling
        .population_at()
        .store(&store)
        .at(at(DAY_MS + 100))
        .max_age(Duration::from_secs(1))
        .call()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(estimate.tracked, 64);
    assert!(
        sampling
            .population_at()
            .store(&store)
            .at(at(DAY_MS + 100))
            .max_age(Duration::from_millis(20))
            .call()
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        sampling
            .population_at()
            .store(&store)
            .at(at(1_000))
            .max_age(Duration::from_secs(1))
            .call()
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn malformed_history_and_storage_failure_are_errors_not_partial_estimates() {
    let store = MemoryStore::new();
    let sampling = sampling();
    put(&store, &observation(&sampling, 1_000, 32, 7)).await;
    let mut malformed = observation(&sampling, 1_100, 32, 7);
    malformed.admitted += 1;
    put(&store, &malformed).await;
    let query = || {
        sampling
            .population_at()
            .store(&store)
            .at(UNIX_EPOCH + Duration::from_millis(1_200))
            .max_age(Duration::from_secs(1))
            .call()
    };
    assert!(query().await.is_err());
    store.set_unavailable(true);
    assert!(query().await.is_err());
}

#[tokio::test(start_paused = true)]
async fn historical_population_uses_the_fraction_and_epoch_at_the_observation() {
    let store = MemoryStore::new();
    let sampling = sampling();
    let mut old = observation(&sampling, 1000, 50, 1);
    old.reporting_epoch = ulid::Ulid::new().to_string();
    // Include ten profiling processes outside the census; these must not
    // inflate the denominator used to estimate the total population.
    old.observed = 60;
    old.admitted += 10;
    put(&store, &old).await;
    let mut new = observation(&sampling, 2000, 25, 2);
    new.reporting_epoch = ulid::Ulid::new().to_string();
    put(&store, &new).await;
    for (time, expected_epoch, fraction) in [
        (1500, &old.reporting_epoch, 0.5),
        (2500, &new.reporting_epoch, 0.25),
    ] {
        let estimate = sampling
            .population_at()
            .store(&store)
            .at(UNIX_EPOCH + Duration::from_millis(time))
            .max_age(Duration::from_secs(1))
            .call()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&estimate.reporting_epoch, expected_epoch);
        assert_eq!(estimate.tracking_probability, fraction);
        assert_eq!(estimate.population, 100.0);
    }
}

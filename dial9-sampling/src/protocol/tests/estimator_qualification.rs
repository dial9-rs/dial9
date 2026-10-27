#![cfg(not(shuttle))]
use crate::protocol::{simulation::MemoryStore, *};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::{collections::BinaryHeap, sync::Arc, time::Duration};

#[tokio::test(start_paused = true)]
#[ignore = "51.2 million seeded hashes; run qualification explicitly with --ignored"]
async fn estimator_monte_carlo_512_materialized_100k_fleets() {
    const N: usize = 100_000;
    const K: usize = 256;
    const TRIALS: usize = 512;
    let mut estimates = Vec::with_capacity(TRIALS);
    let mut relative_errors = Vec::with_capacity(TRIALS);
    for trial in 0..TRIALS {
        let mut bottom: BinaryHeap<(u64, usize)> = BinaryHeap::with_capacity(K + 1);
        for node in 0..N {
            let owner = format!("trial-{trial}-node-{node}");
            let hash = Sha256::digest(owner.as_bytes());
            let rank = u64::from_be_bytes(hash[..8].try_into().unwrap());
            if bottom.len() < K {
                bottom.push((rank, node));
            } else if rank < bottom.peek().unwrap().0 {
                bottom.pop();
                bottom.push((rank, node));
            }
        }
        let store = Arc::new(MemoryStore::new());
        let policy = Config::builder()
            .scope(format!("trial-{trial}"))
            .exact_target(1)
            .if_failed(StartupFailure::disabled())
            .build();
        for (rank, node) in bottom {
            store
                .put(
                    PutRequest::builder()
                        .key(format!(
                            "{}members/{rank:016x}/trial-{trial}-node-{node}",
                            policy.pool_prefix()
                        ))
                        .body(Bytes::from_static(b"live"))
                        .build(),
                )
                .await
                .unwrap();
        }
        // Materialize the exact bottom-k of the seeded population, then ask
        // the real engine to read and estimate it through SamplingStore.
        let node = policy.prepare(store).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        let estimate = node.snapshot().estimated_population().unwrap();
        estimates.push(estimate);
        relative_errors.push(estimate / N as f64 - 1.0);
        node.into_session()
            .unwrap()
            .shutdown(Duration::from_secs(1))
            .await;
    }
    let mean = estimates.iter().sum::<f64>() / TRIALS as f64;
    let variance = estimates.iter().map(|n| (n - mean).powi(2)).sum::<f64>() / (TRIALS - 1) as f64;
    let expected_variance = N as f64 * (N - K + 1) as f64 / (K - 2) as f64;
    let mean_z = (mean - N as f64) / (expected_variance / TRIALS as f64).sqrt();
    assert!(
        mean_z.abs() < 4.5,
        "deterministic seeded mean outside the expected range: {mean_z}"
    );
    assert!((0.7..1.3).contains(&(variance / expected_variance)));
    relative_errors.sort_by(f64::total_cmp);
    let summary = serde_json::json!({
        "trials": TRIALS, "population_per_trial": N, "sample_size": K,
        "hashes_evaluated": N * TRIALS, "mean_estimate": mean, "mean_relative_bias": mean / N as f64 - 1.0,
        "relative_standard_deviation": variance.sqrt() / N as f64,
        "theoretical_relative_standard_deviation": expected_variance.sqrt() / N as f64,
        "observed_p025_relative_error": relative_errors[TRIALS * 25 / 1000],
        "observed_p975_relative_error": relative_errors[TRIALS * 975 / 1000],
        "min_relative_error": relative_errors.first(), "max_relative_error": relative_errors.last(),
        "fraction_within_10_percent": relative_errors.iter().filter(|x| x.abs() <= 0.1).count() as f64 / TRIALS as f64,
        "mean_z": mean_z,
        "scope": "Seeded stable populations materialized as exact bottom-k; estimates are read through the production engine. Not a churn, partition or storage-capacity bound.",
    });
    println!("{summary}");
}

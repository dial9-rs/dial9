#![cfg(not(shuttle))]
use crate::protocol::{
    simulation::{self, MemoryStore},
    *,
};
use std::{sync::Arc, time::Duration};
#[derive(Clone, Copy)]
enum Layout {
    One,
    Even,
    Skewed,
}
fn groups(layout: Layout) -> Vec<usize> {
    match layout {
        Layout::One => vec![100_000],
        Layout::Even => vec![100; 1000],
        Layout::Skewed => (0..1000)
            .map(|g| match g {
                0..10 => 5000,
                10..100 => 300,
                100..200 => 100,
                200..400 => 17,
                _ => 16,
            })
            .collect(),
    }
}

async fn run_large(layout: Layout, name: &str, latency_ms: u64) {
    let sizes = groups(layout);
    assert_eq!(sizes.iter().sum::<usize>(), 100_000);
    let store = Arc::new(MemoryStore::new());
    store.set_latency(Duration::from_millis(latency_ms));
    let began = tokio::time::Instant::now();
    let real_began = std::time::Instant::now();
    let mut tasks = Vec::with_capacity(100_000);
    for (group, &size) in sizes.iter().enumerate() {
        let policy = Config::builder()
            .scope(name)
            .exact_target(1)
            .add_dimension("instance-type", format!("type-{}", group % 16))
            .add_dimension("region", format!("region-{}", group / 16 % 20))
            .add_dimension("az", format!("az-{}", group / 320 % 3))
            .add_dimension("cell", format!("cell-{}", group / 960))
            .if_failed(StartupFailure::disabled())
            .build();
        for _ in 0..size {
            let policy = policy.clone();
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                let start = tokio::time::Instant::now();
                let node = simulation::prepare(&policy, store).await;
                (group, tokio::time::Instant::now() - start, node)
            }));
        }
    }
    let mut fleet = Vec::with_capacity(tasks.len());
    for task in tasks {
        fleet.push(task.await.unwrap());
    }
    let mut decision_us: Vec<_> = fleet.iter().map(|(_, t, _)| t.as_micros() as u64).collect();
    decision_us.sort_unstable();
    assert!(decision_us.last().copied().unwrap() <= 500_000);
    let mut selected = vec![0; sizes.len()];
    for (group, _, node) in &fleet {
        if node.snapshot().recording() {
            selected[*group] += 1;
        }
    }
    assert!(
        selected.iter().all(|&n| n == 1),
        "every dimension tuple should have one profiler"
    );
    let decisions: Vec<_> = fleet
        .iter()
        .map(|(_, _, n)| n.snapshot().recording())
        .collect();
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_secs(61)).await;
    }
    assert_eq!(
        decisions,
        fleet
            .iter()
            .map(|(_, _, n)| n.snapshot().recording())
            .collect::<Vec<_>>()
    );
    let mut per_pool: Vec<Option<f64>> = vec![None; sizes.len()];
    for (group, _, node) in &fleet {
        if let Some(n) = node.snapshot().estimated_population() {
            per_pool[*group] = Some(n);
        }
    }
    let known: Vec<_> = per_pool
        .iter()
        .enumerate()
        .filter_map(|(g, n)| n.map(|n| (sizes[g], n)))
        .collect();
    let expected_known: usize = sizes.iter().filter(|&&n| n >= 256).sum();
    let estimated_known: f64 = known.iter().map(|(_, n)| *n).sum();
    let summary = serde_json::json!({
        "layout": name, "nodes": fleet.len(), "dimension_tuples": sizes.len(), "target_per_tuple": 1,
        "operation_latency_ms": latency_ms, "storage_model": "fixed latency, unlimited request concurrency; no S3 capacity or CPU-latency model", "profilers": selected.iter().sum::<usize>(),
        "decision_median_us": decision_us[decision_us.len()/2],
        "decision_p99_us": decision_us[decision_us.len()*99/100],
        "decision_max_us": decision_us.last(), "virtual_seconds": began.elapsed().as_secs_f64(),
        "known_estimate_pools": known.len(), "unknown_estimate_pools": sizes.len()-known.len(),
        "known_population": expected_known, "estimated_known_population": estimated_known,
        "relative_error_known": if expected_known > 0 { Some(estimated_known / expected_known as f64 - 1.0) } else { None },
        "operations": store.operation_count(), "wall_seconds": real_began.elapsed().as_secs_f64(),
    });
    println!("{summary}");
}

#[tokio::test(start_paused = true)]
#[ignore = "100k-node qualification; run explicitly with --ignored"]
async fn fleet_100k_one_tuple() {
    run_large(Layout::One, "100k-one-tuple", 1).await
}
#[tokio::test(start_paused = true)]
#[ignore = "100k-node qualification; run explicitly with --ignored"]
async fn fleet_100k_1000_even_tuples() {
    run_large(Layout::Even, "100k-even-tuples", 1).await
}
#[tokio::test(start_paused = true)]
#[ignore = "100k-node qualification; run explicitly with --ignored"]
async fn fleet_100k_1000_skewed_tuples() {
    run_large(Layout::Skewed, "100k-skewed-tuples", 1).await
}

//! Thread-per-core architecture using current-thread runtimes.
//!
//! Usage:
//!   cargo run --example thread_per_core --features analysis

use dial9_tokio_telemetry::telemetry::{DiskWriter, TelemetryCore};
use dial9_trace_format::decoder::Decoder;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum Event {
    PollStartEvent {
        worker_id: u64,
    },
    PollEndEvent {
        worker_id: u64,
    },
    SegmentMetadataEvent {
        entries: Vec<(String, String)>,
    },
    #[serde(other)]
    Other,
}

fn main() -> std::io::Result<()> {
    let trace_dir = "/tmp/thread_per_core";
    let _ = std::fs::create_dir_all(trace_dir);
    for entry in std::fs::read_dir(trace_dir)? {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_some_and(|e| e == "bin" || e == "active")
        {
            std::fs::remove_file(entry.path())?;
        }
    }

    let writer = DiskWriter::builder()
        .base_path(format!("{trace_dir}/trace.bin"))
        .max_file_size(1024 * 1024)
        .max_total_size(5 * 1024 * 1024)
        .build()?;

    let guard = TelemetryCore::builder()
        .writer(writer)
        .trace_path(format!("{trace_dir}/trace.bin"))
        .build()?;
    guard.enable();

    let num_cores = std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(2);

    println!("Spawning {num_cores} current-thread runtimes...");

    let threads: Vec<_> = (0..num_cores)
        .map(|core_id| {
            let mut core_builder = tokio::runtime::Builder::new_current_thread();
            core_builder.enable_all();

            let (core_rt, handle) = guard
                .trace_runtime(format!("core-{core_id}"))
                .build(core_builder)
                .unwrap();

            std::thread::Builder::new()
                .name(format!("core-{core_id}"))
                .spawn(move || {
                    core_rt
                        .block_on(handle.spawn(async move {
                            for i in 0..20 {
                                tokio::task::yield_now().await;
                                tokio::time::sleep(Duration::from_millis(2)).await;
                                if i % 10 == 0 {
                                    println!("  [core-{core_id}] processed {i}");
                                }
                            }
                        }))
                        .unwrap();
                })
                .unwrap()
        })
        .collect();

    for t in threads {
        t.join().unwrap();
    }
    println!("All cores finished.\n");

    let _ = guard.graceful_shutdown(Duration::from_secs(5));

    // Read back the trace and verify
    let mut files: Vec<_> = std::fs::read_dir(trace_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
        .collect();
    files.sort();

    let mut runtime_workers: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut poll_counts: BTreeMap<u64, usize> = BTreeMap::new();
    let mut seen_workers: BTreeSet<u64> = BTreeSet::new();
    let mut total_polls = 0usize;
    let mut unknown_polls = 0usize;

    const WORKER_UNKNOWN: u64 = 255;

    for file in &files {
        let data = std::fs::read(file)?;
        let mut decoder =
            Decoder::new(&data).ok_or_else(|| std::io::Error::other("invalid trace header"))?;
        decoder
            .for_each_event(|raw| {
                let ev: Event = raw.deserialize().expect("deserialize");
                match &ev {
                    Event::SegmentMetadataEvent { entries } => {
                        for (key, val) in entries {
                            if let Some(name) = key.strip_prefix("runtime.") {
                                let ids: Vec<u64> =
                                    val.split(',').filter_map(|s| s.parse().ok()).collect();
                                runtime_workers.insert(name.to_string(), ids);
                            }
                        }
                    }
                    Event::PollStartEvent { worker_id } | Event::PollEndEvent { worker_id } => {
                        total_polls += 1;
                        if *worker_id != WORKER_UNKNOWN {
                            seen_workers.insert(*worker_id);
                            *poll_counts.entry(*worker_id).or_default() += 1;
                        } else {
                            unknown_polls += 1;
                        }
                    }
                    Event::Other => {}
                }
            })
            .map_err(|e| std::io::Error::other(e.to_string()))?;
    }

    println!("=== Poll event summary ===");
    println!(
        "  total: {total_polls}, resolved: {}, unknown: {unknown_polls}",
        total_polls - unknown_polls
    );

    println!("=== Runtime → Worker mapping (from segment metadata) ===");
    for (name, ids) in &runtime_workers {
        println!("  {name}: workers {ids:?}");
    }

    println!("\n=== Poll events per worker ===");
    for (worker_id, count) in &poll_counts {
        let runtime = runtime_workers
            .iter()
            .find(|(_, ids)| ids.contains(worker_id))
            .map(|(name, _)| name.as_str())
            .unwrap_or("unknown");
        println!("  worker {worker_id} ({runtime}): {count} poll events");
    }

    let metadata_ids: BTreeSet<u64> = runtime_workers.values().flatten().copied().collect();
    let unaccounted: Vec<_> = seen_workers.difference(&metadata_ids).collect();
    if unaccounted.is_empty() {
        println!("\n✓ All worker IDs are accounted for in runtime metadata.");
    } else {
        println!("\n✗ Workers not in metadata: {unaccounted:?}");
    }

    Ok(())
}

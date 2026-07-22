//! Thread-per-core architecture using current-thread runtimes.
//!
//! Some applications pin one single-threaded tokio runtime per CPU core for
//! cache locality and predictable latency. This example shows how to trace
//! that pattern: a recorder is built, then each per-core runtime is attached to
//! it with `attach_tokio_runtime`.
//!
//! After the workload completes, the trace file is read back and all
//! PollStart/PollEnd worker IDs are printed alongside the runtime→worker
//! mapping from segment metadata — verifying that every core's events
//! landed in the trace with the correct identity.
//!
//! Usage:
//!   cargo run --example thread_per_core --features analysis
//!
//! After running, inspect the trace:
//!   cargo run --example analyze_trace -- /tmp/thread_per_core/trace.0.bin

use dial9::analysis::analysis_events::{Dial9Event, WorkerId};
use dial9::{DiskBuffer, RecorderTokioExt, TokioAttachOptions, recorder};
use dial9_trace_format::decoder::Decoder;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let trace_dir = "/tmp/thread_per_core";
    let _ = std::fs::create_dir_all(trace_dir);
    // Clean up previous runs.
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

    let writer = DiskBuffer::builder()
        .base_path(trace_dir)
        .max_file_size(1024 * 1024)
        .max_total_size(5 * 1024 * 1024)
        .build()?;

    let recorder = recorder(writer).build();

    // Spawn one current-thread runtime per core.
    let num_cores = std::thread::available_parallelism()
        .map(|n| n.get().min(4)) // cap at 4 for the demo
        .unwrap_or(2);

    println!("Spawning {num_cores} current-thread runtimes...");

    // `attach_tokio_runtime_with` hands the recorder back each round, so the loop
    // threads it from core to core.
    let mut recorder = recorder;
    let mut threads = Vec::with_capacity(num_cores);
    for core_id in 0..num_cores {
        let (rec, core_rt) = recorder
            .attach_tokio_runtime_with(
                TokioAttachOptions::builder()
                    .runtime_name(format!("core-{core_id}"))
                    .build(),
                |t| {
                    *t = tokio::runtime::Builder::new_current_thread();
                    t.enable_all();
                },
            )
            .expect("build core runtime");
        recorder = rec;

        threads.push(
            std::thread::Builder::new()
                .name(format!("core-{core_id}"))
                .spawn(move || {
                    core_rt.block_on(async move {
                        // `dial9::spawn` tracks the task on this core's runtime.
                        dial9::spawn(async move {
                            for i in 0..20 {
                                tokio::task::yield_now().await;
                                tokio::time::sleep(Duration::from_millis(2)).await;
                                if i % 10 == 0 {
                                    println!("  [core-{core_id}] processed {i}");
                                }
                            }
                        })
                        .await
                        .unwrap();
                    });
                })
                .unwrap(),
        );
    }

    for t in threads {
        t.join().unwrap();
    }
    println!("All cores finished.\n");

    recorder.graceful_shutdown(Duration::from_secs(1));

    // ── Read back the trace and verify ──────────────────────────────────

    let mut files: Vec<_> = std::fs::read_dir(trace_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
        .collect();
    files.sort();

    // Collect: runtime name → worker IDs (from metadata), and
    //          worker ID → poll event count (from PollStart/PollEnd).
    let mut runtime_workers: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut poll_counts: BTreeMap<WorkerId, usize> = BTreeMap::new();
    let mut seen_workers: BTreeSet<WorkerId> = BTreeSet::new();
    let mut total_polls = 0usize;
    let mut unknown_polls = 0usize;

    for file in &files {
        let data = std::fs::read(file)?;
        let mut decoder =
            Decoder::new(&data).ok_or_else(|| std::io::Error::other("invalid trace header"))?;
        decoder
            .for_each_event(|raw| {
                let ev: Dial9Event = raw.deserialize().expect("deserialize");
                match &ev {
                    Dial9Event::SegmentMetadataEvent(e) => {
                        for (key, val) in &e.entries {
                            if let Some(name) = key.strip_prefix("runtime.") {
                                let ids: Vec<u64> =
                                    val.split(',').filter_map(|s| s.parse().ok()).collect();
                                runtime_workers.insert(name.to_string(), ids);
                            }
                        }
                    }
                    Dial9Event::PollStartEvent(e) => {
                        total_polls += 1;
                        if e.worker_id != WorkerId::UNKNOWN {
                            seen_workers.insert(e.worker_id);
                            *poll_counts.entry(e.worker_id).or_default() += 1;
                        } else {
                            unknown_polls += 1;
                        }
                    }
                    Dial9Event::PollEndEvent(e) => {
                        total_polls += 1;
                        if e.worker_id != WorkerId::UNKNOWN {
                            seen_workers.insert(e.worker_id);
                            *poll_counts.entry(e.worker_id).or_default() += 1;
                        } else {
                            unknown_polls += 1;
                        }
                    }
                    _ => {}
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
        // Find which runtime this worker belongs to.
        let runtime = runtime_workers
            .iter()
            .find(|(_, ids)| ids.contains(&worker_id.0))
            .map(|(name, _)| name.as_str())
            .unwrap_or("unknown");
        println!("  worker {worker_id} ({runtime}): {count} poll events");
    }

    // Verify every worker that emitted events is accounted for in metadata.
    let metadata_ids: BTreeSet<WorkerId> = runtime_workers
        .values()
        .flatten()
        .map(|&id| WorkerId(id))
        .collect();
    let unaccounted: Vec<_> = seen_workers.difference(&metadata_ids).collect();
    if unaccounted.is_empty() {
        println!("\n✓ All worker IDs are accounted for in runtime metadata.");
    } else {
        println!("\n✗ Workers not in metadata: {unaccounted:?}");
    }

    Ok(())
}

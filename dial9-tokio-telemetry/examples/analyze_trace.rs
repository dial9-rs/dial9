//! Example: analyze a dial9 trace file using the serde decode path.
//!
//! Usage:
//!   cargo run --example analyze_trace --features analysis -- <trace_file>

use dial9_tokio_telemetry::telemetry::analysis_events::{Dial9Event, WorkerId};
use dial9_trace_format::decoder::Decoder;
use std::collections::HashMap;
use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <trace_file>", args[0]);
        std::process::exit(1);
    }

    let data = std::fs::read(&args[1]).expect("Failed to read trace file");
    let mut decoder = Decoder::new(&data).expect("Invalid trace header");

    let mut events = Vec::new();
    decoder
        .for_each_event(|raw| {
            let ev: Dial9Event = raw.deserialize().expect("deserialize");
            events.push(ev);
        })
        .expect("decode");

    println!("Read {} events", events.len());

    // Per-worker stats
    let mut worker_polls: HashMap<WorkerId, usize> = HashMap::new();
    let mut worker_parks: HashMap<WorkerId, usize> = HashMap::new();

    // Task spawn locations
    let mut task_locs: HashMap<u64, String> = HashMap::new();

    for e in &events {
        match e {
            Dial9Event::PollStartEvent(ev) => {
                *worker_polls.entry(ev.worker_id).or_default() += 1;
                task_locs
                    .entry(ev.task_id)
                    .or_insert_with(|| ev.spawn_loc.clone());
            }
            Dial9Event::WorkerParkEvent(ev) => {
                *worker_parks.entry(ev.worker_id).or_default() += 1;
            }
            Dial9Event::TaskSpawnEvent(ev) => {
                task_locs
                    .entry(ev.task_id)
                    .or_insert_with(|| ev.spawn_loc.clone());
            }
            _ => {}
        }
    }

    println!("\n=== Per-Worker Stats ===");
    let mut workers: Vec<_> = worker_polls.keys().copied().collect();
    workers.sort();
    for w in &workers {
        // w came from worker_polls.keys(), so it's always present there
        let &polls = worker_polls.get(w).expect("worker present in poll map");
        let parks = match worker_parks.get(w) {
            Some(&p) => p,
            None => 0,
        };
        println!("  worker {w}: polls={polls}, parks={parks}");
    }

    // Wake analysis
    let mut wakes_by_loc: HashMap<&str, usize> = HashMap::new();
    for e in &events {
        if let Dial9Event::WakeEvent(ev) = e {
            let loc = task_locs
                .get(&ev.waker_task_id)
                .map(|s| s.as_str())
                .unwrap_or("<unknown>");
            *wakes_by_loc.entry(loc).or_default() += 1;
        }
    }

    if !wakes_by_loc.is_empty() {
        println!("\n=== Waker Identity ===");
        let mut by_count: Vec<_> = wakes_by_loc.into_iter().collect();
        by_count.sort_by_key(|b| std::cmp::Reverse(b.1));
        for (loc, count) in by_count.iter().take(10) {
            println!("  {count:>8} wakes from {loc}");
        }
    }
}

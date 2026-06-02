//! Example: analyze a dial9 trace file using the serde decode path.
//!
//! Usage:
//!   cargo run --example analyze_trace --features analysis -- <trace_file>

use dial9_trace_format::decoder::Decoder;
use serde::Deserialize;
use std::collections::HashMap;
use std::env;

#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum Event {
    PollStartEvent {
        timestamp_ns: u64,
        worker_id: u64,
        task_id: u64,
        spawn_loc: String,
    },
    PollEndEvent {
        timestamp_ns: u64,
        worker_id: u64,
    },
    WorkerParkEvent {
        timestamp_ns: u64,
        worker_id: u64,
    },
    WorkerUnparkEvent {
        timestamp_ns: u64,
        worker_id: u64,
    },
    TaskSpawnEvent {
        timestamp_ns: u64,
        task_id: u64,
        spawn_loc: String,
    },
    WakeEventEvent {
        timestamp_ns: u64,
        waker_task_id: u64,
        woken_task_id: u64,
    },
    #[serde(other)]
    Other,
}

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
            let ev: Event = raw.deserialize().expect("deserialize");
            events.push(ev);
        })
        .expect("decode");

    println!("Read {} events", events.len());

    // Per-worker stats
    let mut worker_polls: HashMap<u64, usize> = HashMap::new();
    let mut worker_parks: HashMap<u64, usize> = HashMap::new();

    // Task spawn locations
    let mut task_locs: HashMap<u64, String> = HashMap::new();

    for e in &events {
        match e {
            Event::PollStartEvent {
                worker_id,
                task_id,
                spawn_loc,
                ..
            } => {
                *worker_polls.entry(*worker_id).or_default() += 1;
                task_locs
                    .entry(*task_id)
                    .or_insert_with(|| spawn_loc.clone());
            }
            Event::WorkerParkEvent { worker_id, .. } => {
                *worker_parks.entry(*worker_id).or_default() += 1;
            }
            Event::TaskSpawnEvent {
                task_id, spawn_loc, ..
            } => {
                task_locs
                    .entry(*task_id)
                    .or_insert_with(|| spawn_loc.clone());
            }
            _ => {}
        }
    }

    println!("\n=== Per-Worker Stats ===");
    let mut workers: Vec<_> = worker_polls.keys().copied().collect();
    workers.sort();
    for w in &workers {
        let polls = worker_polls.get(w).copied().unwrap_or(0);
        let parks = worker_parks.get(w).copied().unwrap_or(0);
        println!("  worker {w}: polls={polls}, parks={parks}");
    }

    // Wake analysis
    let mut wakes_by_loc: HashMap<&str, usize> = HashMap::new();
    for e in &events {
        if let Event::WakeEventEvent { waker_task_id, .. } = e {
            let loc = task_locs
                .get(waker_task_id)
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

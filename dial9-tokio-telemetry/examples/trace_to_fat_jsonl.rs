//! Convert a dial9 binary trace to "fat" JSONL with all metadata resolved inline.
//!
//! Usage:
//!   cargo run --example trace_to_fat_jsonl --features analysis -- <input.bin> [output.jsonl]

use dial9_trace_format::decoder::Decoder;
use serde::{Deserialize, Serialize};
use std::io::{BufWriter, Write};

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "event")]
enum FatEvent {
    PollStartEvent {
        timestamp_ns: u64,
        worker_id: u64,
        local_queue: u8,
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
        local_queue: u8,
        cpu_time_ns: u64,
        tid: u32,
    },
    WorkerUnparkEvent {
        timestamp_ns: u64,
        worker_id: u64,
        local_queue: u8,
        cpu_time_ns: u64,
        sched_wait_ns: u64,
        tid: u32,
    },
    QueueSampleEvent {
        timestamp_ns: u64,
        global_queue: u8,
    },
    CpuSampleEvent {
        timestamp_ns: u64,
        worker_id: u64,
        source: u8,
        callchain: Vec<u64>,
    },
    WakeEventEvent {
        timestamp_ns: u64,
        waker_task_id: u64,
        woken_task_id: u64,
        target_worker: u8,
    },
    TaskSpawnEvent {
        timestamp_ns: u64,
        task_id: u64,
        spawn_loc: String,
    },
    #[serde(other)]
    Other,
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: trace_to_fat_jsonl <input.bin> [output.jsonl]");
        std::process::exit(1);
    }

    let data = std::fs::read(&args[1])?;
    let mut decoder =
        Decoder::new(&data).ok_or_else(|| std::io::Error::other("invalid trace header"))?;

    let out: Box<dyn Write> = if let Some(path) = args.get(2) {
        Box::new(std::fs::File::create(path)?)
    } else {
        Box::new(std::io::stdout().lock())
    };
    let mut w = BufWriter::new(out);

    let mut count = 0u64;
    decoder
        .for_each_event(|raw| {
            let ev: FatEvent = raw.deserialize().expect("deserialize");
            if !matches!(ev, FatEvent::Other) {
                serde_json::to_writer(&mut w, &ev).expect("write json");
                w.write_all(b"\n").expect("write newline");
                count += 1;
            }
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    w.flush()?;
    eprintln!("{count} events written");
    Ok(())
}

//! On-trigger pipeline runs: buffer trace segments, process them only when
//! the application asks for a dump.
//!
//! By default the dial9 worker processes every sealed segment continuously.
//! Wiring a trigger (`with_trigger(rx)`) flips that same pipeline into
//! on-demand operation: segments keep accumulating in the ring, and the
//! pipeline only runs when a `DumpControl` requests a dump - from a panic
//! hook, an idle-ratio watcher, a `/dump` handler, whatever decides
//! something is worth keeping. Most trace data is uninteresting; this mode
//! pays processing cost only when it matters.
//!
//! This example models the realistic case: a background monitor task samples
//! the ring on an interval and, when it spots an "incident", triggers a dump -
//! the same shape as a watcher checking an idle-ratio or a p999 latency every
//! few hundred milliseconds. A real watcher re-trips on consecutive ticks, so
//! the control is built with `with_debounce(...)`: the first trigger dumps and
//! the burst that follows folds into it (resolving `DumpError::Coalesced`)
//! instead of producing a pile of near-identical dumps.
//!
//! `dump::trigger()` returns a `(control, rx)` pair: `rx` is wired into the
//! runtime config, and `control` is what the application calls to dump. The
//! `#[dial9_tokio_telemetry::main]` macro builds the runtime from the config
//! closure, so `control` is published through a `static` for the body (the
//! monitor task, a panic hook, ...) to reach.
//!
//! This example uses a local `gzip` + `write_back` pipeline (no AWS setup).
//! Dumped segments land as `*.bin.gz` in the trace dir.
//!
//!   cargo run -p dial9-tokio-telemetry --example on_trigger_dump
//!
//! Inspect a dumped segment afterwards:
//!   gunzip /tmp/dial9-on-trigger-dump/trace.0.bin.gz

use std::sync::OnceLock;
use std::time::Duration;

use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::dump::{self, DumpControl, DumpError};
use dial9_tokio_telemetry::telemetry::TelemetryHandle;

const TRACE_DIR: &str = "/tmp/dial9-on-trigger-dump";

/// Published by the config closure so the monitor task (or a panic hook, an
/// HTTP handler, ...) can reach the dump control. `DumpControl` is `Clone`.
static DUMP: OnceLock<DumpControl> = OnceLock::new();

/// Count sealed segments in the ring (`*.bin`, excluding the active file).
fn sealed_segments() -> usize {
    std::fs::read_dir(TRACE_DIR)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".bin"))
                .count()
        })
        .unwrap_or(0)
}

#[dial9_tokio_telemetry::main(config = || {
    let _ = std::fs::remove_dir_all(TRACE_DIR);
    let _ = std::fs::create_dir_all(TRACE_DIR);
    let trace_path = format!("{TRACE_DIR}/trace.bin");

    // Create the trigger pair. `rx` is moved into the builder below;
    // `control` is published so the monitor can dump on demand. The debounce
    // gate folds a burst of re-trips into a single dump.
    let (control, rx) = dump::trigger();
    let _ = DUMP.set(control.with_debounce(Duration::from_secs(30)));

    Dial9Config::builder()
        .on_disk_buffer(trace_path)
        // Fast-rotating writer so the demo seals a segment within a couple of
        // seconds instead of waiting on the default rotation period.
        .max_file_size(4 * 1024)
        .max_total_size(10 * 1024 * 1024)
        .rotation_period(Duration::from_millis(500))
        .with_tokio(|t| { t.worker_threads(2); })
        // The pipeline is whatever you would run continuously (here: gzip +
        // write_back); `with_trigger(rx)` only changes *when* it runs.
        .with_runtime(move |r| r
            .with_task_tracking(true)
            .with_custom_pipeline(|p| p.gzip().write_back())
            .with_trigger(rx))
        .build_or_disabled()
})]
async fn main() {
    let handle = TelemetryHandle::current();
    let control = DUMP
        .get()
        .expect("DumpControl published by config closure")
        .clone();

    // Steady workload so the ring keeps sealing segments. The pipeline stays
    // parked: nothing is gzipped or written back until the monitor dumps.
    for id in 0..8 {
        handle.spawn(async move {
            for _ in 0..200 {
                tokio::time::sleep(Duration::from_millis(25)).await;
                std::hint::black_box(id);
            }
        });
    }

    // Background monitor: sample the ring each tick and decide when to dump.
    // Here the "incident" is simply that segments have accumulated; in a real
    // app this is an idle-ratio drop, a p999 latency spike, a panic hook, etc.
    while sealed_segments() == 0 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!(
        "monitor: incident detected, {} sealed segment(s) buffered",
        sealed_segments()
    );

    // A real watcher re-trips on consecutive ticks. The first trigger dumps;
    // the rest fold into it via the debounce gate.
    let mut receipt = None;
    for tick in 0..3 {
        match control
            .dump_current_data()
            .with_metadata("reason", "idle-ratio-drop")
            .await
        {
            Ok(r) => {
                println!(
                    "monitor: tick {tick}: dump {} captured {} segment(s)",
                    r.dump_id, r.segments_processed
                );
                receipt.get_or_insert(r);
            }
            Err(DumpError::Coalesced { into }) => {
                println!("monitor: tick {tick}: re-trip folded into dump {into}, skipping");
            }
            Err(e) => println!("monitor: tick {tick}: dump error: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let receipt = receipt.expect("at least one dump ran");
    println!("dump complete:");
    println!("  dump_id            = {}", receipt.dump_id);
    println!("  segments_processed = {}", receipt.segments_processed);
    println!("  time_range         = {:?}", receipt.time_range);
    println!("processed to disk: run `ls {TRACE_DIR}/*.bin.gz`");
}

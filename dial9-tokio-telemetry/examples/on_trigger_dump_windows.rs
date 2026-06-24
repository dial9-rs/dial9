//! On-trigger pipeline runs: time windows and concurrent dumps.
//!
//! Companion to `on_trigger_dump.rs`, which covers the minimal case
//! (`dump_current_data` plus a debounce gate). This example exercises the rest
//! of the `DumpTrigger` surface:
//!
//!   * `dump_time_range(lookback, lookforward)` with a pure look-back window
//!     (capture the history the ring still holds, no forward window), and with
//!     a look-forward window (keep the dump open and capture segments as they
//!     seal after the trigger; the await only resolves once the forward
//!     deadline elapses).
//!   * Two overlapping dumps running concurrently: each gets its own `DumpId`,
//!     and a segment whose `[creation, seal]` span falls inside both windows is
//!     captured by both. Off S3 (here) each dump just resolves its own
//!     receipt; against S3 the shared object key would land in both dumps'
//!     manifests.
//!
//! Disk + `gzip` + `write_back`, no AWS setup. Dumped segments land as
//! `*.bin.gz` in the trace dir.
//!
//!   cargo run -p dial9-tokio-telemetry --example on_trigger_dump_windows
//!
//! Inspect a dumped segment afterwards:
//!   gunzip /tmp/dial9-on-trigger-windows/trace.0.bin.gz

use std::time::Duration;

use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::telemetry::{Dial9Handle, Dial9TokioHandle};

const TRACE_DIR: &str = "/tmp/dial9-on-trigger-windows";

#[dial9_tokio_telemetry::main(config = || {
    let _ = std::fs::remove_dir_all(TRACE_DIR);
    let _ = std::fs::create_dir_all(TRACE_DIR);
    let trace_path = format!("{TRACE_DIR}/trace.bin");

    Dial9Config::builder()
        .on_disk_buffer(trace_path)
        // Fast-rotating writer so the demo seals a segment every ~half second.
        .max_file_size(4 * 1024)
        .max_total_size(10 * 1024 * 1024)
        .rotation_period(Duration::from_millis(500))
        .with_tokio(|t| { t.worker_threads(2); })
        // No debounce here: we want the two concurrent dumps to each register,
        // not fold into one another.
        .with_runtime(|r| r
            .with_custom_pipeline(|p| p.gzip().write_back())
            .with_dump_trigger(|_| {}))
        .build_or_disabled()
})]
async fn main() {
    let handle = Dial9TokioHandle::current();
    // Reach the dump trigger through the ambient handle; the runtime stashed
    // it when `with_dump_trigger` was configured.
    let trigger = Dial9Handle::current()
        .dump_trigger()
        .expect("on-demand mode enabled");

    // Steady workload so the ring keeps sealing segments for the whole demo.
    // The pipeline stays parked: nothing is gzipped or written back until a
    // dump asks for data.
    for id in 0..8 {
        handle.spawn(async move {
            for _ in 0..800 {
                tokio::time::sleep(Duration::from_millis(25)).await;
                std::hint::black_box(id);
            }
        });
    }

    // Let some history accumulate in the ring before the first dump.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 1) Pure look-back: everything the ring retained over the last hour, no
    //    forward window. Resolves as soon as the matching ring segments finish
    //    the pipeline.
    let receipt = trigger
        .dump_time_range(Duration::from_secs(3600), Duration::ZERO)
        .with_metadata("reason", "look-back")
        .await
        .expect("look-back dump");
    println!(
        "look-back dump {}: {} segment(s), span {:?}",
        receipt.dump_id, receipt.segments_processed, receipt.time_range,
    );

    // 2) Look-forward: a short look-back plus a 3s forward window. The dump
    //    stays open and captures segments as they seal; the await resolves
    //    only after the forward deadline elapses.
    let receipt = trigger
        .dump_time_range(Duration::from_secs(1), Duration::from_secs(3))
        .with_metadata("reason", "look-forward")
        .await
        .expect("look-forward dump");
    println!(
        "look-forward dump {}: {} segment(s), span {:?}",
        receipt.dump_id, receipt.segments_processed, receipt.time_range,
    );

    // 3) Two overlapping dumps at once. Both carry a forward window, so they
    //    stay open concurrently; a segment sealing inside both windows is
    //    captured by each. They get distinct ids and resolve independently.
    let (a, b) = tokio::join!(
        trigger.dump_time_range(Duration::from_secs(1), Duration::from_secs(3)),
        trigger.dump_time_range(Duration::from_secs(1), Duration::from_secs(3)),
    );
    let a = a.expect("concurrent dump a");
    let b = b.expect("concurrent dump b");
    assert_ne!(a.dump_id, b.dump_id, "concurrent dumps get distinct ids");
    println!(
        "concurrent dumps {} ({} seg) and {} ({} seg) ran independently",
        a.dump_id, a.segments_processed, b.dump_id, b.segments_processed,
    );

    println!("processed to disk: run `ls {TRACE_DIR}/*.bin.gz`");
}

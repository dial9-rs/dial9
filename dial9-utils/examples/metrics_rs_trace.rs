//! Records a few metrics.rs metrics into a dial9 trace, the way an application
//! would.
//!
//! ```sh
//! cargo run -p dial9-utils --features metrics-rs --example metrics_rs_trace -- /tmp/metrics.bin
//! ```
//!
//! Open the resulting trace in the viewer and add a chart from any of the
//! `metricsrs:*` sources.

use std::time::Duration;

use dial9_core::buffer::DiskBuffer;
use dial9_utils::metrics_rs::{self, MetricsRsConfig, RecorderMetricsRsExt};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "metrics-rs-trace.bin".to_owned());

    let metrics_recorder = metrics_rs::recorder();
    metrics::set_global_recorder(metrics_recorder.clone()).expect("no other global recorder");

    let dial9_recorder =
        dial9_core::recorder::recorder(DiskBuffer::single_file(&path).expect("open trace"))
            .with_metrics_rs(
                metrics_recorder,
                MetricsRsConfig::builder()
                    .interval(Duration::from_millis(100))
                    .build(),
            )
            .build();

    // Units are optional, but they let the viewer format the axis.
    metrics::describe_histogram!("query_ms", metrics::Unit::Milliseconds, "db query latency");
    metrics::describe_counter!("bytes_read", metrics::Unit::Bytes, "bytes read from disk");

    for tick in 0..20 {
        metrics::counter!("requests_total", "route" => "/pets").increment(3);
        metrics::counter!("requests_total", "route" => "/owners").increment(1);
        metrics::counter!("bytes_read").increment(4096);
        metrics::gauge!("pool_available").set(8.0 - (tick % 8) as f64);

        for step in 0..50 {
            metrics::histogram!("query_ms").record(f64::from(5 + (step * tick) % 400));
        }
        // An error kind that only shows up partway through, so the trace
        // exercises a metric registering late.
        if tick > 12 {
            metrics::counter!("errors", "kind" => "timeout").increment(1);
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    dial9_recorder.graceful_shutdown(Duration::from_secs(5));
    println!("wrote {path}");
}

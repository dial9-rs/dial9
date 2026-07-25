//! Demonstrates recording metrique unit-of-work metrics into a dial9 trace.
//!
//! The dial9 sink is a peer of the normal metrique pipeline: `tee` delivers
//! every entry both to the human-readable local format (stderr here; EMF in
//! a production service) and to the dial9 trace, where events carry the
//! capturing worker, task, and start/end timestamps from `Dial9Context`.
//!
//! Run with:
//!
//! ```bash
//! cargo run -p dial9 --features metrique-sink --example metrique_metrics
//! ```

use dial9::metrique_sink::{Dial9Context, Dial9Stream, Emit, Interned};
use dial9::{DiskBuffer, RecorderTokioExt, TokioAttachOptions, recorder};
use metrique::ServiceMetrics;
use metrique::local::{LocalFormat, OutputStyle};
use metrique::unit::Millisecond;
use metrique::unit_of_work::metrics;
use metrique::writer::format::FormatExt;
use metrique::writer::stream::tee;
use metrique::writer::{AttachGlobalEntrySinkExt, GlobalEntrySink};
use std::time::Duration;

/// One entry per request. `default_flags(Emit)` opts every field into the
/// dial9 payload; individual fields could opt out with
/// `#[metrics(flags(skip(Emit)))]`.
#[metrics(rename_all = "PascalCase", default_flags(Emit))]
struct RequestMetrics {
    /// Captures worker id, task id, and start/end timestamps for the trace.
    #[metrics(flatten)]
    dial9: Dial9Context,

    /// Low-cardinality string: route it through dial9's string pool.
    #[metrics(flags(Interned))]
    operation: &'static str,

    #[metrics(unit = Millisecond)]
    latency_ms: u64,

    success: bool,
}

async fn handle_request(id: u32) {
    let mut metrics = RequestMetrics {
        dial9: Dial9Context::capture(),
        operation: if id.is_multiple_of(2) { "GetItem" } else { "PutItem" },
        latency_ms: 0,
        success: false,
    }
    .append_on_drop(ServiceMetrics::sink());

    let started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(5 + u64::from(id % 10))).await;

    metrics.latency_ms = started.elapsed().as_millis() as u64;
    metrics.success = true;
    // Dropping `metrics` closes the entry (capturing the end timestamp) and
    // queues it for both sinks.
}

fn main() {
    let writer = DiskBuffer::single_file("metrique_trace.bin").unwrap();
    let (recorder, rt) = recorder(writer)
        .build()
        .attach_tokio_runtime_with(
            TokioAttachOptions::builder()
                .task_tracking_enabled(true)
                .build(),
            |t| {
                t.worker_threads(2);
            },
        )
        .expect("build tokio runtime");

    // Wire the metrique pipeline: local format on stderr, teed with dial9.
    let _join = ServiceMetrics::attach_to_stream(tee(
        LocalFormat::new(OutputStyle::Pretty).output_to_makewriter(|| std::io::stderr().lock()),
        Dial9Stream::new(recorder.handle()),
    ));

    rt.block_on(async {
        let tasks: Vec<_> = (0..20).map(|i| tokio::spawn(handle_request(i))).collect();
        for t in tasks {
            let _ = t.await;
        }
    });

    // Drop the runtime before draining, so its workers flush their buffers.
    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

    println!("Trace written to metrique_trace.bin");
}

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

use dial9::metrique_sink::{Dial9Context, Dial9EntryExt, Dial9Stream};
use dial9::{Dial9HandleTokioExt, DiskBuffer, TokioAttachOptions, recorder};
use metrique::ServiceMetrics;
use metrique::local::{LocalFormat, OutputStyle};
use metrique::unit::Millisecond;
use metrique::unit_of_work::metrics;
use metrique::writer::format::FormatExt;
use metrique::writer::{AttachGlobalEntrySinkExt, GlobalEntrySink};
use std::time::Duration;

/// One entry per request. The flattened `Dial9Context` opts the entry into
/// the dial9 trace; every field is recorded unless flagged `Skip`.
#[metrics(rename_all = "PascalCase")]
struct RequestMetrics {
    #[metrics(flatten)]
    dial9: Dial9Context,

    /// Interned: repeated values hit dial9's string pool.
    #[metrics(flags(dial9::Interned, dial9::SpanName))]
    operation: &'static str,

    #[metrics(unit = Millisecond)]
    latency_ms: u64,

    success: bool,
}

async fn handle_request(id: u32) {
    let mut metrics = RequestMetrics {
        dial9: Dial9Context::capture(),
        operation: if id.is_multiple_of(2) {
            "GetItem"
        } else {
            "PutItem"
        },
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

/// An entry with no dial9 field: `append_on_drop_dial9` attaches the
/// context from the outside, for structs you cannot (or would rather not)
/// change.
#[metrics(rename_all = "PascalCase")]
struct HealthCheckMetrics {
    healthy: bool,
}

async fn health_check() {
    let mut metrics =
        HealthCheckMetrics { healthy: false }.append_on_drop_dial9(ServiceMetrics::sink());
    tokio::time::sleep(Duration::from_millis(1)).await;
    // Field access reaches through the wrapper.
    metrics.healthy = true;
}

fn main() {
    let writer = DiskBuffer::single_file("metrique_trace.bin").unwrap();
    let recorder = recorder(writer).build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(2);

    let rt = recorder
        .handle()
        .attach_tokio_runtime(
            builder,
            TokioAttachOptions::builder()
                .task_tracking_enabled(true)
                .build(),
        )
        .expect("build tokio runtime");

    // Wire the metrique pipeline: local format on stderr, teed with dial9.
    // `Dial9Stream::tee` keeps dial9's own `dial9.` fields out of the local
    // format's output.
    let join = ServiceMetrics::attach_to_stream(Dial9Stream::tee(
        recorder.handle(),
        LocalFormat::new(OutputStyle::Pretty).output_to_makewriter(|| std::io::stderr().lock()),
    ));

    rt.block_on(async {
        let tasks: Vec<_> = (0..20).map(|i| tokio::spawn(handle_request(i))).collect();
        tokio::spawn(health_check())
            .await
            .expect("health check panicked");
        for t in tasks {
            t.await.expect("request task panicked");
        }
    });

    // Shutdown order matters: dropping the attach handle drains the metrique
    // queue into dial9; then drop the runtime and seal the trace.
    drop(join);
    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

    println!("Trace written to metrique_trace.bin");
}

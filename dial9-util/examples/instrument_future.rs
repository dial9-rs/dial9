//! Ad-hoc span instrumentation without a `tracing` subscriber.
//!
//! Demonstrates the [`span`](dial9_util::span) module: attaching spans to
//! futures with [`Instrument`], the [`dial9_span!`] macro for fields, and a
//! synchronous [`enter`] guard. Each instrumented future is polled across
//! several sleeps, producing multiple segments in the viewer.
//!
//! Run with: `cargo run --example instrument_future`
//!
//! [`enter`]: dial9_util::span::Dial9Span::enter
//! [`dial9_span!`]: dial9_util::dial9_span
//! [`Instrument`]: dial9_util::span::Instrument

use dial9_tokio_telemetry::telemetry::{DiskWriter, TracedRuntime};
use dial9_util::dial9_span;
use dial9_util::span::Instrument;
use std::time::Duration;

async fn handle_request(id: u32) {
    // A span attached to a future: one segment per poll, with gaps where the
    // future is suspended at an `.await`. `request_id` is high-cardinality, so
    // `~` stores it inline rather than interning it into the string pool.
    inner_work(id)
        .instrument(dial9_span!("inner_work", task = "inner", request_id = ~id))
        .await;
}

async fn inner_work(id: u32) {
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // A synchronous span over a CPU-bound scope.
    let span = dial9_span!("post_process", request_id = id);
    let _entered = span.enter();
    std::hint::black_box((0..10_000).sum::<u64>());
}

fn main() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let writer = DiskWriter::single_file("instrument_future_trace.bin").unwrap();
    let (runtime, _guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        let tasks: Vec<_> = (0..10)
            .map(|i| {
                // Top-level span per request, attached to the spawned task.
                tokio::spawn(
                    handle_request(i).instrument(dial9_span!("handle_request", request_id = i)),
                )
            })
            .collect();
        for t in tasks {
            let _ = t.await;
        }
    });

    println!("Trace written to instrument_future_trace.bin");
}

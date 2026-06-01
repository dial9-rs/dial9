//! Memory-only pipeline. No filesystem dependency.
//!
//! When disk is unavailable or unwelcome, `InMemoryWriter` keeps sealed segments in process
//! memory and a delivery processor ships them out. The processor pipeline is
//! identical to disk mode.
//!
//! This example uses a stand-in `PrintProcessor` so it runs anywhere with no
//! credentials. In production, swap it for `S3PipelineUploader` (with the
//! `worker-s3` feature), an HTTP poster, or any user-supplied
//! `SegmentProcessor` that actually delivers the bytes somewhere.
//!
//! Usage:
//!   cargo run --example in_memory_pipeline

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use dial9_tokio_telemetry::background_task::{ProcessError, SegmentData, SegmentProcessor};
use dial9_tokio_telemetry::telemetry::{InMemoryWriter, TelemetryHandle, TracedRuntime};

/// Stand-in delivery processor. Inspects each segment, forwards unchanged.
/// Replace with a real uploader in production.
#[derive(Debug, Default)]
struct PrintProcessor;

impl SegmentProcessor for PrintProcessor {
    fn name(&self) -> &'static str {
        "Print"
    }

    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        println!(
            "segment {}  {} bytes",
            data.segment().index(),
            data.payload().len(),
        );
        Box::pin(async move { Ok(data) })
    }
}

async fn workload() {
    let handle = TelemetryHandle::current();
    let tasks: Vec<_> = (0..32)
        .map(|id| {
            handle.spawn(async move {
                for _ in 0..50 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let mut acc: u64 = 0;
                    for i in 0..50_000u64 {
                        acc = acc.wrapping_add((i ^ id).wrapping_mul(31));
                    }
                    std::hint::black_box(acc);
                }
            })
        })
        .collect();
    for t in tasks {
        let _ = t.await;
    }
}

fn main() -> std::io::Result<()> {
    let writer = InMemoryWriter::new(16 * 1024 * 1024)?; // 16 MB

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_custom_pipeline(|p| p.pipe(PrintProcessor))
        .build_and_start(builder, writer)?;

    runtime.block_on(async {
        println!("Running (no files written to disk)…");
        workload().await;
    });

    guard.graceful_shutdown(Duration::from_secs(5))?;
    println!("Done.");
    Ok(())
}

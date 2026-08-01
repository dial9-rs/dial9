//! Demonstrates tracing spans across async sleep boundaries.
//! Each span is polled multiple times (producing multiple segments in the viewer).
use dial9::tracing_layer::Dial9TracingLayer;
use dial9::{Dial9HandleTokioExt, DiskBuffer, TokioAttachOptions, recorder};
use std::time::Duration;
use tracing_subscriber::prelude::*;

#[tracing::instrument]
async fn handle_request(id: u32) {
    inner_work(id).await;
}

#[tracing::instrument]
async fn inner_work(id: u32) {
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = id;
}

fn main() {
    let writer = DiskBuffer::single_file("tracing_sleep_trace.bin").unwrap();
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

    let subscriber = tracing_subscriber::registry().with(Dial9TracingLayer::new());
    tracing::subscriber::set_global_default(subscriber).expect("failed to set subscriber");

    rt.block_on(async {
        let tasks: Vec<_> = (0..10).map(|i| tokio::spawn(handle_request(i))).collect();
        for t in tasks {
            let _ = t.await;
        }
    });

    // Drop the runtime before draining, so its workers flush their buffers.
    drop(rt);
    recorder.graceful_shutdown(std::time::Duration::from_secs(5));

    println!("Trace written to tracing_sleep_trace.bin");
}

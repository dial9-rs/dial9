use dial9::{AttachedRuntime, DiskBuffer, RecorderTokioExt, TokioAttachOptions};
use std::io;
use std::time::Duration;

const TRACE_DIR: &str = "/tmp/simple-local-traces";

fn fibonacci_recursive(n: u32) -> u32 {
    match n {
        0 => 0,
        1 => 1,
        _ => fibonacci_recursive(n - 1) + fibonacci_recursive(n - 2),
    }
}

async fn do_some_work() {
    // do some work here
    fibonacci_recursive(25);
}

fn my_config() -> io::Result<AttachedRuntime> {
    let writer = DiskBuffer::builder()
        .base_path(TRACE_DIR)
        .max_file_size(10_000_000) // 10MB per file
        .max_total_size(50_000_000) // 50MB total
        .build();
    let recorder = dial9::recorder_or_disabled(writer).build();

    recorder.attach_tokio_runtime_with(
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
        |t| {
            t.worker_threads(2);
        },
    )
}

#[dial9::main(config = my_config)]
async fn main() {
    let mut handles = vec![];

    for _ in 0..100 {
        handles.push(dial9::spawn(do_some_work()));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    for h in handles {
        h.await.unwrap();
    }

    println!("\n✓ Trace written to: {TRACE_DIR}");
    println!("  View with: cargo run -p dial9-viewer -- serve --local-dir {TRACE_DIR}");
    println!("  Then open http://localhost:3000");
}

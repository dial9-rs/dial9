use dial9::Dial9TokioHandle;
use dial9::{DiskBuffer, TracedRuntimeBuilder};
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

fn my_config() -> TracedRuntimeBuilder {
    let writer = DiskBuffer::builder()
        .base_path(TRACE_DIR)
        .max_file_size(10_000_000) // 10MB per file
        .max_total_size(50_000_000) // 50MB total
        .build();
    dial9::recorder_or_disabled(writer, |t| {
        t.worker_threads(2);
    })
    .with_task_tracking(true)
}

#[dial9::main(config = my_config)]
async fn main() {
    let handle = Dial9TokioHandle::current();
    let mut handles = vec![];

    for _ in 0..100 {
        handles.push(handle.spawn(do_some_work()));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    for h in handles {
        h.await.unwrap();
    }

    println!("\n✓ Trace written to: {TRACE_DIR}");
    println!("  View with: cargo run -p dial9-viewer -- serve --local-dir {TRACE_DIR}");
    println!("  Then open http://localhost:3000");
}

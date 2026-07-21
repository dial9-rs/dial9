use dial9::{DiskBuffer, recorder};
use dial9::{RecorderBuilderTokioExt, RecorderPerfExt};
use std::time::Duration;

async fn blocking_task(id: usize) {
    for _ in 0..5 {
        // This blocks the worker thread — should show up as a sched event
        std::thread::sleep(Duration::from_millis(10));
        tokio::task::yield_now().await;
    }
    println!("Task {} done", id);
}

fn main() {
    let writer = DiskBuffer::single_file("blocking_sleep_trace.bin").unwrap();
    let traced = recorder(writer)
        .with_cpu_profiling(Default::default())
        .with_sched_events(Default::default())
        .with_tokio(|t| {
            t.worker_threads(2);
        })
        .with_task_tracking(true)
        .build()
        .unwrap();

    traced.runtime().block_on(async {
        let tasks: Vec<_> = (0..4).map(|i| tokio::spawn(blocking_task(i))).collect();
        for t in tasks {
            let _ = t.await;
        }
    });

    println!("Trace written to blocking_sleep_trace.bin");
}

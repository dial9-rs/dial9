mod common;

use common::decode_file;
use dial9_tokio_telemetry::telemetry::analysis_events::{Dial9Event, WorkerId};
use dial9_tokio_telemetry::telemetry::{DiskWriter, TracedRuntime};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const NUM_CLIENTS: usize = 20;

async fn echo_server(listener: TcpListener, running: Arc<AtomicBool>) {
    while running.load(Ordering::Relaxed) {
        let (mut sock, _) = match listener.accept().await {
            Ok(c) => c,
            Err(_) => break,
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            loop {
                let n = match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if sock.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        });
    }
}

async fn run_client(port: u16, running: Arc<AtomicBool>) -> usize {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let msg = b"hello";
    let mut buf = [0u8; 256];
    let mut count = 0;

    while running.load(Ordering::Relaxed) {
        if stream.write_all(msg).await.is_err() {
            break;
        }
        if stream.read(&mut buf).await.is_err() {
            break;
        }
        count += 1;
    }
    count
}

#[test]
fn overhead_bench_validates() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let num_workers = 4;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers).enable_all();

    let writer = DiskWriter::single_file(&trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .build_and_start(builder, writer)
        .unwrap();

    let running = Arc::new(AtomicBool::new(true));

    let tokio_metrics = runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_running = running.clone();
        tokio::spawn(echo_server(listener, server_running));

        let mut handles = Vec::new();
        for _ in 0..NUM_CLIENTS {
            let r = running.clone();
            handles.push(tokio::spawn(run_client(port, r)));
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
        running.store(false, Ordering::Relaxed);

        let mut total_requests = 0;
        for h in handles {
            total_requests += h.await.unwrap();
        }

        let metrics = tokio::runtime::Handle::current().metrics();
        (metrics, total_requests)
    });

    drop(runtime);
    drop(guard);

    let (metrics, total_requests) = tokio_metrics;
    eprintln!("Total requests processed: {total_requests}");
    eprintln!("Total tasks spawned: {}", metrics.spawned_tasks_count());

    // Read trace via serde path
    let sealed_path = dir.path().join("trace.0.bin");
    let events: Vec<Dial9Event> = decode_file(&sealed_path);

    // Basic validation: poll starts == poll ends
    let poll_starts = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::PollStartEvent(_)))
        .count();
    let poll_ends = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::PollEndEvent(_)))
        .count();
    assert_eq!(
        poll_starts, poll_ends,
        "PollStart ({poll_starts}) != PollEnd ({poll_ends})"
    );

    // All active workers should appear
    let metrics_polls: Vec<u64> = (0..num_workers)
        .map(|w| metrics.worker_poll_count(w))
        .collect();
    for (w, &tokio_polls) in metrics_polls.iter().enumerate() {
        if tokio_polls == 0 {
            continue;
        }
        let trace_polls = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::PollStartEvent(ev) if ev.worker_id == WorkerId(w as u64)))
            .count();
        assert!(
            trace_polls > 0,
            "worker {w} had {tokio_polls} tokio polls but 0 trace PollStart events"
        );
        // Allow small discrepancy
        let diff = (trace_polls as i64 - tokio_polls as i64).unsigned_abs();
        assert!(
            diff <= 30,
            "worker {w}: trace polls ({trace_polls}) vs tokio polls ({tokio_polls}) differ by {diff}"
        );
    }
}

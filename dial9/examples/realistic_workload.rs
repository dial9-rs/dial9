use std::io;
use std::time::Duration;

use dial9::{AttachedRuntime, DiskBuffer, RecorderTokioExt, TokioAttachOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn my_config() -> io::Result<AttachedRuntime> {
    let writer = DiskBuffer::builder()
        .base_path("realistic_trace")
        .max_file_size(64 * 1024 * 1024)
        .max_total_size(256 * 1024 * 1024)
        .build();
    let recorder = dial9::recorder_or_disabled(writer).build();
    recorder.attach_runtime_with(
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
        |t| {
            t.worker_threads(4);
        },
    )
}

async fn cpu_bound_work(n: u64) -> u64 {
    let mut result = 0u64;
    for i in 0..n {
        result = result.wrapping_add(i.wrapping_mul(i));
    }
    result
}

async fn network_server(listener: TcpListener) {
    loop {
        if let Ok((mut socket, _)) = listener.accept().await {
            dial9::spawn(async move {
                let mut buf = [0u8; 1024];
                if let Ok(n) = socket.read(&mut buf).await {
                    let result = cpu_bound_work(10000).await;
                    let response = format!("Processed {} bytes, result: {}\n", n, result);
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            });
        }
    }
}

async fn network_client(port: u16, id: usize) {
    tokio::time::sleep(Duration::from_millis(100)).await;

    for i in 0..5000 {
        if let Ok(mut stream) = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await
        {
            let msg = format!("Client {} request {}", id, i);
            let _ = stream.write_all(msg.as_bytes()).await;

            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

async fn mixed_workload(port: u16) {
    let clients: Vec<_> = (0..5)
        .map(|i| dial9::spawn(network_client(port, i)))
        .collect();

    let cpu_tasks: Vec<_> = (0..3)
        .map(|_| {
            dial9::spawn(async {
                for _ in 0..10 {
                    cpu_bound_work(50000).await;
                    tokio::task::yield_now().await;
                }
            })
        })
        .collect();

    for client in clients {
        let _ = client.await;
    }
    for task in cpu_tasks {
        let _ = task.await;
    }
}

#[dial9::main(config = my_config)]
async fn main() {
    println!("Running realistic workload...");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    dial9::spawn(network_server(listener));

    tokio::time::timeout(Duration::from_secs(5), mixed_workload(port))
        .await
        .ok();

    println!("Trace written to realistic_trace/trace.*.bin");
}

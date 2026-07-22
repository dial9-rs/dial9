use std::io;
use std::time::Duration;

use dial9::{AttachedRuntime, DiskBuffer, RecorderTokioExt, TokioAttachOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn my_config() -> io::Result<AttachedRuntime> {
    let writer = DiskBuffer::builder()
        .base_path("long_trace")
        .max_file_size(64 * 1024 * 1024)
        .max_total_size(256 * 1024 * 1024)
        .build();
    let recorder = dial9::recorder_or_disabled(writer);
    recorder.attach_runtime_with(
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
        |t| {
            t.worker_threads(4);
        },
    )
}

async fn cpu_work(iterations: u64) -> u64 {
    let mut result = 0u64;
    for i in 0..iterations {
        result = result.wrapping_add(i.wrapping_mul(i));
    }
    result
}

async fn echo_server(listener: TcpListener) {
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        dial9::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                match socket.read(&mut buf).await {
                    Ok(0) => return,
                    Ok(n) => {
                        cpu_work(5000).await;
                        let _ = socket.write_all(&buf[..n]).await;
                    }
                    Err(_) => return,
                }
            }
        });
    }
}

async fn chatty_client(port: u16, id: usize) {
    tokio::time::sleep(Duration::from_millis(200)).await;
    loop {
        if let Ok(mut stream) = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await
        {
            for i in 0u64.. {
                let msg = format!("client {} msg {}", id, i);
                if stream.write_all(msg.as_bytes()).await.is_err() {
                    break;
                }
                let mut buf = [0u8; 1024];
                if stream.read(&mut buf).await.is_err() {
                    break;
                }
                let delay = match id % 3 {
                    0 => 10,
                    1 => 50,
                    _ => 200,
                };
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn background_cpu_bursts() {
    loop {
        for _ in 0..20 {
            dial9::spawn(async { cpu_work(100_000).await });
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn periodic_yielder() {
    loop {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[dial9::main(config = my_config)]
async fn main() {
    let duration_secs = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30u64);

    println!("Running workload for {}s...", duration_secs);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    dial9::spawn(echo_server(listener));
    for i in 0..8 {
        dial9::spawn(chatty_client(port, i));
    }
    dial9::spawn(background_cpu_bursts());
    dial9::spawn(periodic_yielder());

    tokio::time::sleep(Duration::from_secs(duration_secs)).await;
    println!("Done. Trace written to long_trace/trace.*.bin");
}

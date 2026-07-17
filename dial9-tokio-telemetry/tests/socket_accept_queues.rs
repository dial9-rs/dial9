#![cfg(all(target_os = "linux", feature = "linux-socket"))]

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
use dial9_tokio_telemetry::telemetry::{
    MemoryBuffer, RecorderBuilderTokioExt, RecorderPerfExt, SocketAcceptQueuesConfig, recorder,
};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

#[test]
fn traced_runtime_records_socket_accept_queue_snapshot() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let local_addr = listener.local_addr().unwrap();
    let client = TcpStream::connect(local_addr).unwrap();

    let (capture, batches) = capture_processor();
    let traced = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_socket_accept_queues(
            SocketAcceptQueuesConfig::builder()
                .sample_interval(Duration::ZERO)
                .build(),
        )
        .with_tokio(|t| {
            t.worker_threads(1);
        })
        .with_custom_pipeline(|p| p.pipe(capture))
        .build()
        .unwrap();

    traced.graceful_shutdown(Duration::from_secs(1));
    drop(client);
    drop(listener);

    let batches = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&batches);
    let snapshots: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Dial9Event::TcpAcceptQueueEvent(event) => Some(event),
            _ => None,
        })
        .collect();

    let snapshot = snapshots
        .iter()
        .find(|event| event.local_port == local_addr.port())
        .unwrap_or_else(|| panic!("expected snapshot for listener port {local_addr}"));

    assert!(snapshot.timestamp_ns > 0);
    assert!(snapshot.socket_cookie > 0);
    assert!(snapshot.socket_inode > 0);
    assert_eq!(snapshot.ip_version, 4);
    assert_eq!(snapshot.local_addr, "127.0.0.1");
    assert_eq!(snapshot.local_port, local_addr.port());
    assert!(snapshot.pending_connections >= 1);
    assert!(snapshot.backlog_limit >= snapshot.pending_connections);
}

#[test]
fn traced_runtime_does_not_record_socket_accept_queues_by_default() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let local_addr = listener.local_addr().unwrap();
    let client = TcpStream::connect(local_addr).unwrap();

    let (capture, batches) = capture_processor();
    let traced = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_tokio(|t| {
            t.worker_threads(1);
        })
        .with_custom_pipeline(|p| p.pipe(capture))
        .build()
        .unwrap();

    traced.graceful_shutdown(Duration::from_secs(1));
    drop(client);
    drop(listener);

    let batches = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&batches);

    assert!(
        events
            .iter()
            .all(|event| !matches!(event, Dial9Event::TcpAcceptQueueEvent(_))),
        "socket accept queue snapshots should be opt-in"
    );
}

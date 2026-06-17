//! End-to-end tests for on-trigger pipeline runs: segments buffer in the
//! ring until the application requests a dump, then upload to S3 with
//! `dump-id` tagging and a per-dump manifest.
#![cfg(feature = "worker-s3")]

mod fake_s3;

use dial9_tokio_telemetry::background_task::s3::S3Config;
use dial9_tokio_telemetry::telemetry::{DiskWriter, TracedRuntime};
use fake_s3::fake_s3_client;
use std::future::IntoFuture;
use std::time::{Duration, Instant};

fn test_s3_config() -> S3Config {
    S3Config::builder()
        .bucket("test-bucket")
        .prefix("traces")
        .service_name("test-svc")
        .instance_path("us-east-1/test-host")
        .boot_id("test-boot-id")
        .region("us-east-1")
        .build()
}

fn assertion_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Generate enough events to rotate several segments.
fn run_workload(runtime: &tokio::runtime::Runtime) {
    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..50 {
            handles.push(tokio::spawn(async {
                tokio::task::yield_now().await;
            }));
        }
        for h in handles {
            let _ = h.await;
        }
    });
}

/// Block until the flush thread has sealed at least one segment
/// (`trace.<n>.bin` on disk), so a look-back dump has something to capture.
fn wait_for_sealed_segment(trace_dir: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let sealed = std::fs::read_dir(trace_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".bin") && !name.ends_with("trace.bin")
            });
        if sealed {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no segment sealed within 10s; cannot exercise look-back dump"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The headline behavior: nothing uploads until the dump, then the dump
/// uploads the buffered segments, tags them with `dump-id`, and writes a
/// manifest listing exactly the produced keys.
#[test]
fn nothing_uploads_until_dump_then_manifest_indexes_it() {
    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");
    std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

    let client = fake_s3_client(s3_root.path());
    let writer = DiskWriter::new(&trace_path, 512, 50 * 1024).unwrap();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_s3_uploader(test_s3_config())
        .with_s3_client(client.clone())
        .with_worker_poll_interval(Duration::from_millis(50))
        .with_dump_trigger(|_| {})
        .build_and_start(builder, writer)
        .unwrap();

    let trigger = guard.handle().dump_trigger().expect("trigger wired");

    run_workload(&runtime);
    wait_for_sealed_segment(trace_dir.path());
    // Plenty of poll intervals: a continuous-mode worker would have
    // uploaded by now.
    std::thread::sleep(Duration::from_millis(400));

    let check_rt = assertion_runtime();
    let pre_trigger = check_rt.block_on(async {
        client
            .list_objects_v2()
            .bucket("test-bucket")
            .send()
            .await
            .unwrap()
            .contents
            .unwrap_or_default()
    });
    assert!(
        pre_trigger.is_empty(),
        "no uploads before the trigger, got {} objects",
        pre_trigger.len()
    );

    let receipt = check_rt
        .block_on(async {
            trigger
                .dump_current_data()
                .with_metadata("reason", "e2e-test")
                .await
        })
        .unwrap();
    assert!(receipt.segments_processed >= 1, "buffered segments dumped");
    let manifest_key = receipt.manifest_key.clone().expect("S3 manifest written");
    assert_eq!(
        manifest_key,
        format!("traces/dumps/{}.json", receipt.dump_id)
    );

    // The manifest is discoverable by prefix listing and indexes the dump.
    let manifest: serde_json::Value = check_rt.block_on(async {
        let listed = client
            .list_objects_v2()
            .bucket("test-bucket")
            .prefix("traces/dumps/")
            .send()
            .await
            .unwrap()
            .contents
            .unwrap_or_default();
        assert_eq!(listed.len(), 1, "one manifest under dumps/");
        assert_eq!(listed[0].key().unwrap(), manifest_key);

        let body = client
            .get_object()
            .bucket("test-bucket")
            .key(&manifest_key)
            .send()
            .await
            .unwrap()
            .body
            .collect()
            .await
            .unwrap()
            .into_bytes();
        serde_json::from_slice(&body).unwrap()
    });

    assert_eq!(manifest["dump_id"], receipt.dump_id.to_string());
    assert_eq!(
        manifest["segments_processed"],
        serde_json::json!(receipt.segments_processed)
    );
    assert_eq!(manifest["metadata"]["reason"], "e2e-test");

    let segments = manifest["segments"].as_array().unwrap();
    assert_eq!(segments.len(), receipt.segments_processed);
    check_rt.block_on(async {
        for key in segments {
            let head = client
                .head_object()
                .bucket("test-bucket")
                .key(key.as_str().unwrap())
                .send()
                .await
                .expect("manifest lists a real object");
            let meta = head.metadata().unwrap();
            assert_eq!(meta.get("dump-id").unwrap(), &receipt.dump_id.to_string());
            assert_eq!(meta.get("reason").unwrap(), "e2e-test");
        }
    });

    drop(runtime);
    guard.graceful_shutdown(Duration::from_secs(1)).unwrap();
}

/// A look-forward window keeps the dump open and captures segments sealed
/// after the trigger; the receipt resolves only after the deadline.
#[test]
fn lookforward_dump_captures_post_trigger_segments() {
    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");
    std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

    let client = fake_s3_client(s3_root.path());
    let writer = DiskWriter::new(&trace_path, 512, 50 * 1024).unwrap();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_s3_uploader(test_s3_config())
        .with_s3_client(client.clone())
        .with_worker_poll_interval(Duration::from_millis(50))
        .with_dump_trigger(|_| {})
        .build_and_start(builder, writer)
        .unwrap();

    let trigger = guard.handle().dump_trigger().expect("trigger wired");

    // Trigger before producing anything; the forward window collects the
    // segments the workload seals.
    let lookforward = Duration::from_secs(5);
    let fut = trigger
        .dump_time_range(Duration::from_secs(1), lookforward)
        .into_future();
    let triggered = Instant::now();

    run_workload(&runtime);

    let check_rt = assertion_runtime();
    let receipt = check_rt.block_on(fut).unwrap();
    assert!(
        triggered.elapsed() >= lookforward,
        "receipt resolves only after the forward deadline"
    );
    assert!(receipt.segments_processed >= 1, "forward window captured");

    let manifest: serde_json::Value = check_rt.block_on(async {
        let body = client
            .get_object()
            .bucket("test-bucket")
            .key(receipt.manifest_key.as_ref().unwrap())
            .send()
            .await
            .unwrap()
            .body
            .collect()
            .await
            .unwrap()
            .into_bytes();
        serde_json::from_slice(&body).unwrap()
    });
    assert_eq!(
        manifest["segments"].as_array().unwrap().len(),
        receipt.segments_processed
    );

    drop(runtime);
    guard.graceful_shutdown(Duration::from_secs(1)).unwrap();
}

/// Off-S3 pipelines dump to disk: the receipt works, but there is no
/// manifest (`manifest_key` is `None`).
#[test]
fn off_s3_pipeline_dumps_without_manifest() {
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    let writer = DiskWriter::new(&trace_path, 512, 50 * 1024).unwrap();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_custom_pipeline(|p| p.gzip().write_back())
        .with_worker_poll_interval(Duration::from_millis(50))
        .with_dump_trigger(|_| {})
        .build_and_start(builder, writer)
        .unwrap();

    let trigger = guard.handle().dump_trigger().expect("trigger wired");

    run_workload(&runtime);
    wait_for_sealed_segment(trace_dir.path());

    let check_rt = assertion_runtime();
    let receipt = check_rt
        .block_on(async { trigger.dump_current_data().await })
        .unwrap();
    assert!(receipt.segments_processed >= 1);
    assert!(receipt.manifest_key.is_none(), "no manifest off S3");

    // write_back left gzipped segments in the trace directory.
    let gz_count = std::fs::read_dir(trace_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().to_string_lossy().ends_with(".bin.gz"))
        .count();
    assert!(gz_count >= 1, "dumped segments written back to disk");

    drop(runtime);
    guard.graceful_shutdown(Duration::from_secs(1)).unwrap();
}

/// Shutting down with a look-forward dump still open resolves the awaited
/// handle with a truncated Ok receipt covering what landed.
#[test]
fn shutdown_truncates_open_lookforward_dump() {
    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");
    std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

    let client = fake_s3_client(s3_root.path());
    let writer = DiskWriter::new(&trace_path, 512, 50 * 1024).unwrap();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_s3_uploader(test_s3_config())
        .with_s3_client(client)
        .with_worker_poll_interval(Duration::from_millis(50))
        .with_dump_trigger(|_| {})
        .build_and_start(builder, writer)
        .unwrap();

    let trigger = guard.handle().dump_trigger().expect("trigger wired");

    // Hour-long forward window, then shut down long before the deadline.
    let fut = trigger
        .dump_time_range(Duration::from_secs(1), Duration::from_secs(3600))
        .into_future();
    run_workload(&runtime);

    drop(runtime);
    guard.graceful_shutdown(Duration::from_secs(2)).unwrap();

    let receipt = assertion_runtime()
        .block_on(fut)
        .expect("truncated Ok receipt at shutdown");
    assert!(receipt.segments_processed >= 1, "captured before shutdown");
}

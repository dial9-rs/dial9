//! Integration tests: in-process worker lifecycle and end-to-end S3 upload.
#![cfg(feature = "worker-s3")]

mod common;
mod fake_s3;

use aws_config::Region;
use aws_sdk_s3::Client;
use common::{fast_sealing_writer, wait_for_sealed_segment};
use dial9_destinations_s3::S3Config;
use dial9_tokio_telemetry::telemetry::{
    DiskBuffer, RecorderPipelineExt, RecorderS3ClientExt, TokioAttachOptions, recorder, spawn,
};
use fake_s3::{
    fake_s3_client, fake_s3_client_always_failing, fake_s3_client_flaky, fake_s3_client_hanging,
    fake_s3_client_with_region,
};
use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Create a dummy S3 config + client for tests.
fn dummy_s3(s3_root: &std::path::Path) -> (S3Config, aws_sdk_s3::Client) {
    std::fs::create_dir_all(s3_root.join("dummy-bucket")).unwrap();
    let s3_config = S3Config::builder()
        .bucket("dummy-bucket")
        .service_name("test")
        .instance_path("test")
        .region("us-east-1")
        .build();
    (s3_config, fake_s3_client(s3_root))
}

#[test]
fn worker_thread_starts_and_stops_cleanly() {
    let trace_dir = tempfile::tempdir().unwrap();
    let s3_root = tempfile::tempdir().unwrap();

    let writer = DiskBuffer::builder()
        .base_path(trace_dir.path())
        .max_file_size(1024)
        .max_total_size(10 * 1024)
        .build()
        .unwrap();
    let (s3_config, client) = dummy_s3(s3_root.path());

    let recorder = recorder(writer)
        .worker_poll_interval(std::time::Duration::from_millis(50))
        .with_s3_uploader_client(s3_config.clone(), client.clone())
        .build();
    let rt = common::attach(&recorder, 1, TokioAttachOptions::default());

    rt.block_on(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    });

    drop(rt);
    drop(recorder);
}

#[test]
fn client_future_runs_once_when_pipeline_starts() {
    let trace_dir = tempfile::tempdir().unwrap();
    let s3_root = tempfile::tempdir().unwrap();
    std::fs::create_dir(s3_root.path().join("future-bucket")).unwrap();

    let writer = DiskBuffer::builder()
        .base_path(trace_dir.path())
        .max_file_size(1024)
        .max_total_size(10 * 1024)
        .build()
        .unwrap();
    let s3_config = S3Config::builder()
        .bucket("future-bucket")
        .prefix("traces")
        .service_name("test")
        .instance_path("test")
        .region("us-east-1")
        .build();
    let client = fake_s3_client(s3_root.path());
    let client_for_future = client.clone();
    let future_calls = Arc::new(AtomicUsize::new(0));
    let future_calls_inner = future_calls.clone();

    let recorder = recorder(writer)
        .worker_poll_interval(Duration::from_millis(50))
        .with_s3_uploader_client_future(s3_config, async move {
            tokio::runtime::Handle::try_current()
                .expect("client future must run inside a Tokio runtime");
            assert_eq!(
                std::thread::current().name(),
                Some("dial9-worker"),
                "client future must run on the pipeline worker"
            );
            future_calls_inner.fetch_add(1, Ordering::SeqCst);
            client_for_future
        })
        .with_dump_trigger(|_| {})
        .build();
    let rt = common::attach(&recorder, 1, TokioAttachOptions::default());

    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while future_calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("client future should run when the pipeline starts");
    });
    assert_eq!(future_calls.load(Ordering::SeqCst), 1);

    let trigger = recorder.handle().dump_trigger().expect("trigger wired");
    let receipt = rt
        .block_on(async { trigger.dump_current_data().await })
        .expect("empty dump resolves");
    assert_eq!(receipt.segments_processed, 0);
    let manifest_key = receipt
        .manifest_key
        .expect("empty dump writes an S3 manifest");

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

    assert_eq!(future_calls.load(Ordering::SeqCst), 1);
    assert!(
        s3_root
            .path()
            .join("future-bucket")
            .join(manifest_key)
            .is_file(),
        "manifest should be uploaded with the asynchronously built client"
    );
}

#[test]
fn graceful_shutdown_seals_segments() {
    let trace_dir = tempfile::tempdir().unwrap();
    let s3_root = tempfile::tempdir().unwrap();

    let writer = DiskBuffer::builder()
        .base_path(trace_dir.path())
        .max_file_size(1024)
        .max_total_size(10 * 1024)
        .build()
        .unwrap();
    let (s3_config, client) = dummy_s3(s3_root.path());

    let recorder = recorder(writer)
        .worker_poll_interval(std::time::Duration::from_millis(50))
        .with_s3_uploader_client(s3_config.clone(), client.clone())
        .build();
    let rt = common::attach(&recorder, 1, TokioAttachOptions::default());

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

    let active_files: Vec<_> = std::fs::read_dir(trace_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "active"))
        .collect();
    assert!(active_files.is_empty(), "no .active files should remain");
}

/// End-to-end: attached runtime → DiskBuffer → rotation → worker uploads to
/// s3s → download from s3s → decompress → parse with serde decoder → verify
/// real trace events are present.
#[test]
fn end_to_end_trace_to_s3_roundtrip() {
    use dial9_trace_format::decoder::Decoder;

    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();

    // Create the bucket directory for s3s-fs
    std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

    let client = fake_s3_client(s3_root.path());

    // Small max_file_size to force rotation quickly
    let mut writer = DiskBuffer::builder()
        .base_path(trace_dir.path())
        .max_file_size(512)
        .max_total_size(50 * 1024)
        .build()
        .unwrap();
    writer.update_segment_metadata(vec![("custom-metadata".to_string(), "value".to_string())]);

    let s3_config = S3Config::builder()
        .bucket("test-bucket")
        .prefix("traces")
        .service_name("test-svc")
        .instance_path("us-east-1/test-host")
        .region("us-east-1")
        .build();

    let recorder = recorder(writer)
        .worker_poll_interval(std::time::Duration::from_millis(50))
        .with_s3_uploader_client(s3_config.clone(), client.clone())
        .build();
    let rt = common::attach(
        &recorder,
        2,
        TokioAttachOptions::builder()
            .runtime_name("test-runtime")
            .build(),
    );

    // Run a workload that generates enough events to trigger rotation.
    rt.block_on(async {
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

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

    // List objects in the bucket — should have at least one uploaded segment
    let list_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let objects = list_rt.block_on(async {
        let resp = client
            .list_objects_v2()
            .bucket("test-bucket")
            .prefix("traces/")
            .send()
            .await
            .unwrap();
        resp.contents.unwrap_or_default()
    });

    assert!(
        !objects.is_empty(),
        "expected at least one object in S3, got none"
    );

    // Download every uploaded segment, decompressed.
    let keys: Vec<String> = objects
        .iter()
        .map(|o| o.key().unwrap().to_string())
        .collect();

    let segments: Vec<Vec<u8>> = list_rt.block_on(async {
        let mut out = Vec::new();
        for key in &keys {
            let resp = client
                .get_object()
                .bucket("test-bucket")
                .key(key)
                .send()
                .await
                .unwrap();

            let body = resp.body.collect().await.unwrap().into_bytes();

            // Decompress gzip
            let mut decoder = GzDecoder::new(&body[..]);
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed).unwrap();
            out.push(decompressed);
        }
        out
    });

    #[derive(Debug, serde::Deserialize)]
    #[allow(dead_code, clippy::enum_variant_names)]
    #[serde(tag = "event")]
    enum S3Event {
        SegmentMetadataEvent {
            entries: HashMap<String, String>,
        },
        PollStartEvent {
            timestamp_ns: u64,
        },
        PollEndEvent {
            timestamp_ns: u64,
        },
        WorkerParkEvent {
            timestamp_ns: u64,
        },
        #[serde(other)]
        Other,
    }

    let mut all_events = Vec::new();
    for segment in &segments {
        let mut dec = Decoder::new(segment).unwrap();
        dec.for_each_event(|raw| {
            let ev: S3Event = raw.deserialize().expect("deserialize");
            all_events.push(ev);
        })
        .unwrap();
    }

    let metadata: HashMap<String, String> = all_events
        .iter()
        .filter_map(|e| match e {
            S3Event::SegmentMetadataEvent { entries } => Some(entries.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(metadata["bucket"], "test-bucket");
    assert_eq!(metadata["service_name"], "test-svc");
    // Worker IDs resolve on a worker's first park or poll, so which of the two
    // appear is the scheduler's call. `attach_propagates_second_runtime_metadata`
    // covers the full mapping.
    let workers = &metadata["runtime.test-runtime"];
    let mut worker_ids: Vec<u64> = workers
        .split(',')
        .map(|id| id.parse().expect("numeric worker id"))
        .collect();
    let claimed = worker_ids.len();
    worker_ids.dedup();
    assert!(
        worker_ids.len() == claimed && (1..=2).contains(&claimed),
        "expected 1 or 2 distinct ids from this runtime's block, got {workers:?}"
    );
    assert!(
        worker_ids.iter().all(|id| *id < 2),
        "worker ids should fall in this runtime's block, got {workers:?}"
    );
    assert_eq!(metadata["custom-metadata"], "value");
    let runtime_events: Vec<_> = all_events
        .iter()
        .filter(|e| !matches!(e, S3Event::Other | S3Event::SegmentMetadataEvent { .. }))
        .collect();
    assert!(
        !runtime_events.is_empty(),
        "expected trace events in downloaded segment, got none"
    );

    // Should contain at least some PollStart/PollEnd or WorkerPark events
    assert!(
        runtime_events.iter().any(|e| matches!(
            e,
            S3Event::PollStartEvent { .. }
                | S3Event::PollEndEvent { .. }
                | S3Event::WorkerParkEvent { .. }
        )),
        "expected runtime events with timestamps"
    );
}

/// Verify that the worker auto-detects the bucket region from HeadBucket
/// and corrects the client, even when the initial client has the wrong region.
#[test]
fn region_auto_detection_corrects_wrong_client_region() {
    use dial9_trace_format::decoder::Decoder;

    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();

    std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

    // Client for the worker: wrong region, must auto-detect.
    let client = fake_s3_client_with_region(s3_root.path(), "eu-west-1");

    // Separate client for test verification: correct region.
    let verify_client = Client::from_conf(
        client
            .config()
            .to_builder()
            .region(Region::from_static("eu-west-1"))
            .build(),
    );

    let writer = DiskBuffer::builder()
        .base_path(trace_dir.path())
        .max_file_size(512)
        .max_total_size(50 * 1024)
        .build()
        .unwrap();

    // Do NOT set .region() — force auto-detection.
    let s3_config = S3Config::builder()
        .bucket("test-bucket")
        .prefix("traces")
        .service_name("test-svc")
        .instance_path("test-host")
        .build();

    let recorder = recorder(writer)
        .worker_poll_interval(std::time::Duration::from_millis(50))
        .with_s3_uploader_client(s3_config.clone(), client.clone())
        .build();
    let rt = common::attach(&recorder, 2, TokioAttachOptions::default());

    rt.block_on(async {
        for _ in 0..50 {
            tokio::spawn(async { tokio::task::yield_now().await });
        }
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

    // Verify objects were uploaded despite the wrong initial region.
    let list_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let objects = list_rt.block_on(async {
        let resp = verify_client
            .list_objects_v2()
            .bucket("test-bucket")
            .prefix("traces/")
            .send()
            .await
            .unwrap();
        resp.contents.unwrap_or_default()
    });

    assert!(
        !objects.is_empty(),
        "expected uploads to succeed after region auto-detection"
    );

    // Download and verify the trace is parseable.
    let first_key = objects[0].key().unwrap().to_string();
    let downloaded_path = trace_dir.path().join("downloaded.bin");

    list_rt.block_on(async {
        let resp = verify_client
            .get_object()
            .bucket("test-bucket")
            .key(&first_key)
            .send()
            .await
            .unwrap();
        let body = resp.body.collect().await.unwrap().into_bytes();
        let mut decoder = GzDecoder::new(&body[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        std::fs::write(&downloaded_path, &decompressed).unwrap();
    });

    let trace_data = std::fs::read(&downloaded_path).unwrap();
    let mut dec = Decoder::new(&trace_data).unwrap();
    let mut event_count = 0usize;
    dec.for_each_event(|_raw| {
        event_count += 1;
    })
    .unwrap();
    assert!(
        event_count > 0,
        "expected trace events after region correction"
    );
}

/// Stress test: generate high-throughput trace data against a local S3 server
/// and verify invariants.
///
/// Invariants checked:
/// 1. Every uploaded object is valid gzip containing parseable trace events
/// 2. Compression ratio is sane (compressed < uncompressed)
/// 3. Segment indices are sorted with no duplicates (gaps expected from eviction)
/// 4. Total events across all segments is non-trivial
/// 5. Worker metrics match: success count == object count, sizes non-zero, stages succeed
///
/// Note: some segments may remain on disk after shutdown — the worker drains
/// what it can within the timeout but won't block the application. On restart,
/// the worker would pick up any leftover segments.
#[test]
fn stress_test_all_segments_uploaded_and_valid() {
    use dial9_trace_format::decoder::Decoder;

    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();

    std::fs::create_dir(s3_root.path().join("stress-bucket")).unwrap();
    let client = fake_s3_client(s3_root.path());

    // Small segments to force rotations, but not so many that drain takes forever.
    let segment_size = 64 * 1024;
    let total_size = 2 * 1024 * 1024; // 2 MB disk budget
    let writer = DiskBuffer::builder()
        .base_path(trace_dir.path())
        .max_file_size(segment_size)
        .max_total_size(total_size)
        .build()
        .unwrap();

    let s3_config = S3Config::builder()
        .bucket("stress-bucket")
        .prefix("traces")
        .service_name("stress-svc")
        .instance_path("test-host")
        .region("us-east-1")
        .build();

    let metrique_writer::test_util::TestEntrySink {
        inspector,
        sink: metrics_sink,
    } = metrique_writer::test_util::test_entry_sink();

    let recorder = recorder(writer)
        .worker_poll_interval(std::time::Duration::from_millis(50))
        .metrics_sink(metrics_sink)
        .with_s3_uploader_client(s3_config.clone(), client.clone())
        .build();
    let rt = common::attach(
        &recorder,
        4,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    );

    // Generate load for 1 second — enough to produce several segments at 64KB each.
    rt.block_on(async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        loop {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            let mut joins = Vec::with_capacity(100);
            for _ in 0..100 {
                joins.push(spawn(async {
                    tokio::task::yield_now().await;
                    tokio::task::yield_now().await;
                }));
            }
            for j in joins {
                let _ = j.await;
            }
        }

        // Graceful shutdown: seals final segment, worker drains what it can
        // within the timeout. Some segments may remain — the worker is a "good
        // citizen" that loses data rather than blocking the application.
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

    // List all uploaded objects.
    let list_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let objects = list_rt.block_on(async {
        let mut objects = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut req = client
                .list_objects_v2()
                .bucket("stress-bucket")
                .prefix("traces/");
            if let Some(token) = continuation.take() {
                req = req.continuation_token(token);
            }
            let resp = req.send().await.unwrap();
            for obj in resp.contents() {
                objects.push(obj.key().unwrap().to_string());
            }
            if resp.is_truncated() == Some(true) {
                continuation = resp.next_continuation_token().map(|s| s.to_string());
            } else {
                break;
            }
        }
        objects
    });

    assert!(
        !objects.is_empty(),
        "expected at least one uploaded segment, got 0",
    );

    // Download and validate every object.
    let mut total_events = 0usize;

    for key in &objects {
        assert!(key.ends_with(".bin.gz"), "unexpected key suffix: {key}");

        let (decompressed, compressed_size) = list_rt.block_on(async {
            let resp = client
                .get_object()
                .bucket("stress-bucket")
                .key(key)
                .send()
                .await
                .unwrap();
            let body = resp.body.collect().await.unwrap().into_bytes();
            let compressed_size = body.len() as u64;

            let mut decoder = GzDecoder::new(&body[..]);
            let mut decompressed = Vec::new();
            decoder
                .read_to_end(&mut decompressed)
                .unwrap_or_else(|e| panic!("failed to decompress {key}: {e}"));
            (decompressed, compressed_size)
        });

        // Compression ratio is sane.
        let uncompressed_size = decompressed.len() as u64;
        assert!(
            compressed_size < uncompressed_size,
            "compressed ({compressed_size}) should be smaller than uncompressed ({uncompressed_size}) for {key}"
        );

        // Parseable trace events.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &decompressed).unwrap();
        let trace_data = std::fs::read(tmp.path()).unwrap();
        let mut dec = Decoder::new(&trace_data).expect("valid trace header");
        let mut segment_events = 0usize;
        dec.for_each_event(|_raw| {
            segment_events += 1;
        })
        .unwrap();
        assert!(segment_events > 0, "expected events in {key}, got none");
        total_events += segment_events;
    }

    // Invariant 5: non-trivial total event count.
    assert!(
        total_events > 1000,
        "expected many events across all segments, got {total_events}"
    );

    // Invariant 4: segment indices are sorted with no duplicates.
    // Gaps are expected when the disk budget evicts segments faster than the
    // worker can upload them.
    let mut segment_indices: Vec<u32> = objects
        .iter()
        .filter_map(|key| {
            let filename = key.rsplit('/').next()?;
            let stem = filename.strip_suffix(".bin.gz")?;
            let idx_str = stem.rsplit('-').next()?;
            idx_str.parse().ok()
        })
        .collect();
    segment_indices.sort();
    let before_dedup = segment_indices.len();
    segment_indices.dedup();
    assert_eq!(
        segment_indices.len(),
        before_dedup,
        "segment indices should have no duplicates, but found {} duplicates",
        before_dedup - segment_indices.len(),
    );

    // Invariant 6: worker metrics are consistent with uploaded objects.
    let entries = inspector.entries();
    let successes: Vec<_> = entries
        .iter()
        .filter(|e| e.metrics.get("Success").is_some_and(|v| *v == true))
        .collect();
    assert_eq!(
        successes.len(),
        objects.len(),
        "metric success count ({}) should match uploaded object count ({})",
        successes.len(),
        objects.len(),
    );
    for entry in &successes {
        let compressed = entry.metrics["CompressedSize"].as_u64();
        let uncompressed = entry.metrics["UncompressedSize"].as_u64();
        assert!(compressed > 0, "CompressedSize should be non-zero");
        assert!(uncompressed > 0, "UncompressedSize should be non-zero");
        assert!(
            compressed < uncompressed,
            "compressed ({compressed}) should be < uncompressed ({uncompressed})"
        );
        assert!(
            entry.metrics["Gzip.Success"].as_u64() == 1,
            "Gzip stage should succeed"
        );
        assert!(
            entry.metrics["S3Upload.Success"].as_u64() == 1,
            "S3Upload stage should succeed"
        );
    }
}

/// When S3 hangs permanently (put_object never returns), graceful_shutdown
/// must still complete within its timeout instead of blocking forever.
#[test]
fn graceful_shutdown_completes_when_s3_hangs() {
    let trace_dir = tempfile::tempdir().unwrap();
    let s3_root = tempfile::tempdir().unwrap();

    std::fs::create_dir_all(s3_root.path().join("hang-bucket")).unwrap();
    let client = fake_s3_client_hanging(s3_root.path());

    // Small segments to force rotation quickly.
    let writer = DiskBuffer::builder()
        .base_path(trace_dir.path())
        .max_file_size(512)
        .max_total_size(50 * 1024)
        .build()
        .unwrap();

    let s3_config = S3Config::builder()
        .bucket("hang-bucket")
        .prefix("traces")
        .service_name("test-svc")
        .instance_path("test-host")
        .region("us-east-1")
        .build();

    // graceful_shutdown should complete within the timeout, not hang forever.
    let shutdown_timeout = std::time::Duration::from_secs(3);
    let recorder = recorder(writer)
        .worker_poll_interval(std::time::Duration::from_millis(50))
        .with_s3_uploader_client(s3_config.clone(), client.clone())
        .build();
    let rt = common::attach(&recorder, 2, TokioAttachOptions::default());

    // Generate trace data on the runtime, then let the worker pick it up.
    rt.block_on(async {
        for _ in 0..50 {
            spawn(async { tokio::task::yield_now().await });
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    let test_deadline = std::time::Duration::from_secs(10);

    // Drain on a worker thread so a hung shutdown can't wedge the test.
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        drop(rt);
        recorder.graceful_shutdown(Duration::from_secs(5));
        let _ = tx.send(());
    });

    let result = rx.recv_timeout(test_deadline);

    // If the test-level timeout fires, graceful_shutdown hung — that's the bug.
    assert!(
        result.is_ok(),
        "graceful_shutdown hung beyond {test_deadline:?} — it did not respect its own {shutdown_timeout:?} timeout"
    );

    let _ = t.join();
}

/// Stress test with injected S3 failures.
///
/// Same as `stress_test_all_segments_uploaded_and_valid` but every 3rd
/// S3 operation returns InternalError. The worker must handle failures
/// gracefully and still upload what it can.
#[test]
fn stress_test_with_s3_failures() {
    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();

    std::fs::create_dir(s3_root.path().join("flaky-bucket")).unwrap();
    let client = fake_s3_client_flaky(s3_root.path(), "us-east-1", 3);

    let segment_size = 64 * 1024;
    let total_size = 2 * 1024 * 1024;
    let writer = DiskBuffer::builder()
        .base_path(trace_dir.path())
        .max_file_size(segment_size)
        .max_total_size(total_size)
        .build()
        .unwrap();

    let s3_config = S3Config::builder()
        .bucket("flaky-bucket")
        .prefix("traces")
        .service_name("flaky-svc")
        .instance_path("test-host")
        .region("us-east-1")
        .build();

    let recorder = recorder(writer)
        .worker_poll_interval(std::time::Duration::from_millis(50))
        .with_s3_uploader_client(s3_config.clone(), client.clone())
        .build();
    let rt = common::attach(
        &recorder,
        4,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    );

    rt.block_on(async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        loop {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            let mut joins = Vec::with_capacity(100);
            for _ in 0..100 {
                joins.push(spawn(async {
                    tokio::task::yield_now().await;
                    tokio::task::yield_now().await;
                }));
            }
            for j in joins {
                let _ = j.await;
            }
        }
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

    // Verify some objects landed in S3 despite failures.
    let verify_client = fake_s3_client(s3_root.path());
    let list_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let object_count = list_rt.block_on(async {
        let resp = verify_client
            .list_objects_v2()
            .bucket("flaky-bucket")
            .prefix("traces/")
            .send()
            .await
            .unwrap();
        resp.key_count.unwrap_or(0)
    });

    assert!(
        object_count > 0,
        "expected some successful uploads despite flaky S3"
    );
}

/// When S3 is permanently returning 500s, every segment attempt should
/// produce a failure metric entry.
#[test]
fn permanently_broken_s3_produces_failure_metrics() {
    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();

    std::fs::create_dir_all(s3_root.path().join("broken-bucket")).unwrap();
    let client = fake_s3_client_always_failing(s3_root.path());

    let writer = DiskBuffer::builder()
        .base_path(trace_dir.path())
        .max_file_size(512)
        .max_total_size(50 * 1024)
        .build()
        .unwrap();

    let s3_config = S3Config::builder()
        .bucket("broken-bucket")
        .prefix("traces")
        .service_name("test-svc")
        .instance_path("test-host")
        .region("us-east-1")
        .build();

    let metrique_writer::test_util::TestEntrySink {
        inspector,
        sink: metrics_sink,
    } = metrique_writer::test_util::test_entry_sink();

    let recorder = recorder(writer)
        .worker_poll_interval(std::time::Duration::from_millis(50))
        .metrics_sink(metrics_sink)
        .with_s3_uploader_client(s3_config.clone(), client.clone())
        .build();
    let rt = common::attach(&recorder, 2, TokioAttachOptions::default());

    let has_pipeline_metric = || {
        inspector
            .entries()
            .iter()
            .any(|e| e.metrics.contains_key("Failure") || e.metrics.contains_key("Success"))
    };

    // Generate enough events to seal segments, then poll until the worker has
    // recorded a pipeline (Failure/Success) metric.
    rt.block_on(async {
        for _ in 0..50 {
            tokio::spawn(async { tokio::task::yield_now().await });
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        while !has_pipeline_metric() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

    let entries = inspector.entries();
    // Filter to pipeline metrics only (FlushMetrics entries don't have Failure/Success keys).
    let pipeline_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.metrics.contains_key("Failure") || e.metrics.contains_key("Success"))
        .collect();
    assert!(
        !pipeline_entries.is_empty(),
        "expected at least one pipeline metric entry"
    );

    let failures = pipeline_entries
        .iter()
        .filter(|e| e.metrics["Failure"] == true)
        .count();
    assert_eq!(
        failures,
        pipeline_entries.len(),
        "all {} entries should be failures when S3 is permanently broken, but {} succeeded",
        pipeline_entries.len(),
        pipeline_entries.len() - failures,
    );
}

/// Triggered mode against S3: the pipeline stays parked until a dump is
/// requested, then uploads the captured segments and writes a per-dump
/// manifest at `{prefix}/dumps/{dump_id}.json`. Mirrors the continuous-upload
/// `end_to_end_trace_to_s3_roundtrip` but on the on-demand path.
#[test]
fn dump_trigger_uploads_segments_and_writes_manifest() {
    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();

    std::fs::create_dir(s3_root.path().join("dump-bucket")).unwrap();
    let client = fake_s3_client(s3_root.path());

    let writer = fast_sealing_writer(trace_dir.path());

    let s3_config = S3Config::builder()
        .bucket("dump-bucket")
        .prefix("traces")
        .service_name("dump-svc")
        .instance_path("test-host")
        .region("us-east-1")
        .build();

    let recorder = recorder(writer)
        .worker_poll_interval(Duration::from_millis(50))
        .with_s3_uploader_client(s3_config.clone(), client.clone())
        .with_dump_trigger(|_| {})
        .build();
    let rt = common::attach(&recorder, 2, TokioAttachOptions::default());

    let trigger = recorder.handle().dump_trigger().expect("trigger wired");

    // Before any dump, triggered mode must not have uploaded anything.
    let list_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let list_keys = |prefix: &str| {
        let prefix = prefix.to_string();
        list_rt.block_on(async {
            let resp = client
                .list_objects_v2()
                .bucket("dump-bucket")
                .prefix(&prefix)
                .send()
                .await
                .unwrap();
            resp.contents
                .unwrap_or_default()
                .into_iter()
                .filter_map(|o| o.key)
                .collect::<Vec<_>>()
        })
    };
    assert!(
        list_keys("traces/").is_empty(),
        "triggered mode must not upload until a dump is requested"
    );

    // A triggered worker parks until a dump is requested, so a confirmed-sealed
    // segment persists and the unbounded `dump_current_data` window is
    // guaranteed to match it.
    wait_for_sealed_segment(&rt, trace_dir.path());

    let receipt = rt.block_on(async {
        trigger
            .dump_current_data()
            .with_metadata("reason", "test")
            .await
            .expect("dump resolves")
    });

    assert!(
        receipt.segments_processed > 0,
        "dump should capture at least one sealed segment"
    );
    let dump_id = receipt.dump_id.to_string();
    let expected_manifest = format!("traces/dumps/{dump_id}.json");
    assert_eq!(
        receipt.manifest_key.as_deref(),
        Some(expected_manifest.as_str()),
        "receipt names the manifest key"
    );

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

    // The manifest object exists and lists the produced segment keys.
    let manifest_bytes = list_rt.block_on(async {
        client
            .get_object()
            .bucket("dump-bucket")
            .key(&expected_manifest)
            .send()
            .await
            .expect("manifest object exists")
            .body
            .collect()
            .await
            .unwrap()
            .into_bytes()
    });
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(manifest["dump_id"], dump_id);
    let segments = manifest["segments"].as_array().expect("segments array");
    assert!(!segments.is_empty(), "manifest lists at least one segment");

    // A listed segment object carries the dump-id as user metadata.
    let first_segment = segments[0].as_str().unwrap().to_string();
    let meta = list_rt.block_on(async {
        client
            .head_object()
            .bucket("dump-bucket")
            .key(&first_segment)
            .send()
            .await
            .expect("segment object exists")
            .metadata
            .unwrap_or_default()
    });
    assert_eq!(
        meta.get("dump-id").map(String::as_str),
        Some(dump_id.as_str()),
        "captured segment is tagged with the dump id"
    );
}

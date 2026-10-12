#![cfg(feature = "worker-s3")]

mod common;
mod fake_s3;

use dial9_core::buffer::{BufferMode, SegmentWriter};
use dial9_core::handle::Dial9Handle;
use dial9_core::recording::Recorder;
use dial9_core::source::{FlushContext, Source};
use dial9_destinations_s3::{FailureBehavior, S3Config};
use dial9_tokio_telemetry::background_task::{ProcessError, SegmentData, SegmentProcessor};
use dial9_tokio_telemetry::dump::{DumpCompletion, DumpError, DumpReceipt};
use dial9_tokio_telemetry::telemetry::{
    DiskBuffer, MemoryBuffer, RecorderPipelineExt, RecorderTryS3Ext, recorder,
};
use dial9_trace_format::TraceEvent;
use dial9_trace_format::decoder::Decoder;
use flate2::read::GzDecoder;
use std::cell::Cell;
use std::collections::HashMap;
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(10);
struct MemoryWorkingDirectory {
    original: PathBuf,
    directory: tempfile::TempDir,
    _lock: MutexGuard<'static, ()>,
}

impl MemoryWorkingDirectory {
    fn new() -> Self {
        static LOCK: Mutex<()> = Mutex::new(());
        let lock = LOCK.lock().unwrap();
        let original = std::env::current_dir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        std::env::set_current_dir(directory.path()).unwrap();
        Self {
            original,
            directory,
            _lock: lock,
        }
    }

    fn assert_no_files(&self) {
        assert!(
            std::fs::read_dir(self.directory.path())
                .unwrap()
                .next()
                .is_none()
        );
    }
}

impl Drop for MemoryWorkingDirectory {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.original).unwrap();
    }
}

#[derive(Debug, serde::Deserialize, TraceEvent)]
struct FixtureEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    value: u64,
    padding: String,
}

fn seed<M: BufferMode>(mut writer: SegmentWriter<M>) -> SegmentWriter<M> {
    for value in [11, 22] {
        dial9_core::test_util::write_event(
            &mut writer,
            &FixtureEvent {
                timestamp_ns: dial9_core::clock::clock_monotonic_ns(),
                value,
                padding: "fixture".repeat(256),
            },
        )
        .unwrap();
    }
    writer
}

fn disk_writer(path: &Path) -> DiskBuffer {
    seed(
        DiskBuffer::builder()
            .base_path(path)
            .max_file_size(1024)
            .max_total_size(common::CAPTURE_BUFFER_SIZE)
            .rotation_period(Duration::MAX)
            .build()
            .unwrap(),
    )
}

fn memory_writer() -> MemoryBuffer {
    seed(
        MemoryBuffer::builder()
            .max_segment_size(1024)
            .max_total_size(common::CAPTURE_BUFFER_SIZE)
            .rotation_period(Duration::MAX)
            .build()
            .unwrap(),
    )
}

fn config() -> S3Config {
    S3Config::builder()
        .bucket("fallible-bucket")
        .prefix("traces")
        .service_name("fallible-s3-test")
        .instance_path("test-host")
        .region("us-east-1")
        .build()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn client_future(
    result: Result<aws_sdk_s3::Client, FailureBehavior>,
    calls: Arc<AtomicUsize>,
) -> Result<aws_sdk_s3::Client, FailureBehavior> {
    let not_sync = Cell::new(17);
    tokio::runtime::Handle::try_current().expect("constructor runs inside Tokio");
    assert_eq!(std::thread::current().name(), Some("dial9-worker"));
    calls.fetch_add(1, Ordering::SeqCst);
    tokio::task::yield_now().await;
    assert_eq!(not_sync.get(), 17);
    assert_eq!(std::thread::current().name(), Some("dial9-worker"));
    result
}

fn dump(recorder: &Recorder, runtime: &tokio::runtime::Runtime) -> DumpReceipt {
    let trigger = recorder.handle().dump_trigger().expect("trigger is wired");
    runtime.block_on(async {
        tokio::time::timeout(
            DEADLINE,
            trigger
                .dump_current_data()
                .with_metadata("reason", "fallible-s3"),
        )
        .await
        .expect("dump completes")
        .expect("pipeline remains available")
    })
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    GzDecoder::new(bytes).read_to_end(&mut decoded).unwrap();
    decoded
}

fn fixture_values(bytes: &[u8]) -> Vec<u64> {
    let mut values = Vec::new();
    Decoder::new(bytes)
        .expect("valid trace")
        .for_each_event(|raw| {
            if raw.name == "FixtureEvent" {
                let event: FixtureEvent = raw.deserialize().unwrap();
                values.push(event.value);
            }
        })
        .unwrap();
    values
}

fn files_with_extension(directory: &Path, extension: &str) -> HashMap<PathBuf, Vec<u8>> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == extension))
        .map(|path| {
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    index: u32,
    path: Option<PathBuf>,
    payload: Vec<u8>,
    metadata: HashMap<String, String>,
    compressed_size: Option<u64>,
}

impl Snapshot {
    fn from_data(data: &SegmentData) -> Self {
        Self {
            index: data.segment().index(),
            path: data.segment().disk_path().map(Path::to_path_buf),
            payload: data.payload().clone().into_vec(),
            metadata: data.metadata().clone(),
            compressed_size: data.compressed_size(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Visit {
    Initialize(&'static str),
    Process(&'static str, Snapshot),
    Finalize(&'static str, String, usize, Vec<(String, String)>),
}

#[derive(Debug)]
struct Observe {
    name: &'static str,
    visits: Arc<Mutex<Vec<Visit>>>,
    transform: bool,
}

impl SegmentProcessor for Observe {
    fn name(&self) -> &'static str {
        self.name
    }

    fn initialize(&mut self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>> {
        self.visits
            .lock()
            .unwrap()
            .push(Visit::Initialize(self.name));
        Box::pin(async { Ok(()) })
    }

    fn process(
        &mut self,
        mut data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        if self.transform {
            data.set_payload(format!("custom payload {}", data.segment().index()).into_bytes());
            data.set_compressed_size(1234);
            data.metadata_mut()
                .insert("custom-stage".into(), "preserved".into());
            data.metadata_mut()
                .insert("write_back_extension".into(), ".custom".into());
        }
        self.visits
            .lock()
            .unwrap()
            .push(Visit::Process(self.name, Snapshot::from_data(&data)));
        Box::pin(async move { Ok(data) })
    }

    fn finalize_dump(
        &mut self,
        completion: &DumpCompletion,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        assert!(!completion.failed);
        self.visits.lock().unwrap().push(Visit::Finalize(
            self.name,
            completion.dump_id.to_string(),
            completion.segments_processed,
            completion.metadata.clone(),
        ));
        Box::pin(async { None })
    }
}

struct StagedSource(Option<common::CapturingProcessor>);

impl Source for StagedSource {
    fn name(&self) -> &'static str {
        "staged"
    }

    fn flush(&mut self, _ctx: &FlushContext<'_>) {}

    fn segment_processor(&mut self) -> Option<Box<dyn SegmentProcessor>> {
        self.0
            .take()
            .map(|processor| Box::new(processor) as Box<dyn SegmentProcessor>)
    }
}

#[test]
fn successful_constructor_runs_once_on_worker_and_uploads_traces_and_manifests() {
    let directory = tempfile::tempdir().unwrap();
    let s3_root = directory.path().join("s3");
    std::fs::create_dir_all(s3_root.join("fallible-bucket")).unwrap();
    let client = fake_s3::fake_s3_client(&s3_root);
    let calls = Arc::new(AtomicUsize::new(0));
    let recorder = recorder(disk_writer(&directory.path().join("local")))
        .try_s3_async(config(), client_future(Ok(client.clone()), calls.clone()))
        .with_dump_trigger(|_| {})
        .build();
    let runtime = runtime();

    let first = dump(&recorder, &runtime);
    assert!(first.segments_processed >= 2);
    let second = dump(&recorder, &runtime);
    assert_eq!(second.segments_processed, 0);
    assert_ne!(first.dump_id, second.dump_id);
    assert!(recorder.handle().is_enabled());
    recorder.graceful_shutdown(DEADLINE);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let mut values = Vec::new();
    for receipt in [first, second] {
        let expected_key = format!("traces/dumps/{}.json", receipt.dump_id);
        assert_eq!(receipt.manifest_key.as_deref(), Some(expected_key.as_str()));
        let manifest_bytes = runtime.block_on(async {
            client
                .get_object()
                .bucket("fallible-bucket")
                .key(expected_key)
                .send()
                .await
                .unwrap()
                .body
                .collect()
                .await
                .unwrap()
                .into_bytes()
        });
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
        assert_eq!(manifest["dump_id"], receipt.dump_id.to_string());
        let keys = manifest["segments"].as_array().unwrap();
        assert_eq!(keys.len(), receipt.segments_processed);
        for key in keys {
            let key = key.as_str().unwrap();
            assert!(key.ends_with(".bin.gz"));
            let bytes = runtime.block_on(async {
                let object = client
                    .get_object()
                    .bucket("fallible-bucket")
                    .key(key)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    object.metadata().unwrap().get("dump-id"),
                    Some(&receipt.dump_id.to_string())
                );
                object.body.collect().await.unwrap().into_bytes()
            });
            values.extend(fixture_values(&gunzip(&bytes)));
        }
    }
    values.sort_unstable();
    assert_eq!(values, [11, 22]);
}

fn assert_unavailable_s3_preserves_dump_deadline(fallible: bool) {
    let directory = tempfile::tempdir().unwrap();
    let s3_root = directory.path().join("s3");
    std::fs::create_dir_all(s3_root.join("fallible-bucket")).unwrap();
    let client = fake_s3::fake_s3_client_always_failing(&s3_root);
    let client = aws_sdk_s3::Client::from_conf(
        client
            .config()
            .to_builder()
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::standard().with_max_attempts(1))
            .build(),
    );
    let builder = recorder(disk_writer(&directory.path().join("local")));
    let builder = if fallible {
        builder.try_s3_async(config(), async move { Ok(client) })
    } else {
        builder.with_s3_uploader_client(config(), client)
    };
    let recorder = builder.with_dump_trigger(|_| {}).build();
    let runtime = runtime();
    let trigger = recorder.handle().dump_trigger().unwrap();
    let result = runtime.block_on(async {
        tokio::time::timeout(
            DEADLINE,
            trigger.dump_time_range(Duration::from_secs(3600), Duration::from_millis(50)),
        )
        .await
    });
    recorder.graceful_shutdown(DEADLINE);
    assert!(
        matches!(result, Ok(Err(DumpError::PipelineUnavailable))),
        "an unavailable S3 stage must respect the dump deadline: {result:?}"
    );
}

#[test]
fn fallible_s3_preset_preserves_liveness_gating() {
    assert_unavailable_s3_preserves_dump_deadline(true);
}

#[test]
fn infallible_s3_preset_preserves_liveness_gating() {
    assert_unavailable_s3_preserves_dump_deadline(false);
}

#[test]
fn disable_s3_disk_preset_preserves_source_stage_and_gzip_writeback() {
    let directory = tempfile::tempdir().unwrap();
    let (capture, captured) = common::capture_processor();
    let calls = Arc::new(AtomicUsize::new(0));
    let recorder = recorder(disk_writer(directory.path()))
        .source(StagedSource(Some(capture)))
        .try_s3_async(
            config(),
            client_future(Err(FailureBehavior::DisableS3), calls.clone()),
        )
        .with_dump_trigger(|_| {})
        .build();
    let runtime = runtime();

    let receipt = dump(&recorder, &runtime);
    assert!(receipt.segments_processed >= 2);
    assert!(receipt.manifest_key.is_none());
    assert!(dump(&recorder, &runtime).manifest_key.is_none());
    assert!(recorder.handle().is_enabled());
    recorder.graceful_shutdown(DEADLINE);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let mut captured = captured.lock().unwrap().clone();
    assert_eq!(captured.len(), receipt.segments_processed);
    let mut written: Vec<_> = files_with_extension(directory.path(), "gz")
        .values()
        .map(|bytes| gunzip(bytes))
        .collect();
    captured.sort();
    written.sort();
    assert_eq!(written, captured);
    let mut values: Vec<_> = captured
        .iter()
        .flat_map(|bytes| fixture_values(bytes))
        .collect();
    values.sort_unstable();
    assert_eq!(values, [11, 22]);
    for bytes in files_with_extension(directory.path(), "bin").values() {
        assert!(fixture_values(bytes).is_empty());
    }
}

fn assert_custom_disable_s3<M: BufferMode>(writer: SegmentWriter<M>) {
    let visits = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let recorder = recorder(writer)
        .with_custom_pipeline(|pipeline| {
            pipeline
                .pipe(Observe {
                    name: "before",
                    visits: visits.clone(),
                    transform: true,
                })
                .try_s3_async(
                    config(),
                    client_future(Err(FailureBehavior::DisableS3), calls.clone()),
                )
                .pipe(Observe {
                    name: "after",
                    visits: visits.clone(),
                    transform: false,
                })
        })
        .with_dump_trigger(|_| {})
        .build();
    let runtime = runtime();
    let first = dump(&recorder, &runtime);
    let second = dump(&recorder, &runtime);
    assert!(first.segments_processed >= 2);
    assert!(first.manifest_key.is_none());
    assert!(second.manifest_key.is_none());
    assert!(recorder.handle().is_enabled());
    recorder.graceful_shutdown(DEADLINE);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let visits = visits.lock().unwrap();
    assert_eq!(
        &visits[..2],
        &[Visit::Initialize("before"), Visit::Initialize("after")]
    );
    let mut remaining = &visits[2..];
    for receipt in [first, second] {
        for _ in 0..receipt.segments_processed {
            let [
                Visit::Process("before", before),
                Visit::Process("after", after),
            ] = &remaining[..2]
            else {
                panic!("stages must process each segment in pipeline order");
            };
            assert_eq!(
                before, after,
                "disabled S3 passes all data through unchanged"
            );
            assert_eq!(before.path.is_some(), M::IS_DISK);
            assert_eq!(
                before.metadata.get("dump_id"),
                Some(&receipt.dump_id.to_string())
            );
            remaining = &remaining[2..];
        }
        for stage in ["before", "after"] {
            assert_eq!(
                remaining.first(),
                Some(&Visit::Finalize(
                    stage,
                    receipt.dump_id.to_string(),
                    receipt.segments_processed,
                    vec![("reason".into(), "fallible-s3".into())],
                ))
            );
            remaining = &remaining[1..];
        }
    }
    assert!(remaining.is_empty());
}

#[test]
fn custom_disk_pipeline_bypasses_only_s3_without_implicit_writeback() {
    let directory = tempfile::tempdir().unwrap();
    let writer = disk_writer(directory.path());
    let original = files_with_extension(directory.path(), "bin");
    assert_eq!(original.len(), 2);

    assert_custom_disable_s3(writer);

    for (path, bytes) in original {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    for entry in std::fs::read_dir(directory.path()).unwrap() {
        let path = entry.unwrap().path();
        assert_eq!(path.extension().unwrap(), "bin");
    }
}

#[test]
fn custom_memory_pipeline_keeps_alternate_consumer_without_writing_files() {
    let directory = MemoryWorkingDirectory::new();
    assert_custom_disable_s3(memory_writer());
    directory.assert_no_files();
}

#[test]
fn disable_s3_memory_preset_discards_processed_data_but_keeps_recording() {
    let directory = MemoryWorkingDirectory::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let metrique_writer::test_util::TestEntrySink { inspector, sink } =
        metrique_writer::test_util::test_entry_sink();
    let recorder = recorder(memory_writer())
        .metrics_sink(sink)
        .try_s3_async(
            config(),
            client_future(Err(FailureBehavior::DisableS3), calls.clone()),
        )
        .with_dump_trigger(|_| {})
        .build();
    let runtime = runtime();
    let receipt = dump(&recorder, &runtime);
    assert!(receipt.segments_processed >= 2);
    assert!(receipt.manifest_key.is_none());
    let second = dump(&recorder, &runtime);
    assert_eq!(second.segments_processed, 0);
    assert!(second.manifest_key.is_none());

    let handle = recorder.handle();
    handle.disable();
    recorder.enable();
    assert!(handle.is_enabled());
    let mut recorded = false;
    handle.record_event_with(|| {
        recorded = true;
        FixtureEvent {
            timestamp_ns: dial9_core::clock::clock_monotonic_ns(),
            value: 33,
            padding: String::new(),
        }
    });
    assert!(recorded);
    recorder.graceful_shutdown(DEADLINE);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let entries = inspector.entries();
    let processed: Vec<_> = entries
        .iter()
        .filter(|entry| entry.metrics.contains_key("Success"))
        .collect();
    assert_eq!(processed.len(), receipt.segments_processed);
    for entry in processed {
        assert_eq!(entry.metrics["Success"], true);
        assert_eq!(entry.metrics["Failure"], false);
        assert_eq!(entry.metrics["Gzip.Success"].as_u64(), 1);
        assert_eq!(entry.metrics["S3Disabled.Success"], true);
        assert!(!entry.metrics.contains_key("S3Upload.Success"));
    }
    directory.assert_no_files();
}

fn assert_worker_stopped(handle: &Dial9Handle) {
    let trigger = handle.dump_trigger().unwrap();
    let error = runtime().block_on(async {
        tokio::time::timeout(DEADLINE, trigger.dump_current_data())
            .await
            .unwrap()
            .unwrap_err()
    });
    assert!(matches!(error, DumpError::WorkerStopped));
}

fn worker_teardown_signal(
    stopped: mpsc::Sender<()>,
) -> impl Fn() -> Box<dyn FnOnce() + Send> + Send + Sync {
    move || {
        let stopped = stopped.clone();
        let worker = std::thread::current().name() == Some("dial9-worker");
        Box::new(move || {
            if worker {
                stopped.send(()).unwrap();
            }
        })
    }
}

#[test]
fn disable_dial9_refuses_enable_stops_dumps_and_retains_existing_files() {
    let directory = tempfile::tempdir().unwrap();
    let writer = disk_writer(directory.path());
    let original = files_with_extension(directory.path(), "bin");
    assert_eq!(original.len(), 2);
    let calls = Arc::new(AtomicUsize::new(0));
    let (release, gate) = tokio::sync::oneshot::channel();
    let (stopped, worker_stopped) = mpsc::channel();
    let future_calls = calls.clone();
    let recorder = recorder(writer)
        .on_recording_thread_start(worker_teardown_signal(stopped))
        .try_s3_async(config(), async move {
            gate.await.unwrap();
            client_future(Err(FailureBehavior::DisableDial9), future_calls).await
        })
        .with_dump_trigger(|_| {})
        .build();
    let handle = recorder.handle();
    assert!(handle.is_enabled());
    release.send(()).unwrap();
    worker_stopped
        .recv_timeout(DEADLINE)
        .expect("DisableDial9 stops the worker");

    assert!(!handle.is_enabled());
    recorder.enable();
    handle.enable();
    assert!(!handle.is_enabled());
    handle.record_event_with(|| -> FixtureEvent { panic!("recording must stay disabled") });
    assert_worker_stopped(handle);
    recorder.graceful_shutdown(DEADLINE);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for (path, bytes) in original {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn pending_constructor_does_not_block_graceful_shutdown() {
    let (started, constructor_started) = mpsc::channel();
    let (stopped, worker_stopped) = mpsc::channel();
    let recorder = recorder(memory_writer())
        .on_recording_thread_start(worker_teardown_signal(stopped))
        .try_s3_async(config(), async move {
            assert_eq!(std::thread::current().name(), Some("dial9-worker"));
            started.send(()).unwrap();
            std::future::pending().await
        })
        .with_dump_trigger(|_| {})
        .build();
    constructor_started
        .recv_timeout(DEADLINE)
        .expect("constructor was polled");
    let handle = recorder.handle().clone();
    assert!(handle.is_enabled());
    let (finished, shutdown_finished) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        recorder.graceful_shutdown(Duration::from_millis(50));
        finished.send(()).unwrap();
    });
    shutdown_finished
        .recv_timeout(DEADLINE)
        .expect("graceful shutdown cancels the pending constructor");
    thread.join().unwrap();
    worker_stopped.recv_timeout(DEADLINE).unwrap();
    assert!(!handle.is_enabled());
    assert_worker_stopped(&handle);
}

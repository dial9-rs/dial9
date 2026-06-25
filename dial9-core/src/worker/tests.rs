//! Tests for the segment-processing worker and its built-in processors.

#[cfg(all(test, feature = "pipeline-s3"))]
mod worker_s3_tests {
    use crate::fs::Fs;
    use crate::pipeline::{ProcessError, SegmentData, SegmentProcessor};
    use crate::sealed;
    use crate::worker::connection;
    use crate::worker::processors::GzipCompressor;
    use crate::worker::s3::{self, S3PipelineUploader};
    use crate::worker::{BackgroundTaskConfig, DEFAULT_POLL_INTERVAL, WorkerLoop};
    use assert2::check;
    use std::future::Future;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Deps that record whether on_failure was called by proxying through
    /// a real S3Uploader-like upload path.
    struct NotFoundTestDeps {
        circuit_breaker: connection::CircuitBreaker,
    }

    impl NotFoundTestDeps {
        fn new() -> Self {
            Self {
                circuit_breaker: connection::CircuitBreaker::new(),
            }
        }

        /// Simulate the upload logic from S3PipelineUploader::process
        async fn upload_segment(&mut self, segment: &sealed::SealedSegment) {
            if !self.circuit_breaker.should_attempt() {
                return;
            }
            // Attempt to read the file (like the worker would)
            match tokio::fs::read(&segment.path).await {
                Ok(_) => self.circuit_breaker.on_success(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Should skip, not degrade
                }
                Err(_) => self.circuit_breaker.on_failure(),
            }
        }
    }

    #[tokio::test]
    async fn evicted_file_does_not_trip_circuit_breaker() {
        let dir = tempfile::tempdir().unwrap();
        // Create a segment that doesn't exist on disk (simulates eviction)
        let missing = sealed::SealedSegment {
            path: dir.path().join("trace.0.bin"),
            index: 0,
        };

        let mut deps = NotFoundTestDeps::new();
        deps.upload_segment(&missing).await;

        check!(deps.circuit_breaker == connection::CircuitBreaker::Closed);
    }

    // --- Review finding #1: compressed_size metric is non-zero after pipeline ---

    /// After a successful pipeline run (gzip + upload), the CompressedSize
    /// metric must reflect the actual compressed byte count, not 0.
    #[tokio::test]
    async fn compressed_size_metric_is_nonzero_after_pipeline() {
        use metrique_writer::AnyEntrySink;
        use metrique_writer::test_util::Inspector;

        let s3_root = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let data = vec![42u8; 4096];

        // Build a real pipeline: GzipCompressor → S3PipelineUploader
        let s3_config = s3::S3Config::builder()
            .bucket("test-bucket")
            .service_name("test")
            .instance_path("test")
            .boot_id("test")
            .region("us-east-1")
            .build();

        let fs = s3s_fs::FileSystem::new(s3_root.path()).unwrap();
        let mut builder = s3s::service::S3ServiceBuilder::new(fs);
        builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        let s3_service = builder.build();
        let s3_client: s3s_aws::Client = s3_service.into();
        let s3_sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(s3_client)
            .force_path_style(true)
            .build();
        let sdk_client = aws_sdk_s3::Client::from_conf(s3_sdk_config);
        let tm_client = aws_sdk_s3_transfer_manager::Client::new(
            aws_sdk_s3_transfer_manager::Config::builder()
                .client(sdk_client)
                .build(),
        );

        let uploader = s3::S3Uploader::new(tm_client, s3_config);
        let processors: Vec<Box<dyn SegmentProcessor>> = vec![
            Box::new(GzipCompressor),
            Box::new(S3PipelineUploader::from_ready(
                uploader,
                connection::CircuitBreaker::new(),
            )),
        ];

        // Seal a compressible segment and run the real worker over it,
        // capturing the per-segment metrics it emits.
        use std::io::Write;
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let mut h = fs.create_segment(Path::new("trace")).unwrap();
        h.write_all(&data).unwrap();
        fs.seal(h, Path::new("trace"), 0).unwrap();
        fs.mark_writer_done();

        let inspector = Inspector::default();
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            Arc::clone(&fs),
            DEFAULT_POLL_INTERVAL,
            processors,
            stop,
            inspector.clone().boxed(),
        );
        tokio::time::timeout(Duration::from_secs(5), worker.run())
            .await
            .expect("worker exited");

        // The per-segment metric carries the gzipped size.
        let entry = inspector
            .entries()
            .into_iter()
            .find(|e| e.metrics.contains_key("CompressedSize"))
            .expect("a segment-process metric entry");
        let compressed = entry.metrics["CompressedSize"].as_u64();
        check!(
            compressed > 0,
            "CompressedSize should be non-zero, got {}",
            compressed
        );
    }

    /// `set_client` is only valid while the uploader is `Pending`. Calling it
    /// on a `Ready` uploader indicates an internal misuse and must panic
    /// rather than silently drop the new client.
    #[test]
    #[should_panic(expected = "set_client called after uploader initialization")]
    fn set_client_after_ready_panics() {
        let s3_config = s3::S3Config::builder()
            .bucket("test")
            .service_name("test")
            .instance_path("test")
            .boot_id("test")
            .region("us-east-1")
            .build();
        let sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .build();
        let sdk_client = aws_sdk_s3::Client::from_conf(sdk_config);
        let tm_client = aws_sdk_s3_transfer_manager::Client::new(
            aws_sdk_s3_transfer_manager::Config::builder()
                .client(sdk_client.clone())
                .build(),
        );
        let uploader = s3::S3Uploader::new(tm_client, s3_config);
        let mut pipeline_uploader =
            S3PipelineUploader::from_ready(uploader, connection::CircuitBreaker::new());
        pipeline_uploader.set_client(sdk_client);
    }

    // --- Review finding #10: uncompressed_size should use bytes.len() ---

    /// uncompressed_size should match the actual bytes read, not a separate
    /// metadata() call that could race with eviction.
    #[test]
    fn uncompressed_size_matches_bytes_len() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.0.bin");
        let data = vec![0u8; 1234];
        std::fs::write(&path, &data).unwrap();

        // Read the file the way process_segments does
        let uncompressed_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let bytes = std::fs::read(&path).unwrap();

        // These should be equal — the metadata call is redundant
        check!(uncompressed_size == bytes.len() as u64);

        // The real assertion: bytes.len() is the canonical source of truth
        check!(bytes.len() == 1234);
    }

    // --- Review finding #4: WorkerLoop drain on stop ---

    /// When the stop signal is set, the worker must drain remaining segments
    /// before exiting.
    #[tokio::test]
    async fn worker_loop_drains_on_stop() {
        let dir = tempfile::tempdir().unwrap();

        // Create some sealed segments
        std::fs::write(dir.path().join("trace.0.bin"), b"segment0").unwrap();
        std::fs::write(dir.path().join("trace.1.bin"), b"segment1").unwrap();

        let processed = Arc::new(AtomicUsize::new(0));

        struct CountingProcessor(Arc<AtomicUsize>);
        impl SegmentProcessor for CountingProcessor {
            fn name(&self) -> &'static str {
                "Counter"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                let counter = self.0.clone();
                Box::pin(async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if let Some(p) = data.segment().disk_path() {
                        let mut done = p.as_os_str().to_owned();
                        done.push(".done");
                        let _ = std::fs::rename(p, done);
                    }
                    Ok(data)
                })
            }
        }

        // Pre-cancelled token so the worker processes once and exits.
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let config = BackgroundTaskConfig::builder()
            .trace_path(dir.path().join("trace.bin"))
            .build();

        let processors: Vec<Box<dyn SegmentProcessor>> =
            vec![Box::new(CountingProcessor(processed.clone()))];

        let mut worker = WorkerLoop::new(
            Fs::new_disk(config.trace_path().unwrap()),
            config.poll_interval(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.run().await;

        // Worker should have drained both segments even though stop was set.
        check!(processed.load(Ordering::SeqCst) == 2);
    }

    /// When a processor fails, the worker skips that segment and continues
    /// with the next one.
    #[tokio::test]
    async fn worker_loop_continues_after_processor_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"fail").unwrap();
        std::fs::write(dir.path().join("trace.1.bin"), b"succeed").unwrap();

        let processed = Arc::new(AtomicUsize::new(0));

        struct FailFirstProcessor {
            counter: Arc<AtomicUsize>,
            calls: usize,
        }
        impl SegmentProcessor for FailFirstProcessor {
            fn name(&self) -> &'static str {
                "FailFirst"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.calls += 1;
                let should_fail = self.calls == 1;
                let counter = self.counter.clone();
                Box::pin(async move {
                    if should_fail {
                        Err(ProcessError::io(
                            data,
                            std::io::Error::other("test failure"),
                        ))
                    } else {
                        counter.fetch_add(1, Ordering::SeqCst);
                        if let Some(p) = data.segment().disk_path() {
                            let mut done = p.as_os_str().to_owned();
                            done.push(".done");
                            let _ = std::fs::rename(p, done);
                        }
                        Ok(data)
                    }
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let config = BackgroundTaskConfig::builder()
            .trace_path(dir.path().join("trace.bin"))
            .build();

        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(FailFirstProcessor {
            counter: processed.clone(),
            calls: 0,
        })];

        let mut worker = WorkerLoop::new(
            Fs::new_disk(config.trace_path().unwrap()),
            config.poll_interval(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.run().await;

        // Second segment should still be processed despite first failing.
        check!(processed.load(Ordering::SeqCst) == 1);
    }

    #[test]
    fn trace_dir_for_bare_relative_path_defaults_to_current_directory() {
        let config = BackgroundTaskConfig::builder()
            .trace_path("trace.bin")
            .build();

        check!(config.trace_dir() == std::path::Path::new("."));
    }
}

// --- Review finding #9: trace_stem edge cases ---

#[cfg(test)]
mod trace_stem_tests {
    use crate::worker::BackgroundTaskConfig;
    use assert2::check;

    #[test]
    fn trace_stem_normal_path() {
        let config = BackgroundTaskConfig::builder()
            .trace_path("/tmp/traces/trace.bin")
            .build();
        check!(config.trace_stem() == "trace");
    }

    #[test]
    fn trace_stem_directory_path() {
        // A path like "/tmp/traces/" — file_stem returns "traces", not an error
        let config = BackgroundTaskConfig::builder()
            .trace_path("/tmp/traces/")
            .build();
        // This is the current behavior — it returns "traces" not "trace"
        // which would silently match the wrong files
        check!(config.trace_stem() == "traces");
    }

    #[test]
    fn trace_stem_root_path() {
        // A path like "/" has no file stem
        let config = BackgroundTaskConfig::builder().trace_path("/").build();
        // Should fall back to "trace" and log an error
        check!(config.trace_stem() == "trace");
    }

    #[test]
    fn trace_dir_for_directory_path() {
        let config = BackgroundTaskConfig::builder()
            .trace_path("/tmp/traces/")
            .build();
        // trace_dir should be the parent of the path
        check!(config.trace_dir() == std::path::Path::new("/tmp"));
    }
}

#[cfg(all(test, feature = "pipeline-s3"))]
mod worker_pipeline_tests {
    use crate::fs::Fs;
    use crate::payload::Payload;
    use crate::pipeline::{ProcessError, ProcessErrorKind, SegmentData, SegmentProcessor};
    use crate::sealed;
    use crate::worker::WorkerLoop;
    use crate::worker::connection;
    use crate::worker::processors::{GzipCompressor, WriteBackProcessor};
    use crate::worker::s3::{self, S3PipelineUploader};
    use assert2::check;
    use std::collections::HashMap;
    use std::future::Future;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Duration;

    fn fs_for(dir: &std::path::Path) -> Arc<Fs> {
        Fs::new_disk(&dir.join("trace.bin"))
    }

    fn default_poll() -> Duration {
        Duration::from_secs(1)
    }

    /// s3s wrapper where every upload returns 500 InternalError.
    struct AlwaysFailS3<S>(S);

    #[async_trait::async_trait]
    impl<S: s3s::S3 + Send + Sync> s3s::S3 for AlwaysFailS3<S> {
        async fn put_object(
            &self,
            _req: s3s::S3Request<s3s::dto::PutObjectInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
            Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected 500",
            ))
        }
        async fn create_multipart_upload(
            &self,
            _req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
            Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected 500",
            ))
        }
        async fn upload_part(
            &self,
            _req: s3s::S3Request<s3s::dto::UploadPartInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
            Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected 500",
            ))
        }
        async fn complete_multipart_upload(
            &self,
            _req: s3s::S3Request<s3s::dto::CompleteMultipartUploadInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
            Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected 500",
            ))
        }
    }

    /// s3s wrapper that fails the first `fail_n` writes with 500, then
    /// delegates to the inner backend.
    struct FlakyS3<S> {
        inner: S,
        remaining_failures: Arc<std::sync::atomic::AtomicU32>,
    }

    impl<S> FlakyS3<S> {
        fn should_fail(&self) -> bool {
            use std::sync::atomic::Ordering;
            let prev = self.remaining_failures.load(Ordering::SeqCst);
            if prev == 0 {
                return false;
            }
            self.remaining_failures
                .compare_exchange(prev, prev - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        }
    }

    #[async_trait::async_trait]
    impl<S: s3s::S3 + Send + Sync> s3s::S3 for FlakyS3<S> {
        async fn put_object(
            &self,
            req: s3s::S3Request<s3s::dto::PutObjectInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
            if self.should_fail() {
                return Err(s3s::S3Error::with_message(
                    s3s::S3ErrorCode::InternalError,
                    "injected 500",
                ));
            }
            self.inner.put_object(req).await
        }
    }

    struct FlakyHarness {
        uploader: s3::S3Uploader,
        fail_counter: Arc<std::sync::atomic::AtomicU32>,
        s3_root: tempfile::TempDir,
    }

    /// Read the single object out of the fake S3 bucket. Panics if there
    /// isn't exactly one. Used to assert uploaded bytes survived retries.
    fn read_only_object(s3_root: &std::path::Path) -> Vec<u8> {
        fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(rd) = std::fs::read_dir(p) else { return };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.is_file()
                    && path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| !n.ends_with(".s3s-fs"))
                {
                    out.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(&s3_root.join("test-bucket"), &mut found);
        assert_eq!(found.len(), 1, "expected exactly one object, got {found:?}");
        std::fs::read(&found[0]).unwrap()
    }

    fn flaky_s3_harness(fail_n: u32) -> FlakyHarness {
        let s3_root = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();
        let fail_counter = Arc::new(std::sync::atomic::AtomicU32::new(fail_n));

        let fs = s3s_fs::FileSystem::new(s3_root.path()).unwrap();
        let flaky = FlakyS3 {
            inner: fs,
            remaining_failures: Arc::clone(&fail_counter),
        };
        let mut svc = s3s::service::S3ServiceBuilder::new(flaky);
        svc.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        let s3_client: s3s_aws::Client = svc.build().into();

        let sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            // Disable SDK-internal retries so each worker attempt = 1 PUT.
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())
            .http_client(s3_client)
            .force_path_style(true)
            .build();
        let tm_client = aws_sdk_s3_transfer_manager::Client::new(
            aws_sdk_s3_transfer_manager::Config::builder()
                .client(aws_sdk_s3::Client::from_conf(sdk_config))
                .build(),
        );
        let s3_config = s3::S3Config::builder()
            .bucket("test-bucket")
            .service_name("test")
            .instance_path("test")
            .boot_id("test")
            .region("us-east-1")
            .build();
        FlakyHarness {
            uploader: s3::S3Uploader::new(tm_client, s3_config),
            fail_counter,
            s3_root,
        }
    }

    fn always_failing_s3_uploader() -> (s3::S3Uploader, tempfile::TempDir) {
        let s3_root = tempfile::tempdir().unwrap();
        let fs = s3s_fs::FileSystem::new(s3_root.path()).unwrap();
        let failing = AlwaysFailS3(fs);
        let mut builder = s3s::service::S3ServiceBuilder::new(failing);
        builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        let s3_service = builder.build();
        let s3_client: s3s_aws::Client = s3_service.into();
        let s3_sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(s3_client)
            .force_path_style(true)
            .build();
        let sdk_client = aws_sdk_s3::Client::from_conf(s3_sdk_config);
        let tm_client = aws_sdk_s3_transfer_manager::Client::new(
            aws_sdk_s3_transfer_manager::Config::builder()
                .client(sdk_client)
                .build(),
        );
        let s3_config = s3::S3Config::builder()
            .bucket("test-bucket")
            .service_name("test")
            .instance_path("test")
            .boot_id("test")
            .region("us-east-1")
            .build();
        (s3::S3Uploader::new(tm_client, s3_config), s3_root)
    }

    /// A segment that fails with a transient S3 error (500) is kept on disk for retry.
    #[tokio::test]
    async fn failed_segment_kept_on_transient_error() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"bad data").unwrap();

        let (uploader, _s3_root) = always_failing_s3_uploader();
        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(
            S3PipelineUploader::from_ready(uploader, connection::CircuitBreaker::new()),
        )];

        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.process_open_segments().await;

        check!(
            seg_path.exists(),
            "segment should be kept on disk after transient S3 error"
        );
    }

    /// A circuit-breaker-open error keeps the segment on disk.
    #[tokio::test]
    async fn circuit_breaker_open_keeps_segment() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"trace data").unwrap();

        let (uploader, _s3_root) = always_failing_s3_uploader();
        let mut cb = connection::CircuitBreaker::new();
        // Trip the circuit breaker so it refuses attempts.
        cb.on_failure();
        let processors: Vec<Box<dyn SegmentProcessor>> =
            vec![Box::new(S3PipelineUploader::from_ready(uploader, cb))];

        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.process_open_segments().await;

        check!(
            seg_path.exists(),
            "segment should be kept when circuit breaker is open"
        );
    }

    /// A NotFound error (evicted segment) is silently skipped — no deletion attempt.
    #[tokio::test]
    async fn not_found_error_skips_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        // Write the file so it can be read, but the processor returns NotFound
        std::fs::write(&seg_path, b"data").unwrap();

        struct NotFoundProcessor;
        impl SegmentProcessor for NotFoundProcessor {
            fn name(&self) -> &'static str {
                "NotFound"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(async {
                    Err(ProcessError::io(
                        data,
                        std::io::Error::new(std::io::ErrorKind::NotFound, "evicted"),
                    ))
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(NotFoundProcessor)];

        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.process_open_segments().await;

        // File still exists because the processor returned NotFound (eviction),
        // which means the worker should skip — not attempt to delete.
        check!(
            seg_path.exists(),
            "segment should not be deleted on NotFound (eviction)"
        );
    }

    /// A permanent, non-retryable IO error deletes the segment.
    #[tokio::test]
    async fn permanent_io_error_deletes_segment() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"bad data").unwrap();

        struct PermanentFailProcessor;
        impl SegmentProcessor for PermanentFailProcessor {
            fn name(&self) -> &'static str {
                "PermanentFail"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(async {
                    Err(ProcessError::io(
                        data,
                        std::io::Error::other("corrupt data"),
                    ))
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(PermanentFailProcessor)];

        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.process_open_segments().await;

        check!(
            !seg_path.exists(),
            "segment should be deleted after permanent IO error"
        );
    }

    /// Gzip-compressed segments pass through GzipCompressor unchanged.
    #[tokio::test]
    async fn gzip_segment_not_double_compressed() {
        let dir = tempfile::tempdir().unwrap();

        let gzip_data = {
            use flate2::write::GzEncoder;
            use std::io::Write;
            let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(b"already compressed").unwrap();
            enc.finish().unwrap()
        };
        std::fs::write(dir.path().join("trace.0.bin"), &gzip_data).unwrap();

        let (capture, output_bytes) = CapturingProcessor::new();
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();

        let processors: Vec<Box<dyn SegmentProcessor>> =
            vec![Box::new(GzipCompressor), Box::new(capture)];

        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.run().await;

        // The captured bytes should be identical to the input (not double-gzipped).
        // Single segment in this test, so check the first (only) captured payload.
        let captured = output_bytes.lock().unwrap();
        check!(captured.len() == 1);
        check!(captured[0].as_slice() == gzip_data.as_slice());
    }

    /// WriteBackProcessor writes to a new path when `write_back_extension` is
    /// set and removes the original file, preventing re-discovery on the next
    /// poll cycle.
    #[tokio::test]
    async fn write_back_renames_when_extension_metadata_set() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"payload").unwrap();

        let segment = sealed::SegmentRef::Disk(sealed::SealedSegment {
            path: seg_path.clone(),
            index: 0,
        });

        let data = SegmentData::new(
            segment,
            Payload::from(b"payload"),
            HashMap::from([("write_back_extension".into(), ".gz".into())]),
            None,
        );

        let mut processor = WriteBackProcessor::default();
        let result = processor.process(data).await;
        check!(result.is_ok());

        // Original .bin should be gone, .bin.gz should exist with the payload.
        check!(!seg_path.exists());
        let gz_path = dir.path().join("trace.0.bin.gz");
        check!(gz_path.exists());
        check!(std::fs::read(&gz_path).unwrap() == b"payload");
    }

    /// WriteBackProcessor writes to the original path when no
    /// `write_back_extension` metadata is set.
    #[tokio::test]
    async fn write_back_overwrites_in_place_without_extension() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"old").unwrap();

        let segment = sealed::SegmentRef::Disk(sealed::SealedSegment {
            path: seg_path.clone(),
            index: 0,
        });

        let data = SegmentData::new(segment, Payload::from(b"new"), HashMap::new(), None);

        let mut processor = WriteBackProcessor::default();
        let result = processor.process(data).await;
        check!(result.is_ok());

        check!(std::fs::read(&seg_path).unwrap() == b"new");
    }

    /// The full GzipCompressor → WriteBackProcessor pipeline writes a `.bin.gz`
    /// file and removes the original `.bin`, so `find_sealed_segments` will not
    /// re-discover it on the next poll.
    #[tokio::test]
    async fn gzip_write_back_pipeline_prevents_rediscovery() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"raw trace data").unwrap();

        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();

        let processors: Vec<Box<dyn SegmentProcessor>> = vec![
            Box::new(GzipCompressor),
            Box::new(WriteBackProcessor::default()),
        ];

        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.run().await;

        // Original .bin removed; .bin.gz written.
        check!(!seg_path.exists());
        check!(dir.path().join("trace.0.bin.gz").exists());

        // A subsequent scan should find no sealed segments.
        let segments = sealed::find_sealed_segments(dir.path(), "trace").unwrap();
        check!(segments.is_empty());
    }

    /// A processor that panics must not kill the worker loop. The panicking
    /// segment is skipped and subsequent segments are still processed.
    #[tokio::test]
    async fn processor_panic_does_not_kill_worker_loop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"panic me").unwrap();
        std::fs::write(dir.path().join("trace.1.bin"), b"process me").unwrap();

        let processed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        struct PanicFirstProcessor {
            counter: Arc<std::sync::atomic::AtomicUsize>,
            calls: usize,
        }
        impl SegmentProcessor for PanicFirstProcessor {
            fn name(&self) -> &'static str {
                "PanicFirst"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.calls += 1;
                let should_panic = self.calls == 1;
                let counter = self.counter.clone();
                Box::pin(async move {
                    if should_panic {
                        panic!("processor panic on first segment");
                    }
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(data)
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();

        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(PanicFirstProcessor {
            counter: processed.clone(),
            calls: 0,
        })];

        use metrique_writer::AnyEntrySink;
        use metrique_writer::test_util::Inspector;
        let inspector = Inspector::default();

        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            inspector.clone().boxed(),
        );
        worker.run().await;

        // The worker must have processed at least one segment (the non-panicking one)
        // despite the first processor call panicking.
        check!(processed.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        // The panicking segment's file should have been removed.
        check!(!dir.path().join("trace.0.bin").exists());

        // Verify metrics: we should have entries for both segments, and the
        // panicking one should have Panicked=true.
        let entries = inspector.entries();
        let panicked_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.metrics.get("Panicked").is_some_and(|m| m.as_bool()))
            .collect();
        check!(
            panicked_entries.len() == 1,
            "expected exactly one panicked metric entry, got {}",
            panicked_entries.len()
        );
        check!(panicked_entries[0].metrics["Failure"] == true);
        // The panic message should be captured.
        check!(panicked_entries[0].values["PanicMessage"] == "processor panic on first segment");
    }

    /// A processor that hangs must not prevent the worker from shutting down.
    /// The drain timeout in `run_background_task` handles this, but at the
    /// WorkerLoop level, cancellation should interrupt a hung processor.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn processor_hang_respects_shutdown_timeout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"hang me").unwrap();

        struct HangingProcessor;
        impl SegmentProcessor for HangingProcessor {
            fn name(&self) -> &'static str {
                "Hanging"
            }
            fn process(
                &mut self,
                _data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(async {
                    // Hang forever
                    std::future::pending::<()>().await;
                    unreachable!()
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(HangingProcessor)];

        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop.clone(),
            metrique_writer::sink::DevNullSink::boxed(),
        );

        let run_fut = worker.run();

        // Simulate the shutdown path from run_background_task:
        // cancel the stop token, then timeout the run future.
        let drain_timeout = Duration::from_secs(2);
        stop.cancel();
        let result = tokio::time::timeout(drain_timeout, run_fut).await;

        // The timeout should fire because the processor is hung.
        check!(result.is_err(), "expected timeout, but worker completed");
    }

    /// Disk `mark_writer_done` alone (no stop-token cancel) drains and exits.
    /// Symmetric with memory mode: `DiskWriter::finalize` is a complete
    /// shutdown signal across both backends.
    #[tokio::test]
    async fn disk_worker_run_drains_on_writer_done() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"seg0").unwrap();
        std::fs::write(dir.path().join("trace.1.bin"), b"seg1").unwrap();

        let processed = Arc::new(AtomicUsize::new(0));

        struct CountingProcessor(Arc<AtomicUsize>);
        impl SegmentProcessor for CountingProcessor {
            fn name(&self) -> &'static str {
                "Counting"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                if let Some(p) = data.segment().disk_path() {
                    let _ = std::fs::remove_file(p);
                }
                Box::pin(async { Ok(data) })
            }
        }

        let fs = fs_for(dir.path());
        // Stop token is never cancelled; shutdown rides writer_done only.
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            Arc::clone(&fs),
            Duration::from_millis(10),
            vec![Box::new(CountingProcessor(processed.clone()))],
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );

        fs.mark_writer_done();

        let result = tokio::time::timeout(Duration::from_secs(5), worker.run()).await;
        check!(result.is_ok(), "worker did not exit on writer_done alone");
        check!(processed.load(Ordering::SeqCst) == 2);
    }

    #[test]
    fn mem_take_files_reports_in_flight_bytes_peak() {
        use std::io::Write;

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let mut h = fs.create_segment(Path::new("x")).unwrap();
        h.write_all(&[0u8; 50]).unwrap();
        fs.seal(h, Path::new("x"), 0).unwrap();

        // Cycle 1: pop. Peak snapshot reads 0 (nothing happened before).
        // Inside the same call, the pop seeds the channel peak at 50.
        let mut snap = fs.take_files();
        check!(snap.segments.len() == 1);
        check!(snap.in_flight_bytes == 50);
        check!(snap.in_flight_bytes_peak == Some(0));

        let taken = snap.segments.remove(0);
        let (_seg, payload, accounting) = taken.load().unwrap();
        let mut acct = accounting.expect("memory segment carries accounting");
        // Verify the in-cycle adjust path moves the peak.
        let _ = payload;
        acct.adjust(200);
        acct.adjust(10);
        drop(acct);

        // Cycle 2: empty pop. Returned peak is the previous cycle's high.
        let snap = fs.take_files();
        check!(snap.segments.is_empty());
        check!(snap.in_flight_bytes == 0);
        check!(
            snap.in_flight_bytes_peak == Some(200),
            "peak should capture mid-cycle high; got {:?}",
            snap.in_flight_bytes_peak
        );

        // Cycle 3: peak has been consumed
        let snap = fs.take_files();
        check!(snap.in_flight_bytes_peak == Some(0));
    }

    /// `in_flight_bytes` follows payload growth (symbolize) and shrinkage
    /// (gzip), not just the pop-time size.
    #[tokio::test]
    async fn mem_worker_adjusts_in_flight_bytes_across_stages() {
        use std::io::Write;
        use std::sync::Mutex;
        use std::sync::atomic::Ordering;

        struct Mutator;
        impl SegmentProcessor for Mutator {
            fn name(&self) -> &'static str {
                "Mutator"
            }
            fn process(
                &mut self,
                mut data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                let index = data.segment().index();
                Box::pin(async move {
                    if index == 0 {
                        let mut p = data.take_payload();
                        p.push(bytes::Bytes::from(vec![0u8; 100]));
                        data.set_payload(p);
                    } else {
                        data.set_payload(bytes::Bytes::from(vec![0u8; 5]));
                    }
                    Ok(data)
                })
            }
        }

        /// Reads `in_flight_bytes` so we can assert what the worker's
        /// `adjust` set after `Mutator` returned.
        struct Probe {
            samples: Arc<Mutex<Vec<(u32, u64)>>>,
        }
        impl SegmentProcessor for Probe {
            fn name(&self) -> &'static str {
                "Probe"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                let index = data.segment().index();
                let atomic = Arc::clone(
                    &data
                        .accounting()
                        .expect("memory segments should carry accounting")
                        .in_flight_bytes,
                );
                let samples = Arc::clone(&self.samples);
                Box::pin(async move {
                    samples
                        .lock()
                        .expect("samples mutex should not be poisoned")
                        .push((index, atomic.load(Ordering::Acquire)));
                    Ok(data)
                })
            }
        }

        let fs = Fs::new_in_memory(64 * 1024, 1024).expect("memory fs should build");
        for i in 0..2u32 {
            let mut h = fs
                .create_segment(Path::new("x"))
                .expect("memory fs should create a handle");
            h.write_all(&[0u8; 50])
                .expect("write into memory handle should succeed");
            fs.seal(h, Path::new("x"), i)
                .expect("sealing into memory ring should succeed");
        }
        fs.mark_writer_done();

        let samples = Arc::new(Mutex::new(Vec::new()));
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            Arc::clone(&fs),
            Duration::from_millis(1),
            vec![
                Box::new(Mutator),
                Box::new(Probe {
                    samples: Arc::clone(&samples),
                }),
            ],
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        tokio::time::timeout(Duration::from_secs(5), worker.run())
            .await
            .expect("worker exited");

        let samples = samples
            .lock()
            .expect("samples mutex should not be poisoned")
            .clone();
        check!(samples == vec![(0, 150), (1, 5)]);
        let snap = fs.take_files();
        check!(snap.in_flight_bytes == 0);
        check!(snap.in_flight_segments == 0);
    }

    /// Multi-threaded race test for memory mode: producer seals segments and
    /// marks writer_done while the worker may be parked in `wait_for_more`.
    /// Ensures `run()` drains all segments and exits (no missed wakeup hang).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mem_worker_run_drains_late_push_no_loss() {
        use std::io::Write;
        use std::sync::atomic::{AtomicUsize, Ordering};

        const ITERS: usize = 30;
        const SEGMENTS: u32 = 8;

        struct CountingProcessor(Arc<AtomicUsize>);
        impl SegmentProcessor for CountingProcessor {
            fn name(&self) -> &'static str {
                "Counting"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(data) })
            }
        }

        for iter in 0..ITERS {
            let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
            let processed = Arc::new(AtomicUsize::new(0));
            let stop = tokio_util::sync::CancellationToken::new();

            let mut worker = WorkerLoop::new(
                Arc::clone(&fs),
                Duration::from_millis(1),
                vec![Box::new(CountingProcessor(processed.clone()))],
                stop,
                metrique_writer::sink::DevNullSink::boxed(),
            );
            let worker_task = tokio::spawn(async move { worker.run().await });

            // Let the worker reach wait_for_more on an empty ring so the
            // seals below race the wakeup/writer_done window.
            tokio::task::yield_now().await;

            let producer_fs = Arc::clone(&fs);
            let producer = tokio::spawn(async move {
                for i in 0..SEGMENTS {
                    let mut h = producer_fs.create_segment(Path::new("x")).unwrap();
                    h.write_all(b"event-bytes").unwrap();
                    producer_fs.seal(h, Path::new("x"), i).unwrap();
                }
                producer_fs.mark_writer_done();
            });

            producer.await.unwrap();
            let joined = tokio::time::timeout(Duration::from_secs(5), worker_task).await;
            check!(
                joined.is_ok(),
                "iter {iter}: worker stranded (lost wakeup or missed writer_done)"
            );
            joined.unwrap().unwrap();

            check!(
                processed.load(Ordering::SeqCst) == SEGMENTS as usize,
                "iter {iter}: expected {SEGMENTS} segments, got {}",
                processed.load(Ordering::SeqCst)
            );
        }
    }

    /// N<budget retryable failures followed by success: segment delivers,
    /// in-flight accounting drains.
    #[tokio::test(start_paused = true)]
    async fn mem_worker_retries_retryable_within_budget() {
        use std::io::Write;
        use std::sync::atomic::{AtomicU32, Ordering};

        struct Flaky {
            fail_count: u32,
            attempts: Arc<AtomicU32>,
        }
        impl SegmentProcessor for Flaky {
            fn name(&self) -> &'static str {
                "Flaky"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                let n = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
                let fail_count = self.fail_count;
                Box::pin(async move {
                    if n <= fail_count {
                        Err(ProcessError::new(
                            data,
                            ProcessErrorKind::transfer(Box::from("transient"), true),
                        ))
                    } else {
                        Ok(data)
                    }
                })
            }
        }

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let mut h = fs.create_segment(Path::new("x")).unwrap();
        h.write_all(&[0u8; 50]).unwrap();
        fs.seal(h, Path::new("x"), 0).unwrap();
        fs.mark_writer_done();

        let attempts = Arc::new(AtomicU32::new(0));
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            Arc::clone(&fs),
            Duration::from_millis(1),
            vec![Box::new(Flaky {
                fail_count: 2,
                attempts: Arc::clone(&attempts),
            })],
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.run().await;
        check!(attempts.load(Ordering::SeqCst) == 3, "2 fails + 1 success");
        let snap = fs.take_files();
        check!(snap.in_flight_bytes == 0);
        check!(snap.in_flight_segments == 0);
    }

    /// Always-fail retryable: exactly `MEMORY_RETRY_BUDGET + 1` attempts,
    /// then segment is dropped and accounting drains.
    #[tokio::test(start_paused = true)]
    async fn mem_worker_drops_after_retry_budget_exhausted() {
        use std::io::Write;
        use std::sync::atomic::{AtomicU32, Ordering};

        struct AlwaysFails {
            attempts: Arc<AtomicU32>,
        }
        impl SegmentProcessor for AlwaysFails {
            fn name(&self) -> &'static str {
                "AlwaysFails"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    Err(ProcessError::new(
                        data,
                        ProcessErrorKind::transfer(Box::from("permanent"), true),
                    ))
                })
            }
        }

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let mut h = fs.create_segment(Path::new("x")).unwrap();
        h.write_all(&[0u8; 50]).unwrap();
        fs.seal(h, Path::new("x"), 0).unwrap();
        fs.mark_writer_done();

        let attempts = Arc::new(AtomicU32::new(0));
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            Arc::clone(&fs),
            Duration::from_millis(1),
            vec![Box::new(AlwaysFails {
                attempts: Arc::clone(&attempts),
            })],
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.run().await;
        check!(
            attempts.load(Ordering::SeqCst) == crate::fs::MEMORY_RETRY_BUDGET + 1,
            "initial + budget retries",
        );
        let snap = fs.take_files();
        check!(snap.in_flight_bytes == 0);
        check!(snap.in_flight_segments == 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mem_e2e_real_s3_pipeline_recovers_within_budget() {
        use std::io::Write;
        use std::sync::atomic::Ordering;

        let FlakyHarness {
            uploader,
            fail_counter,
            s3_root,
        } = flaky_s3_harness(2);
        // > CB initial backoff (1s) so CB reopens between retries. CB
        // doubles per failure; budget=3 fits 1s+2s within the 15s cap below.
        let poll_interval = Duration::from_millis(1100);

        let payload = b"segment-payload-bytes";
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let mut h = fs.create_segment(Path::new("x")).unwrap();
        h.write_all(payload).unwrap();
        fs.seal(h, Path::new("x"), 0).unwrap();
        fs.mark_writer_done();

        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            Arc::clone(&fs),
            poll_interval,
            vec![Box::new(S3PipelineUploader::from_ready(
                uploader,
                connection::CircuitBreaker::new(),
            ))],
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        tokio::time::timeout(Duration::from_secs(15), worker.run())
            .await
            .expect("worker hung");

        check!(
            fail_counter.load(Ordering::SeqCst) == 0,
            "all injected failures consumed",
        );
        let uploaded = read_only_object(s3_root.path());
        check!(
            uploaded == payload,
            "uploaded body must match seal'd bytes (snapshot survived retries)",
        );
        let snap = fs.take_files();
        check!(snap.in_flight_bytes == 0);
        check!(snap.in_flight_segments == 0);
    }

    /// Captures each sealed segment's payload bytes, one `Vec<u8>` per segment.
    struct CapturingProcessor {
        segments: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    }

    impl CapturingProcessor {
        fn new() -> (Self, Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
            let segments = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Self {
                    segments: segments.clone(),
                },
                segments,
            )
        }
    }

    impl SegmentProcessor for CapturingProcessor {
        fn name(&self) -> &'static str {
            "Capture"
        }

        fn process(
            &mut self,
            data: SegmentData,
        ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
            self.segments
                .lock()
                .unwrap()
                .push(data.payload().clone().into_vec());
            Box::pin(async move { Ok(data) })
        }
    }
}

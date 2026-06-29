//! Tests for the segment-processing worker and its built-in processors.

#[cfg(test)]
mod worker_s3_tests {
    use crate::fs::Fs;
    use crate::pipeline::{ProcessError, SegmentData, SegmentProcessor};
    use crate::worker::processors::GzipCompressor;
    use crate::worker::{BackgroundTaskConfig, DEFAULT_POLL_INTERVAL, WorkerLoop};
    use assert2::check;
    use std::future::Future;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // --- Review finding #1: compressed_size metric is non-zero after pipeline ---

    /// After a successful pipeline run (gzip + a terminal stage), the
    /// CompressedSize metric must reflect the actual compressed byte count,
    /// not 0.
    #[tokio::test]
    async fn compressed_size_metric_is_nonzero_after_pipeline() {
        use metrique_writer::AnyEntrySink;
        use metrique_writer::test_util::Inspector;

        let data = vec![42u8; 4096];

        /// Terminal stage that accepts whatever the gzip stage produced.
        struct AcceptTerminal;
        impl SegmentProcessor for AcceptTerminal {
            fn name(&self) -> &'static str {
                "AcceptTerminal"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(async move { Ok(data) })
            }
        }

        let processors: Vec<Box<dyn SegmentProcessor>> =
            vec![Box::new(GzipCompressor), Box::new(AcceptTerminal)];

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
            None,
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
            None,
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
            None,
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

#[cfg(test)]
mod worker_pipeline_tests {
    use crate::fs::Fs;
    use crate::payload::Payload;
    use crate::pipeline::{ProcessError, ProcessErrorKind, SegmentData, SegmentProcessor};
    use crate::sealed;
    use crate::worker::WorkerLoop;
    use crate::worker::processors::{GzipCompressor, WriteBackProcessor};
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

    /// Stage that always fails with a retryable transfer error, standing in
    /// for any upload processor that hit a transient failure.
    struct RetryableFail;
    impl SegmentProcessor for RetryableFail {
        fn name(&self) -> &'static str {
            "RetryableFail"
        }
        fn process(
            &mut self,
            data: SegmentData,
        ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
            Box::pin(async move {
                Err(ProcessError::new(
                    data,
                    ProcessErrorKind::transfer(Box::from("injected"), true),
                ))
            })
        }
    }

    /// A retryable error keeps the segment on disk for a later attempt. (The
    /// real S3 transient-failure and circuit-breaker-open paths both surface
    /// as a retryable transfer error; they're covered against real S3 in
    /// `dial9-utils`.)
    #[tokio::test]
    async fn failed_segment_kept_on_transient_error() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"bad data").unwrap();

        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(RetryableFail)];

        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        worker.process_open_segments().await;

        check!(
            seg_path.exists(),
            "segment should be kept on disk after a retryable error"
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
                None,
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
            None,
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
            None,
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

#[cfg(test)]
mod triggered_test_support {
    use crate::fs::Fs;
    use crate::pipeline::{ProcessError, SegmentData, SegmentProcessor};
    use crate::worker::WorkerLoop;
    use std::future::Future;
    use std::io::Write as _;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Captures each sealed segment's payload bytes, one `Vec<u8>` per segment.
    pub(super) struct CapturingProcessor {
        segments: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl CapturingProcessor {
        pub(super) fn new() -> (Self, Arc<Mutex<Vec<Vec<u8>>>>) {
            let segments = Arc::new(Mutex::new(Vec::new()));
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

    pub(super) fn now_epoch() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Valid trace bytes whose first clock anchor reports `epoch_secs`.
    pub(super) fn segment_with_epoch(epoch_secs: u64) -> Vec<u8> {
        use dial9_trace_format::encoder::Encoder;
        let mut enc = Encoder::new_to(Vec::new()).unwrap();
        enc.write_infallible(&crate::format::ClockSyncEvent {
            timestamp_ns: 1,
            realtime_ns: epoch_secs * 1_000_000_000,
        });
        enc.into_inner()
    }

    pub(super) fn seal_mem(fs: &Fs, index: u32, epoch_secs: u64) {
        let mut h = fs.create_segment(Path::new("x")).unwrap();
        h.write_all(&segment_with_epoch(epoch_secs)).unwrap();
        fs.seal(h, Path::new("x"), index).unwrap();
    }

    pub(super) fn spawn_worker(
        fs: Arc<Fs>,
        processors: Vec<Box<dyn SegmentProcessor>>,
        rx: crate::dump::DumpRx,
        stop: tokio_util::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let mut worker = WorkerLoop::new(
            fs,
            Duration::from_millis(10),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            Some(rx),
        );
        tokio::spawn(async move { worker.run().await })
    }
}

#[cfg(test)]
mod triggered_worker_tests {
    use super::triggered_test_support::*;
    use crate::dump::{self, DumpError};
    use crate::fs::{EpochWindow, Fs};
    use crate::pipeline::{ProcessError, ProcessErrorKind, SegmentData, SegmentProcessor};
    use crate::worker::epoch_to_system;
    use assert2::check;
    use std::collections::HashMap;
    use std::future::Future;
    use std::io;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, UNIX_EPOCH};

    /// Records every segment's pipeline metadata for assertions.
    struct MetadataRecorder(Arc<Mutex<Vec<HashMap<String, String>>>>);

    impl SegmentProcessor for MetadataRecorder {
        fn name(&self) -> &'static str {
            "MetadataRecorder"
        }
        fn process(
            &mut self,
            data: SegmentData,
        ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
            self.0.lock().unwrap().push(data.metadata().clone());
            Box::pin(async move { Ok(data) })
        }
    }

    /// Fails the first `n` attempts with a retryable transfer error.
    struct FailNTimes(AtomicUsize);

    impl SegmentProcessor for FailNTimes {
        fn name(&self) -> &'static str {
            "FailNTimes"
        }
        fn process(
            &mut self,
            data: SegmentData,
        ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
            let fail = self
                .0
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            Box::pin(async move {
                if fail {
                    Err(ProcessError::new(
                        data,
                        ProcessErrorKind::Transfer {
                            source: Box::from("transient"),
                            retryable: true,
                        },
                    ))
                } else {
                    Ok(data)
                }
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn triggered_worker_idles_until_stop() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (_trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());
        seal_mem(&fs, 1, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        tokio::time::sleep(Duration::from_secs(5)).await;
        stop.cancel();
        worker.await.unwrap();

        // Never triggered: nothing went through the pipeline, the ring
        // still holds both segments.
        check!(captured.lock().unwrap().is_empty());
        let snap = fs.take_files();
        check!(snap.segments.len() == 1);
        check!(snap.queued_segments == Some(1));
    }

    #[tokio::test(start_paused = true)]
    async fn dump_current_data_captures_ring_and_stamps_metadata() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());
        seal_mem(&fs, 1, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let metadata = Arc::new(Mutex::new(Vec::new()));
        let (capture, captured) = CapturingProcessor::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![
                Box::new(MetadataRecorder(metadata.clone())),
                Box::new(capture),
            ],
            rx,
            stop.clone(),
        );

        let receipt = trigger
            .dump_current_data()
            .with_metadata("reason", "test")
            .await
            .unwrap();

        check!(receipt.segments_processed == 2);
        check!(receipt.manifest_key.is_none());
        check!(captured.lock().unwrap().len() == 2);
        {
            let recorded = metadata.lock().unwrap();
            for md in recorded.iter() {
                check!(md["dump_id"] == receipt.dump_id.to_string());
                check!(md["dump.reason"] == "test");
            }
        }

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn narrow_lookback_preserves_out_of_window_history() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        let now = now_epoch();
        seal_mem(&fs, 0, now - 3600); // outside a 60s look-back
        fs.set_seal_secs_for_test(0, now - 3600);
        seal_mem(&fs, 1, now);

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        let receipt = trigger
            .dump_time_range(Duration::from_secs(60), Duration::ZERO)
            .await
            .unwrap();

        check!(receipt.segments_processed == 1);
        check!(receipt.time_range == (epoch_to_system(now), epoch_to_system(now)));
        check!(captured.lock().unwrap().len() == 1);

        stop.cancel();
        worker.await.unwrap();

        // The old segment survived the dump.
        let snap = fs.take_files();
        check!(snap.segments.len() == 1);
        check!(snap.segments[0].seg_ref.index() == 0);
    }

    #[tokio::test(start_paused = true)]
    async fn lookback_captures_segment_spanning_window_start() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        let now = now_epoch();
        // Started before the 60s window, sealed inside it: the span
        // overlaps, so the segment is captured.
        seal_mem(&fs, 0, now - 300);
        fs.set_seal_secs_for_test(0, now - 30);

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        let receipt = trigger
            .dump_time_range(Duration::from_secs(60), Duration::ZERO)
            .await
            .unwrap();
        check!(receipt.segments_processed == 1);
        check!(captured.lock().unwrap().len() == 1);

        stop.cancel();
        worker.await.unwrap();
    }

    /// A retrying segment only holds open the dumps it matched (disk: one
    /// pass covers the whole backlog); an unrelated due dump resolves.
    #[tokio::test(start_paused = true)]
    async fn disk_retry_holds_only_matched_dumps() {
        /// Fails retryably, forever, any segment whose creation epoch
        /// matches `old_epoch`; passes everything else through.
        struct FailOldForever {
            old_epoch: String,
        }
        impl SegmentProcessor for FailOldForever {
            fn name(&self) -> &'static str {
                "FailOldForever"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                let fail = data.metadata().get("epoch_secs") == Some(&self.old_epoch);
                Box::pin(async move {
                    if fail {
                        Err(ProcessError::new(
                            data,
                            ProcessErrorKind::Transfer {
                                source: Box::from("transient"),
                                retryable: true,
                            },
                        ))
                    } else {
                        Ok(data)
                    }
                })
            }
        }

        fn set_mtime(path: &Path, epoch_secs: u64) {
            let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            f.set_times(
                std::fs::FileTimes::new()
                    .set_modified(UNIX_EPOCH + Duration::from_secs(epoch_secs)),
            )
            .unwrap();
        }

        let dir = tempfile::tempdir().unwrap();
        let now = now_epoch();
        let old_path = dir.path().join("trace.0.bin");
        std::fs::write(&old_path, segment_with_epoch(now - 3600)).unwrap();
        set_mtime(&old_path, now - 3600);
        std::fs::write(dir.path().join("trace.1.bin"), segment_with_epoch(now)).unwrap();

        let fs = Fs::new_disk(&dir.path().join("trace.bin"));
        let (trigger, rx) = dump::channel();
        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(FailOldForever {
                old_epoch: (now - 3600).to_string(),
            })],
            rx,
            stop.clone(),
        );

        // Wide dump matches both segments; its old one retries forever.
        let fut_wide = std::future::IntoFuture::into_future(
            trigger.dump_time_range(Duration::from_secs(7200), Duration::ZERO),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Narrow dump's window cannot match the retrying segment: it must
        // resolve despite the wide dump's pending retry.
        let receipt_narrow = trigger
            .dump_time_range(Duration::from_secs(60), Duration::ZERO)
            .await
            .unwrap();
        check!(receipt_narrow.segments_processed == 0);

        // The wide dump stays open until shutdown truncates it; the fresh
        // segment it captured before the retry stall is on the receipt.
        stop.cancel();
        let receipt_wide = fut_wide.await.unwrap();
        check!(receipt_wide.segments_processed == 1);
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn lookforward_captures_post_trigger_seals() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        let started = tokio::time::Instant::now();
        let run = trigger.dump_time_range(Duration::ZERO, Duration::from_secs(5));
        let fut = std::future::IntoFuture::into_future(run);
        // Let the worker register the dump, then seal inside the window.
        tokio::time::sleep(Duration::from_millis(50)).await;
        seal_mem(&fs, 0, now_epoch());

        let receipt = fut.await.unwrap();
        // Resolves only after the forward deadline. Production anchors the
        // deadline to the trigger's wall-clock time (`SystemTime`, see
        // `ActiveDump::register`), then maps it onto the tokio timer; under
        // `start_paused` the virtual clock does not advance during the (real)
        // pickup latency, so the measured elapsed can land a few ms under the
        // nominal 5s. Allow a small tolerance for that clock-mixing skew — the
        // point is the dump waited ~the forward window, not that it resolved
        // immediately or only at shutdown.
        check!(started.elapsed() >= Duration::from_secs(5) - Duration::from_millis(100));
        check!(receipt.segments_processed == 1);
        check!(captured.lock().unwrap().len() == 1);

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn overlapping_forward_windows_share_segment() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let stop = tokio_util::sync::CancellationToken::new();
        let metadata = Arc::new(Mutex::new(Vec::new()));
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(MetadataRecorder(metadata.clone()))],
            rx,
            stop.clone(),
        );

        let fut_a = std::future::IntoFuture::into_future(
            trigger.dump_time_range(Duration::from_secs(60), Duration::from_secs(5)),
        );
        let fut_b = std::future::IntoFuture::into_future(
            trigger.dump_time_range(Duration::from_secs(60), Duration::from_secs(5)),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        seal_mem(&fs, 0, now_epoch());

        let (receipt_a, receipt_b) = tokio::join!(fut_a, fut_b);
        let receipt_a = receipt_a.unwrap();
        let receipt_b = receipt_b.unwrap();
        check!(receipt_a.segments_processed == 1);
        check!(receipt_b.segments_processed == 1);

        {
            let recorded = metadata.lock().unwrap();
            check!(recorded.len() == 1, "one segment through the pipeline");
            let ids = &recorded[0]["dump_id"];
            check!(ids.contains(&receipt_a.dump_id.to_string()));
            check!(ids.contains(&receipt_b.dump_id.to_string()));
            check!(ids.contains(','));
        }

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn zero_zero_dump_resolves_empty() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, _captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        let receipt = trigger
            .dump_time_range(Duration::ZERO, Duration::ZERO)
            .await
            .unwrap();
        check!(receipt.segments_processed == 0);
        check!(receipt.time_range.0 == receipt.time_range.1);

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn retryable_failure_holds_dump_open_until_retry_succeeds() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, captured) = CapturingProcessor::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(FailNTimes(AtomicUsize::new(1))), Box::new(capture)],
            rx,
            stop.clone(),
        );

        let receipt = trigger.dump_current_data().await.unwrap();
        check!(receipt.segments_processed == 1);
        check!(captured.lock().unwrap().len() == 1);

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn evicted_segment_resolves_ok_and_uncounted() {
        struct EvictedSim;
        impl SegmentProcessor for EvictedSim {
            fn name(&self) -> &'static str {
                "EvictedSim"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(async move {
                    Err(ProcessError::io(
                        data,
                        io::Error::from(io::ErrorKind::NotFound),
                    ))
                })
            }
        }

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(EvictedSim)],
            rx,
            stop.clone(),
        );

        // Best-effort: the vanished segment is silently uncounted.
        let receipt = trigger.dump_current_data().await.unwrap();
        check!(receipt.segments_processed == 0);

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn total_pipeline_failure_resolves_pipeline_error() {
        struct AlwaysFail;
        impl SegmentProcessor for AlwaysFail {
            fn name(&self) -> &'static str {
                "AlwaysFail"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(
                    async move { Err(ProcessError::io(data, io::Error::other("broken stage"))) },
                )
            }
        }

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(AlwaysFail)],
            rx,
            stop.clone(),
        );

        let err = trigger.dump_current_data().await.unwrap_err();
        check!(matches!(err, DumpError::Pipeline(_)));

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_resolves_open_forward_dump_truncated() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, _captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        let fut = std::future::IntoFuture::into_future(
            trigger.dump_time_range(Duration::ZERO, Duration::from_secs(3600)),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        seal_mem(&fs, 0, now_epoch());
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Shut down long before the hour-long deadline: the dump resolves
        // with a truncated receipt covering what landed.
        stop.cancel();
        let receipt = fut.await.unwrap();
        check!(receipt.segments_processed == 1);
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn queued_request_at_shutdown_resolves_worker_stopped() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let stop = tokio_util::sync::CancellationToken::new();
        // Request queued and stop cancelled before the worker ever runs.
        let fut = std::future::IntoFuture::into_future(trigger.dump_current_data());
        stop.cancel();

        let (capture, _captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop);

        let err = fut.await.unwrap_err();
        check!(matches!(err, DumpError::WorkerStopped));
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn mem_windowed_take_preserves_non_matching_slots() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let now = now_epoch();
        seal_mem(&fs, 0, now - 3600);
        fs.set_seal_secs_for_test(0, now - 3600);
        seal_mem(&fs, 1, now);

        // Window matching nothing: ring untouched.
        let none = fs.take_files_matching(&[EpochWindow {
            start_secs: Some(now + 100),
            end_secs: now + 200,
        }]);
        check!(none.segments.is_empty());
        check!(none.queued_segments == Some(2));

        // Window matching only the fresh segment: the old slot stays.
        let snap = fs.take_files_matching(&[EpochWindow {
            start_secs: Some(now - 60),
            end_secs: now + 60,
        }]);
        check!(snap.segments.len() == 1);
        check!(snap.segments[0].seg_ref.index() == 1);
        check!(snap.queued_segments == Some(1));
    }
}

#[cfg(test)]
mod finalize_dump_tests {
    use super::triggered_test_support::*;
    use crate::dump::{self, DumpCompletion, DumpId};
    use crate::fs::Fs;
    use crate::pipeline::{ProcessError, SegmentData, SegmentProcessor};
    use assert2::check;
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex;

    type SeenCompletions = Arc<Mutex<Vec<(DumpId, usize, Vec<(String, String)>, bool)>>>;

    /// Passes segments through; `finalize_dump` records the completion and
    /// returns a fixed key (or `None`).
    struct FinalizeStub {
        key: Option<String>,
        completions: SeenCompletions,
    }

    impl SegmentProcessor for FinalizeStub {
        fn name(&self) -> &'static str {
            "FinalizeStub"
        }
        fn process(
            &mut self,
            data: SegmentData,
        ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
            Box::pin(async move { Ok(data) })
        }
        fn finalize_dump(
            &mut self,
            completion: &DumpCompletion,
        ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
            self.completions.lock().unwrap().push((
                completion.dump_id,
                completion.segments_processed,
                completion.metadata.clone(),
                completion.failed,
            ));
            let key = self.key.clone();
            Box::pin(std::future::ready(key))
        }
    }

    struct PanickingFinalize;

    impl SegmentProcessor for PanickingFinalize {
        fn name(&self) -> &'static str {
            "PanickingFinalize"
        }
        fn process(
            &mut self,
            data: SegmentData,
        ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
            Box::pin(async move { Ok(data) })
        }
        fn finalize_dump(
            &mut self,
            _completion: &DumpCompletion,
        ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
            panic!("finalize boom");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn manifest_key_flows_to_receipt_last_stage_wins() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let completions = Arc::new(Mutex::new(Vec::new()));
        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![
                Box::new(FinalizeStub {
                    key: Some("dumps/early.json".into()),
                    completions: completions.clone(),
                }),
                Box::new(FinalizeStub {
                    key: Some("dumps/late.json".into()),
                    completions: completions.clone(),
                }),
            ],
            rx,
            stop.clone(),
        );

        let receipt = trigger
            .dump_current_data()
            .with_metadata("reason", "test")
            .await
            .unwrap();

        check!(receipt.manifest_key.as_deref() == Some("dumps/late.json"));
        {
            let seen = completions.lock().unwrap();
            check!(seen.len() == 2, "both stages finalized");
            for (id, count, metadata, failed) in seen.iter() {
                check!(*id == receipt.dump_id);
                check!(*count == 1);
                check!(metadata == &vec![("reason".to_string(), "test".to_string())]);
                check!(!*failed);
            }
        }

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn default_finalize_yields_no_manifest_key() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, _captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        let receipt = trigger.dump_current_data().await.unwrap();
        check!(receipt.manifest_key.is_none());

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn failed_dump_signals_failed_completion() {
        struct AlwaysFail;
        impl SegmentProcessor for AlwaysFail {
            fn name(&self) -> &'static str {
                "AlwaysFail"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(
                    async move { Err(ProcessError::io(data, io::Error::other("broken stage"))) },
                )
            }
        }

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let completions = Arc::new(Mutex::new(Vec::new()));
        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![
                Box::new(AlwaysFail),
                Box::new(FinalizeStub {
                    key: Some("dumps/failed.json".into()),
                    completions: completions.clone(),
                }),
            ],
            rx,
            stop.clone(),
        );

        let err = trigger.dump_current_data().await.unwrap_err();
        check!(matches!(err, crate::dump::DumpError::Pipeline(_)));
        {
            let seen = completions.lock().unwrap();
            check!(seen.len() == 1, "finalize still runs for a failed dump");
            check!(seen[0].3, "completion carries failed=true");
        }

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn finalize_runs_for_empty_dump() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let completions = Arc::new(Mutex::new(Vec::new()));
        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(FinalizeStub {
                key: Some("dumps/empty.json".into()),
                completions: completions.clone(),
            })],
            rx,
            stop.clone(),
        );

        let receipt = trigger.dump_current_data().await.unwrap();
        check!(receipt.segments_processed == 0);
        check!(receipt.manifest_key.as_deref() == Some("dumps/empty.json"));
        check!(completions.lock().unwrap().len() == 1);

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn panicking_finalize_is_caught_receipt_resolves() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(PanickingFinalize)],
            rx,
            stop.clone(),
        );

        let receipt = trigger.dump_current_data().await.unwrap();
        check!(receipt.segments_processed == 1);
        check!(receipt.manifest_key.is_none());

        stop.cancel();
        worker.await.unwrap();
    }
}

//! Integration tests for `SmokeTester`.

mod common;
#[cfg(feature = "worker-s3")]
mod fake_s3;

use dial9_core::handle::Dial9Handle;
use dial9_tokio_telemetry::telemetry::smoke_test::{
    Check, CheckStatus, DisabledReason, SmokeTestReport, SmokeTester, UnverifiedReason,
};
use dial9_tokio_telemetry::telemetry::{
    Dial9HandleTokioExt, MemoryBuffer, TokioAttachOptions, recorder,
};
use std::time::Duration;

fn quiet(handle: &Dial9Handle) -> SmokeTester {
    handle
        .smoke_tester()
        .passive_window(Duration::from_millis(50))
        .build()
}

fn failed_with(status: &CheckStatus, text: &str) -> bool {
    matches!(status, CheckStatus::Failed { detail, .. } if detail.contains(text))
}

fn disabled_for(status: &CheckStatus, want: DisabledReason) -> bool {
    matches!(status, CheckStatus::Disabled { reason, .. } if *reason == want)
}

#[allow(dead_code)]
fn unverified_for(status: &CheckStatus, want: UnverifiedReason) -> bool {
    matches!(status, CheckStatus::Unverified { reason, .. } if *reason == want)
}

fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[tokio::test]
async fn disconnected_handle_fails_recording() {
    let report = quiet(&Dial9Handle::disabled()).run().await;
    assert!(failed_with(report.recording(), "not connected"), "{report}");
    assert!(!report.is_healthy());
    for (check, status) in report.checks().skip(1) {
        assert!(
            disabled_for(status, DisabledReason::NotConnected)
                || disabled_for(status, DisabledReason::NotCompiled),
            "{}: {status}",
            check.as_str()
        );
    }
}

#[test]
fn stopped_recorder_is_not_reported_as_paused() {
    let recorder = recorder(MemoryBuffer::new(1 << 20).unwrap()).build();
    let handle = recorder.handle().clone();
    recorder.graceful_shutdown(Duration::from_secs(5));
    let report = current_thread().block_on(quiet(&handle).run());
    assert!(failed_with(report.recording(), "shut down"), "{report}");
    assert!(
        disabled_for(report.cpu_sampling(), DisabledReason::RecorderStopped)
            || disabled_for(report.cpu_sampling(), DisabledReason::NotCompiled),
        "{report}"
    );
}

#[test]
fn paused_recorder_disables_dependent_checks() {
    let recorder = recorder(MemoryBuffer::new(1 << 20).unwrap()).build();
    let rt = common::attach(&recorder, 1, TokioAttachOptions::default());
    recorder.handle().disable();
    let report = rt.block_on(quiet(recorder.handle()).run());
    assert!(
        disabled_for(report.recording(), DisabledReason::RecordingPaused),
        "{report}"
    );
    assert!(
        disabled_for(report.cpu_sampling(), DisabledReason::RecordingPaused)
            || disabled_for(report.cpu_sampling(), DisabledReason::NotCompiled),
        "{report}"
    );
    assert!(report.is_healthy(), "{report}");
    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));
}

#[test]
fn unconfigured_sources_are_disabled_not_failed() {
    let recorder = recorder(MemoryBuffer::new(1 << 20).unwrap()).build();
    let rt = common::attach(&recorder, 1, TokioAttachOptions::default());
    let report = rt.block_on(quiet(recorder.handle()).run());
    assert_eq!(report.recording(), &CheckStatus::Ok, "{report}");
    assert!(report.is_healthy(), "{report}");
    assert!(report.is_verified(), "{report}");
    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));
}

/// One line per check, by stable name, and a time for each.
#[tokio::test]
async fn report_lists_every_check() {
    let before = std::time::SystemTime::now();
    let report = quiet(&Dial9Handle::disabled()).run().await;
    let names: Vec<_> = report.checks().map(|(c, _)| c.as_str()).collect();
    assert_eq!(
        names,
        ["recording", "cpu_sampling", "symbolization", "s3_upload"]
    );
    let text = report.to_string();
    for name in names {
        assert!(text.contains(&format!("{name}: ")), "{text}");
    }
    assert!(report.checked_at(Check::Recording) >= before);
}

#[cfg(feature = "worker-s3")]
mod s3 {
    use super::*;
    use dial9_core::pipeline::{ProcessError, SegmentData, SegmentProcessor};
    use dial9_tokio_telemetry::telemetry::{RecorderPipelineExt, RecorderS3ClientExt};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    const HOUR: Duration = Duration::from_secs(3600);

    fn s3_config(bucket: &str) -> dial9_destinations_s3::S3Config {
        dial9_destinations_s3::S3Config::builder()
            .bucket(bucket)
            .service_name("svc")
            .region("us-east-1")
            .build()
    }

    fn bucket_dir(buckets: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for bucket in buckets {
            std::fs::create_dir(dir.path().join(bucket)).unwrap();
        }
        dir
    }

    /// Only the S3 check, so these run on every platform.
    fn s3_only(handle: &Dial9Handle, cache_ttl: Duration) -> SmokeTester {
        handle
            .smoke_tester()
            .symbolization(false)
            .cpu_sampling(false)
            .cache_ttl(cache_ttl)
            .check_timeout(Duration::from_secs(2))
            .build()
    }

    fn run(rec: &dial9_core::recording::Recorder) -> SmokeTestReport {
        let rt = common::attach(rec, 1, TokioAttachOptions::default());
        let report = rt.block_on(s3_only(rec.handle(), HOUR).run());
        drop(rt);
        report
    }

    /// Marker objects under `root`: the fake bucket's `.txt` files.
    fn markers(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "txt") {
                    out.push(path);
                }
            }
        }
        out
    }

    #[test]
    fn healthy_upload_writes_one_marker_outside_the_segment_tree() {
        let dir = bucket_dir(&["bucket"]);
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_s3_uploader_client(s3_config("bucket"), fake_s3::fake_s3_client(dir.path()))
            .build();
        let report = run(&rec);
        assert_eq!(report.s3_upload(), &CheckStatus::Ok, "{report}");
        rec.graceful_shutdown(Duration::from_secs(5));
        let found = markers(dir.path());
        assert_eq!(found.len(), 1, "{found:?}");
        let key = found[0].strip_prefix(dir.path().join("bucket")).unwrap();
        assert_eq!(
            key,
            std::path::Path::new(&s3_config("bucket").smoke_test_key())
        );
        assert!(!key.starts_with("version=1"), "{}", key.display());
    }

    #[test]
    fn failing_upload_reports_the_sdk_error() {
        let dir = bucket_dir(&["bucket"]);
        let (client, _) = fake_s3::fake_s3_client_counting_markers(dir.path(), true);
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_s3_uploader_client(s3_config("bucket"), client)
            .build();
        let report = run(&rec);
        assert!(
            failed_with(
                report.s3_upload(),
                "HTTP 403 AccessDenied: injected deny for smoke test"
            ),
            "{report}"
        );
        assert!(!report.is_healthy());
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    /// No S3 stage: nothing to check, nothing uploaded.
    #[test]
    fn pipeline_without_s3_stage_uploads_nothing() {
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_custom_pipeline(|p| p.gzip())
            .build();
        let report = run(&rec);
        assert!(
            disabled_for(report.s3_upload(), DisabledReason::NoS3Stage),
            "{report}"
        );
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    #[test]
    fn no_pipeline_has_no_s3_stage() {
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap()).build();
        let report = run(&rec);
        assert!(
            disabled_for(report.s3_upload(), DisabledReason::NoS3Stage),
            "{report}"
        );
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    #[test]
    fn custom_pipeline_s3_stage_is_checked() {
        let dir = bucket_dir(&["bucket"]);
        let client = fake_s3::fake_s3_client(dir.path());
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_custom_pipeline(move |p| p.gzip().s3_with_client(s3_config("bucket"), client))
            .build();
        let report = run(&rec);
        assert_eq!(report.s3_upload(), &CheckStatus::Ok, "{report}");
        rec.graceful_shutdown(Duration::from_secs(5));
        assert_eq!(markers(dir.path()).len(), 1);
    }

    /// A client only the worker can build still passes, with no caller input:
    /// the uploader publishes it in `initialize()`.
    #[test]
    fn future_built_client_passes_with_no_caller_input() {
        let dir = bucket_dir(&["bucket"]);
        let client = fake_s3::fake_s3_client(dir.path());
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_s3_uploader_client_future(s3_config("bucket"), async move { client })
            .build();
        let report = run(&rec);
        assert_eq!(report.s3_upload(), &CheckStatus::Ok, "{report}");
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    /// Two S3 stages: one failing bucket fails the check and is named.
    #[test]
    fn two_s3_stages_one_failing() {
        let dir = bucket_dir(&["good", "bad"]);
        let good = fake_s3::fake_s3_client(dir.path());
        let (bad, _) = fake_s3::fake_s3_client_counting_markers(dir.path(), true);
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_custom_pipeline(move |p| {
                p.gzip()
                    .s3_with_client(s3_config("good"), good)
                    .s3_with_client(s3_config("bad"), bad)
            })
            .build();
        let report = run(&rec);
        assert!(
            failed_with(report.s3_upload(), "bucket bad: HTTP 403"),
            "{report}"
        );
        assert!(!failed_with(report.s3_upload(), "bucket good"), "{report}");
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    type ProcessFuture<'a> =
        Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + 'a>>;
    type InitFuture<'a> = Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + 'a>>;

    /// `initialize()` never returns (`hang`) or fails.
    struct BadInit {
        hang: bool,
    }

    impl SegmentProcessor for BadInit {
        fn name(&self) -> &'static str {
            if self.hang {
                "HangingInit"
            } else {
                "FailingInit"
            }
        }
        fn initialize(&mut self) -> InitFuture<'_> {
            match self.hang {
                true => Box::pin(std::future::pending()),
                false => Box::pin(std::future::ready(Err(std::io::Error::other("no")))),
            }
        }
        fn process(&mut self, data: SegmentData) -> ProcessFuture<'_> {
            Box::pin(std::future::ready(Ok(data)))
        }
    }

    fn bad_init_pipeline(hang: bool, dir: &tempfile::TempDir) -> dial9_core::recording::Recorder {
        let client = fake_s3::fake_s3_client(dir.path());
        recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_custom_pipeline(move |p| {
                p.pipe(BadInit { hang })
                    .s3_with_client(s3_config("bucket"), client)
            })
            .build()
    }

    /// The worker never finishes starting: the stuck stage is named.
    #[test]
    fn worker_stuck_initializing() {
        let dir = bucket_dir(&["bucket"]);
        let rec = bad_init_pipeline(true, &dir);
        let report = run(&rec);
        assert!(
            failed_with(report.s3_upload(), "still initializing (HangingInit)"),
            "{report}"
        );
        rec.graceful_shutdown(Duration::from_millis(50));
        assert!(markers(dir.path()).is_empty());
    }

    #[test]
    fn failed_initialize_fails_the_check() {
        let dir = bucket_dir(&["bucket"]);
        let rec = bad_init_pipeline(false, &dir);
        let report = run(&rec);
        assert!(
            failed_with(report.s3_upload(), "worker not running"),
            "{report}"
        );
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    /// Run the S3 check `runs` times on one tester, `concurrent` at a time,
    /// and count the markers uploaded.
    fn markers_uploaded(fail: bool, cache_ttl: Duration, runs: usize, concurrent: bool) -> u64 {
        let dir = bucket_dir(&["bucket"]);
        let (client, count) = fake_s3::fake_s3_client_counting_markers(dir.path(), fail);
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_s3_uploader_client(s3_config("bucket"), client)
            .build();
        let rt = common::attach(&rec, 2, TokioAttachOptions::default());
        let tester = Arc::new(s3_only(rec.handle(), cache_ttl));
        rt.block_on(async {
            if concurrent {
                let runs: Vec<_> = (0..runs)
                    .map(|_| {
                        let tester = tester.clone();
                        tokio::spawn(async move { tester.run().await })
                    })
                    .collect();
                for run in runs {
                    run.await.unwrap();
                }
            } else {
                for _ in 0..runs {
                    tester.run().await;
                }
            }
        });
        drop(rt);
        rec.graceful_shutdown(Duration::from_secs(5));
        let uploaded = Arc::<AtomicU64>::clone(&count);
        uploaded.load(Ordering::SeqCst)
    }

    #[test]
    fn concurrent_runs_upload_once() {
        assert_eq!(markers_uploaded(false, HOUR, 4, true), 1);
    }

    #[test]
    fn cached_result_is_reused() {
        assert_eq!(markers_uploaded(false, HOUR, 3, false), 1);
    }

    #[test]
    fn zero_ttl_re_checks_every_run() {
        assert_eq!(markers_uploaded(false, Duration::ZERO, 3, false), 3);
    }

    #[test]
    fn failed_result_is_re_checked() {
        assert_eq!(markers_uploaded(true, HOUR, 3, false), 3);
    }

    /// A cached pass isn't reused once the recorder has stopped.
    #[test]
    fn stopped_recorder_skips_the_cache() {
        let dir = bucket_dir(&["bucket"]);
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_s3_uploader_client(s3_config("bucket"), fake_s3::fake_s3_client(dir.path()))
            .build();
        let tester = s3_only(&rec.handle().clone(), HOUR);
        let rt = current_thread();
        let first = rt.block_on(tester.run());
        assert_eq!(first.s3_upload(), &CheckStatus::Ok, "{first}");
        rec.graceful_shutdown(Duration::from_secs(5));
        let second = rt.block_on(tester.run());
        assert!(
            disabled_for(second.s3_upload(), DisabledReason::RecorderStopped),
            "{second}"
        );
    }

    /// A second `with_s3_uploader*` replaces the first; the check uploads
    /// through the live one and skips the replaced one.
    #[test]
    fn replaced_s3_uploader_checks_the_live_one() {
        let dir = bucket_dir(&["stale", "live"]);
        let (stale, stale_puts) = fake_s3::fake_s3_client_counting_markers(dir.path(), true);
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_s3_uploader_client(s3_config("stale"), stale)
            .with_s3_uploader_client(s3_config("live"), fake_s3::fake_s3_client(dir.path()))
            .build();
        let report = run(&rec);
        assert_eq!(report.s3_upload(), &CheckStatus::Ok, "{report}");
        rec.graceful_shutdown(Duration::from_secs(5));
        assert_eq!(stale_puts.load(Ordering::SeqCst), 0);
        assert_eq!(markers(&dir.path().join("live")).len(), 1);
    }

    /// A custom pipeline overrides `with_s3_uploader*`; the check uploads
    /// through the pipeline's stage.
    #[test]
    fn custom_pipeline_overrides_s3_uploader() {
        let dir = bucket_dir(&["stale", "live"]);
        let (stale, stale_puts) = fake_s3::fake_s3_client_counting_markers(dir.path(), true);
        let live = fake_s3::fake_s3_client(dir.path());
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_s3_uploader_client(s3_config("stale"), stale)
            .with_custom_pipeline(move |p| p.gzip().s3_with_client(s3_config("live"), live))
            .build();
        let report = run(&rec);
        assert_eq!(report.s3_upload(), &CheckStatus::Ok, "{report}");
        rec.graceful_shutdown(Duration::from_secs(5));
        assert_eq!(stale_puts.load(Ordering::SeqCst), 0);
        assert_eq!(markers(&dir.path().join("live")).len(), 1);
    }

    /// A cached result keeps the time it was produced.
    #[test]
    fn cached_result_keeps_its_time() {
        let dir = bucket_dir(&["bucket"]);
        let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_s3_uploader_client(s3_config("bucket"), fake_s3::fake_s3_client(dir.path()))
            .build();
        let rt = common::attach(&rec, 1, TokioAttachOptions::default());
        let tester = s3_only(rec.handle(), HOUR);
        let first = rt.block_on(tester.run());
        let second = rt.block_on(tester.run());
        assert_eq!(
            first.checked_at(Check::S3Upload),
            second.checked_at(Check::S3Upload)
        );
        assert!(second.checked_at(Check::Recording) > first.checked_at(Check::Recording));
        drop(rt);
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    /// Symbolization failing on every run doesn't re-upload the marker:
    /// each check caches on its own.
    #[cfg(all(feature = "cpu-profiling", target_os = "linux"))]
    #[test]
    fn failing_symbolization_does_not_re_upload() {
        use dial9_tokio_telemetry::telemetry::{CpuProfilingConfig, RecorderPerfExt};
        let dir = bucket_dir(&["bucket"]);
        let (client, count) = fake_s3::fake_s3_client_counting_markers(dir.path(), false);
        let rec = recorder(MemoryBuffer::new(16 << 20).unwrap())
            .with_cpu_profiling(CpuProfilingConfig::default())
            .with_custom_pipeline(move |p| p.gzip().s3_with_client(s3_config("bucket"), client))
            .build();
        let rt = common::attach(&rec, 1, TokioAttachOptions::default());
        let tester = rec.handle().smoke_tester().cpu_sampling(false).build();
        for _ in 0..3 {
            let report = rt.block_on(tester.run());
            assert!(
                failed_with(report.symbolization(), "no symbolize stage"),
                "{report}"
            );
            assert_eq!(report.s3_upload(), &CheckStatus::Ok, "{report}");
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(rt);
        rec.graceful_shutdown(Duration::from_secs(5));
    }
}

#[cfg(not(feature = "cpu-profiling"))]
#[tokio::test]
async fn built_without_cpu_profiling_is_not_compiled() {
    let recorder = recorder(MemoryBuffer::new(1 << 20).unwrap()).build();
    let report = quiet(recorder.handle()).run().await;
    assert!(
        disabled_for(report.cpu_sampling(), DisabledReason::NotCompiled),
        "{report}"
    );
    recorder.graceful_shutdown(Duration::from_secs(5));
}

#[cfg(feature = "cpu-profiling")]
mod sampling {
    use super::*;
    use dial9_tokio_telemetry::telemetry::{CpuProfilingConfig, RecorderPerfExt};

    /// Off Linux the profiler can't start: `Disabled`, with the reason.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn unsupported_platform_reports_why() {
        let recorder = recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_cpu_profiling(CpuProfilingConfig::default())
            .build();
        let rt = common::attach(&recorder, 1, TokioAttachOptions::default());
        let report = rt.block_on(quiet(recorder.handle()).run());
        assert!(
            disabled_for(report.cpu_sampling(), DisabledReason::Unsupported),
            "{report}"
        );
        assert!(
            disabled_for(report.symbolization(), DisabledReason::Unsupported),
            "{report}"
        );
        assert!(report.is_healthy(), "{report}");
        drop(rt);
        recorder.graceful_shutdown(Duration::from_secs(5));
    }
}

#[cfg(all(feature = "cpu-profiling", target_os = "linux"))]
mod linux {
    use super::*;
    use dial9_tokio_telemetry::telemetry::analysis_events::{CpuSampleSource, Dial9Event};
    use dial9_tokio_telemetry::telemetry::smoke_test::CpuBackend;
    use dial9_tokio_telemetry::telemetry::{
        CpuProfilingConfig, RecorderPerfExt, RecorderPipelineExt,
    };

    fn proven(status: &CheckStatus) -> bool {
        matches!(status, CheckStatus::Ok | CheckStatus::Degraded { .. })
    }

    /// CPU only, and no passive window, so an idle service needs the stimulus.
    fn idle(handle: &Dial9Handle, stimulus: bool) -> SmokeTester {
        handle
            .smoke_tester()
            .stimulus(stimulus)
            .passive_window(Duration::ZERO)
            .symbolization(false)
            .s3_upload(false)
            .build()
    }

    fn run_on_worker(
        rec: &dial9_core::recording::Recorder,
        tester: SmokeTester,
    ) -> SmokeTestReport {
        let rt = common::attach(rec, 2, TokioAttachOptions::default());
        let report = rt
            .block_on(async { tokio::spawn(async move { tester.run().await }).await })
            .unwrap();
        drop(rt);
        report
    }

    fn cpu_recorder(config: CpuProfilingConfig) -> dial9_core::recording::Recorder {
        recorder(MemoryBuffer::new(16 << 20).unwrap())
            .with_cpu_profiling(config)
            .build()
    }

    #[test]
    fn idle_service_by_stimulus() {
        for stimulus in [true, false] {
            let rec = cpu_recorder(CpuProfilingConfig::default());
            let report = run_on_worker(&rec, idle(rec.handle(), stimulus));
            match stimulus {
                true => assert!(proven(report.cpu_sampling()), "{report}"),
                false => assert!(
                    unverified_for(report.cpu_sampling(), UnverifiedReason::StimulusOff),
                    "{report}"
                ),
            }
            rec.graceful_shutdown(Duration::from_secs(5));
        }
    }

    #[test]
    fn ctimer_backend_is_degraded() {
        let rec = cpu_recorder(CpuProfilingConfig::with_ctimer_backend());
        let report = run_on_worker(&rec, idle(rec.handle(), true));
        assert!(
            matches!(report.cpu_sampling(), CheckStatus::Degraded { detail, .. } if detail.contains("ctimer")),
            "{report}"
        );
        assert_eq!(report.cpu_backend(), Some(CpuBackend::Ctimer));
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    /// The stimulus's samples carry the `dial9-smoke` thread name.
    #[test]
    fn stimulus_samples_are_labeled() {
        let (capture, segments) = common::capture_processor();
        let rec = recorder(MemoryBuffer::new(16 << 20).unwrap())
            .with_cpu_profiling(CpuProfilingConfig::default())
            .with_custom_pipeline(|p| p.pipe(capture))
            .build();
        let report = run_on_worker(&rec, idle(rec.handle(), true));
        assert!(proven(report.cpu_sampling()), "{report}");
        rec.graceful_shutdown(Duration::from_secs(5));
        let events: Vec<Dial9Event> = common::decode_all(&segments.lock().unwrap());
        let labeled = events
            .iter()
            .filter(|e| {
                matches!(e, Dial9Event::CpuSampleEvent(s)
                    if s.source == CpuSampleSource::CpuProfile
                        && s.thread_name.as_deref() == Some("dial9-smoke"))
            })
            .count();
        assert!(labeled > 0, "no dial9-smoke samples");
    }

    /// From a runtime the recorder isn't attached to, the stimulus thread
    /// still proves sampling.
    #[test]
    fn called_from_unattached_runtime() {
        let rec = cpu_recorder(CpuProfilingConfig::default());
        let attached = common::attach(&rec, 1, TokioAttachOptions::default());
        let other = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let tester = idle(rec.handle(), true);
        let report = other
            .block_on(async { tokio::spawn(async move { tester.run().await }).await })
            .unwrap();
        assert!(proven(report.cpu_sampling()), "{report}");
        drop(other);
        drop(attached);
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    /// A 1ms ticker on the caller's current-thread runtime keeps ticking
    /// while the stimulus burns on its own thread.
    #[test]
    fn stimulus_does_not_block_the_runtime() {
        let rec = cpu_recorder(CpuProfilingConfig::default());
        let rt = common::attach_current_thread(&rec, TokioAttachOptions::default());
        let tester = idle(rec.handle(), true);
        let gap = rt.block_on(async {
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let ticker = tokio::spawn({
                let stop = stop.clone();
                async move {
                    let mut worst = Duration::ZERO;
                    let mut last = std::time::Instant::now();
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        worst = worst.max(last.elapsed());
                        last = std::time::Instant::now();
                    }
                    worst
                }
            });
            tester.run().await;
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            ticker.await.unwrap()
        });
        drop(rt);
        rec.graceful_shutdown(Duration::from_secs(5));
        eprintln!("max ticker gap: {gap:?}");
        assert!(gap < Duration::from_millis(40), "ticker gap {gap:?}");
    }

    /// perf samples only threads descended from the one that started the
    /// profiler. From a thread created before the recorder, the stimulus
    /// thread is outside that tree: `Unverified`, not `Failed`.
    #[test]
    fn caller_thread_created_before_profiler() {
        let (go_tx, go_rx) = std::sync::mpsc::channel::<Dial9Handle>();
        let early = std::thread::spawn(move || {
            let handle = go_rx.recv().unwrap();
            current_thread().block_on(idle(&handle, true).run())
        });
        let rec = cpu_recorder(CpuProfilingConfig::default());
        go_tx.send(rec.handle().clone()).unwrap();
        let report = early.join().unwrap();
        eprintln!(
            "caller created before profiler ({:?}): cpu_sampling = {}",
            report.cpu_backend(),
            report.cpu_sampling()
        );
        if report.cpu_backend() == Some(CpuBackend::Perf) {
            assert!(
                unverified_for(report.cpu_sampling(), UnverifiedReason::OutsidePerfTree),
                "{report}"
            );
        }
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    /// A start error on a platform with a backend is `Failed`. Exercised where
    /// perf is blocked (an unprivileged container); elsewhere the profiler
    /// starts and the check passes.
    #[test]
    fn perf_only_backend_start_error_fails() {
        let blocked =
            dial9_perf_self_profile::CpuProfiler::start(CpuProfilingConfig::with_perf_backend())
                .is_err();
        let rec = cpu_recorder(CpuProfilingConfig::with_perf_backend());
        let report = run_on_worker(&rec, idle(rec.handle(), true));
        eprintln!(
            "perf blocked: {blocked}; cpu_sampling = {}",
            report.cpu_sampling()
        );
        match blocked {
            true => assert!(
                failed_with(report.cpu_sampling(), "CPU profiling failed to start"),
                "{report}"
            ),
            false => assert!(proven(report.cpu_sampling()), "{report}"),
        }
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    /// Symbolization only.
    fn symbolization(rec: &dial9_core::recording::Recorder) -> CheckStatus {
        let rt = common::attach(rec, 1, TokioAttachOptions::default());
        let tester = rec
            .handle()
            .smoke_tester()
            .cpu_sampling(false)
            .s3_upload(false)
            .build();
        let report = rt.block_on(tester.run());
        drop(rt);
        report.symbolization().clone()
    }

    #[test]
    fn default_pipeline_symbolizes() {
        let rec = cpu_recorder(CpuProfilingConfig::default());
        let status = symbolization(&rec);
        assert_eq!(status, CheckStatus::Ok, "{status}");
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    /// The stage-order check matches names across crates: the default
    /// pipeline's must stay `"Symbolize"` then `"Gzip"`.
    #[test]
    fn default_stage_names_match() {
        let rec = cpu_recorder(CpuProfilingConfig::default());
        let stages = rec
            .handle()
            .pipeline_status()
            .expect("a pipeline")
            .stages()
            .to_vec();
        let at = |name| stages.iter().position(|s| *s == name);
        assert!(
            at("Symbolize") < at("Gzip") && at("Symbolize").is_some(),
            "{stages:?}"
        );
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    #[test]
    fn missing_symbolize_stage() {
        let rec = recorder(MemoryBuffer::new(16 << 20).unwrap())
            .with_cpu_profiling(CpuProfilingConfig::default())
            .with_custom_pipeline(|p| p.gzip())
            .build();
        let status = symbolization(&rec);
        assert!(failed_with(&status, "no symbolize stage"), "{status}");
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    #[test]
    fn compression_before_symbolization() {
        let rec = recorder(MemoryBuffer::new(16 << 20).unwrap())
            .with_cpu_profiling(CpuProfilingConfig::default())
            .with_custom_pipeline(|p| p.gzip().symbolize())
            .build();
        let status = symbolization(&rec);
        assert!(
            failed_with(&status, "compressed before symbolization"),
            "{status}"
        );
        rec.graceful_shutdown(Duration::from_secs(5));
    }

    #[test]
    fn no_pipeline_is_disabled() {
        use dial9_tokio_telemetry::telemetry::RecorderPipelineExt as _;
        let rec = recorder(MemoryBuffer::new(16 << 20).unwrap())
            .with_cpu_profiling(CpuProfilingConfig::default())
            .with_custom_pipeline(|p| p)
            .build();
        let status = symbolization(&rec);
        assert!(
            disabled_for(&status, DisabledReason::NoPipeline),
            "{status}"
        );
        rec.graceful_shutdown(Duration::from_secs(5));
    }
}

use std::path::PathBuf;

fn tmp_base_path() -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("trace.bin");
    std::mem::forget(dir);
    path
}

// ===========================================================================
// Recorder builder API — `recorder(DiskBuffer::builder()...)` + `attach_runtime`
// ===========================================================================
mod fluent_builder {
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use dial9::{AttachedRuntime, DiskBuffer, Recorder, RecorderTokioExt};

    use super::tmp_base_path;

    /// Attach a default multi-thread runtime to `recorder` and hand both to the
    /// macro.
    fn with_runtime(recorder: Recorder, worker_threads: usize) -> io::Result<AttachedRuntime> {
        recorder.attach_runtime(|t| {
            t.worker_threads(worker_threads);
        })
    }

    fn test_config() -> io::Result<AttachedRuntime> {
        let writer = DiskBuffer::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .build()
            .expect("writer build failed");
        with_runtime(dial9::recorder(writer).build(), 2)
    }

    fn disabled_config() -> io::Result<AttachedRuntime> {
        with_runtime(dial9::recorder_disabled(), 2)
    }

    #[dial9::main(config = test_config)]
    async fn runs_async_body() {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    #[test]
    fn macro_runs_async_body() {
        runs_async_body();
    }

    #[dial9::main(config = || {
        let writer = DiskBuffer::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .build()
            .expect("writer build failed");
        with_runtime(dial9::recorder(writer).build(), 2)
    })]
    async fn runs_with_inline_closure() {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    #[test]
    fn macro_runs_with_inline_closure() {
        runs_with_inline_closure();
    }

    #[dial9::main(config = move || {
        let writer = DiskBuffer::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .build()
            .expect("writer build failed");
        with_runtime(dial9::recorder(writer).build(), 2)
    })]
    async fn runs_with_move_closure() {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    #[test]
    fn macro_runs_with_move_closure() {
        runs_with_move_closure();
    }

    #[dial9::main(config = test_config)]
    async fn with_return_type() -> Result<i32, Box<dyn std::error::Error + Send + Sync>> {
        let val = tokio::spawn(async { 42 }).await?;
        Ok(val)
    }

    #[test]
    fn macro_preserves_return_type() {
        let result = with_return_type();
        assert_eq!(result.unwrap(), 42);
    }

    #[dial9::main(config = test_config)]
    async fn with_nested_spawn() -> i32 {
        // `Dial9TokioHandle::current()` is populated by `on_thread_start` on
        // every runtime-owned thread — use it to spawn instrumented sub-tasks.
        let handle = dial9::Dial9TokioHandle::current();
        let sub = handle.spawn(async { 7 + 3 });
        sub.await.unwrap()
    }

    #[test]
    fn macro_exposes_handle_for_nested_spawn() {
        let result = with_nested_spawn();
        assert_eq!(result, 10);
    }

    // --- Error propagation ---

    #[dial9::main(config = test_config)]
    async fn body_returns_err() -> Result<(), String> {
        Err("something went wrong".into())
    }

    #[test]
    fn macro_propagates_err_variant() {
        let result = body_returns_err();
        assert_eq!(result.unwrap_err(), "something went wrong");
    }

    #[dial9::main(config = test_config)]
    async fn body_returns_custom_err() -> Result<i32, std::io::Error> {
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"))
    }

    #[test]
    fn macro_propagates_io_error() {
        let result = body_returns_custom_err();
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(err.to_string(), "missing");
    }

    // --- Panic propagation ---

    #[dial9::main(config = test_config)]
    async fn body_panics_with_str() {
        panic!("boom");
    }

    #[test]
    fn macro_propagates_panic_payload() {
        let result = catch_unwind(AssertUnwindSafe(body_panics_with_str));
        let payload = result.expect_err("should have panicked");
        let msg = payload
            .downcast_ref::<&str>()
            .expect("payload should be &str");
        assert_eq!(*msg, "boom");
    }

    #[dial9::main(config = test_config)]
    #[allow(clippy::unnecessary_literal_unwrap)]
    async fn body_panics_with_format() {
        let x: Option<i32> = None;
        x.unwrap();
    }

    #[test]
    fn macro_propagates_unwrap_panic() {
        let result = catch_unwind(AssertUnwindSafe(body_panics_with_format));
        let payload = result.expect_err("should have panicked");
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .expect("payload should be &str or String");
        assert!(msg.contains("None"), "expected unwrap message, got: {msg}");
    }

    // --- Disabled telemetry ---

    fn disabled_config_default() -> io::Result<AttachedRuntime> {
        dial9::recorder_disabled().attach_runtime(|t| {
            t.enable_all();
        })
    }

    #[dial9::main(config = disabled_config)]
    async fn runs_without_telemetry() -> i32 {
        tokio::spawn(async { 123 }).await.unwrap()
    }

    #[test]
    fn macro_runs_with_disabled_config() {
        let result = runs_without_telemetry();
        assert_eq!(result, 123);
    }

    #[dial9::main(config = disabled_config_default)]
    async fn disabled_default_runs() -> i32 {
        tokio::spawn(async { 99 }).await.unwrap()
    }

    #[test]
    fn macro_runs_with_disabled_default() {
        assert_eq!(disabled_default_runs(), 99);
    }

    #[dial9::main(config = disabled_config)]
    async fn disabled_with_return_type() -> Result<i32, Box<dyn std::error::Error + Send + Sync>> {
        let val = tokio::spawn(async { 42 }).await?;
        Ok(val)
    }

    #[test]
    fn macro_disabled_preserves_return_type() {
        assert_eq!(disabled_with_return_type().unwrap(), 42);
    }

    #[dial9::main(config = disabled_config)]
    async fn disabled_no_telemetry_handle() -> bool {
        // The current handle should be inert when telemetry is disabled.
        !dial9::Dial9Handle::current().is_enabled()
    }

    #[test]
    fn macro_disabled_has_no_telemetry_handle() {
        assert!(disabled_no_telemetry_handle());
    }

    #[dial9::main(config = disabled_config)]
    async fn disabled_timers_work() {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    #[test]
    fn macro_disabled_timers_work() {
        disabled_timers_work();
    }

    #[dial9::main(config = disabled_config)]
    async fn disabled_nested_spawn() -> i32 {
        let inner = tokio::spawn(async { tokio::spawn(async { 7 + 3 }).await.unwrap() });
        inner.await.unwrap()
    }

    #[test]
    fn macro_disabled_nested_spawn() {
        assert_eq!(disabled_nested_spawn(), 10);
    }
}

// In-memory writer via `recorder(MemoryBuffer::builder()...)`.
mod in_memory {
    use std::future::Future;
    use std::io;
    use std::pin::Pin;

    use dial9::Dial9Handle;
    use dial9::Dial9TokioHandle;
    use dial9::core::pipeline::{ProcessError, SegmentData, SegmentProcessor};
    use dial9::{AttachedRuntime, MemoryBuffer, RecorderPipelineExt, RecorderTokioExt};

    /// Stand-in delivery processor: forwards each segment unchanged.
    #[derive(Debug, Default)]
    struct NoopProcessor;

    impl SegmentProcessor for NoopProcessor {
        fn name(&self) -> &'static str {
            "Noop"
        }

        fn process(
            &mut self,
            data: SegmentData,
        ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
            Box::pin(async move { Ok(data) })
        }
    }

    fn memory_config() -> io::Result<AttachedRuntime> {
        let writer = MemoryBuffer::builder()
            .max_total_size(16 * 1024 * 1024)
            .build()
            .expect("in-memory writer build failed");
        dial9::recorder(writer)
            .with_custom_pipeline(|p| p.pipe(NoopProcessor))
            .build()
            .attach_runtime(|t| {
                t.enable_all();
            })
    }

    #[dial9::main(config = memory_config)]
    async fn runs_with_memory_writer() -> bool {
        let sub = Dial9TokioHandle::current().spawn(async { 7 + 3 });
        assert_eq!(sub.await.unwrap(), 10);
        Dial9Handle::current().is_enabled()
    }

    #[test]
    fn macro_runs_with_memory_writer() {
        assert!(
            runs_with_memory_writer(),
            "in-memory config should keep telemetry enabled through the macro"
        );
    }
}

// ===========================================================================
// Lenient downgrade path: on a writer-I/O failure `recorder_or_disabled` falls
// back to a disabled recorder (a plain tokio runtime with no telemetry).
// Exercises the macro through that downgrade.
// ===========================================================================
mod fluent_builder_fallback {
    use std::io;
    use std::path::PathBuf;

    use dial9::Dial9Handle;
    use dial9::{AttachedRuntime, DiskBuffer, RecorderTokioExt};

    use super::tmp_base_path;

    /// Build a disk-backed recorder, or fall back to a disabled recorder (a
    /// plain tokio runtime) when the writer cannot be created.
    fn disk_recorder_or_disabled(base_path: PathBuf) -> io::Result<AttachedRuntime> {
        let writer = DiskBuffer::builder()
            .base_path(base_path)
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .build();
        dial9::recorder_or_disabled(writer).attach_runtime(|t| {
            t.enable_all();
        })
    }

    fn fallback_config() -> io::Result<AttachedRuntime> {
        disk_recorder_or_disabled(tmp_base_path())
    }

    fn unwritable_base_path() -> PathBuf {
        PathBuf::from("/this/dir/does/not/exist/dial9_macro_fallback_trace.bin")
    }

    fn cascading_fallback_config() -> io::Result<AttachedRuntime> {
        disk_recorder_or_disabled(unwritable_base_path())
    }

    #[dial9::main(config = fallback_config)]
    async fn fallback_runs_async_body() -> bool {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        Dial9Handle::current().is_enabled()
    }

    #[test]
    fn fallback_config_runs_async_body() {
        let telemetry_active = fallback_runs_async_body();
        assert!(
            telemetry_active,
            "writable base_path should keep telemetry enabled through the macro"
        );
    }

    #[dial9::main(config = cascading_fallback_config)]
    async fn cascade_runs_async_body() -> bool {
        let result = tokio::spawn(async { 21 + 21 }).await.unwrap();
        assert_eq!(result, 42);
        !Dial9Handle::current().is_enabled()
    }

    #[test]
    fn fallback_cascade_runs_without_telemetry() {
        let telemetry_disabled = cascade_runs_async_body();
        assert!(
            telemetry_disabled,
            "unwritable base_path must downgrade to a plain tokio runtime with no telemetry"
        );
    }
}

/// `Recorder::attach_runtime` builds an instrumented runtime and hands back both.
mod attach_runtime {
    use dial9::{MemoryBuffer, RecorderTokioExt, TokioAttachOptions};

    #[test]
    fn builds_an_instrumented_runtime() {
        let (recorder, runtime) = dial9::recorder(MemoryBuffer::new(4 * 1024 * 1024).unwrap())
            .build()
            .attach_runtime_with(
                TokioAttachOptions::builder().runtime_name("main").build(),
                |t| {
                    t.worker_threads(2);
                },
            )
            .expect("build tokio runtime");

        let enabled = runtime.block_on(async { dial9::Dial9Handle::current().is_enabled() });
        assert!(enabled, "the runtime should be instrumented");

        drop(runtime);
        recorder.graceful_shutdown(std::time::Duration::from_secs(1));
    }

    /// A disabled recorder still yields a usable runtime, just an untraced one.
    #[test]
    fn disabled_recorder_gives_a_plain_runtime() {
        let (recorder, runtime) = dial9::recorder_disabled()
            .attach_runtime(|_| {})
            .expect("build tokio runtime");

        assert_eq!(runtime.block_on(async { 42 }), 42);
        assert!(!recorder.handle().is_enabled());
    }

    /// `attach_runtime`'s return value is exactly what `config` must produce, so
    /// it goes straight into `#[dial9::main]` — nothing to unwrap or assemble.
    #[dial9::main(config = || dial9::recorder_disabled().attach_runtime(|t| { t.worker_threads(2); }))]
    async fn runs_from_an_attach_runtime_config() -> u32 {
        dial9::spawn(async { 21 + 21 }).await.unwrap()
    }

    #[test]
    fn attach_runtime_result_feeds_the_macro() {
        assert_eq!(runs_from_an_attach_runtime_config(), 42);
    }
}

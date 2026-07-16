use std::path::PathBuf;

fn tmp_base_path() -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("trace.bin");
    std::mem::forget(dir);
    path
}

// ===========================================================================
// Recorder builder API — `recorder(DiskBuffer::builder()...).with_tokio(..)`
// ===========================================================================
mod fluent_builder {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use dial9::{DiskBuffer, RecorderBuilderTokioExt, TracedRuntimeBuilder};

    use super::tmp_base_path;

    fn test_config() -> TracedRuntimeBuilder {
        let writer = DiskBuffer::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .build()
            .expect("writer build failed");
        dial9::recorder(writer).with_tokio(|_| {})
    }

    fn disabled_config() -> TracedRuntimeBuilder {
        TracedRuntimeBuilder::disabled().with_tokio(|t| {
            t.worker_threads(2);
        })
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
        dial9::recorder(writer).with_tokio(|_| {})
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
        dial9::recorder(writer).with_tokio(|_| {})
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

    fn disabled_config_default() -> TracedRuntimeBuilder {
        TracedRuntimeBuilder::disabled()
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
    use std::pin::Pin;

    use dial9::Dial9Handle;
    use dial9::Dial9TokioHandle;
    use dial9::core::pipeline::{ProcessError, SegmentData, SegmentProcessor};
    use dial9::{MemoryBuffer, RecorderBuilderTokioExt, TracedRuntimeBuilder};

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

    fn memory_config() -> TracedRuntimeBuilder<dial9::Memory> {
        let writer = MemoryBuffer::builder()
            .max_total_size(16 * 1024 * 1024)
            .build()
            .expect("in-memory writer build failed");
        dial9::recorder(writer)
            .with_tokio(|_| {})
            .with_custom_pipeline(|p| p.pipe(NoopProcessor))
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
// Lenient downgrade path: on a writer-I/O failure the config function falls
// back to `TracedRuntimeBuilder::disabled()` (a plain tokio runtime with no
// telemetry). Exercises the macro through that downgrade.
// ===========================================================================
mod fluent_builder_fallback {
    use std::path::PathBuf;

    use dial9::Dial9Handle;
    use dial9::{DiskBuffer, TracedRuntimeBuilder};

    use super::tmp_base_path;

    /// Build a disk-backed recorder, or fall back to a disabled recorder (a
    /// plain tokio runtime) when the writer cannot be created.
    fn disk_recorder_or_disabled(base_path: PathBuf) -> TracedRuntimeBuilder {
        let writer = DiskBuffer::builder()
            .base_path(base_path)
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .build();
        dial9::recorder_or_disabled(writer, |_| {})
    }

    fn fallback_config() -> TracedRuntimeBuilder {
        disk_recorder_or_disabled(tmp_base_path())
    }

    fn unwritable_base_path() -> PathBuf {
        PathBuf::from("/this/dir/does/not/exist/dial9_macro_fallback_trace.bin")
    }

    fn cascading_fallback_config() -> TracedRuntimeBuilder {
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

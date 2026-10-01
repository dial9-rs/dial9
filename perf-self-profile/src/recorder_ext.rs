//! `.with_*` sugar for plugging this crate's profiling `Source`s into a
//! [`RecorderBuilder`](dial9_core::recorder::RecorderBuilder) in one call. The
//! CPU and scheduler profilers warn on a start failure and register a
//! `StartFailed` placeholder. The other methods register nothing on failure;
//! see each method. Use `.source(CpuProfiler::start(cfg)?)` to propagate the
//! failure instead.

use dial9_core::buffer::BufferMode;
use dial9_core::recorder::RecorderBuilder;

#[cfg(any(feature = "cpu-profiling", feature = "memory-profiling"))]
use dial9_core::rate_limited;

/// Registered in place of a profiling source `T` that failed to start, so
/// [`Dial9Handle::with_source`](dial9_core::handle::Dial9Handle::with_source)
/// can tell "configured but failed" apart from "not configured":
///
/// ```no_run
/// # fn f(handle: dial9_core::handle::Dial9Handle) {
/// use dial9_perf_self_profile::{CpuProfiler, StartFailed};
///
/// let error = handle.with_source(|f: &mut StartFailed<CpuProfiler>| f.message().to_string());
/// if let Some(error) = error {
///     eprintln!("CPU profiling failed to start: {error}");
/// }
/// # }
/// ```
///
/// Records nothing; it only reports `cpu.profile.start_error` or
/// `sched.profile.start_error` as segment metadata.
#[cfg(feature = "cpu-profiling")]
pub struct StartFailed<T: 'static> {
    kind: std::io::ErrorKind,
    message: String,
    metadata_emitted: bool,
    _source: std::marker::PhantomData<fn() -> T>,
}

/// The profilers a [`StartFailed`] can stand in for.
#[cfg(feature = "cpu-profiling")]
mod start_failed_sealed {
    pub trait Profiler: 'static {
        /// Prefix of the profiler's segment metadata keys.
        const METADATA_PREFIX: &'static str;
        /// [`Source::name`](dial9_core::source::Source::name) of the placeholder.
        const FAILED_SOURCE_NAME: &'static str;
    }

    impl Profiler for crate::CpuProfiler {
        const METADATA_PREFIX: &'static str = "cpu.profile";
        const FAILED_SOURCE_NAME: &'static str = "cpu_profile_start_failed";
    }

    impl Profiler for crate::SchedProfiler {
        const METADATA_PREFIX: &'static str = "sched.profile";
        const FAILED_SOURCE_NAME: &'static str = "sched_start_failed";
    }
}

#[cfg(feature = "cpu-profiling")]
impl<T: start_failed_sealed::Profiler> StartFailed<T> {
    fn new(error: &std::io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
            metadata_emitted: false,
            _source: std::marker::PhantomData,
        }
    }

    /// Kind of the start error. On Linux, blocked perf can also report
    /// [`Unsupported`](std::io::ErrorKind::Unsupported), so the kind alone
    /// doesn't say whether the platform has a backend.
    pub fn kind(&self) -> std::io::ErrorKind {
        self.kind
    }

    /// The start error's message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[cfg(feature = "cpu-profiling")]
impl<T: 'static> std::fmt::Debug for StartFailed<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartFailed")
            .field("source", &std::any::type_name::<T>())
            .field("kind", &self.kind)
            .field("message", &self.message)
            .finish()
    }
}

#[cfg(feature = "cpu-profiling")]
impl<T: start_failed_sealed::Profiler> dial9_core::source::Source for StartFailed<T> {
    fn flush(&mut self, _ctx: &dial9_core::source::FlushContext<'_>) {}

    fn name(&self) -> &'static str {
        T::FAILED_SOURCE_NAME
    }

    fn segment_metadata(&mut self, out: &mut Vec<(String, String)>) {
        if self.metadata_emitted {
            return;
        }
        self.metadata_emitted = true;
        out.push((
            format!("{}.start_error", T::METADATA_PREFIX),
            self.message.clone(),
        ));
    }
}

/// `.with_*` convenience for this crate's profiling `Source`s on the core
/// [`RecorderBuilder`].
pub trait RecorderPerfExt: recorder_perf_ext_sealed::Sealed + Sized {
    /// Register the NVIDIA CUDA GPU sampler. Warns and skips when NVML is unavailable.
    #[cfg(feature = "cuda")]
    fn with_cuda_gpu_profiling(self, config: crate::cuda::CudaGpuConfig) -> Self;

    /// Register the NVIDIA CUDA GPU sampler. Returns an error when NVML is unavailable.
    #[cfg(feature = "cuda")]
    fn try_with_cuda_gpu_profiling(
        self,
        config: crate::cuda::CudaGpuConfig,
    ) -> Result<Self, crate::cuda::CudaGpuStartError>;

    /// Register the process-wide CPU profiler. On a start failure, warns and
    /// registers a [`StartFailed`] placeholder.
    #[cfg(feature = "cpu-profiling")]
    fn with_cpu_profiling(self, config: crate::CpuProfilingConfig) -> Self;

    /// Register the per-thread scheduler-event profiler. On a start failure,
    /// warns and registers a [`StartFailed`] placeholder.
    #[cfg(feature = "cpu-profiling")]
    fn with_sched_events(self, config: crate::SchedEventConfig) -> Self;

    /// Register the `getrusage` resource-usage sampler. Warns and skips off unix.
    #[cfg(feature = "process-resource")]
    fn with_process_resource_usage(self, config: crate::ProcessResourceUsageConfig) -> Self;

    /// Register the Linux `sock_diag` accept-queue sampler. Warns and skips off Linux.
    #[cfg(feature = "linux-socket")]
    fn with_socket_accept_queues(self, config: crate::SocketAcceptQueuesConfig) -> Self;

    /// Install sampled memory allocation profiling on the recorder
    /// (needs the global allocator). Installs once recording starts.
    /// Warns and skips on install failure.
    #[cfg(feature = "memory-profiling")]
    fn with_memory_profiling(self, config: crate::memory_profiling::MemoryProfilingConfig) -> Self;
}

mod recorder_perf_ext_sealed {
    use dial9_core::buffer::BufferMode;
    use dial9_core::recorder::RecorderBuilder;

    pub trait Sealed {}
    impl<M: BufferMode> Sealed for RecorderBuilder<M> {}
}

impl<M: BufferMode> RecorderPerfExt for RecorderBuilder<M> {
    #[cfg(feature = "cuda")]
    fn with_cuda_gpu_profiling(self, config: crate::cuda::CudaGpuConfig) -> Self {
        match crate::cuda::CudaGpuSource::start(config) {
            Ok(source) => self.source(source),
            Err(e) => {
                tracing::debug!("CUDA GPU profiling disabled because NVML is unavailable: {e}");
                self
            }
        }
    }

    #[cfg(feature = "cuda")]
    fn try_with_cuda_gpu_profiling(
        self,
        config: crate::cuda::CudaGpuConfig,
    ) -> Result<Self, crate::cuda::CudaGpuStartError> {
        let source = crate::cuda::CudaGpuSource::start(config)?;
        Ok(self.source(source))
    }

    #[cfg(feature = "cpu-profiling")]
    fn with_cpu_profiling(self, config: crate::CpuProfilingConfig) -> Self {
        match crate::CpuProfiler::start(config) {
            Ok(source) => self.source(source).on_recording_thread_start(|| {
                let _ = crate::register_current_thread();
                crate::unregister_current_thread
            }),
            Err(e) => {
                rate_limited!(std::time::Duration::from_secs(60), {
                    tracing::warn!("failed to start CPU profiler: {e}");
                });
                self.source(StartFailed::<crate::CpuProfiler>::new(&e))
            }
        }
    }

    #[cfg(feature = "cpu-profiling")]
    fn with_sched_events(self, config: crate::SchedEventConfig) -> Self {
        match crate::SchedProfiler::new(config) {
            Ok(source) => self.source(source),
            Err(e) => {
                rate_limited!(std::time::Duration::from_secs(60), {
                    tracing::warn!("failed to start scheduler event profiler: {e}");
                });
                self.source(StartFailed::<crate::SchedProfiler>::new(&e))
            }
        }
    }

    #[cfg(feature = "process-resource")]
    fn with_process_resource_usage(self, config: crate::ProcessResourceUsageConfig) -> Self {
        #[cfg(unix)]
        {
            self.source(crate::ProcessResourceUsageSource::new(config))
        }
        #[cfg(not(unix))]
        {
            let _ = config;
            tracing::warn!(
                "process resource usage enabled but getrusage is not available on this platform"
            );
            self
        }
    }

    #[cfg(feature = "linux-socket")]
    fn with_socket_accept_queues(self, config: crate::SocketAcceptQueuesConfig) -> Self {
        #[cfg(target_os = "linux")]
        {
            self.source(crate::SocketAcceptQueuesSource::new(config))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = config;
            tracing::warn!("socket accept queues enabled but sock_diag is only available on Linux");
            self
        }
    }

    #[cfg(feature = "memory-profiling")]
    fn with_memory_profiling(self, config: crate::memory_profiling::MemoryProfilingConfig) -> Self {
        self.on_recording_start(move |handle| {
            if let Err(e) =
                crate::memory_profiling::MemoryProfiler::from_config(config).install(handle.clone())
            {
                rate_limited!(std::time::Duration::from_secs(60), {
                    tracing::warn!("failed to install memory profiler: {e}");
                });
            }
        })
    }
}

// `process-resource` is infallible on unix, so this assertion is deterministic;
// cpu/sched/socket starts are platform-dependent and covered elsewhere.
#[cfg(all(test, feature = "process-resource", unix))]
mod tests {
    use super::RecorderPerfExt;
    use crate::ProcessResourceUsageConfig;
    use dial9_core::buffer::MemoryBuffer;
    use dial9_core::recorder::recorder;
    use std::time::Duration;

    #[test]
    fn with_process_resource_usage_registers_the_source() {
        let writer = MemoryBuffer::new(64 * 1024).expect("writer");
        let recorder = recorder(writer)
            .with_process_resource_usage(ProcessResourceUsageConfig::default())
            .build();
        let names: Vec<String> = recorder
            .shared()
            .expect("enabled recorder")
            .with_sources_mut(|sources| sources.iter().map(|s| s.name().to_string()).collect())
            .expect("sources lock");
        assert!(
            names.iter().any(|n| n == "process_resource_usage"),
            "expected the process resource usage source to be registered, got {names:?}"
        );
        recorder.graceful_shutdown(Duration::ZERO);
    }
}

#[cfg(all(test, feature = "cpu-profiling"))]
mod start_failed_tests {
    use super::StartFailed;
    use crate::CpuProfiler;
    use dial9_core::source::Source;

    #[test]
    fn writes_the_start_error_once() {
        let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let mut failed = StartFailed::<CpuProfiler>::new(&error);
        assert_eq!(failed.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(failed.message(), "denied");
        let mut first = Vec::new();
        failed.segment_metadata(&mut first);
        assert_eq!(
            first,
            [("cpu.profile.start_error".to_string(), "denied".to_string())]
        );
        let mut second = Vec::new();
        failed.segment_metadata(&mut second);
        assert!(
            second.is_empty(),
            "emitted once; the writer keeps it for later segments"
        );
    }

    /// Each profiler's placeholder has its own metadata key and name.
    #[test]
    fn keys_and_names_follow_the_profiler() {
        let error = std::io::Error::other("denied");
        let mut sched = StartFailed::<crate::SchedProfiler>::new(&error);
        let mut out = Vec::new();
        sched.segment_metadata(&mut out);
        assert_eq!(
            out,
            [(
                "sched.profile.start_error".to_string(),
                "denied".to_string()
            )]
        );
        assert_eq!(sched.name(), "sched_start_failed");
        assert_eq!(
            StartFailed::<CpuProfiler>::new(&error).name(),
            "cpu_profile_start_failed"
        );
    }

    /// Off Linux the profiler can't start, so the builder registers the
    /// placeholder instead of skipping the source.
    #[cfg(not(any(
        target_os = "linux",
        all(target_os = "android", target_arch = "aarch64")
    )))]
    #[test]
    fn a_failed_start_registers_the_placeholder() {
        use super::RecorderPerfExt;
        use dial9_core::buffer::MemoryBuffer;
        use dial9_core::recorder::recorder;

        let rec = recorder(MemoryBuffer::new(64 * 1024).expect("writer"))
            .with_cpu_profiling(crate::CpuProfilingConfig::default())
            .build();
        let found = rec
            .handle()
            .with_source(|f: &mut StartFailed<CpuProfiler>| f.message().to_string());
        assert!(found.is_some(), "no StartFailed<CpuProfiler> registered");
        assert!(rec.handle().with_source(|_: &mut CpuProfiler| ()).is_none());
        rec.graceful_shutdown(std::time::Duration::ZERO);
    }
}

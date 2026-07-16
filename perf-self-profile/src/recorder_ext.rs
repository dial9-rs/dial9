//! `.with_*` sugar for plugging this crate's profiling `Source`s into a
//! [`RecorderBuilder`](dial9_core::recorder::RecorderBuilder) in one call. Each
//! method is `.source(<Source>::new(cfg))` that warns and skips on a start
//! failure or unsupported platform. Use `.source(CpuProfiler::start(cfg)?)` to
//! propagate the failure instead.

use dial9_core::recorder::RecorderSourceExt;

#[cfg(any(feature = "cpu-profiling", feature = "memory-profiling"))]
use dial9_core::rate_limited;

/// `.with_*` convenience for this crate's profiling `Source`s, available on any
/// [`RecorderSourceExt`]. The core [`RecorderBuilder`](dial9_core::recorder::RecorderBuilder)
/// and runtime wrappers that forward to it.
pub trait RecorderPerfExt: Sized {
    /// Register the process-wide CPU profiler. Warns and skips on start failure.
    #[cfg(feature = "cpu-profiling")]
    fn with_cpu_profiling(self, config: crate::CpuProfilingConfig) -> Self;

    /// Register the per-thread scheduler-event profiler. Warns and skips on start failure.
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

impl<T: RecorderSourceExt> RecorderPerfExt for T {
    #[cfg(feature = "cpu-profiling")]
    fn with_cpu_profiling(self, config: crate::CpuProfilingConfig) -> Self {
        match crate::CpuProfiler::start(config) {
            Ok(source) => self.source(source),
            Err(e) => {
                rate_limited!(std::time::Duration::from_secs(60), {
                    tracing::warn!("failed to start CPU profiler: {e}");
                });
                self
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
                self
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
            .build_and_start();
        let names: Vec<String> = recorder
            .shared()
            .expect("enabled recorder")
            .with_sources_mut(|sources| sources.iter().map(|s| s.name().to_string()).collect())
            .expect("sources lock");
        assert!(
            names.iter().any(|n| n == "process_resource_usage"),
            "expected the process resource usage source to be registered, got {names:?}"
        );
        recorder
            .graceful_shutdown(Duration::ZERO)
            .expect("shutdown");
    }
}

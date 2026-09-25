//! Experimental sampling of async stacks before capture, with a budget per worker.
//!
//! Requires the `taskdump` feature and the same Linux/Tokio support as
//! [`TaskDumpConfig`](super::TaskDumpConfig). Only Dial9-instrumented futures
//! participate. Configure this through [`TokioAttachOptions`](super::TokioAttachOptions).

/// Experimental alternative to [`TaskDumpConfig`](super::TaskDumpConfig).
///
/// Workers calibrate for one second without capturing. The previous second's
/// eligible transitions determine the probability used for the next second.
/// The configured rate is a target, not a cap: a sharp increase in traffic can
/// cause substantially more captures. Cost protection and data quality are
/// still being evaluated; this mode is not yet intended for production use.
#[derive(Debug, Clone, Copy)]
pub struct TaskSamplingConfig {
    captures_per_second_per_worker: u32,
    rng_seed: Option<u64>,
}

impl Default for TaskSamplingConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

#[bon::bon]
impl TaskSamplingConfig {
    /// Configure experimental task sampling.
    ///
    /// # Panics
    ///
    /// `build()` panics if `captures_per_second_per_worker` is zero.
    #[builder(builder_type = TaskSamplingConfigBuilder, finish_fn = build)]
    pub fn builder(
        /// Target captures per second per worker; not a strict cap.
        #[builder(default = 10)]
        captures_per_second_per_worker: u32,
        /// Fixed seed for reproducible sampling given the same worker IDs and
        /// pending-transition timestamps. Defaults to a timestamp per worker.
        rng_seed: Option<u64>,
    ) -> Self {
        // Same panic convention as MemoryProfilingConfigBuilder; build-time validation per the design doc.
        assert!(
            captures_per_second_per_worker > 0,
            "captures_per_second_per_worker must be positive"
        );
        Self {
            captures_per_second_per_worker,
            rng_seed,
        }
    }

    /// Target captures per second per worker (default: 10).
    pub fn captures_per_second_per_worker(&self) -> u32 {
        self.captures_per_second_per_worker
    }

    /// Optional fixed seed for deterministic sampling.
    pub fn rng_seed(&self) -> Option<u64> {
        self.rng_seed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_explicit_rate() {
        assert_eq!(
            TaskSamplingConfig::default().captures_per_second_per_worker(),
            10
        );
        let config = TaskSamplingConfig::builder()
            .captures_per_second_per_worker(40)
            .rng_seed(7)
            .build();
        assert_eq!(config.captures_per_second_per_worker(), 40);
        assert_eq!(config.rng_seed(), Some(7));
        assert_eq!(
            TaskSamplingConfig::builder()
                .captures_per_second_per_worker(u32::MAX)
                .build()
                .captures_per_second_per_worker(),
            u32::MAX
        );
    }

    #[test]
    fn zero_rate_is_rejected_at_build() {
        let builder = TaskSamplingConfig::builder().captures_per_second_per_worker(0);
        assert!(std::panic::catch_unwind(|| builder.build()).is_err());
    }
}

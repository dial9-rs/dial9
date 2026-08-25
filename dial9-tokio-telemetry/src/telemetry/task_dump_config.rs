//! Configuration for task dump capture.
//!
//! Task dumps capture async backtraces at yield points for tasks that have
//! been idle, using Poisson sampling keyed on idle duration.
//! Pass [`TaskDumpConfig`] to
//! [`TokioAttachOptions`](crate::telemetry::TokioAttachOptions) when attaching a
//! runtime.
//!
//! Capture requires the `taskdump` crate feature, `--cfg tokio_unstable`, and a
//! supported Linux target. With the feature off, this module is still compiled
//! so the configuration API surface stays the same, but no dumps are captured.

use std::time::Duration;

/// Default mean idle duration for Poisson sampling.
const DEFAULT_IDLE_THRESHOLD: Duration = Duration::from_millis(10);

/// Configuration for task dump capture.
///
/// <div class="warning">
///
/// This enables capture but does not instrument every task on the attached
/// runtime. Only futures spawned through a Dial9 spawner, such as
/// `dial9::spawn`, can produce task dumps. Futures spawned directly with
/// `tokio::spawn` remain valid, but do not produce task dumps.
///
/// </div>
#[derive(Debug, Clone, Copy, bon::Builder)]
pub struct TaskDumpConfig {
    /// Mean idle duration for Poisson sampling. On average, one
    /// task dump is emitted per this amount of cumulative idle time. Shorter
    /// idles have a lower (but non-zero) probability of triggering a dump;
    /// longer idles are very likely to trigger. Defaults to 10ms.
    #[builder(default = DEFAULT_IDLE_THRESHOLD)]
    idle_threshold: Duration,

    /// Optional fixed seed for the per-task PRNG. When set, task dump sampling
    /// becomes deterministic (given the same task IDs and idle durations).
    /// Intended for testing. When `None` (default), each task seeds its PRNG
    /// from a timestamp for production uniqueness.
    rng_seed: Option<u64>,
}

impl Default for TaskDumpConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl TaskDumpConfig {
    /// Mean idle duration for Poisson sampling.
    pub fn idle_threshold(&self) -> Duration {
        self.idle_threshold
    }

    /// Optional fixed RNG seed for deterministic sampling.
    pub fn rng_seed(&self) -> Option<u64> {
        self.rng_seed
    }
}

//! Configuration for task dump capture.
//!
//! Task dumps sample pending transitions before capturing async backtraces.
//! Pass [`TaskDumpConfig`] to
//! [`TokioAttachOptions`](crate::telemetry::TokioAttachOptions) when attaching a
//! runtime.
//!
//! Capture requires the `taskdump` crate feature, `--cfg tokio_unstable`, and a
//! supported Linux target. With the feature off, this module is still compiled
//! so the configuration API surface stays the same, but no dumps are captured.

use std::time::Duration;

/// Default expected capture budget: 10 captures/second/worker.
const DEFAULT_CAPTURE_INTERVAL: Duration = Duration::from_millis(100);

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
#[derive(Debug, Clone, Copy)]
pub struct TaskDumpConfig {
    capture_interval: Duration,
    rng_seed: Option<u64>,
}

impl Default for TaskDumpConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

#[bon::bon]
impl TaskDumpConfig {
    /// Configure task dump capture.
    ///
    /// # Panics
    ///
    /// `build()` panics if `captures_per_second_per_worker` is zero.
    #[builder(builder_type = TaskDumpConfigBuilder, finish_fn = build)]
    pub fn builder(
        /// Target captures per second per worker (default: 10).
        /// This is a sampling target, not a strict cap; see
        /// [`TaskDumpConfig::captures_per_second_per_worker`].
        /// Takes precedence over `idle_threshold`, regardless of setter order.
        /// Zero panics in `build()`. Omit the runtime's `task_dump_config` to
        /// disable capture. Rates above nanosecond resolution use a 1ns interval.
        captures_per_second_per_worker: Option<u32>,
        #[builder(setters(vis = "", name = legacy_interval))] idle_threshold: Option<Duration>,
        /// Optional fixed seed for deterministic sampling given the same worker
        /// identities and pending-transition timestamps. Each worker derives its
        /// own PRNG from this seed. By default, workers use a timestamp as seed.
        rng_seed: Option<u64>,
    ) -> Self {
        let capture_interval = match captures_per_second_per_worker {
            Some(rate) => {
                // Same panic convention as MemoryProfilingConfigBuilder; build-time validation per the design doc.
                assert!(rate > 0, "captures_per_second_per_worker must be positive");
                Duration::from_secs_f64(1.0 / f64::from(rate))
            }
            None => idle_threshold.unwrap_or(DEFAULT_CAPTURE_INTERVAL),
        }
        .max(Duration::from_nanos(1));
        Self {
            capture_interval,
            rng_seed,
        }
    }

    /// Expected captures per second per runtime worker (default: 10).
    ///
    /// The rate converges to this target under stable traffic, using the previous
    /// second's eligible-transition count. Repeated traffic changes can keep the
    /// average above the target; it is not a cap. Workers calibrate for one second
    /// before capturing. Low-volume workers may remain below the target.
    pub fn captures_per_second_per_worker(&self) -> f64 {
        1.0 / self.capture_interval.as_secs_f64()
    }

    /// Mean wall-clock capture interval per worker.
    ///
    /// This no longer controls sampling by cumulative task idle time.
    #[deprecated(
        note = "use captures_per_second_per_worker; sampling now budgets captures per worker"
    )]
    pub fn idle_threshold(&self) -> Duration {
        self.capture_interval
    }

    /// Optional fixed RNG seed for deterministic sampling.
    pub fn rng_seed(&self) -> Option<u64> {
        self.rng_seed
    }
}

impl<S: task_dump_config_builder::State> TaskDumpConfigBuilder<S> {
    /// Set the mean wall-clock capture interval per worker.
    ///
    /// This alias no longer samples by cumulative idle time. Zero retains the
    /// old capture-every-transition behavior, after the calibration epoch.
    #[deprecated(
        note = "use captures_per_second_per_worker; sampling now budgets captures per worker"
    )]
    pub fn idle_threshold(
        self,
        interval: Duration,
    ) -> TaskDumpConfigBuilder<task_dump_config_builder::SetIdleThreshold<S>>
    where
        S::IdleThreshold: task_dump_config_builder::IsUnset,
    {
        self.legacy_interval(interval)
    }

    /// Optionally set the deprecated capture-interval alias.
    #[deprecated(note = "use captures_per_second_per_worker")]
    pub fn maybe_idle_threshold(
        self,
        interval: Option<Duration>,
    ) -> TaskDumpConfigBuilder<task_dump_config_builder::SetIdleThreshold<S>>
    where
        S::IdleThreshold: task_dump_config_builder::IsUnset,
    {
        self.maybe_legacy_interval(interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_explicit_rate() {
        assert_eq!(
            TaskDumpConfig::default().captures_per_second_per_worker(),
            10.0
        );
        let config = TaskDumpConfig::builder()
            .captures_per_second_per_worker(40)
            .rng_seed(7)
            .build();
        assert_eq!(config.captures_per_second_per_worker(), 40.0);
        assert_eq!(config.rng_seed(), Some(7));
        assert_eq!(
            TaskDumpConfig::builder()
                .captures_per_second_per_worker(u32::MAX)
                .build()
                .capture_interval,
            Duration::from_nanos(1)
        );
    }

    #[test]
    fn zero_rate_is_rejected_at_build() {
        let builder = TaskDumpConfig::builder().captures_per_second_per_worker(0);
        assert!(std::panic::catch_unwind(|| builder.build()).is_err());
    }

    #[test]
    #[allow(deprecated)]
    fn new_rate_takes_precedence_in_either_order() {
        let interval = Duration::from_secs(1);
        let old_first = TaskDumpConfig::builder()
            .idle_threshold(interval)
            .captures_per_second_per_worker(40)
            .build();
        let new_first = TaskDumpConfig::builder()
            .captures_per_second_per_worker(40)
            .maybe_idle_threshold(Some(interval))
            .build();
        assert_eq!(old_first.captures_per_second_per_worker(), 40.0);
        assert_eq!(new_first.capture_interval, old_first.capture_interval);

        let invalid = TaskDumpConfig::builder()
            .captures_per_second_per_worker(0)
            .idle_threshold(interval);
        assert!(std::panic::catch_unwind(|| invalid.build()).is_err());
        assert_eq!(
            TaskDumpConfig::builder()
                .maybe_captures_per_second_per_worker(None)
                .idle_threshold(interval)
                .build()
                .capture_interval,
            interval
        );
    }

    #[test]
    #[allow(deprecated)]
    fn old_and_new_typestates_are_independent() {
        use task_dump_config_builder::{IsSet, IsUnset, SetCapturesPerSecondPerWorker, State};

        fn set_rate<S: State>(
            builder: TaskDumpConfigBuilder<S>,
        ) -> TaskDumpConfigBuilder<SetCapturesPerSecondPerWorker<S>>
        where
            S::IdleThreshold: IsSet,
            S::CapturesPerSecondPerWorker: IsUnset,
        {
            builder.captures_per_second_per_worker(20)
        }

        let legacy: TaskDumpConfigBuilder<task_dump_config_builder::SetIdleThreshold> =
            TaskDumpConfig::builder().idle_threshold(Duration::from_secs(1));
        assert_eq!(
            set_rate(legacy).build().captures_per_second_per_worker(),
            20.0
        );
    }

    #[test]
    #[allow(deprecated)]
    fn legacy_aliases_share_the_capture_interval() {
        let interval = Duration::from_millis(250);
        let legacy = TaskDumpConfig::builder().idle_threshold(interval).build();
        let new = TaskDumpConfig::builder()
            .captures_per_second_per_worker(4)
            .build();
        assert_eq!(legacy.idle_threshold(), new.idle_threshold());
        assert_eq!(legacy.captures_per_second_per_worker(), 4.0);
        assert_eq!(
            TaskDumpConfig::builder()
                .maybe_idle_threshold(None)
                .build()
                .idle_threshold(),
            DEFAULT_CAPTURE_INTERVAL
        );
        assert_eq!(
            TaskDumpConfig::builder()
                .maybe_idle_threshold(Some(interval))
                .build()
                .idle_threshold(),
            interval
        );
        assert!(
            TaskDumpConfig::builder()
                .idle_threshold(Duration::ZERO)
                .build()
                .captures_per_second_per_worker()
                .is_finite()
        );
    }
}

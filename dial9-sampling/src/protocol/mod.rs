//! Sticky startup selection from one roster, maintained by an elected census leader.
mod census;
mod engine;
mod rate_limit;
pub(crate) use rate_limit::rate_limited;
#[cfg(all(test, shuttle))]
mod storage_shuttle_tests;

use crate::backend::SamplingStore;
use crate::object_key::hive_escape;
#[cfg(all(test, not(shuttle)))]
pub(crate) use engine::{Clock, prepare_using};
pub(crate) use engine::{DecisionReason, PreparedSampling, SamplingSession};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

pub(crate) const CLEANUP_TIMEOUT: Duration = Duration::from_millis(500);

/// Startup choice when a sampling decision could not be made within the timeout
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum StartupFailure {
    /// Profiling disabled on failure
    #[non_exhaustive]
    Disabled {},
    /// Make a decision based on a guess of the total population within this scope
    ///
    /// The startup probability will be `target` / `population`
    #[non_exhaustive]
    AssumePopulation {
        /// Assumed number of live processes. Must be positive.
        population: u64,
    },
}
impl StartupFailure {
    /// Profiling disabled on failure
    pub fn disabled() -> Self {
        Self::Disabled {}
    }
    /// Make a decision based on a guess of the total population within this scope
    ///
    /// The startup probability will be `target` / `population`
    pub fn assume_population(population: u64) -> Self {
        Self::AssumePopulation { population }
    }
}
impl Default for StartupFailure {
    fn default() -> Self {
        Self::disabled()
    }
}

// Each invocation resolves a fresh run identity on the caller's configuration.
// There is no second builder with independent defaults.
pub(crate) type Config = crate::Sampling;
impl Config {
    pub(crate) fn target(&self) -> u32 {
        self.num_target_profilers
    }
    pub(crate) fn rank(&self) -> u64 {
        let mut dimensions = self.dimensions.clone();
        dimensions.sort();
        let canonical = serde_json::to_vec(&(dimensions, &self.invocation_id, "census"))
            .expect("string tuple serializes");
        u64::from_be_bytes(
            Sha256::digest(canonical)[..8]
                .try_into()
                .expect("digest length"),
        )
    }
    pub(crate) fn failure_decision(&self) -> bool {
        self.validate().is_ok() && self.selects(self.failure_cutoff())
    }
    pub(crate) fn failure_cutoff(&self) -> u128 {
        match self.if_failed {
            StartupFailure::Disabled {} => 0,
            StartupFailure::AssumePopulation { population: 0 } => 0,
            StartupFailure::AssumePopulation { population } => {
                ((u128::from(self.num_target_profilers) << 64) / u128::from(population))
                    .min(1u128 << 64)
            }
        }
    }
    fn draw(&self, purpose: &str, round: &str) -> u64 {
        let canonical = serde_json::to_vec(&(&self.invocation_id, purpose, round))
            .expect("string tuple serializes");
        u64::from_be_bytes(
            Sha256::digest(canonical)[..8]
                .try_into()
                .expect("digest length"),
        )
    }
    fn selects(&self, cutoff: u128) -> bool {
        u128::from(self.draw("admission", "")) < cutoff
    }
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.invocation_id.parse::<ulid::Ulid>().is_err() {
            return Err("process run identity must be a valid ULID".into());
        }
        let mut names = std::collections::HashSet::new();
        for (name, value) in &self.dimensions {
            if name.trim().is_empty() || value.trim().is_empty() || !names.insert(name) {
                return Err("dimensions must have unique, nonblank names and values".into());
            }
        }
        if let StartupFailure::AssumePopulation { population: 0 } = self.if_failed {
            return Err("assumed population must be positive".into());
        }
        // Keep all wire times integral and all bounded work finite.
        if self.num_target_profilers > 10000
            || self.settings.poll_interval < Duration::from_millis(100)
            || self.settings.poll_interval > self.settings.bucket_width
            || self.settings.bucket_width < Duration::from_secs(1)
            || self.settings.bucket_width > Duration::from_secs(86400)
            || self.settings.max_clock_skew.saturating_mul(4) >= self.settings.bucket_width
            || [
                self.settings.poll_interval,
                self.settings.bucket_width,
                self.settings.max_clock_skew,
            ]
            .iter()
            .any(|d| !d.subsec_nanos().is_multiple_of(1_000_000))
        {
            return Err("target must be at most 10000; leader cadence must be 100ms..window; window must be 1s..1d and exceed four clock skews; durations need whole milliseconds".into());
        }
        Ok(())
    }
    pub(crate) fn pool_path(&self) -> String {
        let mut dimensions = self.dimensions.clone();
        dimensions.sort();
        dimensions
            .into_iter()
            .map(|(name, value)| format!("{}={}/", hive_escape(&name), hive_escape(&value)))
            .collect()
    }
    pub(crate) fn pool_prefix(&self) -> String {
        format!("v3/pools/{}", self.pool_path())
    }
    fn report_prefix(&self, channel: &str, window: u64) -> String {
        // User dimensions always contain `=`, so this component ends their namespace.
        format!(
            "v3/{channel}/{}windows/window={window:016x}/",
            self.pool_path()
        )
    }
    fn overshoot(&self) -> u32 {
        self.settings
            .max_overshoot
            .unwrap_or_else(|| (3.0 * f64::from(self.num_target_profilers).sqrt()).ceil() as u32)
    }
    pub(crate) async fn prepare_with_timeout(
        &self,
        store: Arc<dyn SamplingStore>,
        timeout: Duration,
    ) -> PreparedSampling {
        engine::prepare_using(self.clone(), store, timeout, Arc::new(engine::WallClock)).await
    }
}

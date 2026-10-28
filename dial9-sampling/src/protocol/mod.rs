//! Sticky startup selection from one roster, maintained by an elected census leader.
mod engine;
mod rate_limit;
pub(crate) use rate_limit::rate_limited;
#[cfg(all(test, shuttle))]
mod storage_shuttle_tests;

use crate::backend::SamplingStore;
pub(crate) use engine::{DecisionReason, PreparedSampling, SamplingSession};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

#[cfg(all(test, not(shuttle)))]
pub(crate) const DEFAULT_PREPARE_TIMEOUT: Duration = Duration::from_millis(1000);

/// Startup choice when no compatible, settled roster is available.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum StartupFailure {
    /// Keeps profiling off. This still participates in the background census.
    #[non_exhaustive]
    Disabled {},
    /// Uses an independent invocation draw with probability `min(1, target / population)`,
    /// capped by the census tracking fraction.
    /// The population assumption is the caller's; it does not bound actual volume.
    #[non_exhaustive]
    AssumePopulation {
        /// Assumed number of live processes. Must be positive.
        population: u64,
    },
}
impl StartupFailure {
    /// Keeps profiling off on startup failure or an absent/unsettled roster.
    pub fn disabled() -> Self {
        Self::Disabled {}
    }
    /// Selects by an independent invocation draw using a positive assumed population.
    pub fn assume_population(population: u64) -> Self {
        Self::AssumePopulation { population }
    }
}
impl Default for StartupFailure {
    fn default() -> Self {
        Self::disabled()
    }
}

// The invocation resolves host_id on the caller's configuration; there is no
// second builder or configuration copy with independent defaults.
pub(crate) type Config = crate::Sampling;
impl Config {
    pub(crate) fn target(&self) -> u32 {
        self.target
    }
    /// An explicit lower population assumption supports reducing census traffic.
    fn slice_bits(&self) -> u32 {
        if self.target == 0 {
            return 0;
        }
        let required = 32 * u64::from(self.target);
        let mut bits = 0;
        while bits < 58 && (self.minimum_population >> (bits + 1)) >= required {
            bits += 1;
        }
        bits
    }
    fn tracked_limit(&self) -> u128 {
        1u128 << (64 - self.slice_bits())
    }
    pub(crate) fn tracked(&self) -> bool {
        u128::from(self.rank()) < self.tracked_limit()
    }
    pub(crate) fn rank(&self) -> u64 {
        let mut dimensions = self.dimensions.clone();
        dimensions.sort();
        let host_id = self
            .host_id
            .as_deref()
            .expect("invocation identity resolved");
        let canonical = serde_json::to_vec(&(&self.scope, dimensions, host_id))
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
                ((u128::from(self.target) << 64) / u128::from(population)).min(self.tracked_limit())
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
        self.tracked() && u128::from(self.draw("admission", "")) < (cutoff << self.slice_bits())
    }
    pub(crate) fn validate(&self) -> Result<(), String> {
        let Some(host_id) = &self.host_id else {
            return Err("invocation identity must be resolved".into());
        };
        if self.scope.trim().is_empty() || host_id.trim().is_empty() || host_id.contains('/') {
            return Err("scope and host_id must be nonblank; host_id must not contain '/'".into());
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
        // Whole milliseconds are the wire format. Bound all time arithmetic and
        // leave a previous bucket visible while a delayed heartbeat is retried.
        if self.target > 10000
            || self.poll_interval < Duration::from_millis(100)
            || self.poll_interval > Duration::from_secs(86400 / 3)
            || self.bucket_width > Duration::from_secs(86400)
            || self.bucket_width <= self.max_clock_skew.saturating_mul(4)
            || self.bucket_width
                < self
                    .poll_interval
                    .saturating_mul(2)
                    .saturating_add(self.max_clock_skew.saturating_mul(2))
            || !self.poll_interval.subsec_nanos().is_multiple_of(1_000_000)
            || !self.bucket_width.subsec_nanos().is_multiple_of(1_000_000)
            || !self.max_clock_skew.subsec_nanos().is_multiple_of(1_000_000)
        {
            return Err("target must be at most 10000; poll must be 100ms..8h; bucket must be <=1d and cover two polls plus clock margins; durations need whole milliseconds".into());
        }
        Ok(())
    }
    pub(crate) fn pool_hash(&self) -> String {
        let mut dimensions = self.dimensions.clone();
        dimensions.sort();
        let canonical =
            serde_json::to_vec(&(&self.scope, dimensions)).expect("string tuple serializes");
        format!("{:x}", Sha256::digest(canonical))
    }
    pub(crate) fn pool_prefix(&self) -> String {
        format!("v2/pools/{}/", self.pool_hash())
    }
    fn members_prefix(&self) -> String {
        format!("v2/members/{}/", self.pool_hash())
    }
    fn overshoot(&self) -> u32 {
        self.max_overshoot
            .unwrap_or_else(|| (3.0 * f64::from(self.target).sqrt()).ceil() as u32)
    }
    pub(crate) async fn prepare_with_timeout(
        &self,
        store: Arc<dyn SamplingStore>,
        timeout: Duration,
    ) -> PreparedSampling {
        engine::prepare_using(self.clone(), store, timeout, Arc::new(engine::WallClock)).await
    }
}

//! Sticky startup selection from one roster, maintained by an elected census leader.
mod engine;
mod rate_limit;
#[cfg(test)]
pub(crate) mod simulation;
#[cfg(all(test, shuttle))]
mod storage_shuttle_tests;

use crate::backend::SamplingStore;
pub(crate) use engine::{
    DecisionReason, PreparedSampling, SamplingSession, SamplingSnapshot, startup_draw,
};
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
    /// Uses the stable rank with probability `min(1, target / population)`.
    /// The population assumption is the caller's; it does not bound actual volume.
    #[non_exhaustive]
    AssumePopulation {
        /// Assumed number of live processes. Must be positive.
        population: u64,
    },
    /// Uses one independent draw. Fleet probability is capped by the tracked fraction.
    #[non_exhaustive]
    EnableWithFixedProbability {
        /// A finite probability in the inclusive range 0–1.
        probability: f64,
    },
}
impl StartupFailure {
    /// Keeps profiling off on startup failure or an absent/unsettled roster.
    pub fn disabled() -> Self {
        Self::Disabled {}
    }
    /// Selects by stable rank using a positive assumed population.
    pub fn assume_population(population: u64) -> Self {
        Self::AssumePopulation { population }
    }
    /// Selects with one independent draw. The fleet-wide probability is capped
    /// by the tracked fraction; actual profilers can exceed the target.
    pub fn enable_with_fixed_probability(probability: f64) -> Self {
        Self::EnableWithFixedProbability { probability }
    }
}
impl Default for StartupFailure {
    fn default() -> Self {
        Self::disabled()
    }
}

/// All hosts in a namespace must use the same policy. Host identity is not policy.
#[derive(Clone, Debug, bon::Builder)]
pub(crate) struct Config {
    #[builder(field)]
    dimensions: Vec<(String, String)>,
    #[builder(into)]
    scope: String,
    exact_target: u32,
    if_failed: StartupFailure,
    #[builder(default = ulid::Ulid::new().to_string(), into)]
    host_id: String,
    #[builder(default)]
    minimum_population: u64,
    max_overshoot: Option<u32>,
    #[builder(default = Duration::from_secs(60))]
    poll_interval: Duration,
    #[builder(default = Duration::from_secs(300))]
    bucket_width: Duration,
    #[builder(default = Duration::from_secs(5))]
    max_clock_skew: Duration,
}
impl<S: config_builder::State> ConfigBuilder<S> {
    pub(crate) fn add_dimension(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.dimensions.push((name.into(), value.into()));
        self
    }
}
impl Config {
    pub(crate) fn target(&self) -> u32 {
        self.exact_target
    }
    /// An explicit lower population assumption supports reducing census traffic.
    fn slice_bits(&self) -> u32 {
        if self.exact_target == 0 {
            return 0;
        }
        let required = 32 * u64::from(self.exact_target);
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
        let canonical = serde_json::to_vec(&(&self.scope, dimensions, &self.host_id))
            .expect("string tuple serializes");
        u64::from_be_bytes(
            Sha256::digest(canonical)[..8]
                .try_into()
                .expect("digest length"),
        )
    }
    pub(crate) fn failure_decision(&self, draw: f64) -> bool {
        self.exact_target > 0
            && self.validate().is_ok()
            && self.tracked()
            && match self.if_failed {
                StartupFailure::Disabled {} => false,
                StartupFailure::AssumePopulation { population } => {
                    let threshold = ((u128::from(self.exact_target) << 64)
                        / u128::from(population))
                    .min(self.tracked_limit());
                    u128::from(self.rank()) < threshold
                }
                StartupFailure::EnableWithFixedProbability { probability } => {
                    // The draw is conditional on membership in the tracked
                    // slice. Preserve the caller's fleet-wide probability
                    // below that slice's unavoidable ceiling.
                    let conditional = (probability * (1u64 << self.slice_bits()) as f64).min(1.0);
                    draw < conditional
                }
            }
    }
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.scope.trim().is_empty()
            || self.host_id.trim().is_empty()
            || self.host_id.contains('/')
        {
            return Err("scope and host_id must be nonblank; host_id must not contain '/'".into());
        }
        let mut names = std::collections::HashSet::new();
        for (name, value) in &self.dimensions {
            if name.trim().is_empty() || value.trim().is_empty() || !names.insert(name) {
                return Err("dimensions must have unique, nonblank names and values".into());
            }
        }
        match self.if_failed {
            StartupFailure::AssumePopulation { population: 0 } => {
                return Err("assumed population must be positive".into());
            }
            StartupFailure::EnableWithFixedProbability { probability }
                if !probability.is_finite() || !(0.0..=1.0).contains(&probability) =>
            {
                return Err("probability must be finite and between zero and one".into());
            }
            _ => {}
        }
        // Whole milliseconds are the wire format. Bound all time arithmetic and
        // leave a previous bucket visible while a delayed heartbeat is retried.
        if self.exact_target > 10000
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
    fn pool_hash(&self) -> String {
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
            .unwrap_or_else(|| (3.0 * f64::from(self.exact_target).sqrt()).ceil() as u32)
    }
    pub(crate) async fn prepare_with_timeout_and_draw(
        &self,
        store: Arc<dyn SamplingStore>,
        timeout: Duration,
        draw: f64,
    ) -> PreparedSampling {
        engine::prepare_using(
            self.clone(),
            store,
            timeout,
            Some(draw),
            Arc::new(engine::WallClock),
        )
        .await
    }
}

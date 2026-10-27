//! One startup decision per process, with conditional leases and no rotation.
//! Continuity and probability fallback can exceed the coordinated slot target.
#![deny(missing_docs)]

mod engine;
#[cfg(all(test, not(shuttle)))]
mod lifecycle_tests;
#[cfg(test)]
pub(crate) mod simulation;
use crate::backend::SamplingStore;
pub use engine::{
    DecisionReason, PreparedSampling, SamplingSession, SamplingSnapshot, startup_draw,
};

use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

/// Default startup preparation budget: 500 milliseconds.
pub const DEFAULT_PREPARE_TIMEOUT: Duration = Duration::from_millis(500);

#[non_exhaustive]
#[derive(Clone, Debug)]
/// Profiling choice when startup storage fails or preparation times out.
pub enum StartupFailure {
    #[non_exhaustive]
    /// Keeps profiling off on a startup failure.
    Disabled {},
    #[non_exhaustive]
    /// Uses one fixed-probability fallback draw; this can exceed the requested target.
    EnableWithFixedProbability {
        /// Probability in the inclusive range 0–1; invalid values fail configuration.
        probability: f64,
    },
}
impl StartupFailure {
    /// Chooses to keep profiling off on startup storage failure or timeout.
    pub fn disabled() -> Self {
        Self::Disabled {}
    }

    /// Best effort; not recommended when exceeding the target is unacceptable.
    pub fn enable_with_fixed_probability(probability: f64) -> Self {
        Self::EnableWithFixedProbability { probability }
    }
}
impl Default for StartupFailure {
    fn default() -> Self {
        Self::disabled()
    }
}
#[non_exhaustive]
#[derive(Clone, Debug)]
/// Behavior after an admitted process loses its lease.
pub enum LeaseLoss {
    #[non_exhaustive]
    /// Keeps profiling after lease loss (the default).
    KeepProfiling {},
    #[non_exhaustive]
    /// Stops permanently after lease loss.
    StopProfiling {},
}
impl Default for LeaseLoss {
    fn default() -> Self {
        Self::KeepProfiling {}
    }
}
impl LeaseLoss {
    /// Keeps the original on choice after lease loss; actual profilers can exceed the target.
    pub fn keep_profiling() -> Self {
        Self::KeepProfiling {}
    }
    /// Permanently stops profiling on lease loss and prevents re-enabling.
    pub fn stop_profiling() -> Self {
        Self::StopProfiling {}
    }
}

/// Processes with the same scope and dimensions share numbered profiling slots.
///
/// Targets affect startup probes, not the identity of the shared slots. During
/// mixed-target rollouts the largest participating target bounds coordinated
/// owners; reducing a target does not stop processes already profiling.
///
#[derive(Clone, bon::Builder)]
pub struct Config {
    #[builder(field)]
    dimensions: Vec<(String, String)>,
    /// Required service/environment scope separating unrelated fleets.
    #[builder(into)]
    scope: String,
    /// Desired profiler count for each complete combination of dimension values.
    exact_target: u32,
    /// Required choice on startup storage failure or timeout.
    if_failed: StartupFailure,
    /// Behavior after losing coordination; defaults to keeping profiling enabled.
    #[builder(default)]
    lease_loss: LeaseLoss,
    /// Server lease duration; defaults to five minutes and must be at most one day.
    #[builder(default = Duration::from_secs(300))]
    lease_duration: Duration,
    /// Maintenance interval; defaults to one minute and must leave lease margins.
    #[builder(default = Duration::from_secs(60))]
    renewal_interval: Duration,
    /// Maximum assumed wall-clock skew; defaults to five seconds.
    #[builder(default = Duration::from_secs(5))]
    max_clock_skew: Duration,
    /// Bottom-k membership sample size, from 2 through 1,000; defaults to 256.
    #[builder(default = 256)]
    membership_sample_size: usize,
}
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("scope", &self.scope)
            .field("dimensions", &self.dimensions)
            .field("exact_target", &self.exact_target)
            .finish_non_exhaustive()
    }
}
impl<S: config_builder::State> ConfigBuilder<S> {
    /// Adds a caller-provided dimension. Names must be unique; names and values must be nonblank.
    pub fn add_dimension(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.dimensions.push((name.into(), value.into()));
        self
    }
}
impl Config {
    /// Returns the caller's requested number of profiling processes for these dimensions.
    pub fn target(&self) -> u32 {
        self.exact_target
    }
    /// Applies the failure policy to one draw in [0, 1). Invalid configuration and target zero stay off.
    pub fn failure_decision(&self, draw: f64) -> bool {
        self.exact_target > 0
            && self.validate().is_ok()
            && match self.if_failed {
                StartupFailure::Disabled {} => false,
                StartupFailure::EnableWithFixedProbability { probability } => draw < probability,
            }
    }
    /// Checks labels, probability, target and lease settings without performing storage I/O.
    pub fn validate(&self) -> Result<(), String> {
        if self.scope.trim().is_empty() {
            return Err("sampling scope is blank".into());
        }
        let mut names = std::collections::HashSet::new();
        for (name, value) in &self.dimensions {
            if name.trim().is_empty() || value.trim().is_empty() || !names.insert(name) {
                return Err(
                    "sampling dimensions must have unique, nonblank names and values".into(),
                );
            }
        }
        if let StartupFailure::EnableWithFixedProbability { probability } = self.if_failed
            && (!probability.is_finite() || !(0.0..=1.0).contains(&probability))
        {
            return Err("sampling probability must be finite and between zero and one".into());
        }
        if self.lease_duration > Duration::from_secs(86400)
            || self.renewal_interval.is_zero()
            || self.lease_duration
                <= self
                    .renewal_interval
                    .saturating_add(self.max_clock_skew.saturating_mul(2))
                    .saturating_add(DEFAULT_PREPARE_TIMEOUT)
        {
            return Err("lease duration must exceed renewal interval and clock-skew margins, and be at most one day".into());
        }
        if !(2..=1000).contains(&self.membership_sample_size) || self.exact_target > 10000 {
            return Err("membership size must be 2..=1000 and target at most 10000".into());
        }
        Ok(())
    }
    /// Returns the relative coordination prefix, hashing scope and sorted dimensions independently of target.
    pub fn pool_prefix(&self) -> String {
        let mut dimensions = self.dimensions.clone();
        dimensions.sort();
        let canonical =
            serde_json::to_vec(&(&self.scope, dimensions)).expect("string tuple serializes");
        format!("v1/pools/{:x}/", Sha256::digest(canonical))
    }
    /// Prepares a test startup choice on the caller's executor without starting profilers.
    #[cfg(all(test, not(shuttle)))]
    pub async fn prepare(&self, store: Arc<dyn SamplingStore>) -> PreparedSampling {
        self.prepare_with_timeout(store, DEFAULT_PREPARE_TIMEOUT)
            .await
    }
    /// Prepares one startup choice within the supplied cooperative budget. Failure uses the configured fallback.
    #[cfg(all(test, not(shuttle)))]
    pub async fn prepare_with_timeout(
        &self,
        store: Arc<dyn SamplingStore>,
        timeout: Duration,
    ) -> PreparedSampling {
        engine::prepare(self.clone(), store, timeout).await
    }
    #[doc(hidden)]
    pub async fn prepare_with_timeout_and_draw(
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

#[cfg(all(test, not(shuttle)))]
mod tests;

//! Retained census observations for retrospective estimates of trace-emitting units.
//!
//! An observation counts visible process/container runs, including
//! those with profiling off. It is a complete enumeration of two membership
//! buckets, not an atomic snapshot of live processes. Missing registrations,
//! churn, crashes and cleanup failures cause additional error that the Gaussian
//! sampling approximation does not cover. Preserve this namespace at least as
//! long as the traces that will be analyzed.

use crate::{
    Sampling,
    backend::{ListRequest, SamplingStore, StoreError, StoreErrorKind},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const DAY_MS: u64 = 86_400_000;

/// The most recent complete census preceding the requested time.
///
/// This is evidence for a population estimate, rather than an exact live count
/// or a profiling inclusion probability. Gaussian uncertainty covers only
/// independent hash sampling of visible membership at a fixed epoch fraction.
/// The fraction is chosen before that epoch's fresh hash salt. Membership
/// delivery, departures, and selecting an epoch's promotion time add error.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct PopulationEstimate {
    /// Time at which the census started listing membership.
    pub observation_started_at: SystemTime,
    /// Time at which the complete census finished.
    pub observation_completed_at: SystemTime,
    /// Requested time minus the census completion time.
    pub age: Duration,
    /// Width of one membership bucket; the census lists two consecutive buckets.
    pub bucket_width: Duration,
    /// Declared maximum host clock skew relative to storage UTC.
    pub max_clock_skew: Duration,
    /// Backend timestamp uncertainty used by the census guards.
    pub timestamp_uncertainty: Duration,
    /// Reporting policy whose membership was enumerated.
    pub reporting_epoch: String,
    /// Number of distinct tracked process/container runs observed.
    pub tracked: u64,
    /// Fraction of independent identities eligible for census tracking.
    pub tracking_probability: f64,
    /// Estimated visible population: `tracked / tracking_probability`.
    pub population: f64,
    /// Plug-in Gaussian standard error, available for >=30 observations or a full census.
    ///
    /// `None` for sparse counts, including zero: a sparse empty census does
    /// not establish an empty population. This is not an error bound for the
    /// instantaneous live population.
    pub gaussian_standard_error: Option<f64>,
    /// Sampler-admitted members; actual profiling requires recorder/trace evidence.
    pub admitted: u64,
    /// Terminally trimmed members still contributing to the census.
    pub carriers: u64,
    /// Members whose frozen startup choice was profiling off.
    pub unarmed: u64,
    /// Visible members with zero startup profiling probability.
    ///
    /// A positive count exposes a cohort that traces cannot reconstruct.
    pub no_support: u64,
}

// Kept separately from the mutable CAS roster and membership cleanup. Even a
// fenced publisher's complete census is useful observational evidence.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CensusObservation {
    pub(crate) schema: u32,
    pub(crate) pool: String,
    pub(crate) started_ms: u64,
    pub(crate) completed_ms: u64,
    pub(crate) bucket: u64,
    pub(crate) bucket_ms: u64,
    pub(crate) skew_ms: u64,
    pub(crate) timestamp_uncertainty_ms: u64,
    pub(crate) reporting_epoch: String,
    pub(crate) reporting_cutoff: Option<u64>,
    pub(crate) observed: u64,
    pub(crate) tracked: u64,
    pub(crate) admitted: u64,
    pub(crate) carriers: u64,
    pub(crate) unarmed: u64,
    pub(crate) no_support: u64,
}

pub(crate) fn prefix(pool: &str, day: u64) -> String {
    format!("v3/census/{pool}{day:016}/")
}

impl CensusObservation {
    pub(crate) fn key(&self) -> String {
        format!(
            "{}{:016}-{}.json",
            prefix(&self.pool, self.completed_ms / DAY_MS),
            self.completed_ms,
            ulid::Ulid::new()
        )
    }

    fn validate(&self, pool: &str, day: u64) -> Result<(), StoreError> {
        let guard = u128::from(self.skew_ms) * 2 + u128::from(self.timestamp_uncertainty_ms);
        let width = u128::from(self.bucket_ms);
        if self.schema != 2
            || self.pool != pool
            || self.reporting_cutoff == Some(0)
            || (self.reporting_epoch != "bootstrap"
                && self.reporting_epoch.parse::<ulid::Ulid>().is_err())
            || self.bucket_ms == 0
            || self.started_ms > self.completed_ms
            || self.completed_ms / DAY_MS != day
            || self.started_ms / self.bucket_ms != self.bucket
            || self.completed_ms / self.bucket_ms != self.bucket
            || self.no_support > self.unarmed
            || [self.started_ms, self.completed_ms]
                .into_iter()
                .any(|time| {
                    width == 0
                        || u128::from(time) % width < guard
                        || u128::from(time) % width + guard >= width
                })
            || self
                .admitted
                .checked_add(self.carriers)
                .and_then(|n| n.checked_add(self.unarmed))
                != Some(self.observed)
            || self.tracked > self.observed
        {
            return Err(invalid("invalid retained census observation"));
        }
        Ok(())
    }

    fn estimate(&self, requested_ms: u64) -> PopulationEstimate {
        let probability = self
            .reporting_cutoff
            .map_or(1.0, |cutoff| cutoff as f64 / (1u128 << 64) as f64);
        let standard_error = (probability == 1.0 || self.tracked >= 30)
            .then(|| (self.tracked as f64 * (1.0 - probability)).sqrt() / probability);
        PopulationEstimate {
            observation_started_at: UNIX_EPOCH + Duration::from_millis(self.started_ms),
            observation_completed_at: UNIX_EPOCH + Duration::from_millis(self.completed_ms),
            age: Duration::from_millis(requested_ms - self.completed_ms),
            bucket_width: Duration::from_millis(self.bucket_ms),
            max_clock_skew: Duration::from_millis(self.skew_ms),
            timestamp_uncertainty: Duration::from_millis(self.timestamp_uncertainty_ms),
            reporting_epoch: self.reporting_epoch.clone(),
            tracked: self.tracked,
            tracking_probability: probability,
            population: self.tracked as f64 / probability,
            gaussian_standard_error: standard_error,
            admitted: self.admitted,
            carriers: self.carriers,
            unarmed: self.unarmed,
            no_support: self.no_support,
        }
    }
}

fn invalid(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> StoreError {
    StoreError::new(StoreErrorKind::invalid_response(), message)
}

#[bon::bon]
impl Sampling {
    /// Reads the latest complete census ending at or before a requested time.
    ///
    /// Call as `sampling.population_at().store(&store).at(time).max_age(age).call().await`.
    /// Storage namespace and dimensions identify the pool; the historical record supplies
    /// its tracking fraction. `max_age` explicitly limits extrapolation and
    /// storage work. No history or no sufficiently recent census returns `None`.
    ///
    /// Clock skew, observation duration and the two-bucket membership window
    /// remain relevant even with age zero. Do not use this population estimate
    /// as the inverse selection probability of every trace: startup cohorts
    /// and terminal trimming can have different probabilities.
    ///
    /// # Errors
    ///
    /// Returns an error for storage failures, incomplete/cyclic pagination,
    /// missing listed objects, malformed evidence, or dates before Unix epoch.
    #[builder]
    pub async fn population_at(
        &self,
        store: &dyn SamplingStore,
        at: SystemTime,
        max_age: Duration,
    ) -> Result<Option<PopulationEstimate>, StoreError> {
        let requested = u64::try_from(at.duration_since(UNIX_EPOCH).map_err(invalid)?.as_millis())
            .map_err(invalid)?;
        let age_ms = u64::try_from(max_age.as_millis()).map_err(invalid)?;
        let earliest = requested.saturating_sub(age_ms);
        let pool = self.pool_path();
        let mut best: Option<(String, CensusObservation)> = None;
        // Search newest day first; once a day has a qualifying observation,
        // preceding days cannot improve it. Page order is never assumed.
        for day in (earliest / DAY_MS..=requested / DAY_MS).rev() {
            let day_prefix = prefix(&pool, day);
            let mut continuation = None;
            let mut cursors = HashSet::new();
            let mut keys = HashSet::new();
            loop {
                let page = store
                    .list(
                        ListRequest::builder()
                            .prefix(day_prefix.clone())
                            .maybe_continuation(continuation)
                            .build(),
                    )
                    .await?;
                for listed in page.objects() {
                    let key = listed.key();
                    if !key.starts_with(&day_prefix) {
                        return Err(invalid("census listing escaped requested prefix"));
                    }
                    if !keys.insert(key.to_owned()) {
                        continue;
                    }
                    let suffix = key.strip_prefix(&day_prefix).expect("prefix checked");
                    let Some((time, id)) = suffix.split_once('-') else {
                        return Err(invalid("invalid retained census key"));
                    };
                    let ended = time.parse::<u64>().map_err(invalid)?;
                    if time.len() != 16
                        || !id.ends_with(".json")
                        || id.trim_end_matches(".json").parse::<ulid::Ulid>().is_err()
                        || ended / DAY_MS != day
                    {
                        return Err(invalid("invalid retained census key"));
                    }
                    // Listing a day is cheap; fetching every observation is
                    // not. Read bodies only within the requested age window.
                    if ended < earliest || ended > requested {
                        continue;
                    }
                    let object = store
                        .get(key.to_owned())
                        .await?
                        .ok_or_else(|| invalid("retained census disappeared during analysis"))?;
                    let observation: CensusObservation =
                        serde_json::from_slice(object.body()).map_err(invalid)?;
                    observation.validate(&pool, day)?;
                    if observation.completed_ms != ended {
                        return Err(invalid("census key/body timestamp mismatch"));
                    }
                    if best.as_ref().is_none_or(|(previous_key, b)| {
                        (observation.completed_ms, observation.started_ms, key)
                            > (b.completed_ms, b.started_ms, previous_key.as_str())
                    }) {
                        best = Some((key.to_owned(), observation));
                    }
                }
                continuation = page.continuation().cloned();
                match &continuation {
                    None => break,
                    Some(cursor) if !cursors.insert(cursor.as_str().to_owned()) => {
                        return Err(invalid("cyclic census pagination"));
                    }
                    Some(_) => {}
                }
            }
            if best.is_some() {
                break;
            }
        }
        Ok(best.map(|(_, observation)| observation.estimate(requested)))
    }
}

#[cfg(all(test, not(shuttle)))]
mod tests;

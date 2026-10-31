//! Expansion-weight metadata, evaluated when the recorder polls its sources.
//! The writer carries the latest snapshot into each segment. Zero means stopped;
//! null means unknown. Process identity and clock information come from the trace.

use crate::backend::{StoreError, StoreErrorKind};
use dial9_core::{
    primitives::sync::{Arc, Mutex},
    recording::Recorder,
    source::{FlushContext, Source},
};

#[derive(Clone, Debug)]
pub(crate) struct TraceWeights {
    pool: String,
    weight: Arc<Mutex<Option<f64>>>,
    emitted_weight: Option<Option<f64>>,
}

impl TraceWeights {
    pub(crate) fn new(pool: String, initial_weight: f64) -> Self {
        assert!(initial_weight.is_finite() && initial_weight > 0.0);
        Self {
            pool,
            weight: Arc::new(Mutex::new(Some(initial_weight))),
            emitted_weight: None,
        }
    }

    /// Weights for a process that was not randomly admitted.
    pub(crate) fn unknown(pool: String) -> Self {
        Self {
            pool,
            weight: Arc::new(Mutex::new(None)),
            emitted_weight: None,
        }
    }

    pub(crate) fn current_expansion_weight(&self) -> Option<f64> {
        *self.weight.lock().expect("sampling weight lock")
    }

    pub(crate) fn multiply(&self, factor: f64) -> Result<(), StoreError> {
        let mut weight = self.weight.lock().expect("sampling weight lock");
        if !weight.is_some_and(|weight| weight > 0.0) {
            return Err(StoreError::new(
                StoreErrorKind::invalid_response(),
                "cannot thin an excluded or unknown sampling weight",
            ));
        }
        *weight = weight
            .filter(|_| factor.is_finite() && factor > 0.0)
            .map(|weight| weight * factor)
            .filter(|weight| weight.is_finite() && *weight > 0.0);
        weight.ok_or_else(|| {
            StoreError::new(
                StoreErrorKind::invalid_response(),
                "sampling expansion weight is no longer finite and positive",
            )
        })?;
        Ok(())
    }

    pub(crate) fn stop(&self) {
        *self.weight.lock().expect("sampling weight lock") = Some(0.0);
    }

    pub(crate) fn attach(&self, recorder: &Recorder) -> bool {
        let attached = recorder.handle().with_source_or_insert(
            || Self {
                pool: self.pool.clone(),
                weight: self.weight.clone(),
                emitted_weight: None,
            },
            |source| Arc::ptr_eq(&source.weight, &self.weight),
        ) == Some(true);
        if !attached {
            crate::protocol::rate_limited!(std::time::Duration::from_secs(60), {
                tracing::warn!(
                    expansion_weight = ?self.current_expansion_weight(),
                    "could not attach sampling trace weights to the recorder"
                );
            });
        }
        attached
    }
}

impl Source for TraceWeights {
    fn flush(&mut self, _: &FlushContext<'_>) {}

    fn name(&self) -> &'static str {
        "sampling_weights"
    }

    fn segment_metadata(&mut self, out: &mut Vec<(String, String)>) {
        let weight = self.current_expansion_weight();
        if self.emitted_weight == Some(weight) {
            return;
        }
        out.extend([
            ("dial9.sampling.pool".into(), self.pool.clone()),
            (
                "dial9.sampling.weight".into(),
                weight.map_or_else(|| "null".into(), |weight| weight.to_string()),
            ),
        ]);
        self.emitted_weight = Some(weight);
    }
}

#[cfg(all(test, not(shuttle)))]
mod tests {
    use super::*;

    #[test]
    fn metadata_snapshots_current_weight_and_skips_unchanged_updates() {
        let weights = TraceWeights::new("pool".into(), 10.0);
        let mut source = weights.clone();
        let mut entries = Vec::new();
        source.segment_metadata(&mut entries);
        assert_eq!(
            entries,
            [
                ("dial9.sampling.pool".into(), "pool".into()),
                ("dial9.sampling.weight".into(), "10".into()),
            ]
        );
        weights.multiply(2.0).unwrap();
        assert_eq!(entries[1].1, "10", "a prior snapshot stays unchanged");
        entries.clear();
        source.segment_metadata(&mut entries);
        assert_eq!(entries[1].1, "20");
        entries.clear();
        source.segment_metadata(&mut entries);
        assert!(entries.is_empty());
        assert!(weights.multiply(f64::MAX).is_err());
        source.segment_metadata(&mut entries);
        assert_eq!(entries[1].1, "null", "overflow is unknown");
        entries.clear();
        source.segment_metadata(&mut entries);
        assert!(entries.is_empty());
    }

    #[test]
    fn terminal_decision_is_known_zero_and_cannot_resume() {
        let weights = TraceWeights::new("pool".into(), 10.0);
        weights.multiply(2.0).unwrap();
        weights.stop();
        weights.stop();
        assert!(weights.multiply(2.0).is_err());
        let mut entries = Vec::new();
        weights.clone().segment_metadata(&mut entries);
        assert_eq!(entries[1].1, "0");
    }
}

#[cfg(all(test, shuttle))]
mod shuttle_tests {
    use super::*;

    dial9_core::shuttle_test! {
        default;
        fn shuttle_weight_snapshot_races_survivor_and_terminal_updates() {
            let weights = TraceWeights::new("pool".into(), 10.0);
            let update = weights.clone();
            let worker = dial9_core::primitives::thread::spawn(move || {
                update.multiply(2.0).unwrap();
                update.stop();
            });
            let mut source = weights.clone();
            let mut last_weight = None;
            for _ in 0..2 {
                let mut entries = Vec::new();
                source.segment_metadata(&mut entries);
                if let Some((_, value)) = entries.last() {
                    assert!(matches!(value.as_str(), "10" | "20" | "0"));
                    if last_weight.as_deref() == Some("0") {
                        assert_eq!(value, "0", "terminal metadata cannot resume");
                    }
                    last_weight = Some(value.clone());
                }
            }
            worker.join().unwrap();
            let mut final_snapshot = Vec::new();
            weights.clone().segment_metadata(&mut final_snapshot);
            assert_eq!(final_snapshot[1].1, "0");
        }
    }
}

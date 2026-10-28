//! Segment metadata for applying recording-time expansion factors to frames.
//! History uses the trace's monotonic clock; coverage ends at the checkpoint.
//! Later segments repeat the merged history and can extend earlier coverage.
//! Use the last change at or before each frame timestamp, never the current
//! weight for an entire segment. Zero excludes late frames after a terminal
//! decision; null means unknown. Frames beyond the checkpoint lack coverage.

use crate::backend::{StoreError, StoreErrorKind};
use dial9_core::{
    clock::clock_monotonic_ns,
    primitives::sync::{Arc, Mutex},
    recording::Recorder,
    source::{FlushContext, Source},
};

#[derive(Debug)]
struct State {
    pool: String,
    incarnation: String,
    history: Vec<(u64, Option<f64>)>,
    observed_through_ns: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct TraceWeights {
    state: Arc<Mutex<State>>,
    emitted_history: usize,
}

impl TraceWeights {
    pub(crate) fn new(pool: String, incarnation: String, initial_weight: f64) -> Self {
        assert!(initial_weight.is_finite() && initial_weight > 0.0);
        let timestamp = clock_monotonic_ns();
        Self {
            state: Arc::new(Mutex::new(State {
                pool,
                incarnation,
                history: vec![(timestamp, Some(initial_weight))],
                observed_through_ns: timestamp,
            })),
            emitted_history: 0,
        }
    }

    pub(crate) fn current_expansion_weight(&self) -> Option<f64> {
        self.state
            .lock()
            .expect("sampling weight lock")
            .history
            .last()
            .expect("initial sampling weight")
            .1
    }

    pub(crate) fn multiply(&self, factor: f64) -> Result<(), StoreError> {
        let mut state = self.state.lock().expect("sampling weight lock");
        let weight = state.history.last().expect("initial sampling weight").1;
        if !weight.is_some_and(|weight| weight > 0.0) {
            return Err(StoreError::new(
                StoreErrorKind::invalid_response(),
                "cannot thin an excluded or unknown sampling weight",
            ));
        }
        let next = weight
            .filter(|_| factor.is_finite() && factor > 0.0)
            .map(|weight| weight * factor)
            .filter(|weight| weight.is_finite() && *weight > 0.0);
        if next != weight {
            let timestamp = clock_monotonic_ns().max(state.observed_through_ns);
            state.history.push((timestamp, next));
            state.observed_through_ns = timestamp;
        }
        next.map(|_| ()).ok_or_else(|| {
            StoreError::new(
                StoreErrorKind::invalid_response(),
                "sampling expansion weight is no longer finite and positive",
            )
        })
    }

    pub(crate) fn stop(&self) {
        let mut state = self.state.lock().expect("sampling weight lock");
        if state.history.last().expect("initial sampling weight").1 != Some(0.0) {
            let timestamp = clock_monotonic_ns().max(state.observed_through_ns);
            state.history.push((timestamp, Some(0.0)));
            state.observed_through_ns = timestamp;
        }
    }

    pub(crate) fn attach(&self, recorder: &Recorder) -> bool {
        let attached = recorder.handle().with_source_or_insert(
            || Self {
                state: self.state.clone(),
                emitted_history: 0,
            },
            |source| Arc::ptr_eq(&source.state, &self.state),
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
        let mut state = self.state.lock().expect("sampling weight lock");
        state.observed_through_ns = clock_monotonic_ns().max(state.observed_through_ns);
        // The checkpoint changes each flush. History changes only on thinning;
        // the writer merges it and repeats the entire map in rotated segments.
        if self.emitted_history != state.history.len() {
            out.extend([
                ("dial9.sampling.schema".into(), "1".into()),
                ("dial9.sampling.unit".into(), "trace_emitting_unit".into()),
                ("dial9.sampling.clock".into(), "monotonic_ns".into()),
                ("dial9.sampling.pool".into(), state.pool.clone()),
                (
                    "dial9.sampling.incarnation".into(),
                    state.incarnation.clone(),
                ),
                (
                    "dial9.sampling.weight_history".into(),
                    serde_json::to_string(&state.history).expect("finite sampling weights"),
                ),
                (
                    "dial9.sampling.weight_known".into(),
                    state
                        .history
                        .last()
                        .expect("initial sampling weight")
                        .1
                        .is_some()
                        .to_string(),
                ),
            ]);
            self.emitted_history = state.history.len();
        }
        out.push((
            "dial9.sampling.observed_through_ns".into(),
            state.observed_through_ns.to_string(),
        ));
    }
}

#[cfg(all(test, not(shuttle)))]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn snapshot(source: &mut TraceWeights, merged: &mut HashMap<String, String>) {
        let mut entries = Vec::new();
        source.segment_metadata(&mut entries);
        merged.extend(entries);
    }

    #[test]
    fn checkpoints_retain_full_chronological_history_and_overflow_is_unknown() {
        let weights = TraceWeights::new("pool".into(), "process".into(), 10.0);
        let mut source = weights.clone();
        let mut metadata = HashMap::new();
        snapshot(&mut source, &mut metadata);
        let initial_checkpoint = metadata["dial9.sampling.observed_through_ns"]
            .parse::<u64>()
            .unwrap();
        weights.multiply(2.0).unwrap();
        snapshot(&mut source, &mut metadata);
        snapshot(&mut source, &mut metadata);
        let history: Vec<(u64, Option<f64>)> =
            serde_json::from_str(&metadata["dial9.sampling.weight_history"]).unwrap();
        assert_eq!(
            history
                .iter()
                .map(|(_, weight)| *weight)
                .collect::<Vec<_>>(),
            [Some(10.0), Some(20.0)]
        );
        assert!(history[1].0 >= initial_checkpoint);
        assert!(history.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert!(
            history.last().unwrap().0
                <= metadata["dial9.sampling.observed_through_ns"]
                    .parse::<u64>()
                    .unwrap()
        );
        assert_eq!(weights.current_expansion_weight(), Some(20.0));
        assert!(weights.multiply(f64::MAX).is_err());
        assert_eq!(weights.current_expansion_weight(), None);
        snapshot(&mut source, &mut metadata);
        assert_eq!(metadata["dial9.sampling.weight_known"], "false");
        let history: Vec<(u64, Option<f64>)> =
            serde_json::from_str(&metadata["dial9.sampling.weight_history"]).unwrap();
        assert_eq!(history.last().unwrap().1, None);
        assert_eq!(history[0].1, Some(10.0), "earlier valid intervals survive");
    }

    #[test]
    fn attaching_is_idempotent_and_rejects_a_different_invocation() {
        let recorder =
            dial9_core::recorder::recorder(dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap())
                .build();
        let weights = TraceWeights::new("pool".into(), "process".into(), 10.0);
        assert!(weights.attach(&recorder));
        assert!(weights.attach(&recorder));
        let different = TraceWeights::new("pool".into(), "other-process".into(), 10.0);
        assert!(!different.attach(&recorder));
        assert!(!weights.attach(&dial9_core::recorder::recorder_disabled()));
        let handle = recorder.handle().clone();
        recorder.graceful_shutdown(std::time::Duration::ZERO);
        assert!(!handle.is_enabled(), "weights do not own the recorder");
        weights.multiply(2.0).unwrap();
        assert_eq!(weights.current_expansion_weight(), Some(20.0));
    }

    #[test]
    fn terminal_decision_is_known_zero_and_cannot_resume() {
        let weights = TraceWeights::new("pool".into(), "process".into(), 10.0);
        weights.multiply(2.0).unwrap();
        weights.stop();
        weights.stop();
        assert!(weights.multiply(2.0).is_err());
        assert_eq!(weights.current_expansion_weight(), Some(0.0));
        let mut metadata = HashMap::new();
        snapshot(&mut weights.clone(), &mut metadata);
        let history: Vec<(u64, Option<f64>)> =
            serde_json::from_str(&metadata["dial9.sampling.weight_history"]).unwrap();
        assert_eq!(
            history.len(),
            3,
            "duplicate stops do not create transitions"
        );
        assert_eq!(history.last().unwrap().1, Some(0.0));
        assert!(history.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert_eq!(metadata["dial9.sampling.weight_known"], "true");
    }

    #[test]
    fn real_segment_weights_old_survivor_and_late_frames_at_their_timestamps() {
        #[derive(dial9::format::TraceEvent)]
        struct WeightMarker {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
        }

        let path = std::env::temp_dir().join(format!("dial9-weights-{}.bin", ulid::Ulid::new()));
        let sealed = path.with_extension("0.bin");
        let weights = TraceWeights::new("pool".into(), "process".into(), 10.0);
        let recorder = dial9_core::recorder::recorder(
            dial9_core::buffer::DiskBuffer::single_file(&path).unwrap(),
        )
        .build();
        assert!(weights.attach(&recorder));
        recorder.handle().record_event(WeightMarker {
            timestamp_ns: clock_monotonic_ns(),
        });
        weights.multiply(2.0).unwrap();
        recorder.handle().record_event(WeightMarker {
            timestamp_ns: clock_monotonic_ns(),
        });
        weights.stop();
        recorder.handle().record_event(WeightMarker {
            timestamp_ns: clock_monotonic_ns(),
        });
        recorder.graceful_shutdown(std::time::Duration::ZERO);
        let bytes = std::fs::read(&sealed).unwrap();
        std::fs::remove_file(sealed).unwrap();
        let mut metadata = HashMap::new();
        let mut markers = Vec::new();
        dial9::format::Decoder::new(&bytes)
            .unwrap()
            .for_each_event(|event| {
                if event.name == "SegmentMetadataEvent" {
                    if let Some(dial9::format::types::FieldValueRef::StringMap(entries)) =
                        event.fields.first()
                    {
                        metadata.extend(
                            entries
                                .iter()
                                .map(|(key, value)| (key.to_owned(), value.to_owned())),
                        );
                    }
                } else if event.name == "WeightMarker" {
                    markers.push(event.timestamp_ns);
                }
            })
            .unwrap();
        let history: Vec<(u64, Option<f64>)> =
            serde_json::from_str(&metadata["dial9.sampling.weight_history"]).unwrap();
        let checkpoint = metadata["dial9.sampling.observed_through_ns"]
            .parse::<u64>()
            .unwrap();
        let inferred: Vec<_> = markers
            .into_iter()
            .map(|timestamp| {
                assert!(timestamp <= checkpoint);
                history
                    .iter()
                    .rev()
                    .find(|(changed, _)| *changed <= timestamp)
                    .unwrap()
                    .1
            })
            .collect();
        assert_eq!(inferred, [Some(10.0), Some(20.0), Some(0.0)]);
        assert_eq!(metadata["dial9.sampling.unit"], "trace_emitting_unit");
    }
}

#[cfg(all(test, shuttle))]
mod shuttle_tests {
    use super::*;
    use std::collections::HashMap;

    dial9_core::shuttle_test! {
        default;
        fn shuttle_weight_checkpoint_races_survivor_and_terminal_updates() {
            let weights = TraceWeights::new("pool".into(), "process".into(), 10.0);
            let update = weights.clone();
            let worker = dial9_core::primitives::thread::spawn(move || {
                update.multiply(2.0).unwrap();
                update.stop();
            });
            let mut source = weights.clone();
            let mut metadata = HashMap::<String, String>::new();
            for _ in 0..2 {
                let mut entries = Vec::new();
                source.segment_metadata(&mut entries);
                metadata.extend(entries);
                let history: Vec<(u64, Option<f64>)> =
                    serde_json::from_str(&metadata["dial9.sampling.weight_history"]).unwrap();
                let checkpoint = metadata["dial9.sampling.observed_through_ns"].parse::<u64>().unwrap();
                assert!(history.iter().all(|(timestamp, _)| *timestamp <= checkpoint));
                assert!(history.windows(2).all(|pair| pair[0].0 <= pair[1].0));
            }
            worker.join().unwrap();
            assert_eq!(weights.current_expansion_weight(), Some(0.0));
            let mut final_entries = Vec::new();
            source.segment_metadata(&mut final_entries);
            metadata.extend(final_entries);
            let history: Vec<(u64, Option<f64>)> =
                serde_json::from_str(&metadata["dial9.sampling.weight_history"]).unwrap();
            assert_eq!(history.iter().map(|(_, weight)| *weight).collect::<Vec<_>>(),
                [Some(10.0), Some(20.0), Some(0.0)]);
        }
    }
}

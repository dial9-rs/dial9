//! Unified interval pairing for span enter/exit events.
//!
//! Modern (`span_instance_id` + `tid`) and legacy (`span_id` only) formats
//! normalize into this one event stream. The exact grouping key remains generic
//! so pairing can never merge distinct lanes through a lossy hash.

use std::hash::Hash;

use rustc_hash::FxHashMap;

use super::clock::{DurationNs, MonoNs};

/// A balanced interval in the producer's monotonic clock domain.
pub(crate) type MonoInterval = (MonoNs, MonoNs);

/// An enter or exit event for interval pairing.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PairingEvent<K> {
    pub(crate) timestamp: MonoNs,
    /// Monotonically increasing wire-decode sequence used to order timestamp ties.
    pub(crate) decode_sequence: u64,
    pub(crate) is_enter: bool,
    /// Exact pairing lane: modern uses `(instance_id, tid)`; legacy uses `span_id`.
    pub(crate) group_key: K,
}

/// Balanced intervals and unmatched accounting for one exact grouping key.
#[derive(Debug, Clone, Default)]
pub(crate) struct PairingResult {
    pub(crate) intervals: Vec<MonoInterval>,
    pub(crate) unmatched_exits: u32,
    pub(crate) unmatched_enters: u32,
}

/// Pair enter/exit events LIFO per exact key.
///
/// Events are sorted by `(timestamp, decode_sequence)`. Consequently an exit
/// encoded before an enter at the same timestamp remains unmatched, while an
/// enter encoded first produces a valid zero-duration interval.
pub(crate) fn pair_intervals<K>(events: &mut [PairingEvent<K>]) -> FxHashMap<K, PairingResult>
where
    K: Copy + Eq + Hash,
{
    events.sort_unstable_by_key(|event| (event.timestamp, event.decode_sequence));

    let mut stacks: FxHashMap<K, Vec<MonoNs>> = FxHashMap::default();
    let mut results: FxHashMap<K, PairingResult> = FxHashMap::default();

    for event in events {
        if event.is_enter {
            stacks
                .entry(event.group_key)
                .or_default()
                .push(event.timestamp);
            continue;
        }

        let matched = if let Some(stack) = stacks.get_mut(&event.group_key)
            && let Some(enter) = stack.pop()
            && event.timestamp >= enter
        {
            results
                .entry(event.group_key)
                .or_default()
                .intervals
                .push((enter, event.timestamp));
            true
        } else {
            false
        };
        if !matched {
            results.entry(event.group_key).or_default().unmatched_exits += 1;
        }
    }

    for (key, stack) in stacks {
        if !stack.is_empty() {
            results.entry(key).or_default().unmatched_enters += stack.len() as u32;
        }
    }

    results
}

/// Merge intervals into a sorted, non-overlapping set.
pub(crate) fn merge_intervals(intervals: &[MonoInterval]) -> Vec<MonoInterval> {
    if intervals.is_empty() {
        return Vec::new();
    }
    let mut sorted = intervals.to_vec();
    sorted.sort_unstable_by_key(|&(start, _)| start);
    let (mut current_start, mut current_end) = sorted[0];
    let mut merged = Vec::with_capacity(sorted.len());
    for &(start, end) in &sorted[1..] {
        if start <= current_end {
            current_end = current_end.max(end);
        } else {
            merged.push((current_start, current_end));
            current_start = start;
            current_end = end;
        }
    }
    merged.push((current_start, current_end));
    merged
}

/// Total duration covered by the union of the supplied intervals.
pub(crate) fn union_interval_duration(intervals: &[MonoInterval]) -> DurationNs {
    merge_intervals(intervals)
        .iter()
        .map(|&(start, end)| end.saturating_sub(start))
        .fold(DurationNs::ZERO, DurationNs::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event<K>(timestamp: u64, sequence: u64, is_enter: bool, key: K) -> PairingEvent<K> {
        PairingEvent {
            timestamp: MonoNs(timestamp),
            decode_sequence: sequence,
            is_enter,
            group_key: key,
        }
    }

    #[test]
    fn pairs_nested_events_lifo() {
        let mut events = [
            event(100, 0, true, 1),
            event(200, 1, true, 1),
            event(300, 2, false, 1),
            event(400, 3, false, 1),
        ];
        let results = pair_intervals(&mut events);
        let result = &results[&1];
        assert_eq!(
            result.intervals,
            vec![(MonoNs(200), MonoNs(300)), (MonoNs(100), MonoNs(400))]
        );
        assert_eq!(result.unmatched_enters, 0);
        assert_eq!(result.unmatched_exits, 0);
    }

    #[test]
    fn equal_timestamp_preserves_wire_order() {
        let mut balanced = [event(100, 0, true, 1), event(100, 1, false, 1)];
        let balanced = pair_intervals(&mut balanced);
        assert_eq!(balanced[&1].intervals, vec![(MonoNs(100), MonoNs(100))]);

        let mut reversed = [event(100, 0, false, 1), event(100, 1, true, 1)];
        let reversed = pair_intervals(&mut reversed);
        assert!(reversed[&1].intervals.is_empty());
        assert_eq!(reversed[&1].unmatched_exits, 1);
        assert_eq!(reversed[&1].unmatched_enters, 1);
    }

    #[test]
    fn exact_tuple_keys_never_share_a_lane() {
        // These collide under the removed `instance * C ^ tid` compression.
        const C: u64 = 0x517c_c1b7_2722_0a95;
        let key_a: (u64, u64) = (1, 0);
        let key_b: (u64, u64) = (2, C ^ C.wrapping_mul(2));
        assert_eq!(
            key_a.0.wrapping_mul(C) ^ key_a.1,
            key_b.0.wrapping_mul(C) ^ key_b.1
        );

        let mut events = [event(10, 0, true, key_a), event(20, 1, false, key_b)];
        let results = pair_intervals(&mut events);
        assert_eq!(results[&key_a].unmatched_enters, 1);
        assert_eq!(results[&key_b].unmatched_exits, 1);
        assert!(results.values().all(|result| result.intervals.is_empty()));
    }

    #[test]
    fn union_merges_overlap_and_adjacency() {
        let intervals = [
            (MonoNs(10), MonoNs(30)),
            (MonoNs(20), MonoNs(40)),
            (MonoNs(40), MonoNs(50)),
            (MonoNs(60), MonoNs(70)),
        ];
        assert_eq!(union_interval_duration(&intervals), DurationNs(50));
    }
}

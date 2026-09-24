//! Sample-to-span membership attribution (stage 3).
//!
//! After span resolution, this module builds a flat sorted
//! interval index and performs a sweep-line attribution of CPU/sched samples
//! to their enclosing spans.
//!
//! CRITICAL: We attach samples only to locally observed active intervals, never
//! to lifecycle envelopes when finer interval evidence is available. An async
//! span must not claim samples that fire during its idle gap.

use rustc_hash::{FxHashMap, FxHashSet};

use super::clock::{ClockOffset, WallNs};
use super::spans::span_builder::compute_span_uid;
use super::{ResolvedSample, ResolvedSpan, SOURCE_CPU_PROFILE};

/// One interval during which a span was executing on a specific runtime worker.
///
/// Legacy tracing guards may remain entered across an `.await`, so adapters
/// must intersect their raw enter/exit ranges with task polls before producing
/// these intervals. Worker identity is required because timestamps alone cannot
/// distinguish spans executing concurrently on different runtime threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttributionInterval {
    pub(crate) start: super::clock::MonoNs,
    pub(crate) end: super::clock::MonoNs,
    pub(crate) worker_id: u32,
}

/// A wall-clock interval mapped back to a resolved span index.
struct SpanInterval {
    start_wall: WallNs,
    end_wall: WallNs,
    span_idx: usize,
}

/// Attribute samples to their enclosing spans using the interval index.
///
/// Mutates `samples` (populates `enclosing_spans`) and `resolved_spans`
/// (increments `cpu_sample_count` / `sched_sample_count`).
///
/// `instance_intervals` maps synthetic span instance_id → list of monotonic
/// (enter, exit) intervals. `boot_id` is needed to compute span_uid for each
/// instance_id.
pub(crate) fn attribute_samples_to_spans(
    samples: &mut [ResolvedSample],
    resolved_spans: &mut [ResolvedSpan],
    instance_intervals: &FxHashMap<u64, Vec<AttributionInterval>>,
    boot_id: &str,
    clock_offset: Option<ClockOffset>,
) {
    let mut intervals_by_worker: FxHashMap<u32, Vec<SpanInterval>> = FxHashMap::default();

    let to_wall = |mono: super::clock::MonoNs| mono.to_wall_or_raw(clock_offset);

    // Index resolved spans by uid once so the per-interval lookup below is O(1).
    // Previously this was `resolved_spans.iter().position(...)` per interval —
    // an O(spans × intervals) scan that dominated attribution time on
    // span-heavy files (seconds on a 325k-span-event segment).
    let span_idx_by_uid: FxHashMap<[u8; 16], usize> = resolved_spans
        .iter()
        .enumerate()
        .map(|(idx, span)| (span.span_uid, idx))
        .collect();

    // Reconstructed intervals are keyed by synthetic instance ids.
    for (synthetic_instance_id, intervals) in instance_intervals {
        let target_uid = compute_span_uid(boot_id, *synthetic_instance_id);
        if let Some(&span_idx) = span_idx_by_uid.get(&target_uid) {
            for interval in intervals {
                intervals_by_worker
                    .entry(interval.worker_id)
                    .or_default()
                    .push(SpanInterval {
                        start_wall: to_wall(interval.start),
                        end_wall: to_wall(interval.end),
                        span_idx,
                    });
            }
        }
    }

    for intervals in intervals_by_worker.values_mut() {
        intervals.sort_unstable_by_key(|iv| (iv.start_wall, iv.span_idx));
    }

    // ── Sweep-line sample attribution ────────────────────────────────────────
    //
    // Partition by worker before sweeping. Scanning one global active set made
    // every sample inspect concurrently active intervals from every runtime
    // worker even though those intervals could never match.
    let mut samples_by_worker: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
    for (sample_idx, sample) in samples.iter().enumerate() {
        if let Some(worker_id) = sample.worker_id {
            samples_by_worker
                .entry(worker_id)
                .or_default()
                .push(sample_idx);
        }
    }

    for (worker_id, mut sample_order) in samples_by_worker {
        let Some(all_intervals) = intervals_by_worker.get(&worker_id) else {
            continue;
        };
        sample_order.sort_unstable_by_key(|&i| samples[i].timestamp_ns);

        let mut active: Vec<usize> = Vec::new();
        let mut interval_cursor = 0;
        let mut seen_span_indices: FxHashSet<usize> = FxHashSet::default();
        let mut sample_span_indices = Vec::new();

        for sample_idx in sample_order {
            let ts = WallNs(samples[sample_idx].timestamp_ns);

            while interval_cursor < all_intervals.len()
                && all_intervals[interval_cursor].start_wall <= ts
            {
                active.push(interval_cursor);
                interval_cursor += 1;
            }

            active.retain(|&iv_idx| all_intervals[iv_idx].end_wall > ts);

            seen_span_indices.clear();
            sample_span_indices.clear();
            for &iv_idx in &active {
                let iv = &all_intervals[iv_idx];
                debug_assert!(iv.start_wall <= ts && iv.end_wall > ts);
                if seen_span_indices.insert(iv.span_idx) {
                    sample_span_indices.push(iv.span_idx);
                }
            }

            for &span_idx in &sample_span_indices {
                if samples[sample_idx].source == SOURCE_CPU_PROFILE {
                    resolved_spans[span_idx].cpu_sample_count += 1;
                } else {
                    resolved_spans[span_idx].sched_sample_count += 1;
                }
                samples[sample_idx]
                    .enclosing_spans
                    .push(u32::try_from(span_idx).expect("resolved span count exceeds u32"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(worker_id: Option<u32>) -> ResolvedSample {
        ResolvedSample {
            timestamp_ns: 15,
            stack_id: [0; 16],
            worker_id,
            source: SOURCE_CPU_PROFILE,
            source_key: "test".to_string(),
            host: "host".to_string(),
            service: "service".to_string(),
            date: "2026-09-24".to_string(),
            poll_duration_ns: None,
            spawn_location: None,
            enclosing_spans: Vec::new(),
        }
    }

    fn span(boot_id: &str, instance_id: u64) -> ResolvedSpan {
        ResolvedSpan {
            span_uid: compute_span_uid(boot_id, instance_id),
            span_type_uid: [instance_id as u8; 16],
            kind: "tracing".to_string(),
            name: "test".to_string(),
            target: "test".to_string(),
            callsite_file: None,
            callsite_line: None,
            start_ns: 10,
            end_ns: 20,
            elapsed_ns: 10,
            active_ns: Some(10),
            observed_active_wall_ns: 10,
            detail_coverage_ns: 10,
            details_complete: true,
            concurrent: false,
            parent_span_uid: None,
            attributes: Vec::new(),
            on_cpu_ns_est: Some(10),
            blocked_ns_est: None,
            async_wait_ns: Some(0),
            scheduler_delay_ns: None,
            unknown_ns: 0,
            cpu_sample_count: 0,
            sched_sample_count: 0,
            attribution_version: 1,
            attribution_flags: 0,
            unbalanced_exits: 0,
            unbalanced_enters: 0,
            identity_quality: "metadata",
            source_key: "test".to_string(),
            host: "host".to_string(),
            service: "service".to_string(),
            date: "2026-09-24".to_string(),
        }
    }

    #[test]
    fn sample_matches_only_spans_on_its_worker() {
        let boot_id = "boot";
        let mut samples = vec![sample(Some(1))];
        let mut spans = vec![span(boot_id, 1), span(boot_id, 2)];
        let intervals = FxHashMap::from_iter([
            (
                1,
                vec![AttributionInterval {
                    start: super::super::clock::MonoNs(10),
                    end: super::super::clock::MonoNs(20),
                    worker_id: 1,
                }],
            ),
            (
                2,
                vec![AttributionInterval {
                    start: super::super::clock::MonoNs(10),
                    end: super::super::clock::MonoNs(20),
                    worker_id: 2,
                }],
            ),
        ]);

        attribute_samples_to_spans(&mut samples, &mut spans, &intervals, boot_id, None);

        assert_eq!(samples[0].enclosing_spans, vec![0]);
        assert_eq!(spans[0].cpu_sample_count, 1);
        assert_eq!(spans[1].cpu_sample_count, 0);
    }

    #[test]
    fn off_runtime_sample_does_not_match_runtime_spans_by_time_alone() {
        let boot_id = "boot";
        let mut samples = vec![sample(None)];
        let mut spans = vec![span(boot_id, 1)];
        let intervals = FxHashMap::from_iter([(
            1,
            vec![AttributionInterval {
                start: super::super::clock::MonoNs(10),
                end: super::super::clock::MonoNs(20),
                worker_id: 1,
            }],
        )]);

        attribute_samples_to_spans(&mut samples, &mut spans, &intervals, boot_id, None);

        assert!(samples[0].enclosing_spans.is_empty());
    }
}

//! Sample-to-span membership attribution (stage 3).
//!
//! After span resolution (modern + legacy), this module builds a flat sorted
//! interval index and performs a sweep-line attribution of CPU/sched samples
//! to their enclosing spans.
//!
//! CRITICAL: We attach samples ONLY to balanced, locally observed entered
//! intervals — never to lifecycle envelopes. An async span that is exited
//! (waiting) must NOT claim samples that fire during its idle gap.

use rustc_hash::FxHashMap;

use super::clock::{ClockOffset, WallNs};
use super::spans::interval_pairing::MonoInterval;
use super::spans::span_builder::compute_span_uid;
use super::{EnclosingSpanSummary, ResolvedSample, ResolvedSpan, SOURCE_CPU_PROFILE};

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
/// `legacy_intervals` maps synthetic span instance_id → list of monotonic
/// (enter, exit) intervals. `boot_id` is needed to compute span_uid for each
/// instance_id.
pub(crate) fn attribute_samples_to_spans(
    samples: &mut [ResolvedSample],
    resolved_spans: &mut [ResolvedSpan],
    legacy_intervals: &FxHashMap<u64, Vec<MonoInterval>>,
    boot_id: &str,
    clock_offset: Option<ClockOffset>,
) {
    let mut all_intervals: Vec<SpanInterval> = Vec::new();

    let to_wall = |mono: super::clock::MonoNs| mono.to_wall_or_raw(clock_offset);

    // Legacy intervals are computed with synthetic instance_ids.
    for (synthetic_instance_id, intervals) in legacy_intervals {
        let target_uid = compute_span_uid(boot_id, *synthetic_instance_id);
        if let Some(span_idx) = resolved_spans.iter().position(|s| s.span_uid == target_uid) {
            for &(enter_ts, exit_ts) in intervals {
                all_intervals.push(SpanInterval {
                    start_wall: to_wall(enter_ts),
                    end_wall: to_wall(exit_ts),
                    span_idx,
                });
            }
        }
    }

    // Sort intervals by start time for sweep-line attribution.
    all_intervals.sort_unstable_by_key(|iv| iv.start_wall);

    // ── Sweep-line sample attribution ────────────────────────────────────────
    //
    // Sort samples by timestamp. For each sample, advance a pointer through
    // intervals (sorted by start) to find all that start <= ts. Maintain an
    // active set of intervals that haven't ended yet.
    let mut sample_order: Vec<usize> = (0..samples.len()).collect();
    sample_order.sort_unstable_by_key(|&i| samples[i].timestamp_ns);

    let mut active: Vec<usize> = Vec::new(); // indices into all_intervals
    let mut interval_cursor: usize = 0;

    for &sample_idx in &sample_order {
        let ts = WallNs(samples[sample_idx].timestamp_ns);

        // Advance interval cursor: add all intervals starting <= ts.
        while interval_cursor < all_intervals.len()
            && all_intervals[interval_cursor].start_wall <= ts
        {
            active.push(interval_cursor);
            interval_cursor += 1;
        }

        // Prune expired intervals from active set (end_wall <= ts).
        active.retain(|&iv_idx| all_intervals[iv_idx].end_wall > ts);

        // Attribute the sample to all active intervals (dedup by span_idx).
        let mut seen_span_indices: Vec<usize> = Vec::new();
        for &iv_idx in &active {
            let iv = &all_intervals[iv_idx];
            debug_assert!(iv.start_wall <= ts && iv.end_wall > ts);

            // Dedup: skip if we already attributed this span index.
            if seen_span_indices.contains(&iv.span_idx) {
                continue;
            }
            seen_span_indices.push(iv.span_idx);

            // Sample is within this entered interval.
            if samples[sample_idx].source == SOURCE_CPU_PROFILE {
                resolved_spans[iv.span_idx].cpu_sample_count += 1;
            } else {
                resolved_spans[iv.span_idx].sched_sample_count += 1;
            }
            let span = &resolved_spans[iv.span_idx];
            samples[sample_idx]
                .enclosing_spans
                .push(EnclosingSpanSummary {
                    span_uid: span.span_uid,
                    span_type_uid: span.span_type_uid,
                    elapsed_ns: span.elapsed_ns,
                    details_complete: span.details_complete,
                });
        }
    }
}

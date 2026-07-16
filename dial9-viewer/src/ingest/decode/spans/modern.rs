//! Modern span adapter: resolves span close summaries (stage 1 producer format)
//! into [`SpanCandidate`]s via the unified interval pairer.
//!
//! Modern format spans carry `span_instance_id` and `tid` on enter/exit events,
//! and a self-contained `SpanCloseSummary` at lifecycle close. Interval pairing
//! groups by `(instance_id, tid)` — a composite key — so re-entrant spans on
//! different threads are paired independently.

use rustc_hash::FxHashMap;

use super::clock::{ClockOffset, MonoNs};
use super::interval_pairing::{self, MonoInterval, PairingEvent};
use super::span_builder::{SpanCandidate, compute_span_uid};
use super::{ResolvedSpan, SpanCloseSummary, SpanEnterEvent, SpanExitEvent};

/// Result of modern span resolution: resolved spans and the per-instance interval
/// map (monotonic timestamps) for reuse in sample attribution.
pub(crate) struct ModernResolution {
    pub(crate) spans: Vec<ResolvedSpan>,
    /// Per span_instance_id: list of monotonic-clock (enter_ts, exit_ts) intervals.
    pub(crate) instance_intervals: FxHashMap<u64, Vec<MonoInterval>>,
}

/// Resolve modern span close summaries into `ResolvedSpan` rows via
/// [`SpanCandidate::finalize`].
///
/// Computes locally-observed active wall time from enter/exit events present in
/// this source file. Spans that started before this file still get a row (with
/// partial coverage). The span's elapsed time is always complete (close event
/// carries start_timestamp_ns).
///
/// Enter/exit events are paired LIFO per (instance_id, tid): each exit matches
/// the most recent unmatched enter on the same instance and thread. This handles
/// re-entrant spans correctly.
///
/// Also returns the per-instance interval map so the caller can reuse it for
/// sample attribution without a second reconstruction pass.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_modern_spans(
    span_closes: Vec<SpanCloseSummary>,
    span_enters: &[SpanEnterEvent],
    span_exits: &[SpanExitEvent],
    source_key: &str,
    boot_id: &str,
    clock_offset: Option<ClockOffset>,
    first_clock_sync_mono: Option<MonoNs>,
    host: &str,
    service: &str,
    date: &str,
    identity_quality: &'static str,
) -> ModernResolution {
    // ── Interval pairing via the unified LIFO pairer ─────────────────────
    //
    // Build PairingEvents from span enters/exits. Modern format groups by
    // (instance_id, tid) — the composite key ensures re-entrant spans on
    // different threads are paired independently.
    let mut pairing_events: Vec<PairingEvent<(u64, u64)>> =
        Vec::with_capacity(span_enters.len() + span_exits.len());
    for enter in span_enters {
        pairing_events.push(PairingEvent {
            timestamp: MonoNs(enter.timestamp_ns),
            decode_sequence: enter.decode_sequence,
            is_enter: true,
            group_key: (enter.span_instance_id, enter.tid),
        });
    }
    for exit in span_exits {
        pairing_events.push(PairingEvent {
            timestamp: MonoNs(exit.timestamp_ns),
            decode_sequence: exit.decode_sequence,
            is_enter: false,
            group_key: (exit.span_instance_id, exit.tid),
        });
    }

    let pairing_results = interval_pairing::pair_intervals(&mut pairing_events);

    // Fold exact per-(instance, tid) lanes into one per-instance interval map.
    let mut local_intervals: FxHashMap<u64, Vec<MonoInterval>> = FxHashMap::default();
    let mut unmatched_exits: FxHashMap<u64, u32> = FxHashMap::default();
    let mut unmatched_enters_by_instance: FxHashMap<u64, u32> = FxHashMap::default();
    for ((instance_id, _tid), result) in pairing_results {
        local_intervals
            .entry(instance_id)
            .or_default()
            .extend(result.intervals);
        *unmatched_exits.entry(instance_id).or_insert(0) += result.unmatched_exits;
        *unmatched_enters_by_instance.entry(instance_id).or_insert(0) += result.unmatched_enters;
    }

    let spans = span_closes
        .into_iter()
        .map(|sc| {
            let intervals = local_intervals
                .get(&sc.span_instance_id)
                .cloned()
                .unwrap_or_default();

            // Check for unbalanced enters for THIS INSTANCE ONLY.
            let instance_unbalanced: u32 = unmatched_enters_by_instance
                .get(&sc.span_instance_id)
                .copied()
                .unwrap_or(0);
            let instance_unmatched_exits = unmatched_exits
                .get(&sc.span_instance_id)
                .copied()
                .unwrap_or(0);

            // Parent span uid (if parent_span_instance_id is known)
            let parent_span_uid = sc
                .parent_span_instance_id
                .map(|parent_id| compute_span_uid(boot_id, parent_id));

            let candidate = SpanCandidate {
                boot_id: boot_id.to_string(),
                instance_id: sc.span_instance_id,
                kind: "tracing",
                name: sc.span_name,
                type_name: None,
                target: sc.target,
                callsite_file: sc.file,
                callsite_line: sc.line,
                start_mono: MonoNs(sc.start_timestamp_ns),
                end_mono: MonoNs(sc.timestamp_ns),
                intervals,
                producer_active_ns: sc.active_ns,
                unmatched_exits: instance_unmatched_exits,
                unmatched_enters: sc.unbalanced_enters.saturating_add(instance_unbalanced),
                loss_observable: sc.loss_observable > 0,
                saturated: sc.saturated > 0,
                concurrent: sc.concurrent > 0,
                first_clock_sync_mono,
                identity_quality,
                on_cpu_ns_est: None,
                blocked_ns_est: None,
                async_wait_ns: None,
                scheduler_delay_ns: None,
                attribution_flags: 0b1111,
                parent_span_uid,
                attributes: sc.attributes,
                source_key: source_key.to_string(),
                host: host.to_string(),
                service: service.to_string(),
                date: date.to_string(),
            };
            candidate.finalize(clock_offset)
        })
        .collect();

    ModernResolution {
        spans,
        instance_intervals: local_intervals,
    }
}

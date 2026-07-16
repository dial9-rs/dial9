//! Legacy span adapter: reconstructs spans from old-producer format events
//! into [`SpanCandidate`]s via the unified interval pairer.
//!
//! Old producers emit:
//! - `SpanEnter:{target}::{name}:{file}:{line}` with fields: worker_id, span_id, span_name, ...
//! - `SpanExit:{target}::{name}:{file}:{line}` with fields: worker_id, span_id, span_name, ...
//! - `SpanCloseEvent` with only: span_id
//!
//! Reconstruction strategy:
//! - Pair enters/exits by `span_id` alone (NOT worker_id): a task can migrate
//!   workers between its enter and exit, so the worker is not a stable lane.
//! - For each span_id that has a close event, synthesize a deterministic
//!   instance_id from span_id + first-enter timestamp.
//! - Parse target/name/file/line from the SpanEnter schema name.
//! - Lifecycle start = first observed enter (conservative).
//! - details_complete = false, identity_quality = "legacy", loss_observable = false.
//! - Handle recycled IDs conservatively: all enters/exits for the same span_id
//!   are merged into one span instance. This is the intended compatibility
//!   policy — the old producer reuses span_ids within a process, but within a
//!   single trace segment (typically 60s), the same span_id almost always
//!   represents the same logical span. A close event marks the lifecycle end.
//! - When the span's owning Tokio task can be resolved (the poll on the enter
//!   worker covering the enter timestamp), split the entered wall time into
//!   estimated on-CPU (poll overlap) vs async wait (in-task gaps between polls).

use rustc_hash::FxHashMap;

use super::clock::{ClockOffset, MonoNs};
use super::interval_pairing::{self, MonoInterval, PairingEvent};
use super::polls::PollRecord;
use super::span_builder::{SpanCandidate, compute_span_uid};
use super::{
    LegacySpanCloseEvent, LegacySpanEnterEvent, LegacySpanExitEvent, LegacySpanSchemaInfo,
    ResolvedSpan, parse_legacy_span_schema_name,
};

/// Result of legacy span resolution.
pub(crate) struct LegacyResolution {
    pub(crate) spans: Vec<ResolvedSpan>,
    /// Per synthetic_instance_id: list of monotonic-clock (enter_ts, exit_ts) intervals.
    pub(crate) instance_intervals: FxHashMap<u64, Vec<MonoInterval>>,
}

/// Resolve legacy (old-producer) span events into `ResolvedSpan` rows via
/// [`SpanCandidate::finalize`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_legacy_spans(
    legacy_enters: &[(String, LegacySpanEnterEvent)],
    legacy_exits: &[(String, LegacySpanExitEvent)],
    legacy_closes: &[LegacySpanCloseEvent],
    polls: &[PollRecord],
    source_key: &str,
    boot_id: &str,
    clock_offset: Option<ClockOffset>,
    host: &str,
    service: &str,
    date: &str,
) -> LegacyResolution {
    // Collect metadata from first enter event per span_id.
    struct SpanContext {
        schema_info: LegacySpanSchemaInfo,
        /// Schema-level type name, distinct from the dynamic runtime span_name.
        type_name: Option<String>,
        span_name: String,
        first_enter_ts: u64,
        /// Worker the first enter ran on. Used only to resolve the owning Tokio
        /// task (the poll on this worker covering `first_enter_ts`) — NOT for
        /// enter/exit pairing, which is span_id-only.
        first_enter_worker: u64,
        parent_span_id: Option<u64>,
    }
    let mut span_contexts: FxHashMap<u64, SpanContext> = FxHashMap::default();

    for (schema_name, enter) in legacy_enters {
        span_contexts.entry(enter.span_id).or_insert_with(|| {
            let parsed_schema = parse_legacy_span_schema_name(schema_name);
            let type_name = parsed_schema.as_ref().map(|info| info.name.clone());
            let schema_info = parsed_schema.unwrap_or(LegacySpanSchemaInfo {
                target: String::new(),
                name: enter.span_name.clone().unwrap_or_default(),
                file: None,
                line: None,
            });
            SpanContext {
                span_name: enter
                    .span_name
                    .clone()
                    .unwrap_or_else(|| schema_info.name.clone()),
                type_name,
                schema_info,
                first_enter_ts: enter.timestamp_ns,
                first_enter_worker: enter.worker_id,
                parent_span_id: enter.parent_span_id.filter(|&id| id > 0),
            }
        });
    }

    // Build a per-span_id enter/exit timeline for LIFO pairing using the
    // unified interval pairer.
    //
    // The pairing key is `span_id` ALONE — deliberately NOT `(span_id,
    // worker_id)`. An async span can enter on one worker and exit on another
    // when its task migrates between workers across an `.await`. Keying on the
    // worker would push the enter onto one worker's stack and look for the exit
    // on another worker's stack, silently dropping the interval. On a measured
    // beta trace, roughly 44% of fully captured spans migrated workers.
    //
    // Decode sequences come directly from the shared wire decoder, so ties
    // preserve the actual interleaving between legacy enters and exits.
    let mut pairing_events: Vec<PairingEvent<u64>> =
        Vec::with_capacity(legacy_enters.len() + legacy_exits.len());

    for (_schema_name, enter) in legacy_enters {
        pairing_events.push(PairingEvent {
            timestamp: MonoNs(enter.timestamp_ns),
            decode_sequence: enter.decode_sequence,
            is_enter: true,
            group_key: enter.span_id,
        });
    }
    for (_schema_name, exit) in legacy_exits {
        pairing_events.push(PairingEvent {
            timestamp: MonoNs(exit.timestamp_ns),
            decode_sequence: exit.decode_sequence,
            is_enter: false,
            group_key: exit.span_id,
        });
    }

    let pairing_results = interval_pairing::pair_intervals(&mut pairing_events);

    // Rebuild the per-span_id interval map and unmatched accounting from results.
    let mut local_intervals: FxHashMap<u64, Vec<MonoInterval>> = FxHashMap::default();
    let mut unmatched_exits_map: FxHashMap<u64, u32> = FxHashMap::default();
    let mut unmatched_enters_map: FxHashMap<u64, u32> = FxHashMap::default();
    for (span_id, result) in &pairing_results {
        if !result.intervals.is_empty() {
            local_intervals
                .entry(*span_id)
                .or_default()
                .extend_from_slice(&result.intervals);
        }
        if result.unmatched_exits > 0 {
            *unmatched_exits_map.entry(*span_id).or_insert(0) += result.unmatched_exits;
        }
        if result.unmatched_enters > 0 {
            *unmatched_enters_map.entry(*span_id).or_insert(0) += result.unmatched_enters;
        }
    }

    // Build a close timestamp map: span_id → close timestamp.
    let mut close_timestamps: FxHashMap<u64, u64> = FxHashMap::default();
    for close in legacy_closes {
        // Use the latest close timestamp for each span_id (conservative).
        close_timestamps
            .entry(close.span_id)
            .and_modify(|ts| *ts = (*ts).max(close.timestamp_ns))
            .or_insert(close.timestamp_ns);
    }

    // ── Poll indices for task-based CPU/wait attribution ─────────────────────
    let mut polls_by_worker: FxHashMap<u64, Vec<(MonoNs, MonoNs, u64)>> = FxHashMap::default();
    let mut polls_by_task: FxHashMap<u64, Vec<MonoInterval>> = FxHashMap::default();
    for poll in polls {
        // task_id 0 is the block-in-place / no-task stub — not a real task.
        if poll.task_id != 0 {
            polls_by_worker.entry(poll.worker_id).or_default().push((
                poll.start,
                poll.end,
                poll.task_id,
            ));
            polls_by_task
                .entry(poll.task_id)
                .or_default()
                .push((poll.start, poll.end));
        }
    }

    // Generate a synthetic instance_id deterministically from span_id + first_enter_ts.
    let synthetic_instance_id = |span_id: u64, first_enter_ts: u64| -> u64 {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"legacy_instance");
        hasher.update(&span_id.to_le_bytes());
        hasher.update(&first_enter_ts.to_le_bytes());
        let hash = hasher.finalize();
        u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap())
    };

    // Build ResolvedSpan rows for each span_id that has EITHER a close event
    // or local intervals (we produce a row even without a close when we have
    // balanced enter/exit pairs).
    let mut resolved_spans: Vec<ResolvedSpan> = Vec::new();
    let mut resolved_intervals: FxHashMap<u64, Vec<MonoInterval>> = FxHashMap::default();

    // Collect all span_ids that have either a close or intervals.
    let mut all_span_ids: Vec<u64> = close_timestamps.keys().copied().collect();
    for &span_id in local_intervals.keys() {
        if !all_span_ids.contains(&span_id) {
            all_span_ids.push(span_id);
        }
    }

    for &span_id in &all_span_ids {
        let ctx = span_contexts.get(&span_id);
        let intervals = local_intervals.get(&span_id);
        let close_ts = close_timestamps.get(&span_id).copied();

        // Skip spans with no context at all (no enters, no close).
        if ctx.is_none() && close_ts.is_none() && intervals.is_none() {
            continue;
        }

        let first_enter_ts = ctx
            .map(|c| c.first_enter_ts)
            .or_else(|| intervals.and_then(|ivs| ivs.iter().map(|&(start, _)| start.raw()).min()));

        // Lifecycle bounds
        let start_ts = first_enter_ts.unwrap_or_else(|| close_ts.unwrap_or(0));
        let end_ts = close_ts.unwrap_or_else(|| {
            intervals
                .and_then(|ivs| ivs.iter().map(|&(_, end)| end.raw()).max())
                .unwrap_or(start_ts)
        });

        // Metadata from schema name or context.
        let (target, name, file, line) = if let Some(ctx) = ctx {
            (
                ctx.schema_info.target.clone(),
                ctx.span_name.clone(),
                ctx.schema_info.file.clone(),
                ctx.schema_info.line,
            )
        } else {
            (String::new(), String::new(), None, None)
        };

        // Task-based CPU/wait attribution.
        let mut on_cpu_ns_est: Option<u64> = None;
        let mut async_wait_ns_est: Option<u64> = None;
        let mut flags: u32 = 0b1111;
        if let (Some(ctx), Some(ivs)) = (ctx, intervals)
            && let Some(worker_polls) = polls_by_worker.get(&ctx.first_enter_worker)
            && let Some(task_id) = resolve_span_task(worker_polls, MonoNs(ctx.first_enter_ts))
            && let Some(task_polls) = polls_by_task.get(&task_id)
        {
            let (on_cpu, async_wait) = attribute_legacy_span_from_polls(ivs, task_polls);
            on_cpu_ns_est = Some(on_cpu);
            async_wait_ns_est = Some(async_wait);
            // Clear bit 2 (worker/tid ambiguous): we resolved the owning task.
            flags &= !0b0100;
        }

        let instance_id = synthetic_instance_id(span_id, start_ts);

        // Parent uid (if parent_span_id known from enters)
        let parent_span_uid = ctx.and_then(|c| c.parent_span_id).map(|parent_id| {
            let parent_start = span_contexts
                .get(&parent_id)
                .map(|p| p.first_enter_ts)
                .unwrap_or(0);
            let parent_instance = synthetic_instance_id(parent_id, parent_start);
            compute_span_uid(boot_id, parent_instance)
        });

        // Propagate unmatched counts from the pairer.
        let unmatched_exits = unmatched_exits_map.get(&span_id).copied().unwrap_or(0);
        let unmatched_enters = unmatched_enters_map.get(&span_id).copied().unwrap_or(0);

        let type_name = ctx.and_then(|context| context.type_name.clone());
        let candidate = SpanCandidate {
            boot_id: boot_id.to_string(),
            instance_id,
            kind: "tracing",
            name,
            type_name,
            target,
            callsite_file: file,
            callsite_line: line,
            start_mono: MonoNs(start_ts),
            end_mono: MonoNs(end_ts),
            intervals: intervals.cloned().unwrap_or_default(),
            producer_active_ns: 0, // Old producer doesn't report active_ns
            unmatched_exits,
            unmatched_enters,
            loss_observable: false,
            saturated: false,
            concurrent: false,
            first_clock_sync_mono: None, // Cannot verify boundary for legacy
            identity_quality: "legacy",
            on_cpu_ns_est,
            blocked_ns_est: None,
            async_wait_ns: async_wait_ns_est,
            scheduler_delay_ns: None,
            attribution_flags: flags,
            parent_span_uid,
            attributes: Vec::new(), // Old format doesn't carry close-time attributes
            source_key: source_key.to_string(),
            host: host.to_string(),
            service: service.to_string(),
            date: date.to_string(),
        };

        resolved_spans.push(candidate.finalize(clock_offset));

        // Store intervals keyed by synthetic_instance_id for sample attribution.
        if let Some(ivs) = intervals {
            resolved_intervals.insert(instance_id, ivs.clone());
        }
    }

    LegacyResolution {
        spans: resolved_spans,
        instance_intervals: resolved_intervals,
    }
}

/// Resolve the Tokio task that owns a span, from the poll on `worker` whose
/// window covers `enter_ts`. `worker_polls` is that worker's polls as
/// `(start, end, task_id)`, sorted by `start` and non-overlapping (a worker
/// runs one poll at a time). Binary search — a worker can hold hundreds of
/// thousands of polls on a large trace. Returns `None` when no poll covers the
/// enter (span entered outside any poll, e.g. on a non-runtime thread).
pub(crate) fn resolve_span_task(
    worker_polls: &[(MonoNs, MonoNs, u64)],
    enter_ts: MonoNs,
) -> Option<u64> {
    let mut lo = 0usize;
    let mut hi = worker_polls.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        let (start, end, task_id) = worker_polls[mid];
        if end < enter_ts {
            lo = mid + 1;
        } else if start > enter_ts {
            hi = mid;
        } else {
            return Some(task_id);
        }
    }
    None
}

/// Split a span's entered wall time into estimated on-CPU vs async-wait using
/// its owning task's poll timeline.
///
/// The span's entered intervals (`entered`, monotonic ns) mark when the guard
/// was held. On-CPU time is where those intervals overlap one of the task's
/// polls (`task_polls`, `(start, end)` sorted by start, non-overlapping); the
/// remaining entered wall time is the task alive-but-not-polled — async wait.
///
/// Returns `(on_cpu_ns, async_wait_ns)` over the UNION of entered intervals, so
/// the two sum to `observed_active_wall_ns` and never double-count concurrent or
/// overlapping enters.
pub(crate) fn attribute_legacy_span_from_polls(
    entered: &[MonoInterval],
    task_polls: &[MonoInterval],
) -> (u64, u64) {
    // Union the entered intervals first so overlapping/re-entrant enters are
    // counted once (matches observed_active_wall_ns).
    let entered_union = interval_pairing::merge_intervals(entered);
    let active_wall = interval_pairing::union_interval_duration(&entered_union).raw();
    if active_wall == 0 || task_polls.is_empty() {
        return (0, active_wall);
    }

    // On-CPU = total overlap between the entered union and the task's polls.
    // Both lists are sorted by start and internally non-overlapping, so sweep
    // them together in linear time.
    let mut on_cpu: u64 = 0;
    let mut pi = 0usize;
    for &(entered_start, entered_end) in &entered_union {
        // Advance past polls that end before this entered interval starts.
        while pi < task_polls.len() && task_polls[pi].1 <= entered_start {
            pi += 1;
        }
        let mut j = pi;
        while j < task_polls.len() && task_polls[j].0 < entered_end {
            let (poll_start, poll_end) = task_polls[j];
            let overlap_start = poll_start.max(entered_start);
            let overlap_end = poll_end.min(entered_end);
            if overlap_end > overlap_start {
                on_cpu = on_cpu.saturating_add(overlap_end.saturating_sub(overlap_start).raw());
            }
            j += 1;
        }
    }
    let on_cpu = on_cpu.min(active_wall);
    (on_cpu, active_wall - on_cpu)
}

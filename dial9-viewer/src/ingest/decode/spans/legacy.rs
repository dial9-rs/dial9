//! Legacy span adapter: reconstructs spans from old-producer format events
//! into [`SpanCandidate`]s via the unified interval pairer.
//!
//! Old producers emit:
//! - `SpanEnter:{target}::{name}:{file}:{line}` with fields: worker_id, span_id, span_name, ...
//! - `SpanExit:{target}::{name}:{file}:{line}` with fields: worker_id, span_id, span_name, ...
//! - `SpanCloseEvent` with only: span_id
//!
//! Reconstruction strategy:
//! - Segment a span_id's lifecycle at close boundaries: tracing recycles a
//!   `span_id` only after its span closes, so a `SpanCloseEvent` marks the id
//!   "done" and the next enter with that recycled id begins a NEW instance.
//!   Each close-delimited segment becomes one row. Without this, every reuse of
//!   a span_id collapses into one span whose lifecycle runs from the first enter
//!   to the last close (e.g. a 1ms request span decoded as a 14.5s span).
//! - Pair enters/exits by `(span_id, segment)` — NOT worker_id: a task can
//!   migrate workers between its enter and exit, so the worker is not a stable
//!   lane.
//! - Synthesize a deterministic instance_id from span_id + first-enter
//!   timestamp; distinct segments have distinct first enters, so distinct ids.
//! - Parse target/name/file/line from the SpanEnter schema name.
//! - Lifecycle start = first observed enter (conservative).
//! - details_complete = false, identity_quality = "legacy", loss_observable = false.
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
    // ── Close-delimited segmentation ─────────────────────────────────────────
    //
    // Tracing recycles a `span_id` after its span closes, so a `SpanCloseEvent`
    // marks that `span_id` "done": the next enter carrying the same (recycled)
    // id begins a *new* logical span instance. Without this, every reuse of a
    // span_id across the segment collapses into one row whose lifecycle spans
    // from the first enter to the last close (e.g. a 1ms request span decoded as
    // a 14.5s span covering the whole window).
    //
    // We therefore assign each enter/exit/close a per-span_id *segment ordinal*.
    // Walking a span_id's lifecycle events in wire order, a close is assigned to
    // the current segment (it terminates it) and then bumps the ordinal for
    // everything that follows. All downstream grouping keys become
    // `(span_id, segment)` rather than bare `span_id`.
    #[derive(Clone, Copy)]
    enum LifecycleKind {
        Enter,
        Exit,
        Close,
    }
    let mut timelines: FxHashMap<u64, Vec<(u64, u64, LifecycleKind)>> = FxHashMap::default();
    for (_schema_name, enter) in legacy_enters {
        timelines.entry(enter.span_id).or_default().push((
            enter.timestamp_ns,
            enter.decode_sequence,
            LifecycleKind::Enter,
        ));
    }
    for (_schema_name, exit) in legacy_exits {
        timelines.entry(exit.span_id).or_default().push((
            exit.timestamp_ns,
            exit.decode_sequence,
            LifecycleKind::Exit,
        ));
    }
    for close in legacy_closes {
        timelines.entry(close.span_id).or_default().push((
            close.timestamp_ns,
            close.decode_sequence,
            LifecycleKind::Close,
        ));
    }

    // decode_sequence (globally unique per enter/exit) → segment ordinal.
    let mut enter_exit_segment: FxHashMap<u64, u32> = FxHashMap::default();
    // (span_id, segment) → close timestamp terminating that segment.
    let mut close_timestamps: FxHashMap<(u64, u32), u64> = FxHashMap::default();
    for (span_id, mut events) in timelines {
        // Order by (timestamp, decode_sequence): the same tie-break the pairer
        // uses, so a close never splits an enter/exit that shares its timestamp
        // but was encoded after it.
        events.sort_unstable_by_key(|&(ts, seq, _)| (ts, seq));
        let mut segment: u32 = 0;
        for (ts, seq, kind) in events {
            match kind {
                LifecycleKind::Enter | LifecycleKind::Exit => {
                    enter_exit_segment.insert(seq, segment);
                }
                LifecycleKind::Close => {
                    // The close belongs to the current segment and ends it.
                    // Later closes with no intervening enter form empty segments
                    // (dropped below). Keep the latest close per segment.
                    close_timestamps
                        .entry((span_id, segment))
                        .and_modify(|existing| *existing = (*existing).max(ts))
                        .or_insert(ts);
                    segment = segment.saturating_add(1);
                }
            }
        }
    }

    // Segment ordinal for an enter/exit, looked up by its globally-unique wire
    // decode_sequence. Events without an assigned segment (should not happen —
    // every enter/exit is walked above) default to segment 0.
    let segment_of = |decode_sequence: u64| -> u32 {
        enter_exit_segment
            .get(&decode_sequence)
            .copied()
            .unwrap_or(0)
    };

    // Collect metadata from the first enter event per (span_id, segment).
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
        /// User-attached attributes (e.g. `request_id`, `status_code`) captured
        /// from the span's enter event. Exit-event attributes override these
        /// (mirroring the live viewer), applied in a later pass.
        attributes: Vec<(String, String)>,
    }
    let mut span_contexts: FxHashMap<(u64, u32), SpanContext> = FxHashMap::default();

    for (schema_name, enter) in legacy_enters {
        let key = (enter.span_id, segment_of(enter.decode_sequence));
        span_contexts.entry(key).or_insert_with(|| {
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
                attributes: enter.attributes.clone(),
            }
        });
    }

    // Exit-event attributes override enter attributes for the same span
    // instance, so close-time values (e.g. a `status_code` known only at exit)
    // win. This matches the viewer's `buildSpanData`, which replaces a span's
    // fields with its exit fields when present.
    for (_schema_name, exit) in legacy_exits {
        if !exit.attributes.is_empty() {
            let key = (exit.span_id, segment_of(exit.decode_sequence));
            if let Some(ctx) = span_contexts.get_mut(&key) {
                ctx.attributes = exit.attributes.clone();
            }
        }
    }

    // Build a per-(span_id, segment) enter/exit timeline for LIFO pairing using
    // the unified interval pairer.
    //
    // The pairing key omits `worker_id` — deliberately NOT `(span_id,
    // worker_id)`. An async span can enter on one worker and exit on another
    // when its task migrates between workers across an `.await`. Keying on the
    // worker would push the enter onto one worker's stack and look for the exit
    // on another worker's stack, silently dropping the interval. On a measured
    // beta trace, roughly 44% of fully captured spans migrated workers.
    //
    // Decode sequences come directly from the shared wire decoder, so ties
    // preserve the actual interleaving between legacy enters and exits.
    let mut pairing_events: Vec<PairingEvent<(u64, u32)>> =
        Vec::with_capacity(legacy_enters.len() + legacy_exits.len());

    for (_schema_name, enter) in legacy_enters {
        pairing_events.push(PairingEvent {
            timestamp: MonoNs(enter.timestamp_ns),
            decode_sequence: enter.decode_sequence,
            is_enter: true,
            group_key: (enter.span_id, segment_of(enter.decode_sequence)),
        });
    }
    for (_schema_name, exit) in legacy_exits {
        pairing_events.push(PairingEvent {
            timestamp: MonoNs(exit.timestamp_ns),
            decode_sequence: exit.decode_sequence,
            is_enter: false,
            group_key: (exit.span_id, segment_of(exit.decode_sequence)),
        });
    }

    let pairing_results = interval_pairing::pair_intervals(&mut pairing_events);

    // Rebuild the per-(span_id, segment) interval map and unmatched accounting.
    let mut local_intervals: FxHashMap<(u64, u32), Vec<MonoInterval>> = FxHashMap::default();
    let mut unmatched_exits_map: FxHashMap<(u64, u32), u32> = FxHashMap::default();
    let mut unmatched_enters_map: FxHashMap<(u64, u32), u32> = FxHashMap::default();
    for (key, result) in &pairing_results {
        if !result.intervals.is_empty() {
            local_intervals
                .entry(*key)
                .or_default()
                .extend_from_slice(&result.intervals);
        }
        if result.unmatched_exits > 0 {
            *unmatched_exits_map.entry(*key).or_insert(0) += result.unmatched_exits;
        }
        if result.unmatched_enters > 0 {
            *unmatched_enters_map.entry(*key).or_insert(0) += result.unmatched_enters;
        }
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

    // Resolve a parent (span_id, segment) instance from a bare parent span_id
    // and the child's start timestamp: pick the parent segment whose first
    // enter is the latest one at or before the child's start. This selects the
    // parent instance that was live when the child began, since a recycled
    // parent span_id may have several segments across the file.
    // Per parent span_id: its segments' first-enter timestamps, SORTED. A single
    // span_id may be recycled into many close-delimited segments (a hot parent
    // span reused thousands of times), so `resolve_parent_start` must binary
    // search rather than linear-scan — a linear scan per child is O(children ×
    // parent_segments), which is quadratic when one parent id dominates and cost
    // tens of seconds of span resolution on a real 900k-span-event file.
    let parent_first_enters: FxHashMap<u64, Vec<u64>> = {
        let mut map: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        for (&(span_id, _segment), ctx) in &span_contexts {
            map.entry(span_id).or_default().push(ctx.first_enter_ts);
        }
        for enters in map.values_mut() {
            enters.sort_unstable();
        }
        map
    };
    let resolve_parent_start = |parent_id: u64, child_start_ts: u64| -> u64 {
        parent_first_enters
            .get(&parent_id)
            .and_then(|enters| {
                // Largest first_enter_ts <= child_start_ts: the parent instance
                // live when the child began. `partition_point` gives the count of
                // elements <= child_start_ts; the one before it is the answer.
                let idx = enters.partition_point(|&ts| ts <= child_start_ts);
                idx.checked_sub(1).map(|i| enters[i])
            })
            .unwrap_or(0)
    };

    // Build ResolvedSpan rows for each (span_id, segment) that has EITHER a
    // close event or local intervals (we produce a row even without a close
    // when we have balanced enter/exit pairs).
    let mut resolved_spans: Vec<ResolvedSpan> = Vec::new();
    let mut resolved_intervals: FxHashMap<u64, Vec<MonoInterval>> = FxHashMap::default();

    // Collect all (span_id, segment) keys that have either a close or intervals.
    // Dedup via a set, not `Vec::contains` in a loop — the latter is
    // O(keys²) and dominated span resolution on span-heavy files (seconds on a
    // 325k-span-event segment).
    let mut all_keys: Vec<(u64, u32)> = close_timestamps.keys().copied().collect();
    let mut seen_keys: rustc_hash::FxHashSet<(u64, u32)> = all_keys.iter().copied().collect();
    for &key in local_intervals.keys() {
        if seen_keys.insert(key) {
            all_keys.push(key);
        }
    }

    for &key @ (span_id, _segment) in &all_keys {
        let ctx = span_contexts.get(&key);
        let intervals = local_intervals.get(&key);
        let close_ts = close_timestamps.get(&key).copied();

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

        // The synthetic instance_id keys on span_id + first_enter_ts, so
        // distinct segments (each with its own first enter) get distinct ids.
        let instance_id = synthetic_instance_id(span_id, start_ts);

        // Parent uid (if parent_span_id known from enters). The parent's
        // instance_id is derived from the parent segment live at the child's
        // start, matching how this span's own instance_id was computed.
        let parent_span_uid = ctx.and_then(|c| c.parent_span_id).map(|parent_id| {
            let parent_start = resolve_parent_start(parent_id, start_ts);
            let parent_instance = synthetic_instance_id(parent_id, parent_start);
            compute_span_uid(boot_id, parent_instance)
        });

        // Propagate unmatched counts from the pairer.
        let unmatched_exits = unmatched_exits_map.get(&key).copied().unwrap_or(0);
        let unmatched_enters = unmatched_enters_map.get(&key).copied().unwrap_or(0);

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
            attributes: ctx.map(|c| c.attributes.clone()).unwrap_or_default(),
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

//! Legacy span adapter: reconstructs spans from old-producer format events
//! into [`SpanCandidate`]s via the unified interval pairer.
//!
//! Tracing-layer producers emit:
//! - `SpanEnter:{target}::{name}:{file}:{line}` with fields:
//!   dial9.tokio.task_id (current) or worker_id (legacy), span_id, span_name, ...
//! - `SpanExit:{target}::{name}:{file}:{line}` with the corresponding fields
//! - `SpanCloseEvent` with only: span_id
//!
//! Reconstruction strategy:
//! - Segment a span_id's lifecycle at close boundaries: tracing recycles a
//!   `span_id` only after its span closes, so a `SpanCloseEvent` marks the id
//!   "done" and the next enter with that recycled id begins a NEW instance.
//!   Each close-delimited segment becomes one row. Without this, every reuse of
//!   a span_id collapses into one span whose lifecycle runs from the first enter
//!   to the last close (e.g. a 1ms request span decoded as a 14.5s span).
//! - Pair enters/exits by `(span_id, segment, task lane)`. A span can
//!   be entered concurrently by multiple tasks; pairing all lanes through one
//!   global LIFO stack cross-pairs their exits and fabricates long overlapping
//!   active intervals. Task identity survives worker migration. For older
//!   events, infer the task from the poll covering the event. If only one side
//!   has evidence, reconcile it to the sole task known for that span segment;
//!   otherwise keep missing evidence in a conservative unknown lane.
//! - Synthesize a deterministic instance_id from span_id + segment ordinal +
//!   first-enter timestamp.
//! - Parse target/name/file/line from the SpanEnter schema name.
//! - Lifecycle start = first observed enter (conservative).
//! - details_complete = false, identity_quality = "legacy".
//! - Prefer the producer-recorded Tokio task ID. For old traces, resolve the
//!   owning task from the poll on the enter worker covering the enter timestamp.
//!   Then split entered wall time into estimated on-CPU (poll overlap) vs async
//!   wait (in-task gaps between polls).

use rustc_hash::FxHashMap;

use super::super::attribution::AttributionInterval;
use super::clock::{ClockOffset, MonoNs};
use super::interval_pairing::{self, MonoInterval, PairingEvent};
use super::polls::{PollIndex, PollRecord};
use super::span_builder::{SpanCandidate, compute_span_uid};
use super::{
    LegacySpanCloseEvent, LegacySpanEnterEvent, LegacySpanExitEvent, LegacySpanSchemaInfo,
    ResolvedSpan, parse_legacy_span_schema_name,
};

/// Result of legacy span resolution.
pub(crate) struct LegacyResolution {
    pub(crate) spans: Vec<ResolvedSpan>,
    /// Per synthetic instance: poll-bounded execution intervals, including the
    /// worker lane required for safe CPU-sample attribution.
    pub(crate) instance_intervals: FxHashMap<u64, Vec<AttributionInterval>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PairingLane {
    Task(u64),
    Unknown,
}

fn pairing_lane(
    poll_index: &PollIndex<'_>,
    task_id: Option<u64>,
    worker_id: Option<u64>,
    timestamp: MonoNs,
    is_enter: bool,
) -> PairingLane {
    if let Some(task_id) = task_id.filter(|&id| id != 0) {
        return PairingLane::Task(task_id);
    }
    let Some(worker_id) = worker_id else {
        return PairingLane::Unknown;
    };
    let task_id = if is_enter {
        poll_index.task_at(worker_id, timestamp)
    } else {
        poll_index.task_at_exit(worker_id, timestamp)
    };
    task_id
        .map(PairingLane::Task)
        .unwrap_or(PairingLane::Unknown)
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
    // decode_sequence. Every enter/exit was inserted into `timelines` above.
    let segment_of = |decode_sequence: u64| -> u32 {
        enter_exit_segment
            .get(&decode_sequence)
            .copied()
            .expect("every legacy span enter/exit must have a segment")
    };

    // Collect metadata from the first enter event per (span_id, segment).
    struct SpanContext {
        schema_info: LegacySpanSchemaInfo,
        /// Schema-level type name, distinct from the dynamic runtime span_name.
        type_name: Option<String>,
        span_name: String,
        first_enter_ts: u64,
        first_enter_decode_sequence: u64,
        parent_span_id: Option<u64>,
        /// User-attached attributes (e.g. `request_id`, `status_code`) captured
        /// from the span's enter event. Exit-event attributes override these
        /// (mirroring the live viewer), applied in a later pass.
        attributes: Vec<(String, String)>,
        last_exit_order: Option<(u64, u64)>,
    }
    let mut span_contexts: FxHashMap<(u64, u32), SpanContext> = FxHashMap::default();

    let context_from_enter = |schema_name: &str, enter: &LegacySpanEnterEvent| {
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
            first_enter_decode_sequence: enter.decode_sequence,
            parent_span_id: enter.parent_span_id.filter(|&id| id > 0),
            attributes: enter.attributes.clone(),
            last_exit_order: None,
        }
    };

    for (schema_name, enter) in legacy_enters {
        let key = (enter.span_id, segment_of(enter.decode_sequence));
        match span_contexts.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(context_from_enter(schema_name, enter));
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let existing = entry.get();
                let existing_order = (
                    existing.first_enter_ts,
                    existing.first_enter_decode_sequence,
                );
                if (enter.timestamp_ns, enter.decode_sequence) < existing_order {
                    entry.insert(context_from_enter(schema_name, enter));
                }
            }
        }
    }

    // Exit-event attributes override enter attributes for the same span
    // instance, so close-time values (e.g. a `status_code` known only at exit)
    // win. This matches the viewer's `buildSpanData`, which replaces a span's
    // fields with its exit fields when present.
    for (_schema_name, exit) in legacy_exits {
        if !exit.attributes.is_empty() {
            let key = (exit.span_id, segment_of(exit.decode_sequence));
            let exit_order = (exit.timestamp_ns, exit.decode_sequence);
            if let Some(ctx) = span_contexts.get_mut(&key)
                && ctx
                    .last_exit_order
                    .is_none_or(|current| exit_order > current)
            {
                ctx.attributes = exit.attributes.clone();
                ctx.last_exit_order = Some(exit_order);
            }
        }
    }

    let poll_index = PollIndex::new(polls);
    // Build a per-(span_id, segment, execution lane) enter/exit timeline for
    // LIFO pairing using the unified interval pairer.
    //
    // A tracing span instance may be entered concurrently by many tasks. A
    // single stack per span instance pairs exits against whichever task entered
    // most recently, fabricating intervals that remain "active" across other
    // tasks' work. Prefer task id so a task may migrate workers safely. For
    // older producers without task ids, infer the task from the poll containing
    // each event, assigning an exit at PollStop to the poll that just ended.
    //
    // Decode sequences come directly from the shared wire decoder, so ties
    // preserve the actual interleaving between legacy enters and exits.
    let mut pairing_events: Vec<PairingEvent<(u64, u32, PairingLane)>> =
        Vec::with_capacity(legacy_enters.len() + legacy_exits.len());

    for (_schema_name, enter) in legacy_enters {
        pairing_events.push(PairingEvent {
            timestamp: MonoNs(enter.timestamp_ns),
            decode_sequence: enter.decode_sequence,
            is_enter: true,
            group_key: (
                enter.span_id,
                segment_of(enter.decode_sequence),
                pairing_lane(
                    &poll_index,
                    enter.task_id,
                    enter.worker_id,
                    MonoNs(enter.timestamp_ns),
                    true,
                ),
            ),
        });
    }
    for (_schema_name, exit) in legacy_exits {
        pairing_events.push(PairingEvent {
            timestamp: MonoNs(exit.timestamp_ns),
            decode_sequence: exit.decode_sequence,
            is_enter: false,
            group_key: (
                exit.span_id,
                segment_of(exit.decode_sequence),
                pairing_lane(
                    &poll_index,
                    exit.task_id,
                    exit.worker_id,
                    MonoNs(exit.timestamp_ns),
                    false,
                ),
            ),
        });
    }

    // A rotation can drop poll evidence for only one side of a migrated
    // enter/exit pair. If this span segment has exactly one evidenced task,
    // conservatively attach its unknown events to that task. With multiple
    // evidenced tasks the assignment is ambiguous, so leave unknown events in
    // their own lane instead of risking a cross-task pair.
    let mut sole_task_by_span: FxHashMap<(u64, u32), Option<u64>> = FxHashMap::default();
    for event in &pairing_events {
        let (span_id, segment, lane) = event.group_key;
        let PairingLane::Task(task_id) = lane else {
            continue;
        };
        sole_task_by_span
            .entry((span_id, segment))
            .and_modify(|sole_task| {
                if sole_task.is_some_and(|existing| existing != task_id) {
                    *sole_task = None;
                }
            })
            .or_insert(Some(task_id));
    }
    for event in &mut pairing_events {
        let (span_id, segment, lane) = &mut event.group_key;
        if *lane == PairingLane::Unknown
            && let Some(Some(task_id)) = sole_task_by_span.get(&(*span_id, *segment))
        {
            *lane = PairingLane::Task(*task_id);
        }
    }

    let pairing_results = interval_pairing::pair_intervals(&mut pairing_events);

    // Rebuild the per-(span_id, segment) interval map and unmatched accounting.
    let mut local_intervals: FxHashMap<(u64, u32), Vec<MonoInterval>> = FxHashMap::default();
    let mut interval_enter_sequences: FxHashMap<(u64, u32), Vec<u64>> = FxHashMap::default();
    let mut task_by_enter_sequence: FxHashMap<u64, u64> = FxHashMap::default();
    let mut unmatched_exits_map: FxHashMap<(u64, u32), u32> = FxHashMap::default();
    let mut unmatched_enters_map: FxHashMap<(u64, u32), u32> = FxHashMap::default();
    for (&(span_id, segment, lane), result) in &pairing_results {
        let key = (span_id, segment);
        if !result.intervals.is_empty() {
            local_intervals
                .entry(key)
                .or_default()
                .extend_from_slice(&result.intervals);
            interval_enter_sequences
                .entry(key)
                .or_default()
                .extend_from_slice(&result.enter_decode_sequences);
            if let PairingLane::Task(task_id) = lane {
                for &enter_sequence in &result.enter_decode_sequences {
                    task_by_enter_sequence.insert(enter_sequence, task_id);
                }
            }
        }
        if result.unmatched_exits > 0 {
            *unmatched_exits_map.entry(key).or_insert(0) += result.unmatched_exits;
        }
        if result.unmatched_enters > 0 {
            *unmatched_enters_map.entry(key).or_insert(0) += result.unmatched_enters;
        }
    }

    // Generate a synthetic instance_id deterministically from span identity
    // evidence. The segment distinguishes recycled ids even when timestamps tie.
    let synthetic_instance_id = |span_id: u64, segment: u32, first_enter_ts: u64| -> u64 {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"legacy_instance");
        hasher.update(&span_id.to_le_bytes());
        hasher.update(&segment.to_le_bytes());
        hasher.update(&first_enter_ts.to_le_bytes());
        let hash = hasher.finalize();
        u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap())
    };

    // Resolve a parent (span_id, segment) instance from a bare parent span_id
    // and the child's start timestamp: pick the parent segment whose first
    // enter is the latest one at or before the child's start. This selects the
    // parent instance that was live when the child began, since a recycled
    // parent span_id may have several segments across the file.
    // Per parent span_id: its segment identities and first-enter timestamps,
    // sorted by timestamp then segment. A single
    // span_id may be recycled into many close-delimited segments (a hot parent
    // span reused thousands of times), so `resolve_parent_start` must binary
    // search rather than linear-scan — a linear scan per child is O(children ×
    // parent_segments), which is quadratic when one parent id dominates and cost
    // tens of seconds of span resolution on a real 900k-span-event file.
    let parent_first_enters: FxHashMap<u64, Vec<(u64, u32)>> = {
        let mut map: FxHashMap<u64, Vec<(u64, u32)>> = FxHashMap::default();
        for (&(span_id, segment), ctx) in &span_contexts {
            map.entry(span_id)
                .or_default()
                .push((ctx.first_enter_ts, segment));
        }
        for enters in map.values_mut() {
            enters.sort_unstable();
        }
        map
    };
    let resolve_parent = |parent_id: u64, child_start_ts: u64| -> Option<(u64, u32)> {
        parent_first_enters.get(&parent_id).and_then(|enters| {
            // Largest first_enter_ts <= child_start_ts: the parent instance
            // live when the child began. `partition_point` gives the count of
            // elements <= child_start_ts; the one before it is the answer.
            let idx = enters.partition_point(|&(ts, _segment)| ts <= child_start_ts);
            idx.checked_sub(1).map(|i| enters[i])
        })
    };

    // Build ResolvedSpan rows for each (span_id, segment) that has EITHER a
    // close event or local intervals (we produce a row even without a close
    // when we have balanced enter/exit pairs).
    let mut resolved_spans: Vec<ResolvedSpan> = Vec::new();
    let mut resolved_intervals: FxHashMap<u64, Vec<AttributionInterval>> = FxHashMap::default();

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

    for &key @ (span_id, segment) in &all_keys {
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
        if let (Some(ivs), Some(enter_sequences)) = (intervals, interval_enter_sequences.get(&key))
        {
            let mut resolved_task = None;
            let mut task_is_unambiguous = ivs.len() == enter_sequences.len();
            for enter_sequence in enter_sequences {
                let task_id = task_by_enter_sequence.get(enter_sequence).copied();
                match (resolved_task, task_id) {
                    (None, Some(task_id)) => resolved_task = Some(task_id),
                    (Some(existing), Some(task_id)) if existing == task_id => {}
                    _ => {
                        task_is_unambiguous = false;
                        break;
                    }
                }
            }
            if task_is_unambiguous
                && let Some(task_id) = resolved_task
                && let Some(task_polls) = poll_index.task_polls(task_id)
            {
                let task_poll_intervals: Vec<_> = task_polls
                    .iter()
                    .map(|poll| (poll.start, poll.end))
                    .collect();
                let (on_cpu, async_wait) =
                    attribute_legacy_span_from_polls(ivs, &task_poll_intervals);
                on_cpu_ns_est = Some(on_cpu);
                async_wait_ns_est = Some(async_wait);
                // Clear bit 2 (worker/tid ambiguous): every paired interval
                // resolved to the same owning task.
                flags &= !0b0100;
            }
        }

        let instance_id = synthetic_instance_id(span_id, segment, start_ts);

        // Parent uid (if parent_span_id known from enters). The parent's
        // instance_id is derived from the parent segment live at the child's
        // start, matching how this span's own instance_id was computed.
        let parent_span_uid = ctx.and_then(|c| c.parent_span_id).and_then(|parent_id| {
            resolve_parent(parent_id, start_ts).map(|(parent_start, parent_segment)| {
                let parent_instance =
                    synthetic_instance_id(parent_id, parent_segment, parent_start);
                compute_span_uid(boot_id, parent_instance)
            })
        });

        // Propagate unmatched counts from the pairer.
        let unmatched_exits = unmatched_exits_map.get(&key).copied().unwrap_or(0);
        let unmatched_enters = unmatched_enters_map.get(&key).copied().unwrap_or(0);

        let type_name = ctx.and_then(|context| context.type_name.clone());
        let candidate = SpanCandidate {
            boot_id: boot_id.to_string(),
            instance_id,
            kind: "tracing".to_string(),
            name,
            type_name,
            target,
            callsite_file: file,
            callsite_line: line,
            start_mono: MonoNs(start_ts),
            end_mono: MonoNs(end_ts),
            intervals: intervals.cloned().unwrap_or_default(),
            unmatched_exits,
            unmatched_enters,
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

        // Store only intervals during which the owning task was actually being
        // polled, retaining the worker lane for CPU-sample attribution. A raw
        // tracing guard may remain entered across `.await`; its enter→exit
        // envelope is not continuous execution.
        if let (Some(ivs), Some(enter_sequences)) = (intervals, interval_enter_sequences.get(&key))
        {
            let mut attribution_intervals = Vec::new();
            for (&(enter, exit), enter_sequence) in ivs.iter().zip(enter_sequences) {
                let task_id = task_by_enter_sequence.get(enter_sequence).copied();
                if let Some(task_id) = task_id {
                    attribution_intervals.extend(poll_index.intersections(task_id, enter, exit));
                }
            }
            if !attribution_intervals.is_empty() {
                resolved_intervals.insert(instance_id, attribution_intervals);
            }
        }
    }

    LegacyResolution {
        spans: resolved_spans,
        instance_intervals: resolved_intervals,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = "SpanEnter:svc::operation:src/lib.rs:10";

    fn enter(
        timestamp_ns: u64,
        decode_sequence: u64,
        worker_id: u64,
        span_id: u64,
        parent_span_id: Option<u64>,
        value: &str,
    ) -> (String, LegacySpanEnterEvent) {
        (
            SCHEMA.to_string(),
            LegacySpanEnterEvent {
                timestamp_ns,
                worker_id: Some(worker_id),
                task_id: None,
                span_id,
                parent_span_id,
                span_name: Some(value.to_string()),
                decode_sequence,
                attributes: vec![("value".to_string(), value.to_string())],
            },
        )
    }

    fn exit(
        timestamp_ns: u64,
        decode_sequence: u64,
        span_id: u64,
        value: &str,
    ) -> (String, LegacySpanExitEvent) {
        exit_on(timestamp_ns, decode_sequence, 1, None, span_id, value)
    }

    fn exit_on(
        timestamp_ns: u64,
        decode_sequence: u64,
        worker_id: u64,
        task_id: Option<u64>,
        span_id: u64,
        value: &str,
    ) -> (String, LegacySpanExitEvent) {
        (
            SCHEMA.replacen("SpanEnter", "SpanExit", 1),
            LegacySpanExitEvent {
                timestamp_ns,
                worker_id: Some(worker_id),
                task_id,
                span_id,
                span_name: Some("operation".to_string()),
                decode_sequence,
                attributes: vec![("value".to_string(), value.to_string())],
            },
        )
    }

    fn close(timestamp_ns: u64, decode_sequence: u64, span_id: u64) -> LegacySpanCloseEvent {
        LegacySpanCloseEvent {
            timestamp_ns,
            span_id,
            decode_sequence,
        }
    }

    fn resolve(
        enters: &[(String, LegacySpanEnterEvent)],
        exits: &[(String, LegacySpanExitEvent)],
        closes: &[LegacySpanCloseEvent],
        polls: &[PollRecord],
    ) -> LegacyResolution {
        resolve_legacy_spans(
            enters,
            exits,
            closes,
            polls,
            "source",
            "boot",
            None,
            "host",
            "service",
            "2026-07-21",
        )
    }

    #[test]
    fn context_uses_timestamp_order_not_buffer_flush_order() {
        let enters = vec![
            enter(200, 0, 1, 1, None, "later-enter"),
            enter(100, 1, 1, 1, None, "earliest-enter"),
        ];
        let exits = vec![exit(400, 2, 1, "final"), exit(300, 3, 1, "stale")];
        let resolution = resolve(&enters, &exits, &[close(500, 4, 1)], &[]);

        assert_eq!(resolution.spans.len(), 1);
        let span = &resolution.spans[0];
        assert_eq!(span.start_ns, 100);
        assert_eq!(span.name, "earliest-enter");
        assert_eq!(
            span.attributes,
            vec![("value".to_string(), "final".to_string())]
        );
    }

    #[test]
    fn equal_timestamp_recycled_segments_have_distinct_identity() {
        let enters = vec![
            enter(100, 0, 1, 1, None, "first"),
            enter(100, 3, 1, 1, None, "second"),
        ];
        let exits = vec![exit(100, 1, 1, "first"), exit(100, 4, 1, "second")];
        let closes = vec![close(100, 2, 1), close(100, 5, 1)];
        let polls = vec![PollRecord {
            start: MonoNs(0),
            end: MonoNs(200),
            worker_id: 1,
            task_id: 11,
            spawn_loc: None,
            readiness: None,
            task_instrumented: None,
        }];
        let resolution = resolve(&enters, &exits, &closes, &polls);

        assert_eq!(resolution.spans.len(), 2);
        assert_ne!(resolution.spans[0].span_uid, resolution.spans[1].span_uid);
        assert!(resolution.instance_intervals.is_empty());
    }

    #[test]
    fn missing_parent_enter_does_not_fabricate_parent_uid() {
        let enters = vec![enter(100, 0, 1, 1, Some(99), "child")];
        let exits = vec![exit(200, 1, 1, "child")];
        let resolution = resolve(&enters, &exits, &[close(250, 2, 1)], &[]);

        assert_eq!(resolution.spans.len(), 1);
        assert_eq!(resolution.spans[0].parent_span_uid, None);
    }

    #[test]
    fn direct_task_id_takes_precedence_over_worker_correlation() {
        let mut direct = enter(100, 0, 1, 1, None, "direct");
        direct.1.task_id = Some(22);
        let polls = vec![
            PollRecord {
                start: MonoNs(0),
                end: MonoNs(110),
                worker_id: 1,
                task_id: 11,
                spawn_loc: None,
                readiness: None,
                task_instrumented: None,
            },
            PollRecord {
                start: MonoNs(0),
                end: MonoNs(300),
                worker_id: 2,
                task_id: 22,
                spawn_loc: None,
                readiness: None,
                task_instrumented: None,
            },
        ];
        let resolution = resolve(
            &[direct],
            &[exit_on(250, 1, 2, Some(22), 1, "direct")],
            &[close(300, 2, 1)],
            &polls,
        );

        let span = &resolution.spans[0];
        assert_eq!(span.on_cpu_ns_est, Some(150));
        assert_eq!(span.async_wait_ns, Some(0));
        assert_eq!(span.attribution_flags & 0b0100, 0);
        assert_eq!(
            resolution.instance_intervals.values().next().unwrap(),
            &vec![AttributionInterval {
                start: MonoNs(100),
                end: MonoNs(250),
                worker_id: 2,
            }]
        );
    }

    #[test]
    fn legacy_worker_correlation_remains_the_fallback() {
        let polls = vec![PollRecord {
            start: MonoNs(0),
            end: MonoNs(300),
            worker_id: 1,
            task_id: 11,
            spawn_loc: None,
            readiness: None,
            task_instrumented: None,
        }];
        let resolution = resolve(
            &[enter(100, 0, 1, 1, None, "legacy")],
            &[exit(250, 1, 1, "legacy")],
            &[close(300, 2, 1)],
            &polls,
        );

        let span = &resolution.spans[0];
        assert_eq!(span.on_cpu_ns_est, Some(150));
        assert_eq!(span.async_wait_ns, Some(0));
    }

    #[test]
    fn missing_poll_evidence_preserves_worker_migration_pairing() {
        let poll_index = PollIndex::new(&[]);

        assert_eq!(
            pairing_lane(&poll_index, None, Some(1), MonoNs(100), true),
            PairingLane::Unknown
        );
        assert_eq!(
            pairing_lane(&poll_index, None, Some(2), MonoNs(100), false),
            PairingLane::Unknown
        );
    }

    #[test]
    fn partial_poll_evidence_preserves_worker_migration_pairing() {
        let polls = vec![PollRecord {
            start: MonoNs(50),
            end: MonoNs(150),
            worker_id: 1,
            task_id: 11,
            spawn_loc: None,
            readiness: None,
            task_instrumented: None,
        }];
        let resolution = resolve(
            &[enter(100, 0, 1, 1, None, "migrated")],
            &[exit_on(250, 1, 2, None, 1, "migrated")],
            &[close(300, 2, 1)],
            &polls,
        );

        let span = &resolution.spans[0];
        assert_eq!(span.unbalanced_enters, 0);
        assert_eq!(span.unbalanced_exits, 0);
        assert_eq!(span.on_cpu_ns_est, Some(50));
        assert_eq!(span.async_wait_ns, Some(100));
    }

    #[test]
    fn exit_only_poll_evidence_preserves_attribution_after_migration() {
        let polls = vec![PollRecord {
            start: MonoNs(200),
            end: MonoNs(250),
            worker_id: 1,
            task_id: 11,
            spawn_loc: None,
            readiness: None,
            task_instrumented: None,
        }];
        let resolution = resolve(
            &[enter(100, 0, 2, 1, None, "migrated")],
            &[exit_on(250, 1, 1, None, 1, "migrated")],
            &[close(300, 2, 1)],
            &polls,
        );

        let span = &resolution.spans[0];
        assert_eq!(span.unbalanced_enters, 0);
        assert_eq!(span.unbalanced_exits, 0);
        assert_eq!(span.on_cpu_ns_est, Some(50));
        assert_eq!(span.async_wait_ns, Some(100));
        assert_eq!(
            resolution.instance_intervals.values().next().unwrap(),
            &vec![AttributionInterval {
                start: MonoNs(200),
                end: MonoNs(250),
                worker_id: 1,
            }]
        );
    }

    #[test]
    fn legacy_exit_at_poll_stop_pairs_with_the_finishing_task() {
        let polls = vec![PollRecord {
            start: MonoNs(0),
            end: MonoNs(250),
            worker_id: 1,
            task_id: 11,
            spawn_loc: None,
            readiness: None,
            task_instrumented: None,
        }];
        let resolution = resolve(
            &[enter(100, 0, 1, 1, None, "legacy")],
            &[exit(250, 1, 1, "legacy")],
            &[close(300, 2, 1)],
            &polls,
        );

        assert_eq!(resolution.spans.len(), 1);
        let span = &resolution.spans[0];
        assert_eq!(span.unbalanced_enters, 0);
        assert_eq!(span.unbalanced_exits, 0);
        assert_eq!(span.on_cpu_ns_est, Some(150));
    }

    #[test]
    fn missing_task_and_worker_does_not_assume_worker_zero() {
        let mut uncorrelated = enter(100, 0, 0, 1, None, "outside-task");
        uncorrelated.1.worker_id = None;
        let polls = vec![PollRecord {
            start: MonoNs(0),
            end: MonoNs(300),
            worker_id: 0,
            task_id: 99,
            spawn_loc: None,
            readiness: None,
            task_instrumented: None,
        }];
        let resolution = resolve(
            &[uncorrelated],
            &[exit(250, 1, 1, "outside-task")],
            &[close(300, 2, 1)],
            &polls,
        );

        let span = &resolution.spans[0];
        assert_eq!(span.on_cpu_ns_est, None);
        assert_eq!(span.async_wait_ns, None);
        assert_ne!(span.attribution_flags & 0b0100, 0);
    }

    #[test]
    fn intervals_from_multiple_tasks_remain_ambiguous() {
        let enters = vec![
            enter(100, 0, 1, 1, None, "outer"),
            enter(150, 1, 2, 1, None, "inner"),
        ];
        let exits = vec![
            exit_on(200, 2, 2, None, 1, "inner"),
            exit_on(250, 3, 1, None, 1, "outer"),
        ];
        let polls = vec![
            PollRecord {
                start: MonoNs(0),
                end: MonoNs(300),
                worker_id: 1,
                task_id: 11,
                spawn_loc: None,
                readiness: None,
                task_instrumented: None,
            },
            PollRecord {
                start: MonoNs(0),
                end: MonoNs(300),
                worker_id: 2,
                task_id: 22,
                spawn_loc: None,
                readiness: None,
                task_instrumented: None,
            },
        ];
        let resolution = resolve(&enters, &exits, &[close(300, 4, 1)], &polls);

        assert_eq!(resolution.spans.len(), 1);
        let span = &resolution.spans[0];
        assert!(span.concurrent);
        assert_eq!(span.on_cpu_ns_est, None);
        assert_eq!(span.async_wait_ns, None);
        assert_ne!(span.attribution_flags & 0b0100, 0);
    }
}

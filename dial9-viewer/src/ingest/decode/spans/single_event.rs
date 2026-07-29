//! Single-event span adapter.
//!
//! An annotated event carries its lifecycle start while its packed event
//! timestamp is the end. When the event carries (or can infer) a Tokio task id,
//! the task's polls provide the active intervals within that lifecycle.

use rustc_hash::FxHashMap;

use super::clock::{ClockOffset, MonoNs};
use super::interval_pairing::{self, MonoInterval};
use super::legacy::resolve_span_task;
use super::polls::PollTimeline;
use super::span_builder::SpanCandidate;
use super::{ResolvedSpan, SingleEventSpanEvent};

/// Result of single-event span resolution.
pub(crate) struct SingleEventResolution {
    pub(crate) spans: Vec<ResolvedSpan>,
    /// Per synthetic instance id: monotonic active intervals used for sample
    /// attribution.
    pub(crate) instance_intervals: FxHashMap<u64, Vec<MonoInterval>>,
}

/// Resolve completed annotated events into span rows.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_single_event_spans(
    events: &[SingleEventSpanEvent],
    poll_timeline: &PollTimeline,
    source_key: &str,
    boot_id: &str,
    clock_offset: Option<ClockOffset>,
    first_clock_sync_mono: Option<MonoNs>,
    identity_quality: &'static str,
    host: &str,
    service: &str,
    date: &str,
) -> SingleEventResolution {
    let polls = poll_timeline.records();
    let mut polls_by_task: FxHashMap<u64, Vec<MonoInterval>> = FxHashMap::default();
    let mut polls_by_worker: FxHashMap<u64, Vec<(MonoNs, MonoNs, u64)>> = FxHashMap::default();
    for poll in polls {
        if poll.task_id == 0 {
            continue;
        }
        polls_by_task
            .entry(poll.task_id)
            .or_default()
            .push((poll.start, poll.end));
        polls_by_worker.entry(poll.worker_id).or_default().push((
            poll.start,
            poll.end,
            poll.task_id,
        ));
    }

    let mut spans = Vec::with_capacity(events.len());
    let mut instance_intervals = FxHashMap::default();

    for event in events {
        let start = MonoNs(event.start_ns);
        let end = MonoNs(event.end_ns);
        // Prefer a directly captured task. Otherwise use a captured worker, or
        // resolve a stable worker from the OS thread, then find the poll that
        // covers the span start.
        let task_id = event.task_id.or_else(|| {
            let tid = event.thread_id.and_then(|tid| u32::try_from(tid).ok());
            if tid.is_some_and(|tid| poll_timeline.tid_is_in_handoff_gap(tid, start)) {
                return None;
            }
            let worker = event.worker_id.or_else(|| {
                let tid = tid?;
                poll_timeline.worker_for_tid_at(tid, start)
            })?;
            resolve_span_task(polls_by_worker.get(&worker)?, start)
        });

        let mut intervals = Vec::new();
        let mut on_cpu_ns_est = None;
        let mut async_wait_ns = None;
        let mut attribution_flags = 0b1111;
        if let Some(task_polls) = task_id.and_then(|id| polls_by_task.get(&id)) {
            let observable_start = first_clock_sync_mono
                .map(|boundary| start.max(boundary))
                .unwrap_or(start)
                .min(end);
            let intersections = intersect_lifecycle_with_polls(observable_start, end, task_polls);
            if !intersections.is_empty() {
                let on_cpu = interval_pairing::union_interval_duration(&intersections).raw();
                intervals = intersections;
                on_cpu_ns_est = Some(on_cpu);
                async_wait_ns = Some(end.saturating_sub(observable_start).raw() - on_cpu);
                // The task and its polls provide unambiguous runtime placement.
                attribution_flags &= !0b0100;
            }
        }

        let instance_id = synthetic_instance_id(source_key, event);
        let candidate = SpanCandidate {
            boot_id: boot_id.to_string(),
            instance_id,
            kind: event.span_type.clone(),
            name: event.name.clone(),
            type_name: Some(event.schema_name.clone()),
            target: String::new(),
            callsite_file: None,
            callsite_line: None,
            start_mono: start,
            end_mono: end,
            intervals: intervals.clone(),
            unmatched_exits: 0,
            unmatched_enters: 0,
            first_clock_sync_mono,
            identity_quality,
            on_cpu_ns_est,
            blocked_ns_est: None,
            async_wait_ns,
            scheduler_delay_ns: None,
            attribution_flags,
            parent_span_uid: None,
            attributes: event.attributes.clone(),
            source_key: source_key.to_string(),
            host: host.to_string(),
            service: service.to_string(),
            date: date.to_string(),
        };

        spans.push(candidate.finalize(clock_offset));
        if !intervals.is_empty() {
            instance_intervals.insert(instance_id, intervals);
        }
    }

    SingleEventResolution {
        spans,
        instance_intervals,
    }
}

/// Intersect one lifecycle with a task's sorted, non-overlapping polls.
fn intersect_lifecycle_with_polls(
    start: MonoNs,
    end: MonoNs,
    task_polls: &[MonoInterval],
) -> Vec<MonoInterval> {
    let mut intersections = Vec::new();
    let first = task_polls.partition_point(|&(_, poll_end)| poll_end <= start);
    for &(poll_start, poll_end) in &task_polls[first..] {
        if poll_start >= end {
            break;
        }
        let overlap_start = poll_start.max(start);
        let overlap_end = poll_end.min(end);
        if overlap_end > overlap_start {
            intersections.push((overlap_start, overlap_end));
        }
    }
    intersections
}

fn synthetic_instance_id(source_key: &str, event: &SingleEventSpanEvent) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"single_event_span_instance");
    for value in [source_key, event.schema_name.as_str()] {
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.update(&event.start_ns.to_le_bytes());
    hasher.update(&event.end_ns.to_le_bytes());
    hasher.update(&event.decode_sequence.to_le_bytes());
    let hash = hasher.finalize();
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::decode::TraceEvent;
    use crate::ingest::decode::events::{PollEnd, PollStart, WorkerUnpark};

    #[test]
    fn lifecycle_intersects_task_polls() {
        let polls = vec![
            (MonoNs(50), MonoNs(120)),
            (MonoNs(180), MonoNs(220)),
            (MonoNs(300), MonoNs(400)),
        ];
        assert_eq!(
            intersect_lifecycle_with_polls(MonoNs(100), MonoNs(250), &polls),
            vec![(MonoNs(100), MonoNs(120)), (MonoNs(180), MonoNs(220))]
        );
    }

    #[test]
    fn synthetic_identity_distinguishes_equal_events_in_one_source() {
        let event = |decode_sequence| SingleEventSpanEvent {
            schema_name: "work:Request".to_string(),
            span_type: "test-producer".to_string(),
            name: "Request".to_string(),
            start_ns: 100,
            end_ns: 150,
            thread_id: None,
            task_id: None,
            worker_id: None,
            decode_sequence,
            attributes: Vec::new(),
        };
        assert_ne!(
            synthetic_instance_id("source", &event(0)),
            synthetic_instance_id("source", &event(1))
        );
    }

    #[test]
    fn no_poll_evidence_does_not_create_active_time_or_attribution_intervals() {
        let event = SingleEventSpanEvent {
            schema_name: "work:RequestMetrics".to_string(),
            span_type: "test-producer".to_string(),
            name: "request".to_string(),
            start_ns: 100,
            end_ns: 150,
            thread_id: None,
            task_id: None,
            worker_id: None,
            decode_sequence: 0,
            attributes: Vec::new(),
        };
        let resolution = resolve_single_event_spans(
            &[event],
            &PollTimeline::reconstruct(&[]),
            "source",
            "boot",
            None,
            Some(MonoNs(0)),
            "path",
            "host",
            "service",
            "date",
        );

        assert!(resolution.instance_intervals.is_empty());
        let span = &resolution.spans[0];
        assert_eq!(span.active_ns, None);
        assert_eq!(span.observed_active_wall_ns, 0);
        assert_eq!(span.detail_coverage_ns, 0);
        assert_eq!(span.on_cpu_ns_est, None);
        assert_eq!(span.async_wait_ns, None);
        assert_eq!(span.unknown_ns, 50);
    }

    #[test]
    fn schema_name_distinguishes_span_types_with_same_operation() {
        let event = |schema_name: &str, decode_sequence| SingleEventSpanEvent {
            schema_name: schema_name.to_string(),
            span_type: "test-producer".to_string(),
            name: "SharedOperation".to_string(),
            start_ns: 100,
            end_ns: 150,
            thread_id: None,
            task_id: None,
            worker_id: None,
            decode_sequence,
            attributes: Vec::new(),
        };
        let resolution = resolve_single_event_spans(
            &[
                event("work:FirstMetrics", 0),
                event("work:SecondMetrics", 1),
            ],
            &PollTimeline::reconstruct(&[]),
            "source",
            "boot",
            None,
            Some(MonoNs(0)),
            "path",
            "host",
            "service",
            "date",
        );

        assert_eq!(resolution.spans[0].name, resolution.spans[1].name);
        assert_ne!(
            resolution.spans[0].span_type_uid,
            resolution.spans[1].span_type_uid
        );
    }

    #[test]
    fn remapped_thread_resolves_worker_at_span_start() {
        let timeline = PollTimeline::reconstruct(&[
            TraceEvent::WorkerUnpark(WorkerUnpark {
                timestamp_ns: 50,
                worker_id: 0,
                tid: 77,
            }),
            TraceEvent::PollStart(PollStart {
                timestamp_ns: 100,
                worker_id: 0,
                task_id: 1,
                spawn_loc: None,
            }),
            TraceEvent::PollEnd(PollEnd {
                timestamp_ns: 200,
                worker_id: 0,
            }),
            TraceEvent::WorkerUnpark(WorkerUnpark {
                timestamp_ns: 250,
                worker_id: 1,
                tid: 77,
            }),
            TraceEvent::PollStart(PollStart {
                timestamp_ns: 300,
                worker_id: 1,
                task_id: 2,
                spawn_loc: None,
            }),
            TraceEvent::PollEnd(PollEnd {
                timestamp_ns: 400,
                worker_id: 1,
            }),
        ]);
        let event = SingleEventSpanEvent {
            schema_name: "work:Request".to_string(),
            span_type: "test-producer".to_string(),
            name: "request".to_string(),
            start_ns: 310,
            end_ns: 390,
            thread_id: Some(77),
            task_id: None,
            worker_id: None,
            decode_sequence: 0,
            attributes: Vec::new(),
        };

        let resolution = resolve_single_event_spans(
            &[event],
            &timeline,
            "source",
            "boot",
            None,
            Some(MonoNs(0)),
            "path",
            "host",
            "service",
            "date",
        );

        assert_eq!(
            resolution.instance_intervals.values().next().unwrap(),
            &vec![(MonoNs(310), MonoNs(390))]
        );
        assert_eq!(resolution.spans[0].on_cpu_ns_est, Some(80));
    }

    #[test]
    fn context_does_not_resolve_task_inside_block_in_place_handoff_gap() {
        let timeline = PollTimeline::reconstruct(&[
            TraceEvent::WorkerUnpark(WorkerUnpark {
                timestamp_ns: 50,
                worker_id: 0,
                tid: 77,
            }),
            TraceEvent::PollStart(PollStart {
                timestamp_ns: 100,
                worker_id: 0,
                task_id: 1,
                spawn_loc: None,
            }),
            TraceEvent::PollEnd(PollEnd {
                timestamp_ns: 180,
                worker_id: 0,
            }),
            TraceEvent::WorkerPark(crate::ingest::decode::events::WorkerPark {
                timestamp_ns: 200,
                worker_id: 0,
                tid: 88,
            }),
        ]);
        for worker_id in [None, Some(0)] {
            let event = SingleEventSpanEvent {
                schema_name: "work:Request".to_string(),
                span_type: "test-producer".to_string(),
                name: "request".to_string(),
                start_ns: 110,
                end_ns: 170,
                thread_id: Some(77),
                task_id: None,
                worker_id,
                decode_sequence: 0,
                attributes: Vec::new(),
            };

            let resolution = resolve_single_event_spans(
                &[event],
                &timeline,
                "source",
                "boot",
                None,
                Some(MonoNs(0)),
                "path",
                "host",
                "service",
                "date",
            );

            assert!(resolution.instance_intervals.is_empty());
            assert_eq!(resolution.spans[0].on_cpu_ns_est, None);
            assert_eq!(resolution.spans[0].active_ns, None);
        }
    }

    #[test]
    fn time_before_file_boundary_remains_unknown() {
        let timeline = PollTimeline::reconstruct(&[
            TraceEvent::PollStart(PollStart {
                timestamp_ns: 120,
                worker_id: 0,
                task_id: 1,
                spawn_loc: None,
            }),
            TraceEvent::PollEnd(PollEnd {
                timestamp_ns: 140,
                worker_id: 0,
            }),
        ]);
        let event = SingleEventSpanEvent {
            schema_name: "work:Request".to_string(),
            span_type: "test-producer".to_string(),
            name: "request".to_string(),
            start_ns: 50,
            end_ns: 200,
            thread_id: None,
            task_id: Some(1),
            worker_id: None,
            decode_sequence: 0,
            attributes: Vec::new(),
        };

        let resolution = resolve_single_event_spans(
            &[event],
            &timeline,
            "source",
            "boot",
            None,
            Some(MonoNs(100)),
            "path",
            "host",
            "service",
            "date",
        );
        let span = &resolution.spans[0];
        assert_eq!(span.on_cpu_ns_est, Some(20));
        assert_eq!(span.async_wait_ns, Some(80));
        assert_eq!(span.unknown_ns, 50);
    }
}

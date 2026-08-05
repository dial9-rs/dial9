//! Poll reconstruction and sample-to-poll attribution.
//!
//! The module owns worker/tid correlation, open-poll recovery, binary-search
//! indices, and per-poll sample counts behind one interface. All internal poll
//! timestamps remain monotonic until [`PollTimeline::resolved`] creates the
//! public Parquet-facing rows.

use rustc_hash::FxHashMap;

use super::clock::{ClockOffset, MonoNs};
use super::events::{PollEnd, PollStart, TaskSpawn, TaskTerminate, WakeEvent, WorkerPark};
use super::types::SchedulingDelayKind;
use super::{ResolvedPoll, TraceEvent};

/// Readiness evidence: when a task became runnable and how we know.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReadyEvidence {
    pub(crate) timestamp: MonoNs,
    pub(crate) kind: SchedulingDelayKind,
    pub(crate) waker_task_id: Option<u64>,
}

/// One reconstructed poll in the producer's monotonic clock domain.
#[derive(Debug, Clone)]
pub(crate) struct PollRecord {
    pub(crate) start: MonoNs,
    pub(crate) end: MonoNs,
    pub(crate) worker_id: u64,
    pub(crate) task_id: u64,
    pub(crate) spawn_loc: Option<String>,
    /// Readiness evidence for this poll, when the segment could prove it.
    pub(crate) readiness: Option<ReadyEvidence>,
    /// Whether the polled task used dial9's traced waker. `None` when the source
    /// trace predates the `instrumented` field.
    pub(crate) task_instrumented: Option<bool>,
}

pub(crate) struct PollTimeline {
    tid_to_worker: FxHashMap<u32, u64>,
    tid_bindings: FxHashMap<u32, Vec<(MonoNs, u64)>>,
    tid_handoff_gaps: FxHashMap<u32, Vec<(MonoNs, MonoNs)>>,
    records: Vec<PollRecord>,
    by_worker: FxHashMap<u64, Vec<usize>>,
    sample_counts: Vec<(u32, u32)>,
}

/// Per-task readiness bookkeeping used while reconstructing polls.
#[derive(Default)]
struct TaskState {
    ready: Option<ReadyEvidence>,
    instrumented: Option<bool>,
    polling: bool,
    terminated: bool,
}

/// A poll open on a worker while its `PollEnd` has not yet arrived.
struct OpenPoll {
    start: MonoNs,
    worker_id: u64,
    task_id: u64,
    spawn_loc: Option<String>,
    readiness: Option<ReadyEvidence>,
    task_instrumented: Option<bool>,
    /// A wake seen while this poll was running, made effective at PollEnd.
    wake_during_poll: Option<ReadyEvidence>,
}

/// Reconstructs poll spans while consuming per-task readiness evidence. This is
/// intentionally segment-local: a wake or spawn in another source file cannot
/// be ordered reliably while files fold independently.
#[derive(Default)]
struct Reconstructor {
    open_by_worker: FxHashMap<u64, OpenPoll>,
    open_worker_by_task: FxHashMap<u64, u64>,
    tasks: FxHashMap<u64, TaskState>,
    records: Vec<PollRecord>,
}

impl Reconstructor {
    fn task_spawn(&mut self, event: &TaskSpawn) {
        self.tasks.insert(
            event.task_id,
            TaskState {
                ready: Some(ReadyEvidence {
                    timestamp: MonoNs(event.timestamp_ns),
                    kind: SchedulingDelayKind::Spawn,
                    waker_task_id: None,
                }),
                instrumented: event.instrumented,
                polling: false,
                terminated: false,
            },
        );
    }

    fn task_terminate(&mut self, event: &TaskTerminate) {
        let state = self.tasks.entry(event.task_id).or_default();
        state.ready = None;
        state.terminated = true;
    }

    fn wake(&mut self, event: &WakeEvent) {
        let state = self.tasks.entry(event.woken_task_id).or_default();
        if state.terminated {
            return;
        }

        let evidence = ReadyEvidence {
            timestamp: MonoNs(event.timestamp_ns),
            kind: SchedulingDelayKind::Wake,
            waker_task_id: Some(event.waker_task_id),
        };
        if state.polling {
            if let Some(worker_id) = self.open_worker_by_task.get(&event.woken_task_id)
                && let Some(open) = self.open_by_worker.get_mut(worker_id)
            {
                // Wakes coalesce while a task is already scheduled. Keep the
                // first one until the readiness transition is consumed.
                open.wake_during_poll.get_or_insert(evidence);
            }
        } else {
            state.ready.get_or_insert(evidence);
        }
    }

    fn poll_start(&mut self, event: &PollStart) {
        // Missing end events are segment/recovery artifacts. Close the span for
        // duration accounting, but do not infer readiness from a wake inside it:
        // only an explicit PollEnd proves when the task stopped running.
        self.close_poll(event.worker_id, MonoNs(event.timestamp_ns), false);
        if let Some(other_worker) = self.open_worker_by_task.get(&event.task_id).copied()
            && other_worker != event.worker_id
        {
            self.close_poll(other_worker, MonoNs(event.timestamp_ns), false);
        }

        let state = self.tasks.entry(event.task_id).or_default();
        state.terminated = false;
        let readiness = state.ready.take();
        let task_instrumented = state.instrumented;
        state.polling = true;

        self.open_worker_by_task
            .insert(event.task_id, event.worker_id);
        self.open_by_worker.insert(
            event.worker_id,
            OpenPoll {
                start: MonoNs(event.timestamp_ns),
                worker_id: event.worker_id,
                task_id: event.task_id,
                spawn_loc: event.spawn_loc.clone(),
                readiness,
                task_instrumented,
                wake_during_poll: None,
            },
        );
    }

    fn poll_end(&mut self, event: &PollEnd) {
        self.close_poll(event.worker_id, MonoNs(event.timestamp_ns), true);
    }

    fn worker_park(&mut self, event: &WorkerPark) {
        self.close_poll(event.worker_id, MonoNs(event.timestamp_ns), false);
    }

    fn close_poll(&mut self, worker_id: u64, end: MonoNs, explicit_end: bool) {
        let Some(open) = self.open_by_worker.remove(&worker_id) else {
            return;
        };
        self.open_worker_by_task.remove(&open.task_id);

        let state = self.tasks.entry(open.task_id).or_default();
        state.polling = false;
        if explicit_end
            && !state.terminated
            && let Some(mut wake) = open.wake_during_poll
        {
            wake.timestamp = end;
            wake.kind = SchedulingDelayKind::WakeDuringPoll;
            state.ready = Some(wake);
        }

        self.records.push(PollRecord {
            start: open.start,
            end,
            worker_id: open.worker_id,
            task_id: open.task_id,
            spawn_loc: open.spawn_loc,
            readiness: open.readiness,
            task_instrumented: open.task_instrumented,
        });
    }
}

impl PollTimeline {
    pub(crate) fn reconstruct(events: &[TraceEvent]) -> Self {
        // Build both the existing last-seen tid → worker map and a historical
        // binding timeline from all park/unpark events.
        let mut tid_to_worker = FxHashMap::default();
        let mut tid_bindings: FxHashMap<u32, Vec<(MonoNs, u64)>> = FxHashMap::default();
        let mut tid_handoff_gaps: FxHashMap<u32, Vec<(MonoNs, MonoNs)>> = FxHashMap::default();
        struct WorkerTidState {
            current_tid: Option<u32>,
            last_seen_tid: u32,
            last_event: MonoNs,
        }
        let mut worker_tid_state: FxHashMap<u64, WorkerTidState> = FxHashMap::default();
        for event in events {
            let binding = match event {
                TraceEvent::WorkerPark(event) => Some((
                    MonoNs(event.timestamp_ns),
                    event.tid,
                    event.worker_id,
                    false,
                )),
                TraceEvent::WorkerUnpark(event) => {
                    Some((MonoNs(event.timestamp_ns), event.tid, event.worker_id, true))
                }
                _ => None,
            };
            let Some((timestamp, tid, worker_id, is_unpark)) = binding else {
                continue;
            };

            tid_to_worker.insert(tid, worker_id);
            let bindings = tid_bindings.entry(tid).or_default();
            if bindings
                .last()
                .is_none_or(|(_, previous)| *previous != worker_id)
            {
                bindings.push((timestamp, worker_id));
            }

            if let Some(state) = worker_tid_state.get_mut(&worker_id) {
                let expected_tid = state.current_tid.unwrap_or(state.last_seen_tid);
                if expected_tid != tid {
                    let gap = (state.last_event, timestamp);
                    tid_handoff_gaps.entry(expected_tid).or_default().push(gap);
                    tid_handoff_gaps.entry(tid).or_default().push(gap);
                }
                state.current_tid = is_unpark.then_some(tid);
                state.last_seen_tid = tid;
                state.last_event = timestamp;
            } else {
                worker_tid_state.insert(
                    worker_id,
                    WorkerTidState {
                        current_tid: is_unpark.then_some(tid),
                        last_seen_tid: tid,
                        last_event: timestamp,
                    },
                );
            }
        }
        for gaps in tid_handoff_gaps.values_mut() {
            gaps.sort_unstable_by_key(|(start, _)| *start);
        }

        // Reconstruct poll spans and consume per-task readiness evidence
        // (spawns, wakes, terminations) in one timestamp-ordered pass.
        let mut reconstructor = Reconstructor::default();
        for event in events {
            match event {
                TraceEvent::TaskSpawn(event) => reconstructor.task_spawn(event),
                TraceEvent::TaskTerminate(event) => reconstructor.task_terminate(event),
                TraceEvent::Wake(event) => reconstructor.wake(event),
                TraceEvent::PollStart(event) => reconstructor.poll_start(event),
                TraceEvent::PollEnd(event) => reconstructor.poll_end(event),
                TraceEvent::WorkerPark(event) => reconstructor.worker_park(event),
                TraceEvent::CpuSample(_) | TraceEvent::WorkerUnpark(_) => {}
            }
        }
        // Unclosed polls are segment-boundary artifacts and remain discarded.
        let mut records = reconstructor.records;

        records.sort_unstable_by_key(|poll| poll.start);
        let mut by_worker: FxHashMap<u64, Vec<usize>> = FxHashMap::default();
        for (index, poll) in records.iter().enumerate() {
            by_worker.entry(poll.worker_id).or_default().push(index);
        }
        let sample_counts = vec![(0, 0); records.len()];
        Self {
            tid_to_worker,
            tid_bindings,
            tid_handoff_gaps,
            records,
            by_worker,
            sample_counts,
        }
    }

    /// Attribute one sample and return `(worker, poll_duration, spawn_location)`.
    pub(crate) fn attribute_sample(
        &mut self,
        tid: u32,
        timestamp: MonoNs,
        source: u8,
    ) -> (Option<u32>, Option<u64>, Option<String>) {
        // UNKNOWN=255 and BLOCKING=254 are off-runtime, represented as None.
        let worker = match self.tid_to_worker.get(&tid).copied() {
            Some(worker) if worker < 254 => Some(worker as u32),
            _ => None,
        };
        let Some(worker_id) = worker else {
            return (None, None, None);
        };
        let Some(indices) = self.by_worker.get(&(worker_id as u64)) else {
            return (worker, None, None);
        };
        let position = indices.partition_point(|&index| self.records[index].start <= timestamp);
        if position == 0 {
            return (worker, None, None);
        }
        let poll_index = indices[position - 1];
        let poll = &self.records[poll_index];
        if timestamp < poll.start || timestamp >= poll.end {
            return (worker, None, None);
        }
        if source == 0 {
            self.sample_counts[poll_index].0 += 1;
        } else {
            self.sample_counts[poll_index].1 += 1;
        }
        (
            worker,
            Some(poll.end.saturating_sub(poll.start).raw()),
            poll.spawn_loc.clone(),
        )
    }

    pub(crate) fn records(&self) -> &[PollRecord] {
        &self.records
    }

    /// Resolve the worker bound to `tid` at a historical monotonic timestamp.
    ///
    /// The last-seen map remains separate for existing sample attribution
    /// behavior.
    pub(crate) fn worker_for_tid_at(&self, tid: u32, timestamp: MonoNs) -> Option<u64> {
        if self.tid_is_in_handoff_gap(tid, timestamp) {
            return None;
        }
        let bindings = self.tid_bindings.get(&tid)?;
        let index = bindings.partition_point(|(bound_at, _)| *bound_at <= timestamp);
        index.checked_sub(1).map(|index| bindings[index].1)
    }

    pub(crate) fn tid_is_in_handoff_gap(&self, tid: u32, timestamp: MonoNs) -> bool {
        self.tid_handoff_gaps.get(&tid).is_some_and(|gaps| {
            gaps.iter()
                .take_while(|(start, _)| *start <= timestamp)
                .any(|(_, end)| timestamp < *end)
        })
    }

    pub(crate) fn resolved(
        &self,
        clock_offset: Option<ClockOffset>,
        host: &str,
        service: &str,
        date: &str,
    ) -> Vec<ResolvedPoll> {
        self.records
            .iter()
            .zip(&self.sample_counts)
            .map(|(poll, &(cpu_sample_count, sched_sample_count))| {
                let start = poll.start.to_wall_or_raw(clock_offset);
                let end = poll.end.to_wall_or_raw(clock_offset);
                // Only trust readiness that precedes the poll start; a later
                // timestamp would be a reconstruction artifact, not a delay.
                let readiness = poll.readiness.filter(|ready| ready.timestamp <= poll.start);
                ResolvedPoll {
                    start_ns: start.raw(),
                    end_ns: end.raw(),
                    // Saturate defensively so corrupt clock data cannot underflow.
                    duration_ns: end.saturating_sub(start).raw(),
                    worker_id: poll.worker_id as u32,
                    task_id: poll.task_id,
                    spawn_loc: poll.spawn_loc.clone(),
                    cpu_sample_count,
                    sched_sample_count,
                    host: host.to_string(),
                    service: service.to_string(),
                    date: date.to_string(),
                    ready_at_ns: readiness
                        .map(|ready| ready.timestamp.to_wall_or_raw(clock_offset).raw()),
                    scheduling_delay_ns: readiness
                        .map(|ready| poll.start.saturating_sub(ready.timestamp).raw()),
                    scheduling_delay_kind: readiness.map(|ready| ready.kind),
                    waker_task_id: readiness.and_then(|ready| ready.waker_task_id),
                    task_instrumented: poll.task_instrumented,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::decode::events::{WorkerPark, WorkerUnpark};

    #[test]
    fn historical_tid_mapping_resolves_remapped_tids_at_event_time() {
        let events = vec![
            TraceEvent::WorkerUnpark(WorkerUnpark {
                timestamp_ns: 10,
                worker_id: 1,
                tid: 100,
            }),
            TraceEvent::WorkerPark(WorkerPark {
                timestamp_ns: 20,
                worker_id: 1,
                tid: 100,
            }),
            TraceEvent::WorkerUnpark(WorkerUnpark {
                timestamp_ns: 30,
                worker_id: 2,
                tid: 200,
            }),
            TraceEvent::WorkerPark(WorkerPark {
                timestamp_ns: 40,
                worker_id: 3,
                tid: 200,
            }),
        ];

        let timeline = PollTimeline::reconstruct(&events);
        assert_eq!(timeline.worker_for_tid_at(100, MonoNs(15)), Some(1));
        assert_eq!(timeline.worker_for_tid_at(200, MonoNs(35)), Some(2));
        assert_eq!(timeline.worker_for_tid_at(200, MonoNs(45)), Some(3));
        assert_eq!(timeline.worker_for_tid_at(200, MonoNs(25)), None);
        assert_eq!(timeline.tid_to_worker.get(&200), Some(&3));
    }

    #[test]
    fn historical_tid_mapping_is_unknown_inside_handoff_gap() {
        let events = vec![
            TraceEvent::WorkerUnpark(WorkerUnpark {
                timestamp_ns: 10,
                worker_id: 1,
                tid: 100,
            }),
            TraceEvent::WorkerPark(WorkerPark {
                timestamp_ns: 20,
                worker_id: 1,
                tid: 200,
            }),
        ];

        let timeline = PollTimeline::reconstruct(&events);
        assert_eq!(timeline.worker_for_tid_at(100, MonoNs(15)), None);
        assert_eq!(timeline.worker_for_tid_at(200, MonoNs(15)), None);
        assert_eq!(timeline.worker_for_tid_at(100, MonoNs(20)), Some(1));
    }

    fn start(timestamp_ns: u64, task_id: u64) -> PollStart {
        PollStart {
            timestamp_ns,
            worker_id: 0,
            task_id,
            spawn_loc: Some("src/test.rs:1".to_string()),
        }
    }

    fn end(timestamp_ns: u64) -> PollEnd {
        PollEnd {
            timestamp_ns,
            worker_id: 0,
        }
    }

    #[test]
    fn spawn_infers_first_poll_without_wake_events() {
        let mut reconstructor = Reconstructor::default();
        reconstructor.task_spawn(&TaskSpawn {
            timestamp_ns: 100,
            task_id: 7,
            instrumented: Some(false),
        });
        reconstructor.poll_start(&start(160, 7));
        reconstructor.poll_end(&end(180));
        reconstructor.poll_start(&start(500, 7));
        reconstructor.poll_end(&end(520));

        assert_eq!(reconstructor.records.len(), 2);
        let first = reconstructor.records[0].readiness.unwrap();
        assert_eq!(first.timestamp, MonoNs(100));
        assert_eq!(first.kind, SchedulingDelayKind::Spawn);
        assert_eq!(reconstructor.records[0].task_instrumented, Some(false));
        assert!(
            reconstructor.records[1].readiness.is_none(),
            "later polls without wake evidence must remain unknown"
        );
    }

    #[test]
    fn idle_wake_measures_next_poll_and_coalesces_to_earliest_wake() {
        let mut reconstructor = Reconstructor::default();
        reconstructor.wake(&WakeEvent {
            timestamp_ns: 200,
            waker_task_id: 11,
            woken_task_id: 7,
        });
        reconstructor.wake(&WakeEvent {
            timestamp_ns: 250,
            waker_task_id: 12,
            woken_task_id: 7,
        });
        reconstructor.poll_start(&start(300, 7));
        reconstructor.poll_end(&end(320));

        let ready = reconstructor.records[0].readiness.unwrap();
        assert_eq!(ready.timestamp, MonoNs(200));
        assert_eq!(ready.kind, SchedulingDelayKind::Wake);
        assert_eq!(ready.waker_task_id, Some(11));
    }

    #[test]
    fn wake_during_poll_becomes_ready_at_explicit_poll_end() {
        let mut reconstructor = Reconstructor::default();
        reconstructor.poll_start(&start(100, 7));
        reconstructor.wake(&WakeEvent {
            timestamp_ns: 150,
            waker_task_id: 11,
            woken_task_id: 7,
        });
        reconstructor.poll_end(&end(200));
        reconstructor.poll_start(&start(500, 7));
        reconstructor.poll_end(&end(520));

        let ready = reconstructor.records[1].readiness.unwrap();
        assert_eq!(ready.timestamp, MonoNs(200));
        assert_eq!(ready.kind, SchedulingDelayKind::WakeDuringPoll);
        assert_eq!(ready.waker_task_id, Some(11));
    }

    #[test]
    fn synthetic_poll_close_does_not_infer_mid_poll_readiness() {
        let mut reconstructor = Reconstructor::default();
        reconstructor.poll_start(&start(100, 7));
        reconstructor.wake(&WakeEvent {
            timestamp_ns: 150,
            waker_task_id: 11,
            woken_task_id: 7,
        });
        // A second PollStart closes the first poll synthetically because its
        // PollEnd is missing. The wake cannot establish the true ready time.
        reconstructor.poll_start(&start(500, 7));
        reconstructor.poll_end(&end(520));

        assert_eq!(reconstructor.records.len(), 2);
        assert!(reconstructor.records[1].readiness.is_none());
    }
}

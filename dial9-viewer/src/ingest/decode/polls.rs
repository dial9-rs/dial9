//! Poll reconstruction and sample-to-poll attribution.
//!
//! The module owns worker/tid correlation, open-poll recovery, binary-search
//! indices, and per-poll sample counts behind one interface. All internal poll
//! timestamps remain monotonic until [`PollTimeline::resolved`] creates the
//! public Parquet-facing rows.

use rustc_hash::FxHashMap;

use super::clock::{ClockOffset, MonoNs};
use super::{ResolvedPoll, TraceEvent};

/// One reconstructed poll in the producer's monotonic clock domain.
#[derive(Debug, Clone)]
pub(crate) struct PollRecord {
    pub(crate) start: MonoNs,
    pub(crate) end: MonoNs,
    pub(crate) worker_id: u64,
    pub(crate) task_id: u64,
    pub(crate) spawn_loc: Option<String>,
}

pub(crate) struct PollTimeline {
    tid_to_worker: FxHashMap<u32, u64>,
    records: Vec<PollRecord>,
    by_worker: FxHashMap<u64, Vec<usize>>,
    sample_counts: Vec<(u32, u32)>,
}

impl PollTimeline {
    pub(crate) fn reconstruct(events: &[TraceEvent]) -> Self {
        // Build tid → worker_id mapping from ALL park/unpark events. A tid is
        // bound to the same worker for the entire segment lifetime.
        //
        // TODO(block-in-place): when a worker hands its tid off via
        // block_in_place, samples inside the handoff interval are not confidently
        // attributable and the JS reference rewrites them to off-runtime (see
        // `deriveBlockInPlaceGaps` in trace_parser.js, ADR-0001/0002). We don't
        // do that gap detection here yet; for now a tid keeps its last-seen
        // worker. Closing this requires detecting tid changes between consecutive
        // park/unpark events per worker.
        let mut tid_to_worker = FxHashMap::default();
        for event in events {
            match event {
                TraceEvent::WorkerPark(event) => {
                    tid_to_worker.insert(event.tid, event.worker_id);
                }
                TraceEvent::WorkerUnpark(event) => {
                    tid_to_worker.insert(event.tid, event.worker_id);
                }
                _ => {}
            }
        }

        struct OpenPoll {
            start: MonoNs,
            worker_id: u64,
            task_id: u64,
            spawn_loc: Option<String>,
        }

        let mut open: FxHashMap<u64, OpenPoll> = FxHashMap::default();
        let mut records = Vec::new();
        let close = |open: OpenPoll, end: MonoNs| PollRecord {
            start: open.start,
            end,
            worker_id: open.worker_id,
            task_id: open.task_id,
            spawn_loc: open.spawn_loc,
        };

        for event in events {
            match event {
                TraceEvent::PollStart(event) => {
                    // A second start closes a missing-end poll, matching the JS
                    // buildWorkerSpans behavior.
                    if let Some(previous) = open.remove(&event.worker_id) {
                        records.push(close(previous, MonoNs(event.timestamp_ns)));
                    }
                    open.insert(
                        event.worker_id,
                        OpenPoll {
                            start: MonoNs(event.timestamp_ns),
                            worker_id: event.worker_id,
                            task_id: event.task_id,
                            spawn_loc: event.spawn_loc.clone(),
                        },
                    );
                }
                TraceEvent::PollEnd(event) => {
                    if let Some(previous) = open.remove(&event.worker_id) {
                        records.push(close(previous, MonoNs(event.timestamp_ns)));
                    }
                }
                TraceEvent::WorkerPark(event) => {
                    if let Some(previous) = open.remove(&event.worker_id) {
                        records.push(close(previous, MonoNs(event.timestamp_ns)));
                    }
                }
                _ => {}
            }
        }
        // Unclosed polls are segment-boundary artifacts and remain discarded.

        records.sort_unstable_by_key(|poll| poll.start);
        let mut by_worker: FxHashMap<u64, Vec<usize>> = FxHashMap::default();
        for (index, poll) in records.iter().enumerate() {
            by_worker.entry(poll.worker_id).or_default().push(index);
        }
        let sample_counts = vec![(0, 0); records.len()];
        Self {
            tid_to_worker,
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
                }
            })
            .collect()
    }
}

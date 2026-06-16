// This module is gated behind `#[cfg(feature = "analysis")]` and used by tests.

use crate::telemetry::analysis_events::{CpuSampleSource, Dial9Event, WorkerId};
use dial9_trace_format::decoder::Decoder;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::io::{Read as _, Result};

/// Reads a trace file written in the `dial9-trace-format` binary format.
///
/// Decodes the entire file eagerly and populates lookup tables for spawn
/// locations (from the string pool), task→spawn-location mappings, callframe
/// symbols, thread names, and segment metadata.
#[derive(Debug)]
pub struct TraceReader {
    /// All decoded events (including metadata like TaskSpawn).
    pub all_events: Vec<Dial9Event>,
    /// Runtime events only (excludes TaskSpawn, SegmentMetadata, ClockSync).
    pub runtime_events: Vec<Dial9Event>,
    /// Task ID → spawn location string mapping built from TaskSpawn and PollStart events.
    pub task_spawn_locs: HashMap<u64, String>,
    /// OS tid → thread name mapping built from CpuSampleEvent thread_name.
    pub thread_names: HashMap<u32, String>,
    /// Key-value metadata from the most recent SegmentMetadata event.
    pub segment_metadata: HashMap<String, String>,
}

impl TraceReader {
    /// Read and decode a trace file at the given path.
    pub fn new(path: &str) -> Result<Self> {
        let data = read_trace_file(path)?;
        let mut dec = Decoder::new(&data).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid trace header")
        })?;

        let mut task_spawn_locs: HashMap<u64, String> = HashMap::new();
        let mut thread_names = HashMap::new();
        let mut segment_metadata = HashMap::new();
        let mut events = Vec::new();

        dec.for_each_event(|ev| {
            // Deserialize into Dial9Event via serde
            let raw: Dial9Event = match ev.deserialize() {
                Ok(e) => e,
                Err(e) => {
                    tracing::debug!(event_name = ev.name, error = %e, "skipping unrecognized event");
                    return;
                }
            };

            match &raw {
                Dial9Event::TaskSpawnEvent(e) => {
                    task_spawn_locs.insert(e.task_id, e.spawn_loc.clone());
                }
                Dial9Event::PollStartEvent(e) => {
                    task_spawn_locs
                        .entry(e.task_id)
                        .or_insert_with(|| e.spawn_loc.clone());
                }
                Dial9Event::CpuSampleEvent(e) => {
                    if let Some(ref name) = e.thread_name {
                        thread_names.insert(e.tid, name.clone());
                    }
                }
                Dial9Event::SegmentMetadataEvent(e) => {
                    segment_metadata = e.entries.clone();
                }
                Dial9Event::Other | Dial9Event::ProcessResourceUsageEvent(_) => {
                    // Unknown event: deserialize as CustomEvent to get fields.
                    match ev.deserialize::<crate::telemetry::analysis_events::CustomEvent>() {
                        Ok(custom) => {
                            events.push(Dial9Event::Custom(custom));
                        }
                        Err(e) => {
                            tracing::debug!(
                                event_name = ev.name,
                                error = %e,
                                "failed to deserialize custom event"
                            );
                        }
                    }
                    return;
                }
                _ => {}
            }
            events.push(raw);
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        let runtime_events = events
            .iter()
            .filter(|e| {
                !matches!(
                    e,
                    Dial9Event::TaskSpawnEvent { .. }
                        | Dial9Event::SegmentMetadataEvent { .. }
                        | Dial9Event::ClockSyncEvent { .. }
                )
            })
            .cloned()
            .collect();

        Ok(Self {
            all_events: events,
            runtime_events,
            task_spawn_locs,
            thread_names,
            segment_metadata,
        })
    }
}

fn read_trace_file(path: &str) -> Result<Vec<u8>> {
    let data = std::fs::read(path)?;
    maybe_decompress_gzip(data)
}

fn maybe_decompress_gzip(data: Vec<u8>) -> Result<Vec<u8>> {
    const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

    if !data.starts_with(&GZIP_MAGIC) {
        return Ok(data);
    }

    let mut decoder = flate2::read::GzDecoder::new(&data[..]);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to decompress gzip trace: {error}"),
        )
    })?;
    Ok(decompressed)
}

/// Aggregated statistics for a single Tokio worker thread.
#[derive(Debug, Default)]
pub struct WorkerStats {
    /// Number of task polls executed by this worker.
    pub poll_count: usize,
    /// Number of times this worker parked (went idle).
    pub park_count: usize,
    /// Number of times this worker was unparked (resumed).
    pub unpark_count: usize,
    /// Cumulative time spent polling tasks, in nanoseconds.
    pub total_poll_time_ns: u64,
    /// Maximum observed local queue depth for this worker.
    pub max_local_queue: usize,
    /// Maximum OS scheduling wait observed during any single unpark, in nanoseconds.
    pub max_sched_wait_ns: u64,
    /// Cumulative OS scheduling wait across all unparks, in nanoseconds.
    pub total_sched_wait_ns: u64,
}

/// Aggregated poll statistics for a single spawn location.
#[derive(Debug, Default)]
pub struct SpawnLocationStats {
    /// Number of polls from tasks spawned at this location.
    pub poll_count: usize,
    /// Cumulative poll time for tasks from this location, in nanoseconds.
    pub total_poll_time_ns: u64,
}

/// Summary of a decoded trace: event counts, timing, and per-worker statistics.
#[derive(Debug, Default)]
pub struct TraceAnalysis {
    /// Total number of events in the trace.
    pub total_events: usize,
    /// Wall-clock duration of the trace in nanoseconds.
    pub duration_ns: u64,
    /// Per-worker aggregated statistics.
    pub worker_stats: HashMap<WorkerId, WorkerStats>,
    /// Maximum observed global queue depth across all samples.
    pub max_global_queue: usize,
    /// Average global queue depth across all samples.
    pub avg_global_queue: f64,
    /// Per-spawn-location statistics (only populated when task tracking is enabled).
    pub spawn_location_stats: HashMap<String, SpawnLocationStats>,
}

/// Helper: get timestamp from a Dial9Event.
fn event_timestamp(ev: &Dial9Event) -> Option<u64> {
    match ev {
        Dial9Event::PollStartEvent(e) => Some(e.timestamp_ns),
        Dial9Event::PollEndEvent(e) => Some(e.timestamp_ns),
        Dial9Event::WorkerParkEvent(e) => Some(e.timestamp_ns),
        Dial9Event::WorkerUnparkEvent(e) => Some(e.timestamp_ns),
        Dial9Event::QueueSampleEvent(e) => Some(e.timestamp_ns),
        Dial9Event::TaskSpawnEvent(e) => Some(e.timestamp_ns),
        Dial9Event::TaskTerminateEvent(e) => Some(e.timestamp_ns),
        Dial9Event::CpuSampleEvent(e) => Some(e.timestamp_ns),
        Dial9Event::TaskDumpEvent(e) => Some(e.timestamp_ns),
        Dial9Event::WakeEvent(e) => Some(e.timestamp_ns),
        Dial9Event::SegmentMetadataEvent(e) => Some(e.timestamp_ns),
        Dial9Event::ClockSyncEvent(e) => Some(e.timestamp_ns),
        Dial9Event::AllocEvent(e) => Some(e.timestamp_ns),
        Dial9Event::FreeEvent(e) => Some(e.timestamp_ns),
        Dial9Event::ProcessResourceUsageEvent(e) => Some(e.timestamp_ns),
        Dial9Event::TcpAcceptQueueEvent(e) => Some(e.timestamp_ns),
        Dial9Event::Custom(e) => e.timestamp_ns,
        Dial9Event::Other => None,
    }
}

/// Build a sorted list of (timestamp, global_queue_depth) from QueueSample events.
fn build_global_queue_timeline(events: &[Dial9Event]) -> Vec<(u64, usize)> {
    let mut timeline: Vec<(u64, usize)> = events
        .iter()
        .filter_map(|e| match e {
            Dial9Event::QueueSampleEvent(q) => Some((q.timestamp_ns, q.global_queue as usize)),
            _ => None,
        })
        .collect();
    timeline.sort_by_key(|&(ts, _)| ts);
    timeline
}

/// Look up the most recent global queue depth at or before the given timestamp.
/// Returns 0 if no sample has been recorded yet.
fn lookup_global_queue_depth(timeline: &[(u64, usize)], timestamp: u64) -> usize {
    match timeline.binary_search_by_key(&timestamp, |&(ts, _)| ts) {
        Ok(idx) => timeline[idx].1,
        Err(0) => 0,
        Err(idx) => timeline[idx - 1].1,
    }
}

/// Analyze a slice of telemetry events and produce a [`TraceAnalysis`] summary.
pub fn analyze_trace(events: &[Dial9Event]) -> TraceAnalysis {
    let mut worker_stats: HashMap<WorkerId, WorkerStats> = HashMap::new();
    let mut poll_starts: HashMap<WorkerId, u64> = HashMap::new();
    let mut poll_start_locs: HashMap<WorkerId, String> = HashMap::new();
    let mut spawn_location_stats: HashMap<String, SpawnLocationStats> = HashMap::new();
    let mut max_global_queue = 0;
    let mut global_queue_sum = 0u64;
    let mut global_queue_count = 0u64;

    let Some(start_time) = events.first().and_then(event_timestamp) else {
        return TraceAnalysis::default();
    };
    let Some(end_time) = events.last().and_then(event_timestamp) else {
        return TraceAnalysis::default();
    };

    for event in events {
        match event {
            Dial9Event::QueueSampleEvent(q) => {
                let depth = q.global_queue as usize;
                max_global_queue = max_global_queue.max(depth);
                global_queue_sum += depth as u64;
                global_queue_count += 1;
            }
            Dial9Event::PollStartEvent(e) => {
                let wid = e.worker_id;
                let stats = worker_stats.entry(wid).or_default();
                stats.max_local_queue = stats.max_local_queue.max(e.local_queue as usize);
                stats.poll_count += 1;
                poll_starts.insert(wid, e.timestamp_ns);
                if !e.spawn_loc.is_empty() {
                    spawn_location_stats
                        .entry(e.spawn_loc.clone())
                        .or_default()
                        .poll_count += 1;
                    poll_start_locs.insert(wid, e.spawn_loc.clone());
                }
            }
            Dial9Event::PollEndEvent(e) => {
                let wid = e.worker_id;
                let stats = worker_stats.entry(wid).or_default();
                if let Some(start) = poll_starts.get(&wid) {
                    let duration = e.timestamp_ns.saturating_sub(*start);
                    stats.total_poll_time_ns += duration;
                    if let Some(loc_id) = poll_start_locs.remove(&wid) {
                        spawn_location_stats
                            .entry(loc_id)
                            .or_default()
                            .total_poll_time_ns += duration;
                    }
                }
            }
            Dial9Event::WorkerParkEvent(e) => {
                let wid = e.worker_id;
                let stats = worker_stats.entry(wid).or_default();
                stats.max_local_queue = stats.max_local_queue.max(e.local_queue as usize);
                stats.park_count += 1;
            }
            Dial9Event::WorkerUnparkEvent(e) => {
                let wid = e.worker_id;
                let stats = worker_stats.entry(wid).or_default();
                stats.max_local_queue = stats.max_local_queue.max(e.local_queue as usize);
                stats.unpark_count += 1;
                stats.total_sched_wait_ns += e.sched_wait_ns;
                stats.max_sched_wait_ns = stats.max_sched_wait_ns.max(e.sched_wait_ns);
            }
            _ => {}
        }
    }

    TraceAnalysis {
        total_events: events.len(),
        duration_ns: end_time.saturating_sub(start_time),
        worker_stats,
        max_global_queue,
        avg_global_queue: if global_queue_count > 0 {
            global_queue_sum as f64 / global_queue_count as f64
        } else {
            0.0
        },
        spawn_location_stats,
    }
}

/// Compute wake→poll scheduling delays from wake events and poll starts.
/// Returns a sorted vec of delay durations in nanoseconds.
pub fn compute_wake_to_poll_delays(events: &[Dial9Event]) -> Vec<u64> {
    let mut wakes_by_task: HashMap<u64, Vec<u64>> = HashMap::new();
    for e in events {
        if let Dial9Event::WakeEvent(w) = e {
            wakes_by_task
                .entry(w.woken_task_id)
                .or_default()
                .push(w.timestamp_ns);
        }
    }
    for v in wakes_by_task.values_mut() {
        v.sort_unstable();
    }

    let mut delays = Vec::new();
    for e in events {
        if let Dial9Event::PollStartEvent(p) = e
            && let Some(wakes) = wakes_by_task.get(&p.task_id)
        {
            let idx = wakes.partition_point(|&t| t <= p.timestamp_ns);
            if idx > 0 {
                let delay = p.timestamp_ns - wakes[idx - 1];
                if delay > 0 && delay < 1_000_000_000 {
                    delays.push(delay);
                }
            }
        }
    }
    delays.sort_unstable();
    delays
}

/// An active period between WorkerUnpark and WorkerPark, with scheduling ratio.
#[derive(Debug)]
pub struct ActivePeriod {
    /// Worker thread that was active during this period.
    pub worker_id: WorkerId,
    /// Timestamp when the worker was unparked (nanos).
    pub start_ns: u64,
    /// Timestamp when the worker parked again (nanos).
    pub end_ns: u64,
    /// CPU time consumed during this active period (nanos).
    pub cpu_delta_ns: u64,
    /// Fraction of wall time the thread was actually on-CPU (0.0–1.0).
    pub scheduling_ratio: f64,
}

/// Compute scheduling ratios for each active period (unpark→park) per worker.
pub fn compute_active_periods(events: &[Dial9Event]) -> Vec<ActivePeriod> {
    let mut periods = Vec::new();
    let mut unpark_state: HashMap<WorkerId, (u64, u64)> = HashMap::new();

    for event in events {
        match event {
            Dial9Event::WorkerUnparkEvent(e) => {
                unpark_state.insert(e.worker_id, (e.timestamp_ns, e.cpu_time_ns));
            }
            Dial9Event::WorkerParkEvent(e) => {
                let wid = e.worker_id;
                if let Some((start_wall, start_cpu)) = unpark_state.remove(&wid) {
                    let wall_delta = e.timestamp_ns.saturating_sub(start_wall);
                    let cpu_delta = e.cpu_time_ns.saturating_sub(start_cpu);
                    let ratio = if wall_delta > 0 {
                        (cpu_delta as f64 / wall_delta as f64).min(1.0)
                    } else {
                        1.0
                    };
                    periods.push(ActivePeriod {
                        worker_id: wid,
                        start_ns: start_wall,
                        end_ns: e.timestamp_ns,
                        cpu_delta_ns: cpu_delta,
                        scheduling_ratio: ratio,
                    });
                }
            }
            _ => {}
        }
    }
    periods
}

/// Print a human-readable summary of a [`TraceAnalysis`] to stdout.
pub fn print_analysis(analysis: &TraceAnalysis) {
    println!("\n=== Trace Analysis ===");
    println!("Total events: {}", analysis.total_events);
    println!(
        "Duration: {:.2}s",
        analysis.duration_ns as f64 / 1_000_000_000.0
    );
    println!("Max global queue depth: {}", analysis.max_global_queue);
    println!("Avg global queue depth: {:.2}", analysis.avg_global_queue);

    println!("\n=== Worker Statistics ===");
    for (worker_id, stats) in &analysis.worker_stats {
        println!("\nWorker {}:", worker_id);
        println!("  Polls: {}", stats.poll_count);
        println!("  Parks: {}", stats.park_count);
        println!("  Unparks: {}", stats.unpark_count);
        println!(
            "  Avg poll time: {:.2}µs",
            if stats.poll_count > 0 {
                stats.total_poll_time_ns as f64 / stats.poll_count as f64 / 1000.0
            } else {
                0.0
            }
        );
        println!("  Max local queue: {}", stats.max_local_queue);
        if stats.unpark_count > 0 {
            println!(
                "  Sched wait: avg {:.1}µs, max {:.1}µs",
                stats.total_sched_wait_ns as f64 / stats.unpark_count as f64 / 1000.0,
                stats.max_sched_wait_ns as f64 / 1000.0,
            );
        }
    }

    if !analysis.spawn_location_stats.is_empty() {
        println!("\n=== Spawn Locations (by poll count) ===");
        let mut locs: Vec<_> = analysis.spawn_location_stats.iter().collect();
        locs.sort_by_key(|b| Reverse(b.1.poll_count));
        for (id, stats) in locs {
            let name = id.as_str();
            let avg_poll_us = if stats.poll_count > 0 {
                stats.total_poll_time_ns as f64 / stats.poll_count as f64 / 1000.0
            } else {
                0.0
            };
            println!(
                "  {} — {} polls, avg {:.2}µs",
                name, stats.poll_count, avg_poll_us
            );
        }
    }
}

/// Detect periods where a worker was parked while the global queue had pending work.
pub fn detect_idle_workers(events: &[Dial9Event]) -> Vec<(WorkerId, u64, usize)> {
    let global_queue_timeline = build_global_queue_timeline(events);
    let mut idle_periods = Vec::new();
    let mut worker_park_times: HashMap<WorkerId, u64> = HashMap::new();

    for event in events {
        match event {
            Dial9Event::WorkerParkEvent(e) => {
                worker_park_times.insert(e.worker_id, e.timestamp_ns);
            }
            Dial9Event::WorkerUnparkEvent(e) => {
                let wid = e.worker_id;
                if let Some(park_time) = worker_park_times.remove(&wid) {
                    let idle_duration = e.timestamp_ns.saturating_sub(park_time);
                    let global_queue_at_unpark =
                        lookup_global_queue_depth(&global_queue_timeline, e.timestamp_ns);
                    if idle_duration > 1_000_000 && global_queue_at_unpark > 0 {
                        idle_periods.push((wid, idle_duration, global_queue_at_unpark));
                    }
                }
            }
            _ => {}
        }
    }

    idle_periods
}

/// A poll that exceeded the given duration threshold.
#[derive(Debug)]
pub struct LongPoll {
    /// Worker that executed the poll.
    pub worker_id: WorkerId,
    /// Poll start timestamp (nanos).
    pub start_ns: u64,
    /// Poll end timestamp (nanos).
    pub end_ns: u64,
    /// Duration of the poll in nanoseconds.
    pub duration_ns: u64,
    /// Task that was being polled.
    pub task_id: u64,
    /// Spawn location of the task.
    pub spawn_loc: String,
}

/// Detect polls that exceed `threshold_ns` nanoseconds.
pub fn detect_long_polls(events: &[Dial9Event], threshold_ns: u64) -> Vec<LongPoll> {
    let mut long_polls = Vec::new();
    let mut poll_starts: HashMap<WorkerId, (u64, u64, String)> = HashMap::new();

    for event in events {
        match event {
            Dial9Event::PollStartEvent(e) => {
                poll_starts.insert(
                    e.worker_id,
                    (e.timestamp_ns, e.task_id, e.spawn_loc.clone()),
                );
            }
            Dial9Event::PollEndEvent(e) => {
                let wid = e.worker_id;
                if let Some((start, task_id, spawn_loc)) = poll_starts.remove(&wid) {
                    let duration = e.timestamp_ns.saturating_sub(start);
                    if duration >= threshold_ns {
                        long_polls.push(LongPoll {
                            worker_id: wid,
                            start_ns: start,
                            end_ns: e.timestamp_ns,
                            duration_ns: duration,
                            task_id,
                            spawn_loc,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    long_polls
}

/// A park period where the OS scheduler delayed the worker thread.
#[derive(Debug)]
pub struct SchedDelay {
    /// Worker that experienced the scheduling delay.
    pub worker_id: WorkerId,
    /// Timestamp when the worker parked (nanos).
    pub park_ns: u64,
    /// Timestamp when the worker was unparked (nanos).
    pub unpark_ns: u64,
    /// OS scheduling wait during this park period (nanos).
    pub sched_wait_ns: u64,
}

/// Detect park periods where OS scheduling wait exceeded `threshold_ns`.
pub fn detect_sched_delays(events: &[Dial9Event], threshold_ns: u64) -> Vec<SchedDelay> {
    let mut delays = Vec::new();
    let mut park_times: HashMap<WorkerId, u64> = HashMap::new();

    for event in events {
        match event {
            Dial9Event::WorkerParkEvent(e) => {
                park_times.insert(e.worker_id, e.timestamp_ns);
            }
            Dial9Event::WorkerUnparkEvent(e) => {
                let wid = e.worker_id;
                if let Some(park_ns) = park_times.remove(&wid)
                    && e.sched_wait_ns >= threshold_ns
                {
                    delays.push(SchedDelay {
                        worker_id: wid,
                        park_ns,
                        unpark_ns: e.timestamp_ns,
                        sched_wait_ns: e.sched_wait_ns,
                    });
                }
            }
            _ => {}
        }
    }
    delays
}

/// A wake-to-poll scheduling delay that exceeded a threshold.
#[derive(Debug)]
pub struct WakeDelay {
    /// Worker that polled the task after the wake.
    pub worker_id: WorkerId,
    /// Timestamp of the wake event (nanos).
    pub wake_ns: u64,
    /// Timestamp of the subsequent poll start (nanos).
    pub poll_ns: u64,
    /// Delay between wake and poll in nanoseconds.
    pub delay_ns: u64,
    /// Task that experienced the scheduling delay.
    pub task_id: u64,
}

/// Detect wake-to-poll delays exceeding `threshold_ns`.
pub fn detect_wake_delays(events: &[Dial9Event], threshold_ns: u64) -> Vec<WakeDelay> {
    const MAX_REASONABLE_DELAY_NS: u64 = 1_000_000_000;

    let mut wakes_by_task: HashMap<u64, Vec<u64>> = HashMap::new();
    for event in events {
        if let Dial9Event::WakeEvent(w) = event {
            wakes_by_task
                .entry(w.woken_task_id)
                .or_default()
                .push(w.timestamp_ns);
        }
    }
    for v in wakes_by_task.values_mut() {
        v.sort_unstable();
    }

    let mut delays = Vec::new();
    for event in events {
        if let Dial9Event::PollStartEvent(p) = event
            && let Some(wakes) = wakes_by_task.get(&p.task_id)
        {
            let idx = wakes.partition_point(|&t| t <= p.timestamp_ns);
            if idx > 0 {
                let delay = p.timestamp_ns.saturating_sub(wakes[idx - 1]);
                if delay >= threshold_ns && delay < MAX_REASONABLE_DELAY_NS {
                    delays.push(WakeDelay {
                        worker_id: p.worker_id,
                        wake_ns: wakes[idx - 1],
                        poll_ns: p.timestamp_ns,
                        delay_ns: delay,
                        task_id: p.task_id,
                    });
                }
            }
        }
    }
    delays
}

/// A poll that had CPU or scheduler samples collected during its execution.
#[derive(Debug)]
pub struct SampledPoll {
    /// Worker that executed the poll.
    pub worker_id: WorkerId,
    /// Poll start timestamp (nanos).
    pub start_ns: u64,
    /// Poll end timestamp (nanos).
    pub end_ns: u64,
    /// Task that was being polled.
    pub task_id: u64,
    /// Spawn location of the task.
    pub spawn_loc: String,
    /// Number of CPU profile samples collected during this poll.
    pub cpu_sample_count: usize,
    /// Number of scheduler event samples collected during this poll.
    pub sched_sample_count: usize,
}

/// Find polls that had CPU profile or scheduler event samples collected during
/// their execution.
pub fn detect_sampled_polls(events: &[Dial9Event]) -> Vec<SampledPoll> {
    struct PollSpan {
        worker_id: WorkerId,
        start_ns: u64,
        end_ns: u64,
        task_id: u64,
        spawn_loc: String,
        cpu_samples: usize,
        sched_samples: usize,
    }

    let mut polls: Vec<PollSpan> = Vec::new();
    let mut poll_starts: HashMap<WorkerId, (u64, u64, String)> = HashMap::new();

    for event in events {
        match event {
            Dial9Event::PollStartEvent(e) => {
                poll_starts.insert(
                    e.worker_id,
                    (e.timestamp_ns, e.task_id, e.spawn_loc.clone()),
                );
            }
            Dial9Event::PollEndEvent(e) => {
                let wid = e.worker_id;
                if let Some((start, task_id, spawn_loc)) = poll_starts.remove(&wid) {
                    polls.push(PollSpan {
                        worker_id: wid,
                        start_ns: start,
                        end_ns: e.timestamp_ns,
                        task_id,
                        spawn_loc,
                        cpu_samples: 0,
                        sched_samples: 0,
                    });
                }
            }
            _ => {}
        }
    }

    polls.sort_unstable_by_key(|p| (p.worker_id, p.start_ns));

    for event in events {
        if let Dial9Event::CpuSampleEvent(e) = event {
            let wid = e.worker_id;
            let start_idx = polls.partition_point(|p| p.worker_id < wid);
            let end_idx = polls.partition_point(|p| p.worker_id <= wid);
            let worker_polls = &mut polls[start_idx..end_idx];

            let idx = worker_polls.partition_point(|p| p.start_ns <= e.timestamp_ns);
            if idx > 0 && e.timestamp_ns <= worker_polls[idx - 1].end_ns {
                match e.source {
                    CpuSampleSource::CpuProfile => worker_polls[idx - 1].cpu_samples += 1,
                    CpuSampleSource::SchedEvent => worker_polls[idx - 1].sched_samples += 1,
                    CpuSampleSource::Unknown(_) => {}
                }
            }
        }
    }

    polls
        .into_iter()
        .filter(|p| p.cpu_samples > 0 || p.sched_samples > 0)
        .map(|p| SampledPoll {
            worker_id: p.worker_id,
            start_ns: p.start_ns,
            end_ns: p.end_ns,
            task_id: p.task_id,
            spawn_loc: p.spawn_loc,
            cpu_sample_count: p.cpu_samples,
            sched_sample_count: p.sched_samples,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::analysis_events::*;
    use dial9_trace_format::InternedString;
    use dial9_trace_format::encoder::Encoder;

    #[test]
    fn trace_reader_reads_gzip_trace_files() {
        use crate::telemetry::buffer::ThreadLocalBuffer;
        use crate::telemetry::format::WorkerParkEvent;
        use crate::telemetry::writer::DiskWriter;
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let raw_path = dir.path().join("trace.bin");

        let mut writer = DiskWriter::single_file(&raw_path).unwrap();
        let batch = crate::telemetry::collector::Batch::new(
            ThreadLocalBuffer::encode_single(&WorkerParkEvent {
                timestamp_ns: 1_000,
                worker_id: crate::telemetry::format::WorkerId::from(7usize),
                local_queue: 3,
                cpu_time_ns: 11,
                tid: 0,
            }),
            1,
        );
        writer.write_encoded_batch(&batch).unwrap();
        writer.flush().unwrap();
        let active = writer.current_active_path().to_owned();
        drop(writer);

        let raw = std::fs::read(&active).unwrap();
        let gzip_path = dir.path().join("trace.bin.gz");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&raw).unwrap();
        let compressed = encoder.finish().unwrap();
        std::fs::write(&gzip_path, compressed).unwrap();

        let reader = TraceReader::new(gzip_path.to_str().unwrap()).unwrap();
        let events = &reader.runtime_events;

        assert_eq!(events.len(), 1);
        let Dial9Event::WorkerParkEvent(ref e) = events[0] else {
            panic!("expected WorkerParkEvent, got {:?}", events[0]);
        };
        assert_eq!(e.timestamp_ns, 1_000);
        assert_eq!(e.worker_id, WorkerId(7));
        assert_eq!(e.local_queue, 3);
        assert_eq!(e.cpu_time_ns, 11);
    }

    #[test]
    fn test_analyze_empty() {
        let events: Vec<Dial9Event> = vec![];
        let analysis = analyze_trace(&events);
        assert_eq!(analysis.total_events, 0);
        assert_eq!(analysis.max_global_queue, 0);
    }

    #[test]
    fn test_global_queue_from_samples_only() {
        let events = vec![
            Dial9Event::PollStartEvent(PollStartEvent {
                timestamp_ns: 1_000_000,
                worker_id: WorkerId(0),
                local_queue: 3,
                task_id: 0,
                spawn_loc: String::new(),
            }),
            Dial9Event::QueueSampleEvent(QueueSampleEvent {
                timestamp_ns: 2_000_000,
                global_queue: 42,
            }),
            Dial9Event::PollEndEvent(PollEndEvent {
                timestamp_ns: 3_000_000,
                worker_id: WorkerId(0),
            }),
        ];
        let analysis = analyze_trace(&events);
        assert_eq!(analysis.max_global_queue, 42);
        assert_eq!(analysis.total_events, 3);
        assert!((analysis.avg_global_queue - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_local_queue_tracked_on_all_worker_events() {
        let events = vec![
            Dial9Event::PollStartEvent(PollStartEvent {
                timestamp_ns: 1_000_000,
                worker_id: WorkerId(0),
                local_queue: 10,
                task_id: 0,
                spawn_loc: String::new(),
            }),
            Dial9Event::PollEndEvent(PollEndEvent {
                timestamp_ns: 2_000_000,
                worker_id: WorkerId(0),
            }),
        ];
        let analysis = analyze_trace(&events);
        let stats = analysis.worker_stats.get(&WorkerId(0)).unwrap();
        assert_eq!(stats.max_local_queue, 10);
    }

    #[test]
    fn test_lookup_global_queue_depth() {
        let timeline = vec![(1_000_000u64, 5), (3_000_000, 10), (5_000_000, 2)];
        assert_eq!(lookup_global_queue_depth(&timeline, 500_000), 0);
        assert_eq!(lookup_global_queue_depth(&timeline, 1_000_000), 5);
        assert_eq!(lookup_global_queue_depth(&timeline, 2_000_000), 5);
        assert_eq!(lookup_global_queue_depth(&timeline, 4_000_000), 10);
        assert_eq!(lookup_global_queue_depth(&timeline, 9_000_000), 2);
    }

    #[test]
    fn test_detect_idle_workers_with_samples() {
        let events = vec![
            Dial9Event::QueueSampleEvent(QueueSampleEvent {
                timestamp_ns: 1_000_000,
                global_queue: 15,
            }),
            Dial9Event::WorkerParkEvent(WorkerParkEvent {
                timestamp_ns: 2_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                cpu_time_ns: 0,
                tid: 0,
            }),
            Dial9Event::QueueSampleEvent(QueueSampleEvent {
                timestamp_ns: 5_000_000,
                global_queue: 20,
            }),
            Dial9Event::WorkerUnparkEvent(WorkerUnparkEvent {
                timestamp_ns: 6_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                cpu_time_ns: 0,
                sched_wait_ns: 0,
                tid: 0,
            }),
        ];
        let idle = detect_idle_workers(&events);
        assert_eq!(idle.len(), 1);
        assert_eq!(idle[0].0, WorkerId(0));
        assert_eq!(idle[0].1, 4_000_000);
        assert_eq!(idle[0].2, 20);
    }

    #[test]
    fn test_detect_long_polls_above_threshold() {
        let events = vec![
            Dial9Event::PollStartEvent(PollStartEvent {
                timestamp_ns: 1_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                task_id: 1,
                spawn_loc: String::new(),
            }),
            Dial9Event::PollEndEvent(PollEndEvent {
                timestamp_ns: 3_000_000,
                worker_id: WorkerId(0),
            }),
            Dial9Event::PollStartEvent(PollStartEvent {
                timestamp_ns: 4_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                task_id: 2,
                spawn_loc: String::new(),
            }),
            Dial9Event::PollEndEvent(PollEndEvent {
                timestamp_ns: 4_500_000,
                worker_id: WorkerId(0),
            }),
        ];
        let long = detect_long_polls(&events, 1_000_000);
        assert_eq!(long.len(), 1);
        assert_eq!(long[0].worker_id, WorkerId(0));
        assert_eq!(long[0].duration_ns, 2_000_000);
        assert_eq!(long[0].task_id, 1);
    }

    #[test]
    fn test_detect_long_polls_none_above_threshold() {
        let events = vec![
            Dial9Event::PollStartEvent(PollStartEvent {
                timestamp_ns: 1_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                task_id: 0,
                spawn_loc: String::new(),
            }),
            Dial9Event::PollEndEvent(PollEndEvent {
                timestamp_ns: 1_500_000,
                worker_id: WorkerId(0),
            }),
        ];
        let long = detect_long_polls(&events, 1_000_000);
        assert!(long.is_empty());
    }

    #[test]
    fn test_detect_long_polls_multiple_workers() {
        let events = vec![
            Dial9Event::PollStartEvent(PollStartEvent {
                timestamp_ns: 1_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                task_id: 1,
                spawn_loc: String::new(),
            }),
            Dial9Event::PollStartEvent(PollStartEvent {
                timestamp_ns: 1_000_000,
                worker_id: WorkerId(1),
                local_queue: 0,
                task_id: 2,
                spawn_loc: String::new(),
            }),
            Dial9Event::PollEndEvent(PollEndEvent {
                timestamp_ns: 5_000_000,
                worker_id: WorkerId(0),
            }),
            Dial9Event::PollEndEvent(PollEndEvent {
                timestamp_ns: 8_000_000,
                worker_id: WorkerId(1),
            }),
        ];
        let long = detect_long_polls(&events, 1_000_000);
        assert_eq!(long.len(), 2);
        assert_eq!(long[0].worker_id, WorkerId(0));
        assert_eq!(long[0].duration_ns, 4_000_000);
        assert_eq!(long[1].worker_id, WorkerId(1));
        assert_eq!(long[1].duration_ns, 7_000_000);
    }

    #[test]
    fn test_detect_sched_delays_above_threshold() {
        let events = vec![
            Dial9Event::WorkerParkEvent(WorkerParkEvent {
                timestamp_ns: 1_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                cpu_time_ns: 0,
                tid: 0,
            }),
            Dial9Event::WorkerUnparkEvent(WorkerUnparkEvent {
                timestamp_ns: 5_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                cpu_time_ns: 0,
                sched_wait_ns: 200_000,
                tid: 0,
            }),
        ];
        let delays = detect_sched_delays(&events, 100_000);
        assert_eq!(delays.len(), 1);
        assert_eq!(delays[0].worker_id, WorkerId(0));
        assert_eq!(delays[0].sched_wait_ns, 200_000);
        assert_eq!(delays[0].park_ns, 1_000_000);
        assert_eq!(delays[0].unpark_ns, 5_000_000);
    }

    #[test]
    fn test_detect_sched_delays_below_threshold() {
        let events = vec![
            Dial9Event::WorkerParkEvent(WorkerParkEvent {
                timestamp_ns: 1_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                cpu_time_ns: 0,
                tid: 0,
            }),
            Dial9Event::WorkerUnparkEvent(WorkerUnparkEvent {
                timestamp_ns: 2_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                cpu_time_ns: 0,
                sched_wait_ns: 50_000,
                tid: 0,
            }),
        ];
        let delays = detect_sched_delays(&events, 100_000);
        assert!(delays.is_empty());
    }

    #[test]
    fn test_detect_sched_delays_multiple_workers() {
        let events = vec![
            Dial9Event::WorkerParkEvent(WorkerParkEvent {
                timestamp_ns: 1_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                cpu_time_ns: 0,
                tid: 0,
            }),
            Dial9Event::WorkerParkEvent(WorkerParkEvent {
                timestamp_ns: 1_000_000,
                worker_id: WorkerId(1),
                local_queue: 0,
                cpu_time_ns: 0,
                tid: 0,
            }),
            Dial9Event::WorkerUnparkEvent(WorkerUnparkEvent {
                timestamp_ns: 3_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                cpu_time_ns: 0,
                sched_wait_ns: 500_000,
                tid: 0,
            }),
            Dial9Event::WorkerUnparkEvent(WorkerUnparkEvent {
                timestamp_ns: 4_000_000,
                worker_id: WorkerId(1),
                local_queue: 0,
                cpu_time_ns: 0,
                sched_wait_ns: 10_000,
                tid: 0,
            }),
        ];
        let delays = detect_sched_delays(&events, 100_000);
        assert_eq!(delays.len(), 1);
        assert_eq!(delays[0].worker_id, WorkerId(0));
    }

    #[test]
    fn test_detect_wake_delays_above_threshold() {
        let events = vec![
            Dial9Event::WakeEvent(WakeEvent {
                timestamp_ns: 1_000_000,
                waker_task_id: 0,
                woken_task_id: 1,
                target_worker: 0,
            }),
            Dial9Event::PollStartEvent(PollStartEvent {
                timestamp_ns: 1_500_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                task_id: 1,
                spawn_loc: String::new(),
            }),
            Dial9Event::PollEndEvent(PollEndEvent {
                timestamp_ns: 1_600_000,
                worker_id: WorkerId(0),
            }),
        ];
        let delays = detect_wake_delays(&events, 100_000);
        assert_eq!(delays.len(), 1);
        assert_eq!(delays[0].delay_ns, 500_000);
        assert_eq!(delays[0].task_id, 1);
        assert_eq!(delays[0].worker_id, WorkerId(0));
    }

    #[test]
    fn test_detect_wake_delays_below_threshold() {
        let events = vec![
            Dial9Event::WakeEvent(WakeEvent {
                timestamp_ns: 1_000_000,
                waker_task_id: 0,
                woken_task_id: 1,
                target_worker: 0,
            }),
            Dial9Event::PollStartEvent(PollStartEvent {
                timestamp_ns: 1_050_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                task_id: 1,
                spawn_loc: String::new(),
            }),
            Dial9Event::PollEndEvent(PollEndEvent {
                timestamp_ns: 1_100_000,
                worker_id: WorkerId(0),
            }),
        ];
        let delays = detect_wake_delays(&events, 100_000);
        assert!(delays.is_empty());
    }

    #[test]
    fn test_detect_wake_delays_discards_idle_tasks() {
        let events = vec![
            Dial9Event::WakeEvent(WakeEvent {
                timestamp_ns: 1_000_000,
                waker_task_id: 0,
                woken_task_id: 1,
                target_worker: 0,
            }),
            Dial9Event::PollStartEvent(PollStartEvent {
                timestamp_ns: 2_000_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                task_id: 1,
                spawn_loc: String::new(),
            }),
            Dial9Event::PollEndEvent(PollEndEvent {
                timestamp_ns: 2_000_100_000,
                worker_id: WorkerId(0),
            }),
        ];
        let delays = detect_wake_delays(&events, 100_000);
        assert!(delays.is_empty());
    }

    #[test]
    fn test_detect_sampled_polls_with_samples() {
        let events = vec![
            Dial9Event::PollStartEvent(PollStartEvent {
                timestamp_ns: 1_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                task_id: 1,
                spawn_loc: String::new(),
            }),
            Dial9Event::CpuSampleEvent(CpuSampleEvent {
                timestamp_ns: 1_500_000,
                worker_id: WorkerId(0),
                tid: 100,
                source: CpuSampleSource::CpuProfile,
                thread_name: None,
                callchain: vec![],
                cpu: None,
            }),
            Dial9Event::CpuSampleEvent(CpuSampleEvent {
                timestamp_ns: 1_800_000,
                worker_id: WorkerId(0),
                tid: 100,
                source: CpuSampleSource::SchedEvent,
                thread_name: None,
                callchain: vec![],
                cpu: None,
            }),
            Dial9Event::PollEndEvent(PollEndEvent {
                timestamp_ns: 2_000_000,
                worker_id: WorkerId(0),
            }),
        ];
        let sampled = detect_sampled_polls(&events);
        assert_eq!(sampled.len(), 1);
        assert_eq!(sampled[0].cpu_sample_count, 1);
        assert_eq!(sampled[0].sched_sample_count, 1);
        assert_eq!(sampled[0].task_id, 1);
    }

    #[test]
    fn test_detect_sampled_polls_no_samples() {
        let events = vec![
            Dial9Event::PollStartEvent(PollStartEvent {
                timestamp_ns: 1_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                task_id: 0,
                spawn_loc: String::new(),
            }),
            Dial9Event::PollEndEvent(PollEndEvent {
                timestamp_ns: 2_000_000,
                worker_id: WorkerId(0),
            }),
        ];
        let sampled = detect_sampled_polls(&events);
        assert!(sampled.is_empty());
    }

    #[test]
    fn test_detect_sampled_polls_sample_outside_poll() {
        let events = vec![
            Dial9Event::PollStartEvent(PollStartEvent {
                timestamp_ns: 1_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                task_id: 0,
                spawn_loc: String::new(),
            }),
            Dial9Event::PollEndEvent(PollEndEvent {
                timestamp_ns: 2_000_000,
                worker_id: WorkerId(0),
            }),
            Dial9Event::CpuSampleEvent(CpuSampleEvent {
                timestamp_ns: 3_000_000,
                worker_id: WorkerId(0),
                tid: 100,
                source: CpuSampleSource::CpuProfile,
                thread_name: None,
                callchain: vec![],
                cpu: None,
            }),
        ];
        let sampled = detect_sampled_polls(&events);
        assert!(sampled.is_empty());
    }

    #[test]
    fn test_detect_idle_workers_no_queue_pressure() {
        let events = vec![
            Dial9Event::QueueSampleEvent(QueueSampleEvent {
                timestamp_ns: 1_000_000,
                global_queue: 0,
            }),
            Dial9Event::WorkerParkEvent(WorkerParkEvent {
                timestamp_ns: 2_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                cpu_time_ns: 0,
                tid: 0,
            }),
            Dial9Event::WorkerUnparkEvent(WorkerUnparkEvent {
                timestamp_ns: 6_000_000,
                worker_id: WorkerId(0),
                local_queue: 0,
                cpu_time_ns: 0,
                sched_wait_ns: 0,
                tid: 0,
            }),
        ];
        let idle = detect_idle_workers(&events);
        assert!(idle.is_empty());
    }

    #[test]
    fn trace_reader_custom_events_resolve_interned_at_parse_time() {
        #[derive(dial9_trace_format::TraceEvent)]
        struct MyEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            label: InternedString,
            count: u32,
        }

        let mut enc = Encoder::new();
        let s = enc.intern_string_infallible("alpha");
        enc.write(&MyEvent {
            timestamp_ns: 1_000,
            label: s,
            count: 1,
        })
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.bin");
        std::fs::write(&path, enc.finish()).unwrap();

        let reader = TraceReader::new(path.to_str().unwrap()).unwrap();
        assert_eq!(reader.all_events.len(), 1);
        let Dial9Event::Custom(ref e) = reader.all_events[0] else {
            panic!("expected Custom, got {:?}", reader.all_events[0]);
        };
        assert_eq!(e.name, "MyEvent");
        assert_eq!(e.timestamp_ns, Some(1_000));
        assert_eq!(
            e.fields.get("label"),
            Some(&dial9_trace_format::FieldValue::String("alpha".to_string()))
        );
        assert_eq!(
            e.fields.get("count"),
            Some(&dial9_trace_format::FieldValue::Varint(1))
        );
    }
}

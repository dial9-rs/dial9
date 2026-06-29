//! Decode raw dial9 trace bytes into CPU samples with resolved symbols.
//!
//! Events within a trace segment are not guaranteed to be in timestamp order
//! (threads flush buffers independently). This module collects all relevant
//! events, sorts them by `timestamp_ns`, then processes them in order so that
//! worker_id can be inferred from WorkerPark/WorkerUnpark tid correlation.

use dial9_trace_format::decoder::Decoder;
use lasso::{Rodeo, Spur};
use rustc_hash::FxHashMap;
use serde::Deserialize;
use std::collections::HashMap;

// ─── Lightweight serde structs (select only needed fields) ───────────────────

#[derive(Debug, Deserialize)]
struct CpuSample {
    timestamp_ns: u64,
    tid: u32,
    source: u64,
    callchain: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct WorkerPark {
    timestamp_ns: u64,
    worker_id: u64,
    tid: u32,
}

#[derive(Debug, Deserialize)]
struct WorkerUnpark {
    timestamp_ns: u64,
    worker_id: u64,
    tid: u32,
}

#[derive(Debug, Deserialize)]
struct PollStart {
    timestamp_ns: u64,
    worker_id: u64,
    task_id: u64,
    #[serde(default)]
    spawn_loc: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PollEnd {
    timestamp_ns: u64,
    worker_id: u64,
}

#[derive(Debug, Deserialize)]
struct ClockSync {
    timestamp_ns: u64,
    realtime_ns: u64,
}

#[derive(Debug, Deserialize)]
struct SymbolEntry {
    addr: u64,
    inline_depth: u64,
    symbol_name: String,
}

// ─── Parsed event enum (sorted by timestamp) ────────────────────────────────

enum TraceEvent {
    CpuSample(CpuSample),
    WorkerPark(WorkerPark),
    WorkerUnpark(WorkerUnpark),
    PollStart(PollStart),
    PollEnd(PollEnd),
}

impl TraceEvent {
    fn timestamp_ns(&self) -> u64 {
        match self {
            Self::CpuSample(e) => e.timestamp_ns,
            Self::WorkerPark(e) => e.timestamp_ns,
            Self::WorkerUnpark(e) => e.timestamp_ns,
            Self::PollStart(e) => e.timestamp_ns,
            Self::PollEnd(e) => e.timestamp_ns,
        }
    }
}

// ─── Public types ────────────────────────────────────────────────────────────

/// A resolved CPU sample ready for Parquet output.
#[derive(Debug, Clone)]
pub struct ResolvedSample {
    pub timestamp_ns: u64,
    pub stack_id: [u8; 16],
    /// Runtime worker this sample is attributed to, or `None` when it cannot be
    /// attributed to a worker (a non-runtime thread, or the producer's
    /// `WorkerId::UNKNOWN`/`BLOCKING` sentinels — see [`decode_samples`]). The
    /// on/off-runtime split downstream is exactly `Some` vs `None`; there is no
    /// in-band sentinel value.
    pub worker_id: Option<u32>,
    pub source: u8,
    pub source_key: String,
    /// Extracted from source_key path
    pub host: String,
    pub service: String,
    pub date: String,
    /// Duration of the enclosing poll span (ns), or `None` if the sample didn't
    /// land inside a poll (off-worker, between polls, etc.).
    pub poll_duration_ns: Option<u64>,
    /// The spawn location of the task that was being polled when this sample
    /// fired, or `None` if the sample didn't land inside a poll or the task
    /// has no recorded spawn location.
    pub spawn_location: Option<String>,
}

/// A reconstructed poll span: one invocation of `Future::poll` on a task.
///
/// Public because it is the third element of [`DecodeResult`], the return type
/// of the public [`decode_samples`].
#[derive(Debug, Clone)]
pub struct ResolvedPoll {
    pub start_ns: u64,
    pub end_ns: u64,
    pub duration_ns: u64,
    pub worker_id: u32,
    pub task_id: u64,
    pub spawn_loc: Option<String>,
    /// CPU profile samples that landed inside this poll.
    pub cpu_sample_count: u32,
    /// Off-CPU / scheduler samples that landed inside this poll.
    pub sched_sample_count: u32,
    pub host: String,
    pub service: String,
    pub date: String,
}

/// Parse `(date, service, host)` from a source key, anchored on the
/// `YYYY-MM-DD` date component so a leading prefix (e.g. `traces/`) does not
/// shift the positions. Layout: `…/{date}/{HHMM}/{service}/{host}/{boot}/{file}`.
///
/// This MUST stay in lockstep with `aggregate::parse_scope_fields`: the scope
/// filter (which decides *which* files to fold and how the output path is
/// partitioned) uses the date-anchored parse, so the `host`/`service`/`date`
/// columns embedded in the Parquet here have to agree with it. A fixed-index
/// parse silently produced wrong columns for any prefixed key.
fn parse_source_key(key: &str) -> (String, String, String) {
    // Strip s3://bucket/ prefix if present
    let path = if let Some(rest) = key.strip_prefix("s3://") {
        rest.split_once('/').map_or(rest, |(_, p)| p)
    } else {
        key
    };
    let parts: Vec<&str> = path.split('/').collect();
    if let Some(anchor) = parts.iter().position(|p| is_date(p)) {
        let date = parts.get(anchor).copied().unwrap_or("").to_string();
        let service = parts.get(anchor + 2).copied().unwrap_or("").to_string();
        let host = parts.get(anchor + 3).copied().unwrap_or("").to_string();
        (date, service, host)
    } else {
        // No date anchor — fall back to the legacy fixed-index parse.
        let date = parts.first().copied().unwrap_or("").to_string();
        let service = parts.get(2).copied().unwrap_or("").to_string();
        let host = parts.get(3).copied().unwrap_or("").to_string();
        (date, service, host)
    }
}

/// True if `s` is a `YYYY-MM-DD` date component.
fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..].iter().all(u8::is_ascii_digit)
}

/// Extract CPU samples from raw (already gunzipped) trace bytes.
///
/// Events are sorted by timestamp within the segment to correctly infer
/// worker_id from WorkerPark/WorkerUnpark tid correlation.
///
/// Returns the resolved samples and a map of stack_id → frame names for the
/// stacks dictionary.
/// Return type for [`decode_samples`]: resolved samples, stacks dictionary, and poll spans.
pub type DecodeResult = (
    Vec<ResolvedSample>,
    HashMap<[u8; 16], Vec<String>>,
    Vec<ResolvedPoll>,
);

pub fn decode_samples(data: &[u8], source_key: &str) -> anyhow::Result<DecodeResult> {
    let mut decoder = Decoder::new(data).ok_or_else(|| anyhow::anyhow!("invalid trace header"))?;

    let mut interner = Rodeo::default();
    let mut addr_to_keys: FxHashMap<u64, Vec<(u64, Spur)>> = FxHashMap::default();
    let mut events: Vec<TraceEvent> = Vec::new();
    let mut clock_offset_ns: Option<i128> = None;

    decoder
        .for_each_event(|ev| match ev.name {
            "ClockSyncEvent" => {
                if let Ok(cs) = ev.deserialize::<ClockSync>()
                    && clock_offset_ns.is_none()
                    && cs.realtime_ns > 0
                    && cs.timestamp_ns > 0
                {
                    clock_offset_ns = Some(cs.realtime_ns as i128 - cs.timestamp_ns as i128);
                }
            }
            "CpuSampleEvent" | "CpuSample" => {
                if let Ok(s) = ev.deserialize::<CpuSample>()
                    && !s.callchain.is_empty()
                {
                    events.push(TraceEvent::CpuSample(s));
                }
            }
            "WorkerParkEvent" => {
                if let Ok(p) = ev.deserialize::<WorkerPark>() {
                    events.push(TraceEvent::WorkerPark(p));
                }
            }
            "WorkerUnparkEvent" => {
                if let Ok(u) = ev.deserialize::<WorkerUnpark>() {
                    events.push(TraceEvent::WorkerUnpark(u));
                }
            }
            "PollStartEvent" => {
                if let Ok(p) = ev.deserialize::<PollStart>() {
                    events.push(TraceEvent::PollStart(p));
                }
            }
            "PollEndEvent" => {
                if let Ok(p) = ev.deserialize::<PollEnd>() {
                    events.push(TraceEvent::PollEnd(p));
                }
            }
            "SymbolTableEntry" => {
                if let Ok(sym) = ev.deserialize::<SymbolEntry>() {
                    let key = interner.get_or_intern(&sym.symbol_name);
                    addr_to_keys
                        .entry(sym.addr)
                        .or_default()
                        .push((sym.inline_depth, key));
                }
            }
            _ => {}
        })
        .map_err(|e| anyhow::anyhow!("decode error: {e}"))?;

    tracing::info!("sorting {} events", events.len());
    // Sort events by timestamp for correct worker_id inference.
    events.sort_unstable_by_key(|e| e.timestamp_ns());

    // Pre-sort symbol entries by inline depth.
    for entries in addr_to_keys.values_mut() {
        entries.sort_unstable_by_key(|(d, _)| *d);
    }

    // Build tid → worker_id mapping from ALL park/unpark events.
    // A tid is bound to the same worker for the entire segment lifetime.
    //
    // TODO(block-in-place): when a worker hands its tid off via block_in_place,
    // samples inside the handoff interval are not confidently attributable and
    // the JS reference rewrites them to off-runtime (see
    // `deriveBlockInPlaceGaps` in trace_parser.js, ADR-0001/0002). We don't do
    // that gap detection here yet; it's rare in practice, so for now a tid keeps
    // its last-seen worker across such a handoff. Closing this requires sorting
    // park/unpark per worker and detecting tid changes between consecutive events.
    let mut tid_to_worker: FxHashMap<u32, u64> = FxHashMap::default();
    for event in &events {
        match event {
            TraceEvent::WorkerPark(p) => {
                tid_to_worker.insert(p.tid, p.worker_id);
            }
            TraceEvent::WorkerUnpark(u) => {
                tid_to_worker.insert(u.tid, u.worker_id);
            }
            _ => {}
        }
    }

    // Reconstruct poll spans per worker. Track open poll for each worker_id.
    struct OpenPoll {
        start_ns: u64,
        worker_id: u64,
        task_id: u64,
        spawn_loc: Option<String>,
    }
    let mut open_polls: FxHashMap<u64, OpenPoll> = FxHashMap::default();
    // Completed polls: (start_ns, end_ns, worker_id, task_id, spawn_loc)
    let mut polls: Vec<(u64, u64, u64, u64, Option<String>)> = Vec::new();

    for event in &events {
        match event {
            TraceEvent::PollStart(p) => {
                // If there's an open poll for this worker (no PollEnd arrived),
                // close it at this timestamp (matches JS buildWorkerSpans behavior).
                if let Some(open) = open_polls.remove(&p.worker_id) {
                    polls.push((
                        open.start_ns,
                        p.timestamp_ns,
                        open.worker_id,
                        open.task_id,
                        open.spawn_loc,
                    ));
                }
                open_polls.insert(
                    p.worker_id,
                    OpenPoll {
                        start_ns: p.timestamp_ns,
                        worker_id: p.worker_id,
                        task_id: p.task_id,
                        spawn_loc: p.spawn_loc.clone(),
                    },
                );
            }
            TraceEvent::PollEnd(p) => {
                if let Some(open) = open_polls.remove(&p.worker_id) {
                    polls.push((
                        open.start_ns,
                        p.timestamp_ns,
                        open.worker_id,
                        open.task_id,
                        open.spawn_loc,
                    ));
                }
            }
            TraceEvent::WorkerPark(p) => {
                // Close any open poll at park time (same as JS).
                if let Some(open) = open_polls.remove(&p.worker_id) {
                    polls.push((
                        open.start_ns,
                        p.timestamp_ns,
                        open.worker_id,
                        open.task_id,
                        open.spawn_loc,
                    ));
                }
            }
            _ => {}
        }
    }
    // Discard unclosed polls (segment-boundary artifacts).
    drop(open_polls);

    // Sort polls by start_ns for efficient binary search during sample attribution.
    polls.sort_unstable_by_key(|(start, _, _, _, _)| *start);

    // Build per-worker poll index for sample attribution.
    // For each worker, polls are sorted by start_ns.
    let mut polls_by_worker: FxHashMap<u64, Vec<usize>> = FxHashMap::default();
    for (i, &(_, _, worker_id, _, _)) in polls.iter().enumerate() {
        polls_by_worker.entry(worker_id).or_default().push(i);
    }

    let mut stacks_dict: FxHashMap<[u8; 16], Vec<String>> = FxHashMap::default();
    let mut stack_cache: FxHashMap<Vec<u64>, [u8; 16]> = FxHashMap::default();
    let mut samples = Vec::new();
    // Track per-poll sample counts: (cpu_count, sched_count) indexed by poll index.
    let mut poll_sample_counts: Vec<(u32, u32)> = vec![(0, 0); polls.len()];

    let (parsed_date, parsed_service, parsed_host) = parse_source_key(source_key);

    for event in &events {
        match event {
            TraceEvent::WorkerPark(_)
            | TraceEvent::WorkerUnpark(_)
            | TraceEvent::PollStart(_)
            | TraceEvent::PollEnd(_) => {}
            TraceEvent::CpuSample(s) => {
                // Attribute the sample to a worker via its tid. `None` (tid never
                // bound to a worker) and the producer sentinels (`UNKNOWN` = 255,
                // `BLOCKING` = 254) are all "off-runtime" — represented as `None`,
                // not an in-band sentinel.
                let worker_id = match tid_to_worker.get(&s.tid).copied() {
                    Some(w) if w < 254 => Some(w as u32),
                    _ => None,
                };

                // Find the enclosing poll for this sample (if on a worker).
                let (poll_duration_ns, spawn_location) = if let Some(w) = worker_id {
                    find_enclosing_poll(
                        &polls,
                        &polls_by_worker,
                        w as u64,
                        s.timestamp_ns,
                        &mut poll_sample_counts,
                        s.source as u8,
                    )
                } else {
                    (None, None)
                };

                let stack_id = if let Some(&cached) = stack_cache.get(&s.callchain) {
                    cached
                } else {
                    let mut hasher = blake3::Hasher::new();
                    let mut first = true;
                    let mut frame_strings: Vec<String> = Vec::new();

                    for &addr in &s.callchain {
                        if let Some(entries) = addr_to_keys.get(&addr) {
                            for (_, key) in entries {
                                let name = interner.resolve(key);
                                if !first {
                                    hasher.update(b"\x00");
                                }
                                hasher.update(name.as_bytes());
                                frame_strings.push(name.to_string());
                                first = false;
                            }
                        } else {
                            let hex = format!("0x{addr:x}");
                            if !first {
                                hasher.update(b"\x00");
                            }
                            hasher.update(hex.as_bytes());
                            frame_strings.push(hex);
                            first = false;
                        }
                    }

                    if frame_strings.is_empty() {
                        continue;
                    }

                    let hash = hasher.finalize();
                    let mut id = [0u8; 16];
                    id.copy_from_slice(&hash.as_bytes()[..16]);

                    stacks_dict.entry(id).or_insert(frame_strings);
                    stack_cache.insert(s.callchain.clone(), id);
                    id
                };

                let wall_ns = match clock_offset_ns {
                    Some(offset) => (s.timestamp_ns as i128 + offset) as u64,
                    None => s.timestamp_ns,
                };
                samples.push(ResolvedSample {
                    timestamp_ns: wall_ns,
                    stack_id,
                    worker_id,
                    source: s.source as u8,
                    source_key: source_key.to_string(),
                    host: parsed_host.clone(),
                    service: parsed_service.clone(),
                    date: parsed_date.clone(),
                    poll_duration_ns,
                    spawn_location,
                });
            }
        }
    }

    // Build resolved polls with sample counts and wall-clock timestamps.
    let resolved_polls: Vec<ResolvedPoll> = polls
        .iter()
        .enumerate()
        .map(|(i, &(start, end, worker_id, task_id, ref spawn_loc))| {
            let (cpu_count, sched_count) = poll_sample_counts[i];
            let wall_start = match clock_offset_ns {
                Some(offset) => (start as i128 + offset) as u64,
                None => start,
            };
            let wall_end = match clock_offset_ns {
                Some(offset) => (end as i128 + offset) as u64,
                None => end,
            };
            ResolvedPoll {
                start_ns: wall_start,
                end_ns: wall_end,
                // Polls are closed in sorted-timestamp order, so end >= start
                // holds in practice; saturate defensively so a corrupt or
                // clock-skewed segment can't underflow (panic in debug, wrap in
                // release) instead of just yielding a zero-length poll.
                duration_ns: wall_end.saturating_sub(wall_start),
                worker_id: worker_id as u32,
                task_id,
                spawn_loc: spawn_loc.clone(),
                cpu_sample_count: cpu_count,
                sched_sample_count: sched_count,
                host: parsed_host.clone(),
                service: parsed_service.clone(),
                date: parsed_date.clone(),
            }
        })
        .collect();

    Ok((samples, stacks_dict.into_iter().collect(), resolved_polls))
}

/// Find the poll enclosing a sample on a given worker. Returns the poll
/// duration in ns and spawn location if found, and increments the poll's sample count.
fn find_enclosing_poll(
    polls: &[(u64, u64, u64, u64, Option<String>)],
    polls_by_worker: &FxHashMap<u64, Vec<usize>>,
    worker_id: u64,
    sample_ts: u64,
    poll_sample_counts: &mut [(u32, u32)],
    source: u8,
) -> (Option<u64>, Option<String>) {
    let Some(indices) = polls_by_worker.get(&worker_id) else {
        return (None, None);
    };
    // Binary search for the last poll starting <= sample_ts.
    let pos = indices.partition_point(|&i| polls[i].0 <= sample_ts);
    if pos == 0 {
        return (None, None);
    }
    let poll_idx = indices[pos - 1];
    let (start, end, _, _, ref spawn_loc) = polls[poll_idx];
    // Check sample is within [start, end).
    if sample_ts >= start && sample_ts < end {
        let duration = end - start;
        if source == 0 {
            poll_sample_counts[poll_idx].0 += 1;
        } else {
            poll_sample_counts[poll_idx].1 += 1;
        }
        (Some(duration), spawn_loc.clone())
    } else {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_source_key_is_date_anchored() {
        // Unprefixed: {date}/{HHMM}/{service}/{host}/{boot}/{file}
        assert_eq!(
            parse_source_key("2026-06-19/1300/svc/host-a/boot/0-0.bin.gz"),
            (
                "2026-06-19".to_string(),
                "svc".to_string(),
                "host-a".to_string()
            )
        );
        // Prefixed with `traces/` — a fixed-index parse would return
        // ("traces", "1300", "svc"); the date anchor keeps it correct.
        assert_eq!(
            parse_source_key("traces/2026-06-19/1300/svc/host-a/boot/0-0.bin.gz"),
            (
                "2026-06-19".to_string(),
                "svc".to_string(),
                "host-a".to_string()
            )
        );
        // s3:// URI with a prefix is stripped and still date-anchored.
        assert_eq!(
            parse_source_key("s3://bucket/traces/2026-06-19/1300/svc/host-a/boot/0-0.bin.gz"),
            (
                "2026-06-19".to_string(),
                "svc".to_string(),
                "host-a".to_string()
            )
        );
        // No date component — legacy fixed-index fallback.
        assert_eq!(
            parse_source_key("a/b/c/d"),
            ("a".to_string(), "c".to_string(), "d".to_string())
        );
    }

    fn load_demo_trace() -> Vec<u8> {
        let data =
            std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/demo-trace.bin")).unwrap();
        let mut dec = flate2::read::GzDecoder::new(data.as_slice());
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut buf).unwrap();
        buf
    }

    #[test]
    fn test_stack_id_deterministic() {
        let decompressed = load_demo_trace();
        let (s1, d1, _) = decode_samples(&decompressed, "test").unwrap();
        let (s2, d2, _) = decode_samples(&decompressed, "test").unwrap();
        assert_eq!(s1.len(), s2.len());
        assert_eq!(d1.len(), d2.len());
        for (a, b) in s1.iter().zip(s2.iter()) {
            assert_eq!(a.stack_id, b.stack_id);
            assert_eq!(a.timestamp_ns, b.timestamp_ns);
            assert_eq!(a.worker_id, b.worker_id);
            assert_eq!(a.source, b.source);
        }
    }

    #[test]
    fn test_decode_demo_trace() {
        let decompressed = load_demo_trace();
        let (samples, stacks, polls) = decode_samples(&decompressed, "demo-trace.bin").unwrap();
        assert!(!samples.is_empty(), "expected CPU samples in demo trace");
        assert!(!stacks.is_empty(), "expected stacks in dictionary");
        for sample in &samples {
            assert!(stacks.contains_key(&sample.stack_id));
        }
        // Verify timestamps are wall-clock (Unix epoch nanoseconds), not monotonic.
        let min_ts = samples.iter().map(|s| s.timestamp_ns).min().unwrap();
        assert!(
            min_ts > 1_500_000_000_000_000_000,
            "timestamps should be wall-clock epoch ns, got {min_ts}"
        );
        // Verify poll spans were reconstructed.
        assert!(!polls.is_empty(), "expected poll spans in demo trace");
        // Some samples should be attributed to a poll.
        let attributed = samples
            .iter()
            .filter(|s| s.poll_duration_ns.is_some())
            .count();
        assert!(
            attributed > 0,
            "expected some samples attributed to a poll, got 0"
        );
        eprintln!(
            "decoded {} samples ({} poll-attributed), {} unique stacks, {} polls",
            samples.len(),
            attributed,
            stacks.len(),
            polls.len(),
        );
    }

    #[test]
    fn test_worker_id_inferred_from_park_unpark() {
        // Verify that samples on a tid bound to a worker get an attributed
        // worker_id (Some), and unattributable samples get None.
        let decompressed = load_demo_trace();
        let (samples, _, _) = decode_samples(&decompressed, "test").unwrap();
        let worker_samples = samples.iter().filter(|s| s.worker_id.is_some()).count();
        // The demo trace has worker threads; we should infer at least some worker samples.
        assert!(
            worker_samples > 0,
            "expected some samples attributed to a worker via tid correlation"
        );
        eprintln!(
            "{} of {} samples attributed to a worker (worker_id = Some)",
            worker_samples,
            samples.len()
        );
    }

    #[test]
    fn test_decode_real_trace() {
        let path = "/tmp/dial9-ingest-test/2026-06-19/1459/shale/ip-10-2-123-116.us-west-2.compute.internal/kxgw-1/1781881195-9725.bin.gz";
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping: real trace not available");
            return;
        }
        let compressed = std::fs::read(path).unwrap();
        let decompressed = {
            use std::io::Read;
            let mut dec = flate2::read::GzDecoder::new(compressed.as_slice());
            let mut buf = Vec::new();
            dec.read_to_end(&mut buf).unwrap();
            buf
        };
        let (samples, stacks, _polls) = decode_samples(&decompressed, path).unwrap();
        eprintln!(
            "decoded {} samples, {} unique stacks",
            samples.len(),
            stacks.len()
        );
        assert!(!samples.is_empty(), "expected CPU samples in real trace");
        assert!(!stacks.is_empty(), "expected stacks in dictionary");
    }
}

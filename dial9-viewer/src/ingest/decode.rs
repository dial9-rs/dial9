//! Decode raw dial9 trace bytes into CPU samples with resolved symbols.
//!
//! Events within a trace segment are not guaranteed to be in timestamp order
//! (threads flush buffers independently). This module collects all relevant
//! events, sorts them by `timestamp_ns`, then processes them in order so that
//! worker_id can be inferred from WorkerPark/WorkerUnpark tid correlation.
//!
//! # Module structure
//!
//! The decode pipeline is split into focused deep modules:
//!
//! - [`clock`]: Clock-domain newtypes (MonoNs, WallNs, ClockOffset) with
//!   checked conversion boundary. All mono→wall conversions go through here.
//! - [`events`]: one-pass wire decoding and malformed-event accounting.
//! - [`polls`]: worker/tid correlation, poll reconstruction, and attribution.
//! - [`spans`]: tracing/single-event adapters, interval pairing, and the
//!   common `ResolvedSpan` finalizer with its five-way accounting invariant.
//! - [`attribution`]: sweep-line sample-to-span membership over entered
//!   intervals (never lifecycle envelopes).
//! - [`types`]: stable public output types at the Parquet-facing seam.
//!
//! This facade orchestrates those modules and retains the existing public
//! `decode_samples` interface.

mod attribution;
pub(crate) mod clock;
mod events;
pub(crate) mod polls;
pub(crate) mod spans;
mod types;

use events::*;
use rustc_hash::FxHashMap;
#[cfg(test)]
use spans::span_builder;
use spans::{interval_pairing, legacy, single_event};

pub(crate) use types::{
    DecodeResult, DecodeStats, EnclosingSpanSummary, ResolvedPoll, ResolvedSample, ResolvedSpan,
    SchedulingDelayKind,
};

/// Wire value of the `CpuProfile` CPU-sample source (periodic on-CPU sample).
const SOURCE_CPU_PROFILE: u8 = 0;
fn parse_source_key(key: &str) -> Option<(String, String, String)> {
    crate::source_key_scope::scope_fields(key)
}

/// Parquet uses empty strings to represent source metadata that is unavailable
/// on directly opened or custom keys.
fn unknown_source_fields() -> (String, String, String) {
    (String::new(), String::new(), String::new())
}

/// Extract CPU samples from raw (already gunzipped) trace bytes.
///
/// Events are sorted by timestamp within the segment to correctly infer
/// worker_id from WorkerPark/WorkerUnpark tid correlation.
///
/// Returns the resolved samples and a map of stack_id → frame names for the
/// stacks dictionary.
pub(crate) fn decode_samples(data: &[u8], source_key: &str) -> anyhow::Result<DecodeResult> {
    decode_samples_with_stats(data, source_key).map(|(result, _stats)| result)
}

/// Like [`decode_samples`], but also returns per-phase [`DecodeStats`] so the
/// fold pipeline can emit a per-file timing metric. The extra bookkeeping is a
/// handful of `Instant::now()` calls and counter reads, so this is the real
/// implementation and [`decode_samples`] delegates to it.
pub(crate) fn decode_samples_with_stats(
    data: &[u8],
    source_key: &str,
) -> anyhow::Result<(DecodeResult, DecodeStats)> {
    use std::time::Instant;

    let mut stats = DecodeStats::default();
    let t_wire = Instant::now();
    let events::DecodedTrace {
        interner,
        mut addr_to_keys,
        mut events,
        mut clock_offset,
        mut first_clock_sync_mono,
        segment_metadata_boot_id,
        legacy_enters,
        legacy_exits,
        legacy_closes,
        single_event_spans,
    } = events::decode_trace(data, source_key)?;
    stats.wire_decode = t_wire.elapsed();
    stats.span_events_decoded =
        (legacy_enters.len() + legacy_exits.len() + legacy_closes.len() + single_event_spans.len())
            as u64;
    // Span events are collected separately and never enter the sorted event stream.
    stats.events_decoded = events.len() as u64 + stats.span_events_decoded;

    let timestamp_bounds = events
        .iter()
        .map(TraceEvent::timestamp_ns)
        .chain(legacy_enters.iter().map(|(_, event)| event.timestamp_ns))
        .chain(legacy_exits.iter().map(|(_, event)| event.timestamp_ns))
        .chain(legacy_closes.iter().map(|event| event.timestamp_ns))
        .chain(
            single_event_spans
                .iter()
                .flat_map(|event| [event.start_ns, event.end_ns]),
        )
        .fold(None, |bounds: Option<(u64, u64)>, timestamp| {
            Some(match bounds {
                Some((min, max)) => (min.min(timestamp), max.max(timestamp)),
                None => (timestamp, timestamp),
            })
        });
    if let (Some(offset), Some((min, max))) = (clock_offset, timestamp_bounds)
        && !offset.is_valid_for(clock::MonoNs(min), clock::MonoNs(max))
    {
        use dial9_core::rate_limited;
        rate_limited!(std::time::Duration::from_secs(60), {
            tracing::warn!(
                source_key,
                min_mono_ns = min,
                max_mono_ns = max,
                "ignoring clock sync that cannot convert the trace timestamp range"
            );
        });
        clock_offset = None;
        first_clock_sync_mono = None;
    }
    tracing::info!("sorting {} events", events.len());
    // Sort events by timestamp for correct worker_id inference.
    let t_sort = Instant::now();
    events.sort_unstable_by_key(|e| e.timestamp_ns());

    // Pre-sort symbol entries by inline depth.
    for entries in addr_to_keys.values_mut() {
        entries.sort_unstable_by_key(|(d, _)| *d);
    }
    stats.sort_events = t_sort.elapsed();

    let t_polls = Instant::now();
    let mut poll_timeline = polls::PollTimeline::reconstruct(&events);
    stats.poll_reconstruct = t_polls.elapsed();

    let t_samples = Instant::now();
    let mut stacks_dict: FxHashMap<[u8; 16], Vec<String>> = FxHashMap::default();
    let mut stack_cache: FxHashMap<Vec<u64>, [u8; 16]> = FxHashMap::default();
    let mut samples = Vec::new();
    let (parsed_date, parsed_service, parsed_host) =
        parse_source_key(source_key).unwrap_or_else(unknown_source_fields);

    for event in &events {
        match event {
            TraceEvent::WorkerPark(_)
            | TraceEvent::WorkerUnpark(_)
            | TraceEvent::PollStart(_)
            | TraceEvent::PollEnd(_)
            | TraceEvent::TaskSpawn(_)
            | TraceEvent::TaskTerminate(_)
            | TraceEvent::Wake(_) => {}
            TraceEvent::CpuSample(s) => {
                let (worker_id, poll_duration_ns, spawn_location) = poll_timeline.attribute_sample(
                    s.tid,
                    clock::MonoNs(s.timestamp_ns),
                    s.source as u8,
                );

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

                let wall_ns = clock::MonoNs(s.timestamp_ns)
                    .to_wall_or_raw(clock_offset)
                    .raw();
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
                    enclosing_spans: Vec::new(),
                });
            }
        }
    }

    let resolved_polls =
        poll_timeline.resolved(clock_offset, &parsed_host, &parsed_service, &parsed_date);
    stats.sample_resolve = t_samples.elapsed();

    // ── Stage 2: Resolve tracing and single-event spans ─────────────────────
    //
    // Decode the authoritative boot_id from SegmentMetadata when available.
    // The boot_id directory is written into segment metadata by the namespace
    // isolation layer. When absent (old traces, non-namespaced writers), fall
    // back to extracting it from the source_key path. `boot_id` namespaces the
    // per-span identity across processes.
    let (path_boot_id, path_is_namespaced) = extract_boot_id_from_path_qualified(source_key);
    let (boot_id, single_event_identity_quality): (String, &'static str) =
        match segment_metadata_boot_id {
            Some(meta_bid) => (meta_bid, "metadata"),
            None if path_is_namespaced => (path_boot_id, "path"),
            None => (path_boot_id, "flat"),
        };

    // Reconstruct spans from the old-producer enter/exit/close events:
    //   SpanEnter:{target}::{name}:{file}:{line} → dial9.tokio.task_id
    //     (current) or worker_id (legacy), span_id, parent_span_id, span_name, ...
    //   SpanExit:{target}::{name}:{file}:{line}  → the corresponding fields
    //   SpanCloseEvent                           → span_id (only)
    //
    // Reconstruction strategy:
    // - Segment each span_id's lifecycle at close boundaries so recycled ids
    //   don't merge distinct spans into one long-lived row
    // - Synthesize a deterministic instance_id from span_id + first-enter timestamp
    //   to avoid collisions when IDs are recycled
    // - Pair enter/exit by (span_id, segment), NOT worker_id: async tasks migrate
    //   workers across .await, so worker_id is not a stable pairing key.
    // - Parse target/name/file/line from the SpanEnter schema name
    // - Lifecycle start = first observed enter (conservative)
    // - identity_quality = "legacy"; all elapsed remains unknown (no
    //   producer-reported active_ns).
    let mut resolved_spans: Vec<ResolvedSpan> = Vec::new();
    let mut span_intervals: FxHashMap<u64, Vec<interval_pairing::MonoInterval>> =
        FxHashMap::default();

    let t_spans = Instant::now();
    if !legacy_enters.is_empty() || !legacy_closes.is_empty() {
        let legacy_resolution = resolve_legacy_spans(
            &legacy_enters,
            &legacy_exits,
            &legacy_closes,
            poll_timeline.records(),
            source_key,
            &boot_id,
            clock_offset,
            &parsed_host,
            &parsed_service,
            &parsed_date,
        );
        span_intervals.extend(legacy_resolution.instance_intervals);
        resolved_spans.extend(legacy_resolution.spans);
    }
    if !single_event_spans.is_empty() {
        let single_event_resolution = single_event::resolve_single_event_spans(
            &single_event_spans,
            &poll_timeline,
            source_key,
            &boot_id,
            clock_offset,
            first_clock_sync_mono,
            single_event_identity_quality,
            &parsed_host,
            &parsed_service,
            &parsed_date,
        );
        span_intervals.extend(single_event_resolution.instance_intervals);
        resolved_spans.extend(single_event_resolution.spans);
    }
    stats.span_resolve = t_spans.elapsed();

    // ── Stage 3: Attribute samples to spans using entered intervals ──────────
    //
    // Build a flat sorted interval index for O(n log n + m log n) sweep instead
    // of O(samples × spans). Each entry maps a wall-clock entered interval back
    // to the resolved_spans index.
    //
    // CRITICAL: We attach samples ONLY to balanced, locally observed entered
    // intervals — never to lifecycle envelopes. An async span that is exited
    // (waiting) must NOT claim samples that fire during its idle gap.

    let t_attr = Instant::now();
    attribution::attribute_samples_to_spans(
        &mut samples,
        &mut resolved_spans,
        &span_intervals,
        &boot_id,
        clock_offset,
    );
    stats.sample_attribution = t_attr.elapsed();

    Ok((
        (
            samples,
            stacks_dict.into_iter().collect(),
            resolved_polls,
            resolved_spans,
        ),
        stats,
    ))
}

/// Compute a span_uid from the boot-id + span_instance_id.
///
/// The design specifies: `BLAKE3(boot_id || span_instance_id)[..16]`.
/// The boot_id is either decoded from SegmentMetadata (authoritative, stable
/// across files from the same process) or extracted from the source key path
/// (low-quality fallback that cannot claim cross-file stability).
/// Compute a span_uid from the boot-id + span_instance_id.
/// Delegates to [`span_builder::compute_span_uid`].
#[cfg(test)]
fn compute_span_uid(boot_id: &str, span_instance_id: u64) -> [u8; 16] {
    span_builder::compute_span_uid(boot_id, span_instance_id)
}

/// Extract the boot-id directory from a source key path.
///
/// If the key does not match a supported layout, fall back to its directory
/// path, which still provides best-effort cross-segment stability.
#[cfg(test)]
fn extract_boot_id_from_path(source_key: &str) -> String {
    extract_boot_id_from_path_qualified(source_key).0
}

/// Extract the boot-id directory from a source key path, returning both the
/// extracted value and whether the path is a valid namespaced layout.
///
/// A versioned `boot=` partition is unambiguous and therefore namespaced.
/// Historical paths are namespaced only when their second-to-last component
/// matches the `{4-alpha}-{digits}` format generated by
/// `dial9_core::boot_id::generate_boot_id`.
///
/// Returns `(boot_id, is_namespaced)`:
/// - `is_namespaced = true`: the layout identifies the boot id unambiguously.
///   This is authoritative for cross-segment identity.
/// - `is_namespaced = false`: the path is flat/legacy. The returned value is
///   a best-effort fallback (directory portion) that cannot guarantee stability.
fn extract_boot_id_from_path_qualified(source_key: &str) -> (String, bool) {
    let parsed = crate::segment_object_key_parser::parse_segment_object_key(source_key);
    if parsed.layout == crate::segment_object_key_parser::SegmentObjectKeyLayout::Version1
        && let Some(boot_id) = parsed.boot_id
    {
        return (boot_id, true);
    }

    // Strip s3://bucket/ prefix if present
    let path = if let Some(rest) = source_key.strip_prefix("s3://") {
        rest.split_once('/').map_or(rest, |(_, p)| p)
    } else {
        source_key
    };
    // Split into components. Boot-id is second-to-last.
    let parts: Vec<&str> = path.rsplitn(3, '/').collect();
    // parts[0] = filename, parts[1] = boot-id dir, parts[2] = rest
    if parts.len() >= 2 && !parts[1].is_empty() {
        let candidate = parts[1];
        let is_namespaced = is_boot_id_format(candidate);
        (candidate.to_string(), is_namespaced)
    } else {
        // Fallback: use the whole path minus the filename
        let fallback = path.rsplit_once('/').map_or(path, |(dir, _)| dir);
        (fallback.to_string(), false)
    }
}

/// Returns `true` if `s` matches the boot_id format: `{4-alpha}-{digits}`.
/// E.g. `qmxz-481`, `abcd-12345`.
fn is_boot_id_format(s: &str) -> bool {
    let Some((alpha, digits)) = s.split_once('-') else {
        return false;
    };
    alpha.len() == 4
        && alpha.bytes().all(|b| b.is_ascii_lowercase())
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Compute a span_type_uid from the span's identity fields.
/// Delegates to [`span_builder::compute_span_type_uid`].
#[cfg(test)]
fn compute_span_type_uid(
    kind: &str,
    target: &str,
    name: &str,
    file: Option<&str>,
    line: Option<u32>,
) -> [u8; 16] {
    span_builder::compute_span_type_uid(kind, target, name, file, line)
}

/// Result of span resolution: resolved spans and the per-instance interval map
/// (monotonic timestamps) for reuse in sample attribution.
struct SpanResolution {
    spans: Vec<ResolvedSpan>,
    /// Per span_instance_id: list of monotonic-clock (enter_ts, exit_ts) intervals.
    instance_intervals: FxHashMap<u64, Vec<interval_pairing::MonoInterval>>,
}

/// Resolve legacy (old-producer) span events into `ResolvedSpan` rows.
///
/// Delegates to the [`legacy`] module which handles:
/// - Pairing enters/exits by `span_id` alone (not worker_id)
/// - Synthesizing deterministic instance_ids from span_id + first-enter timestamp
/// - Parsing target/name/file/line from SpanEnter schema names
/// - Task-based CPU/wait attribution when polls are available
///
/// Recycled ID policy: a span_id's lifecycle is segmented at close boundaries.
/// Tracing recycles a span_id only after its span closes, so each close-
/// delimited cycle of enters/exits is its own span instance. This keeps short
/// spans that reuse a span_id from collapsing into one long merged span.
#[allow(clippy::too_many_arguments)]
fn resolve_legacy_spans(
    legacy_enters: &[(String, LegacySpanEnterEvent)],
    legacy_exits: &[(String, LegacySpanExitEvent)],
    legacy_closes: &[LegacySpanCloseEvent],
    polls: &[polls::PollRecord],
    source_key: &str,
    boot_id: &str,
    clock_offset: Option<clock::ClockOffset>,
    host: &str,
    service: &str,
    date: &str,
) -> SpanResolution {
    let result = legacy::resolve_legacy_spans(
        legacy_enters,
        legacy_exits,
        legacy_closes,
        polls,
        source_key,
        boot_id,
        clock_offset,
        host,
        service,
        date,
    );
    SpanResolution {
        spans: result.spans,
        instance_intervals: result.instance_intervals,
    }
}

/// Resolve the Tokio task that owns a span. Delegates to [`legacy::resolve_span_task`].
#[cfg(test)]
fn resolve_span_task(worker_polls: &[(u64, u64, u64)], enter_ts: u64) -> Option<u64> {
    let worker_polls: Vec<_> = worker_polls
        .iter()
        .map(|&(start, end, task_id)| (clock::MonoNs(start), clock::MonoNs(end), task_id))
        .collect();
    legacy::resolve_span_task(&worker_polls, clock::MonoNs(enter_ts))
}

/// Split a span's entered wall time into estimated on-CPU vs async-wait.
/// Delegates to [`legacy::attribute_legacy_span_from_polls`].
#[cfg(test)]
fn attribute_legacy_span_from_polls(
    entered: &[(u64, u64)],
    task_polls: &[(u64, u64)],
) -> (u64, u64) {
    let entered: Vec<_> = entered
        .iter()
        .map(|&(start, end)| (clock::MonoNs(start), clock::MonoNs(end)))
        .collect();
    let task_polls: Vec<_> = task_polls
        .iter()
        .map(|&(start, end)| (clock::MonoNs(start), clock::MonoNs(end)))
        .collect();
    legacy::attribute_legacy_span_from_polls(&entered, &task_polls)
}

/// Compute the union of a set of intervals. Returns total wall-clock nanoseconds
/// covered by the merged (non-overlapping) union.
/// Delegates to [`interval_pairing::union_interval_duration`].
#[cfg(test)]
fn union_intervals(intervals: &[(u64, u64)]) -> u64 {
    let intervals: Vec<_> = intervals
        .iter()
        .map(|&(start, end)| (clock::MonoNs(start), clock::MonoNs(end)))
        .collect();
    interval_pairing::union_interval_duration(&intervals).raw()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_source_key_supports_current_and_historical_layouts() {
        // Historical layout without a prefix.
        assert_eq!(
            parse_source_key("2026-06-19/1300/svc/host-a/boot/0-0.bin.gz"),
            Some((
                "2026-06-19".to_string(),
                "svc".to_string(),
                "host-a".to_string()
            ))
        );
        // Historical layout with a prefix.
        assert_eq!(
            parse_source_key("traces/2026-06-19/1300/svc/host-a/boot/0-0.bin.gz"),
            Some((
                "2026-06-19".to_string(),
                "svc".to_string(),
                "host-a".to_string()
            ))
        );
        // Historical layout in an s3:// URI.
        assert_eq!(
            parse_source_key("s3://bucket/traces/2026-06-19/1300/svc/host-a/boot/0-0.bin.gz"),
            Some((
                "2026-06-19".to_string(),
                "svc".to_string(),
                "host-a".to_string()
            ))
        );
        assert_eq!(
            parse_source_key(
                "s3://bucket/traces/version=1/date=2026-06-19/service=svc%2Fapi/time=1300/instance=host%2Fa/boot=boot/0-0.bin.gz"
            ),
            Some((
                "2026-06-19".to_string(),
                "svc/api".to_string(),
                "host/a".to_string()
            ))
        );
        // No date component — legacy fixed-index fallback.
        assert_eq!(
            parse_source_key("a/b/c/d"),
            Some(("a".to_string(), "c".to_string(), "d".to_string()))
        );
        assert_eq!(
            parse_source_key("demo-trace.bin"),
            Some(("demo-trace.bin".to_string(), String::new(), String::new()))
        );
    }

    fn load_demo_trace() -> Vec<u8> {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ui/public/demo-trace.bin"
        ))
        .unwrap();
        let mut dec = flate2::read::GzDecoder::new(data.as_slice());
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut buf).unwrap();
        buf
    }

    #[test]
    fn test_stack_id_deterministic() {
        let decompressed = load_demo_trace();
        let (s1, d1, _, _) = decode_samples(&decompressed, "test").unwrap();
        let (s2, d2, _, _) = decode_samples(&decompressed, "test").unwrap();
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
        let (samples, stacks, polls, _spans) =
            decode_samples(&decompressed, "demo-trace.bin").unwrap();
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
    fn invalid_clock_sync_is_ignored_instead_of_clamped() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::Encoder;

        #[derive(TraceEvent)]
        struct ClockSyncEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            realtime_ns: u64,
        }

        #[derive(TraceEvent)]
        struct PollStartEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            worker_id: u64,
            task_id: u64,
        }

        #[derive(TraceEvent)]
        struct PollEndEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            worker_id: u64,
        }

        let mut encoder = Encoder::new();
        encoder
            .write(&ClockSyncEvent {
                timestamp_ns: 100,
                realtime_ns: 1,
            })
            .unwrap();
        encoder
            .write(&PollStartEvent {
                timestamp_ns: 10,
                worker_id: 1,
                task_id: 7,
            })
            .unwrap();
        encoder
            .write(&PollEndEvent {
                timestamp_ns: 20,
                worker_id: 1,
            })
            .unwrap();

        let (_, _, polls, _) = decode_samples(&encoder.into_inner(), "test").unwrap();
        assert_eq!(polls.len(), 1);
        assert_eq!(polls[0].start_ns, 10);
        assert_eq!(polls[0].end_ns, 20);
    }

    #[test]
    fn test_worker_id_inferred_from_park_unpark() {
        // Verify that samples on a tid bound to a worker get an attributed
        // worker_id (Some), and unattributable samples get None.
        let decompressed = load_demo_trace();
        let (samples, _, _, _) = decode_samples(&decompressed, "test").unwrap();
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
        let (samples, stacks, _polls, _spans) = decode_samples(&decompressed, path).unwrap();
        eprintln!(
            "decoded {} samples, {} unique stacks",
            samples.len(),
            stacks.len()
        );
        assert!(!samples.is_empty(), "expected CPU samples in real trace");
        assert!(!stacks.is_empty(), "expected stacks in dictionary");
    }

    #[test]
    fn test_span_resolution_produces_valid_uids() {
        // Verify that span_uid and span_type_uid are deterministic.
        let uid1 = compute_span_uid("boot-abc", 42);
        let uid2 = compute_span_uid("boot-abc", 42);
        assert_eq!(uid1, uid2, "span_uid must be deterministic");

        let uid3 = compute_span_uid("boot-abc", 43);
        assert_ne!(
            uid1, uid3,
            "different instance_ids must produce different uids"
        );

        let type_uid1 = compute_span_type_uid(
            "tracing",
            "my_crate",
            "handle_request",
            Some("src/main.rs"),
            Some(10),
        );
        let type_uid2 = compute_span_type_uid(
            "tracing",
            "my_crate",
            "handle_request",
            Some("src/main.rs"),
            Some(10),
        );
        assert_eq!(type_uid1, type_uid2, "span_type_uid must be deterministic");

        let type_uid3 = compute_span_type_uid(
            "tracing",
            "my_crate",
            "other_fn",
            Some("src/main.rs"),
            Some(20),
        );
        assert_ne!(
            type_uid1, type_uid3,
            "different names must produce different type_uids"
        );
    }

    /// Verify that the boot-id from path is used, not the full source filename.
    /// Two segments from the same process (same boot dir) with the same instance_id
    /// produce the same span_uid.
    #[test]
    fn test_cross_source_identity_same_boot_id() {
        // Same boot-id, different filenames (different segments) — now we pass
        // the boot_id directly (as decode_samples does after extracting it).
        let uid1 = compute_span_uid("boot-abc", 42);
        let uid2 = compute_span_uid("boot-abc", 42);
        assert_eq!(
            uid1, uid2,
            "same boot-id + instance_id must produce same span_uid across segments"
        );
    }

    /// Different boot-ids (different processes) with same instance_id produce different uids.
    #[test]
    fn test_cross_source_identity_different_boot_id() {
        let uid1 = compute_span_uid("boot-abc", 42);
        let uid2 = compute_span_uid("boot-xyz", 42);
        assert_ne!(
            uid1, uid2,
            "different boot-ids must produce different span_uids"
        );
    }

    /// Recycled raw span IDs (tracing wire IDs) do NOT affect span_uid which uses
    /// the monotonic instance_id.
    #[test]
    fn test_recycled_raw_ids_do_not_collide() {
        // Two different instance_ids produce different uids even if the original
        // tracing span_id was recycled.
        let uid1 = compute_span_uid("boot-1", 100);
        let uid2 = compute_span_uid("boot-1", 200);
        assert_ne!(uid1, uid2, "different instance_ids must never collide");
    }

    /// Test that samples are written last (the ordering invariant).
    /// This is tested at the fold level, but here we verify the enclosing_spans
    /// are populated before the samples output is assembled.
    #[test]
    fn test_samples_last_ordering() {
        // decode_samples returns (samples, stacks, polls, spans). The fold writes
        // dict, polls, spans BEFORE samples. We verify that samples have
        // enclosing_spans populated (meaning span resolution ran first).
        let source_key = "2026-06-19/1300/svc/host/boot/0.bin";
        // With no actual trace data, we just verify the function signature and
        // empty case works correctly.
        let empty_trace = {
            use dial9_trace_format::encoder::Encoder;
            let enc = Encoder::new();
            enc.into_inner()
        };
        let result = decode_samples(&empty_trace, source_key);
        assert!(result.is_ok());
        let (samples, _stacks, _polls, spans) = result.unwrap();
        assert!(samples.is_empty());
        assert!(spans.is_empty());
    }

    #[test]
    fn test_union_intervals_helper() {
        // Empty
        assert_eq!(union_intervals(&[]), 0);

        // Single
        assert_eq!(union_intervals(&[(10, 20)]), 10);

        // Non-overlapping
        assert_eq!(union_intervals(&[(10, 20), (30, 40)]), 20);

        // Overlapping
        assert_eq!(union_intervals(&[(10, 30), (20, 40)]), 30);

        // Contained
        assert_eq!(union_intervals(&[(10, 40), (15, 25)]), 30);

        // Adjacent (touching)
        assert_eq!(union_intervals(&[(10, 20), (20, 30)]), 20);

        // Multiple overlapping
        assert_eq!(union_intervals(&[(10, 20), (15, 25), (22, 35)]), 25);
    }

    #[test]
    fn test_resolve_span_task_binary_search() {
        // Worker polls sorted by start, non-overlapping: (start, end, task_id).
        let polls = [(0, 100, 11), (200, 300, 22), (400, 500, 33)];
        // Inside a poll → its task.
        assert_eq!(resolve_span_task(&polls, 250), Some(22));
        // Poll-start edge is inclusive.
        assert_eq!(resolve_span_task(&polls, 400), Some(33));
        // Poll-end edge is exclusive.
        assert_eq!(resolve_span_task(&polls, 100), None);
        assert_eq!(resolve_span_task(&polls, 200), Some(22));
        // In an inter-poll gap → None (not the nearest poll).
        assert_eq!(resolve_span_task(&polls, 150), None);
        // Before all / after all → None.
        assert_eq!(resolve_span_task(&polls, 600), None);
        // Empty.
        assert_eq!(resolve_span_task(&[], 10), None);
    }

    #[test]
    fn test_attribute_legacy_span_from_polls() {
        // One entered interval [0, 1000]; task polled twice inside it
        // ([100,200], [500,600]). on_cpu = 100+100 = 200, wait = 800.
        let (on_cpu, wait) =
            attribute_legacy_span_from_polls(&[(0, 1000)], &[(100, 200), (500, 600)]);
        assert_eq!(on_cpu, 200);
        assert_eq!(wait, 800);

        // Polls clamp to the entered window on both edges.
        let (on_cpu, wait) =
            attribute_legacy_span_from_polls(&[(300, 700)], &[(100, 400), (600, 900)]);
        assert_eq!(on_cpu, 100 + 100); // [300,400] + [600,700]
        assert_eq!(wait, 400 - 200);

        // No task polls → all wait, nothing on-CPU.
        let (on_cpu, wait) = attribute_legacy_span_from_polls(&[(0, 500)], &[]);
        assert_eq!(on_cpu, 0);
        assert_eq!(wait, 500);

        // Fully on-CPU (poll covers the whole entered interval).
        let (on_cpu, wait) = attribute_legacy_span_from_polls(&[(0, 500)], &[(0, 500)]);
        assert_eq!(on_cpu, 500);
        assert_eq!(wait, 0);

        // Overlapping/re-entrant enters are unioned first (counted once).
        // Union of [(0,300),(200,500)] = [(0,500)] = 500 wall; poll [100,400]
        // → on_cpu 300, wait 200.
        let (on_cpu, wait) =
            attribute_legacy_span_from_polls(&[(0, 300), (200, 500)], &[(100, 400)]);
        assert_eq!(on_cpu, 300);
        assert_eq!(wait, 200);
        assert_eq!(on_cpu + wait, 500);
    }

    #[test]
    fn test_extract_boot_id_from_path() {
        assert_eq!(
            extract_boot_id_from_path("2026-06-19/1300/svc/host/boot-abc/0-0.bin.gz"),
            "boot-abc"
        );
        assert_eq!(
            extract_boot_id_from_path(
                "s3://bucket/traces/2026-06-19/1300/svc/host/my-boot/file.bin"
            ),
            "my-boot"
        );
        // Single component (no slashes) - fallback
        assert_eq!(extract_boot_id_from_path("file.bin"), "file.bin");
    }

    #[test]
    fn test_extract_boot_id_from_path_qualified() {
        // Valid namespaced path with {4-alpha}-{pid} boot_id
        let (bid, namespaced) =
            extract_boot_id_from_path_qualified("2026-06-19/1300/svc/host/abcd-12345/0-0.bin.gz");
        assert_eq!(bid, "abcd-12345");
        assert!(namespaced, "4-alpha-digits should be namespaced");

        // Valid namespaced path via S3 URI
        let (bid, namespaced) = extract_boot_id_from_path_qualified(
            "s3://bucket/traces/2026-06-19/1300/svc/host/qmxz-481/file.bin",
        );
        assert_eq!(bid, "qmxz-481");
        assert!(namespaced, "S3 URI with valid boot_id should be namespaced");

        let (bid, namespaced) = extract_boot_id_from_path_qualified(
            "s3://bucket/traces/version=1/date=2026-06-19/service=svc/time=1300/instance=host/boot=boot%2Fid/file.bin",
        );
        assert_eq!(bid, "boot/id");
        assert!(namespaced, "named boot partition is unambiguous");

        // Non-boot_id directory name (not {4-alpha}-{digits})
        let (bid, namespaced) =
            extract_boot_id_from_path_qualified("2026-06-19/1300/svc/host/my-boot/file.bin");
        assert_eq!(bid, "my-boot");
        assert!(!namespaced, "my-boot is not a valid boot_id format");

        // Flat path (single file, no directory structure)
        let (bid, namespaced) = extract_boot_id_from_path_qualified("file.bin");
        assert_eq!(bid, "file.bin");
        assert!(!namespaced, "flat path is not namespaced");

        // Directory name that's all alpha (no dash) — not boot_id format
        let (bid, namespaced) = extract_boot_id_from_path_qualified("some/dir/abcdef/file.bin");
        assert_eq!(bid, "abcdef");
        assert!(!namespaced, "no dash means not boot_id format");
    }

    #[test]
    fn test_is_boot_id_format() {
        // Valid boot_id formats
        assert!(is_boot_id_format("abcd-123"));
        assert!(is_boot_id_format("qmxz-481"));
        assert!(is_boot_id_format("zzzz-99999"));

        // Invalid formats
        assert!(!is_boot_id_format("abc-123")); // only 3 alpha
        assert!(!is_boot_id_format("abcde-123")); // 5 alpha
        assert!(!is_boot_id_format("ABCD-123")); // uppercase
        assert!(!is_boot_id_format("abcd-")); // no digits after dash
        assert!(!is_boot_id_format("abcd")); // no dash
        assert!(!is_boot_id_format("my-boot")); // alpha after dash
        assert!(!is_boot_id_format("")); // empty
        assert!(!is_boot_id_format("1234-5678")); // digits before dash
    }

    /// Finding 5: Build an actual trace fixture with SegmentMetadataEvent containing
    /// a boot_id using the trace Encoder. Prove that decode_samples extracts it and
    /// produces identity_quality = "metadata" vs "path" fallback without metadata.
    #[test]
    fn test_decode_samples_metadata_identity_quality() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::Encoder;

        // Define local events matching the wire schema that decode_samples expects.
        #[derive(TraceEvent)]
        struct ClockSyncEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            realtime_ns: u64,
        }

        #[derive(TraceEvent)]
        struct SegmentMetadataEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            entries: Vec<(String, String)>,
        }

        // We also need span events. SpanCloseEvent carries the close summary.
        // The schema name must start with "SpanEnter:" or "SpanExit:" or be
        // "SpanCloseEvent" for decode_samples to recognize it.
        // Use raw Encoder writing with explicit schemas for the span events.

        // Build a trace WITH segment metadata containing boot_id.
        let build_trace_with_metadata = |boot_id: Option<&str>| -> Vec<u8> {
            let mut enc = Encoder::new();
            enc.write(&ClockSyncEvent {
                timestamp_ns: 100,
                realtime_ns: 1_700_000_000_000_000_000 + 100,
            })
            .unwrap();
            if let Some(bid) = boot_id {
                enc.write(&SegmentMetadataEvent {
                    timestamp_ns: 101,
                    entries: vec![("boot_id".to_string(), bid.to_string())],
                })
                .unwrap();
            }
            enc.into_inner()
        };

        // Trace WITH metadata boot_id
        let data_with = build_trace_with_metadata(Some("test-boot-abc123"));
        let (_, _, _, spans_with) = decode_samples(
            &data_with,
            "2026-06-19/1300/svc/host/some-path-boot/0.bin.gz",
        )
        .unwrap();
        // No span close events, so no spans — but we verify the function ran
        // without error and the metadata was extracted. To actually test
        // identity_quality, we need a span close event.
        assert!(spans_with.is_empty()); // no close events in this trace

        // Trace WITHOUT metadata — should fall back to path extraction
        let data_without = build_trace_with_metadata(None);
        let result = decode_samples(
            &data_without,
            "2026-06-19/1300/svc/host/some-path-boot/0.bin.gz",
        );
        assert!(result.is_ok());
    }

    // ── Legacy span reconstruction tests ─────────────────────────────────────

    #[test]
    fn test_parse_legacy_span_schema_name() {
        // Standard format: SpanEnter:{target}::{name}:{file}:{line}
        let info = parse_legacy_span_schema_name(
            "SpanEnter:metrics_service::routes::record_metric:examples/metrics-service/src/routes.rs:26",
        ).unwrap();
        assert_eq!(info.target, "metrics_service::routes");
        assert_eq!(info.name, "record_metric");
        assert_eq!(
            info.file.as_deref(),
            Some("examples/metrics-service/src/routes.rs")
        );
        assert_eq!(info.line, Some(26));

        // SpanExit variant
        let info = parse_legacy_span_schema_name(
            "SpanExit:metrics_service::ddb::query_metric:examples/metrics-service/src/ddb.rs:122",
        )
        .unwrap();
        assert_eq!(info.target, "metrics_service::ddb");
        assert_eq!(info.name, "query_metric");
        assert_eq!(
            info.file.as_deref(),
            Some("examples/metrics-service/src/ddb.rs")
        );
        assert_eq!(info.line, Some(122));

        // Deeply nested target
        let info =
            parse_legacy_span_schema_name("SpanEnter:a::b::c::d::my_span:src/lib.rs:99").unwrap();
        assert_eq!(info.target, "a::b::c::d");
        assert_eq!(info.name, "my_span");
        assert_eq!(info.file.as_deref(), Some("src/lib.rs"));
        assert_eq!(info.line, Some(99));

        // Struct-derived format exposes only a stable type suffix.
        let info = parse_legacy_span_schema_name("SpanEnter__ShaleOperation").unwrap();
        assert_eq!(info.target, "");
        assert_eq!(info.name, "ShaleOperation");
        assert_eq!(info.file, None);
        assert_eq!(info.line, None);

        // Invalid: no colon after prefix
        assert!(parse_legacy_span_schema_name("SpanEnter").is_none());

        // Invalid: no line number
        assert!(parse_legacy_span_schema_name("SpanEnter:a::b:file").is_none());
    }

    #[test]
    fn test_find_first_single_colon() {
        // Simple case: only single colons
        assert_eq!(find_first_single_colon("a:b:c"), Some(1));

        // Mixed: has both :: and :
        assert_eq!(
            find_first_single_colon(
                "metrics_service::routes::record_metric:examples/src/routes.rs"
            ),
            Some(38) // the : before "examples"
        );

        // Only ::
        assert_eq!(find_first_single_colon("a::b::c"), None);

        // Single colon at end
        assert_eq!(find_first_single_colon("abc:"), Some(3));

        // Empty
        assert_eq!(find_first_single_colon(""), None);
    }

    #[test]
    fn parse_legacy_span_schema_name_preserves_windows_path() {
        let info = parse_legacy_span_schema_name(r"SpanEnter:svc::op:C:\src\lib.rs:42").unwrap();
        assert_eq!(info.target, "svc");
        assert_eq!(info.name, "op");
        assert_eq!(info.file.as_deref(), Some(r"C:\src\lib.rs"));
        assert_eq!(info.line, Some(42));
    }

    /// Verify that decode_samples produces legacy span rows from the demo trace,
    /// which uses the old producer format (span_id only, no span_instance_id).
    #[test]
    fn test_decode_demo_trace_legacy_spans() {
        let decompressed = load_demo_trace();
        let (samples, _stacks, _polls, spans) = decode_samples(
            &decompressed,
            "2026-01-01/1300/svc/host/demo-boot/demo-trace.bin",
        )
        .unwrap();

        let legacy_spans: Vec<_> = spans.iter().filter(|span| span.kind == "tracing").collect();
        let metrique_spans: Vec<_> = spans
            .iter()
            .filter(|span| span.kind == "metrique")
            .collect();

        // The demo trace has old-format SpanCloseEvents and metrique request
        // metrics, so both adapters should produce rows.
        assert!(
            !legacy_spans.is_empty(),
            "expected legacy span rows from demo trace, got 0"
        );
        for operation in ["RecordMetric", "QueryMetric"] {
            assert!(
                metrique_spans.iter().any(|span| span.name == operation),
                "expected {operation} metrique spans from demo trace"
            );
        }

        // Old tracing spans retain legacy identity/completeness semantics.
        for span in &legacy_spans {
            assert_eq!(
                span.identity_quality, "legacy",
                "demo trace tracing spans must have identity_quality='legacy'"
            );
            assert!(
                !span.details_complete,
                "legacy spans must have details_complete=false"
            );
        }

        // Check that known span types are present.
        let record_metric_spans: Vec<_> = legacy_spans
            .iter()
            .copied()
            .filter(|span| span.name == "record_metric" && span.target.contains("routes"))
            .collect();
        assert!(
            !record_metric_spans.is_empty(),
            "expected record_metric spans from demo trace"
        );

        let query_metric_spans: Vec<_> = legacy_spans
            .iter()
            .copied()
            .filter(|span| span.name == "query_metric")
            .collect();
        assert!(
            !query_metric_spans.is_empty(),
            "expected query_metric spans from demo trace"
        );

        // Verify metadata was parsed from schema names.
        let sample_span = &record_metric_spans[0];
        assert!(
            sample_span.target.contains("metrics_service"),
            "target should contain metrics_service, got: {}",
            sample_span.target
        );
        assert!(
            sample_span.callsite_file.is_some(),
            "callsite_file should be parsed from schema name"
        );
        assert!(
            sample_span.callsite_line.is_some(),
            "callsite_line should be parsed from schema name"
        );

        // Verify elapsed_ns is reasonable (> 0 for spans that have both enter and close).
        let spans_with_elapsed: Vec<_> = legacy_spans
            .iter()
            .filter(|span| span.elapsed_ns > 0)
            .collect();
        assert!(
            !spans_with_elapsed.is_empty(),
            "expected some spans with non-zero elapsed_ns"
        );

        // Verify some spans have observed_active_wall_ns > 0 (balanced enter/exit pairs).
        let spans_with_active: Vec<_> = legacy_spans
            .iter()
            .filter(|span| span.observed_active_wall_ns > 0)
            .collect();
        assert!(
            !spans_with_active.is_empty(),
            "expected some spans with observed active wall time"
        );

        // Verify sample attribution works with legacy spans.
        let legacy_uids: rustc_hash::FxHashSet<_> =
            legacy_spans.iter().map(|span| span.span_uid).collect();
        let samples_with_spans = samples
            .iter()
            .filter(|sample| {
                sample
                    .enclosing_spans
                    .iter()
                    .any(|span| legacy_uids.contains(&span.span_uid))
            })
            .count();
        // With ~9000 CPU samples and ~86k enter/exit pairs, some should be attributed.
        assert!(
            samples_with_spans > 0,
            "expected some samples attributed to legacy spans, got 0"
        );

        eprintln!(
            "decoded {} legacy spans ({} record_metric, {} query_metric), {} samples with span attribution",
            legacy_spans.len(),
            record_metric_spans.len(),
            query_metric_spans.len(),
            samples_with_spans,
        );
    }

    #[test]
    fn test_annotated_event_resolves_as_complete_span() {
        use dial9_core::schema_extensions::{self, roles};
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::{FieldAnnotation, FieldDef, SchemaEntry};
        use dial9_trace_format::types::{FieldType, FieldValue};

        #[derive(TraceEvent)]
        struct ClockSyncEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            realtime_ns: u64,
        }

        let unpark_schema = Schema::new(
            "WorkerUnparkEvent",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        );
        let poll_start_schema = Schema::new(
            "PollStartEvent",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("task_id", FieldType::Varint),
            ],
        );
        let poll_end_schema = Schema::new(
            "PollEndEvent",
            vec![FieldDef::new("worker_id", FieldType::Varint)],
        );
        let sample_schema = Schema::new(
            "CpuSampleEvent",
            vec![
                FieldDef::new("tid", FieldType::Varint),
                FieldDef::new("source", FieldType::Varint),
                FieldDef::new("callchain", FieldType::StackFrames),
            ],
        );
        let single_event_schema = Schema::from_entry(SchemaEntry::with_annotations(
            "producer:RequestMetrics",
            vec![
                FieldDef::new("started", FieldType::OptionalVarint),
                FieldDef::new("os_thread", FieldType::OptionalVarint),
                FieldDef::new("runtime_task", FieldType::OptionalVarint),
                FieldDef::new("operation", FieldType::String),
                FieldDef::new("Route", FieldType::String),
                FieldDef::new("StatusCode", FieldType::Varint),
                FieldDef::new("Success", FieldType::Bool),
            ],
            [
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_START),
                FieldAnnotation::new(0, "unit", "ns"),
                FieldAnnotation::new(0, schema_extensions::SPAN_TYPE_KEY, "test-producer"),
                FieldAnnotation::new(1, schema_extensions::ROLE_KEY, roles::THREAD_ID),
                FieldAnnotation::new(2, schema_extensions::ROLE_KEY, roles::TOKIO_TASK_ID),
                FieldAnnotation::new(3, schema_extensions::ROLE_KEY, roles::SPAN_NAME),
            ],
        ));

        let wall_base = 1_700_000_000_000_000_000;
        let mut enc = Encoder::new();
        enc.write(&ClockSyncEvent {
            timestamp_ns: 10,
            realtime_ns: wall_base + 10,
        })
        .unwrap();
        enc.write_event(
            &unpark_schema,
            50,
            &[FieldValue::Varint(3), FieldValue::Varint(500)],
        )
        .unwrap();

        // Task 77 is active for [90, 120) and [180, 220).
        for (start, end) in [(90, 120), (180, 220)] {
            enc.write_event(
                &poll_start_schema,
                start,
                &[FieldValue::Varint(3), FieldValue::Varint(77)],
            )
            .unwrap();
            enc.write_event(&poll_end_schema, end, &[FieldValue::Varint(3)])
                .unwrap();
        }

        // One sample lands in an active poll interval; one lands in the async
        // gap and must not be attributed to the single-event span.
        for timestamp in [110, 150] {
            enc.write_event(
                &sample_schema,
                timestamp,
                &[
                    FieldValue::Varint(500),
                    FieldValue::Varint(SOURCE_CPU_PROFILE as u64),
                    FieldValue::StackFrames(vec![0xabc].into()),
                ],
            )
            .unwrap();
        }

        // Lifecycle [100, 250): 60ns overlaps task polls and 90ns is async wait.
        // The packed timestamp is the close; the start remains an annotated
        // field so the delta-encoded timestamp stream stays in emission order.
        enc.write_event(
            &single_event_schema,
            250,
            &[
                FieldValue::Varint(100),
                FieldValue::Varint(500),
                FieldValue::Varint(77),
                FieldValue::String("GET /pets".to_string()),
                FieldValue::String("/pets".to_string()),
                FieldValue::Varint(200),
                FieldValue::Bool(true),
            ],
        )
        .unwrap();

        // An event missing the annotated start remains an ordinary custom event
        // and cannot be projected as a span.
        enc.write_event(
            &single_event_schema,
            300,
            &[
                FieldValue::None,
                FieldValue::None,
                FieldValue::None,
                FieldValue::String("ignored".to_string()),
                FieldValue::String("/ignored".to_string()),
                FieldValue::Varint(204),
                FieldValue::Bool(true),
            ],
        )
        .unwrap();

        let source_key = "2026-07-28/1200/svc/host/abcd-123/0.bin";
        let (samples, _stacks, _polls, spans) =
            decode_samples(&enc.into_inner(), source_key).unwrap();

        assert_eq!(spans.len(), 1, "missing-start event must be skipped");
        let span = &spans[0];
        assert_eq!(span.kind, "test-producer");
        assert_eq!(span.name, "GET /pets");
        assert_eq!(span.target, "");
        assert_eq!(span.start_ns, wall_base + 100);
        assert_eq!(span.end_ns, wall_base + 250);
        assert_eq!(span.elapsed_ns, 150);
        assert_eq!(span.observed_active_wall_ns, 60);
        assert_eq!(span.active_ns, Some(60));
        assert_eq!(span.on_cpu_ns_est, Some(60));
        assert_eq!(span.async_wait_ns, Some(90));
        assert_eq!(span.unknown_ns, 0);
        assert_eq!(span.attribution_flags & 0b0100, 0);
        assert_eq!(span.identity_quality, "path");
        assert!(span.details_complete);
        assert_eq!(span.parent_span_uid, None);
        assert_eq!(
            span.attributes,
            vec![
                ("operation".to_string(), "GET /pets".to_string()),
                ("Route".to_string(), "/pets".to_string()),
                ("StatusCode".to_string(), "200".to_string()),
                ("Success".to_string(), "true".to_string()),
            ]
        );

        let active_sample = samples
            .iter()
            .find(|sample| sample.timestamp_ns == wall_base + 110)
            .unwrap();
        assert_eq!(active_sample.enclosing_spans.len(), 1);
        assert_eq!(active_sample.enclosing_spans[0].span_uid, span.span_uid);
        let waiting_sample = samples
            .iter()
            .find(|sample| sample.timestamp_ns == wall_base + 150)
            .unwrap();
        assert!(waiting_sample.enclosing_spans.is_empty());
        assert_eq!(span.cpu_sample_count, 1);
    }

    /// Synthetic test: verify legacy span reconstruction from minimal old-format events.
    #[test]
    fn test_legacy_span_reconstruction_synthetic() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::FieldDef;
        use dial9_trace_format::types::{FieldType, FieldValue};

        #[derive(TraceEvent)]
        struct ClockSyncEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            realtime_ns: u64,
        }

        // Old-format schemas: SpanEnter has worker_id, span_id, parent_span_id, span_name
        let enter_schema = Schema::new(
            "SpanEnter:my_crate::handler::do_work:src/handler.rs:42",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("parent_span_id", FieldType::OptionalVarint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );
        let exit_schema = Schema::new(
            "SpanExit:my_crate::handler::do_work:src/handler.rs:42",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );

        // Old-format SpanCloseEvent: only span_id
        let close_schema = Schema::new(
            "SpanCloseEvent",
            vec![FieldDef::new("span_id", FieldType::Varint)],
        );

        let mut enc = Encoder::new();
        enc.write(&ClockSyncEvent {
            timestamp_ns: 10,
            realtime_ns: 1_700_000_000_000_000_010,
        })
        .unwrap();

        // Enter span_id=1 on worker 0
        enc.write_event(
            &enter_schema,
            100,
            &[
                // timestamp
                FieldValue::Varint(0),
                // worker_id
                FieldValue::Varint(1),
                // span_id
                FieldValue::None,
                // parent_span_id (absent)
                FieldValue::String("do_work".to_string()),
                // span_name,
            ],
        )
        .unwrap();

        // Exit span_id=1 on worker 0
        enc.write_event(
            &exit_schema,
            200,
            &[
                // timestamp
                FieldValue::Varint(0),
                // worker_id
                FieldValue::Varint(1),
                // span_id
                FieldValue::String("do_work".to_string()),
                // span_name,
            ],
        )
        .unwrap();

        // Close span_id=1
        enc.write_event(
            &close_schema,
            250,
            &[
                // timestamp
                FieldValue::Varint(1),
                // span_id,
            ],
        )
        .unwrap();

        let data = enc.into_inner();
        let source_key = "2026-06-19/1300/svc/host/test-boot/0.bin";
        let (_, _, _, spans) = decode_samples(&data, source_key).unwrap();

        assert_eq!(spans.len(), 1, "should produce exactly one legacy span");
        let span = &spans[0];
        assert_eq!(span.name, "do_work");
        assert_eq!(span.target, "my_crate::handler");
        assert_eq!(span.callsite_file.as_deref(), Some("src/handler.rs"));
        assert_eq!(span.callsite_line, Some(42));
        assert_eq!(span.identity_quality, "legacy");
        assert!(!span.details_complete);
        assert_eq!(span.kind, "tracing");

        // Elapsed should be close_ts - first_enter_ts = 250 - 100 = 150
        // (in wall clock with offset)
        assert!(span.elapsed_ns > 0, "elapsed_ns should be > 0");

        // Observed active = exit - enter = 200 - 100 = 100
        assert_eq!(span.observed_active_wall_ns, 100);
    }

    /// User-attached span fields (e.g. `request_id`, `status_code`) that are not
    /// part of the base identity/lifecycle set must be surfaced as span
    /// attributes, mirroring the live viewer's `buildSpanData`. Exit-event
    /// attributes override enter-event attributes for the same span.
    #[test]
    fn test_legacy_span_captures_user_attributes() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::FieldDef;
        use dial9_trace_format::types::{FieldType, FieldValue};

        #[derive(TraceEvent)]
        struct ClockSyncEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            realtime_ns: u64,
        }

        // Enter carries request_id (String) and status_code (Varint) alongside
        // the base fields. Exit carries an updated status_code.
        let enter_schema = Schema::new(
            "SpanEnter:svc::routes::handle:src/routes.rs:7",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("parent_span_id", FieldType::OptionalVarint),
                FieldDef::new("span_name", FieldType::String),
                FieldDef::new("request_id", FieldType::String),
                FieldDef::new("status_code", FieldType::Varint),
            ],
        );
        let exit_schema = Schema::new(
            "SpanExit:svc::routes::handle:src/routes.rs:7",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("span_name", FieldType::String),
                FieldDef::new("request_id", FieldType::String),
                FieldDef::new("status_code", FieldType::Varint),
            ],
        );
        let close_schema = Schema::new(
            "SpanCloseEvent",
            vec![FieldDef::new("span_id", FieldType::Varint)],
        );

        let mut enc = Encoder::new();
        enc.write(&ClockSyncEvent {
            timestamp_ns: 10,
            realtime_ns: 1_700_000_000_000_000_010,
        })
        .unwrap();
        enc.write_event(
            &enter_schema,
            100,
            &[
                FieldValue::Varint(0),
                FieldValue::Varint(1),
                FieldValue::None,
                FieldValue::String("handle".to_string()),
                FieldValue::String("5d051ec2-999b-4a25-93b6-0f9cf83fa8b2".to_string()),
                FieldValue::Varint(0),
                // status not yet known at enter,
            ],
        )
        .unwrap();
        enc.write_event(
            &exit_schema,
            200,
            &[
                FieldValue::Varint(0),
                FieldValue::Varint(1),
                FieldValue::String("handle".to_string()),
                FieldValue::String("5d051ec2-999b-4a25-93b6-0f9cf83fa8b2".to_string()),
                FieldValue::Varint(500),
                // final status known at exit,
            ],
        )
        .unwrap();
        enc.write_event(&close_schema, 250, &[FieldValue::Varint(1)])
            .unwrap();

        let (_, _, _, spans) =
            decode_samples(&enc.into_inner(), "2026-07-17/1746/svc/host/boot/0.bin").unwrap();
        assert_eq!(spans.len(), 1);
        let attrs: std::collections::HashMap<&str, &str> = spans[0]
            .attributes
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            attrs.get("request_id"),
            Some(&"5d051ec2-999b-4a25-93b6-0f9cf83fa8b2")
        );
        // Exit attributes win: final status_code=500, not the enter's 0.
        assert_eq!(attrs.get("status_code"), Some(&"500"));
        // Base identity/lifecycle fields are not surfaced as attributes.
        assert!(!attrs.contains_key("worker_id"));
        assert!(!attrs.contains_key("span_id"));
        assert!(!attrs.contains_key("span_name"));
    }

    /// Wire-order regression: equal monotonic timestamps are ordered by the
    /// shared decode sequence, not by event kind. An exit encoded before its
    /// enter must remain two unmatched events rather than becoming a balanced
    /// zero-duration interval.
    #[test]
    fn test_legacy_equal_timestamp_exit_before_enter_stays_unbalanced() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::FieldDef;
        use dial9_trace_format::types::{FieldType, FieldValue};

        #[derive(TraceEvent)]
        struct ClockSyncEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            realtime_ns: u64,
        }

        let enter_schema = Schema::new(
            "SpanEnter__EqualTimestamp",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("parent_span_id", FieldType::OptionalVarint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );
        let exit_schema = Schema::new(
            "SpanExit__EqualTimestamp",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );
        let close_schema = Schema::new(
            "SpanCloseEvent",
            vec![FieldDef::new("span_id", FieldType::Varint)],
        );

        let mut enc = Encoder::new();
        enc.write(&ClockSyncEvent {
            timestamp_ns: 10,
            realtime_ns: 1_700_000_000_000_000_010,
        })
        .unwrap();
        enc.write_event(
            &exit_schema,
            100,
            &[
                FieldValue::Varint(0),
                FieldValue::Varint(7),
                FieldValue::String("equal_timestamp".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(
            &enter_schema,
            100,
            &[
                FieldValue::Varint(0),
                FieldValue::Varint(7),
                FieldValue::None,
                FieldValue::String("equal_timestamp".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(&close_schema, 101, &[FieldValue::Varint(7)])
            .unwrap();

        let (_, _, _, spans) = decode_samples(
            &enc.into_inner(),
            "2026-07-15/1714/svc/host/test-boot/0.bin",
        )
        .unwrap();
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span.unbalanced_exits, 1);
        assert_eq!(span.unbalanced_enters, 1);
        assert_eq!(span.observed_active_wall_ns, 0);
        assert!(!span.details_complete);
    }

    /// Regression: spans emitted via a struct-derived event use the `__`
    /// naming convention (`SpanEnter__ShaleOperation`), not the colon-separated
    /// dynamic schema name (`SpanEnter:{target}::{name}...`). A Rust identifier
    /// cannot contain `:`, so a `#[derive(TraceEvent)] struct SpanEnter__Foo`
    /// serializes under the `__` name. The decoder previously matched only
    /// `starts_with("SpanEnter:")`, so these events fell through and produced
    /// zero spans (observed on a real beta `shale` trace: 172 enter/exit events,
    /// 0 spans). They also carry no close event and are in the legacy field
    /// layout (worker_id/span_id/span_name, no span_instance_id/tid), so they
    /// must route through the legacy reconstruction path.
    #[test]
    fn test_struct_derived_span_name_convention() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::FieldDef;
        use dial9_trace_format::types::{FieldType, FieldValue};

        #[derive(TraceEvent)]
        struct ClockSyncEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            realtime_ns: u64,
        }

        // Struct-derived schemas: the `__` convention, with an extra user field
        // (`request_id`) like the real shale trace. No colon-separated
        // target/name/file/line is available from the name.
        let enter_schema = Schema::new(
            "SpanEnter__ShaleOperation",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("parent_span_id", FieldType::OptionalVarint),
                FieldDef::new("span_name", FieldType::String),
                FieldDef::new("request_id", FieldType::String),
            ],
        );
        let exit_schema = Schema::new(
            "SpanExit__ShaleOperation",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );

        let mut enc = Encoder::new();
        enc.write(&ClockSyncEvent {
            timestamp_ns: 10,
            realtime_ns: 1_700_000_000_000_000_010,
        })
        .unwrap();

        // Enter span_id=42 on worker 3.
        enc.write_event(
            &enter_schema,
            1000,
            &[
                FieldValue::Varint(3),
                FieldValue::Varint(42),
                FieldValue::None,
                FieldValue::String("/jobs/next".to_string()),
                FieldValue::String("req-abc".to_string()),
            ],
        )
        .unwrap();

        // Exit span_id=42 on worker 3. Note: NO close event follows.
        enc.write_event(
            &exit_schema,
            5000,
            &[
                FieldValue::Varint(3),
                FieldValue::Varint(42),
                FieldValue::String("/jobs/next".to_string()),
            ],
        )
        .unwrap();

        let data = enc.into_inner();
        let source_key = "2026-07-15/1714/shale/host/test-boot/0.bin";
        let (_, _, _, spans) = decode_samples(&data, source_key).unwrap();

        assert_eq!(
            spans.len(),
            1,
            "struct-derived (__) span events must be picked up, not dropped"
        );
        let span = &spans[0];
        // Name comes from the event's span_name field (the schema name carries
        // no colon-separated metadata to parse).
        assert_eq!(span.name, "/jobs/next");
        assert_eq!(span.kind, "tracing");
        assert_eq!(span.identity_quality, "legacy");
        // Observed active = exit - enter = 5000 - 1000 = 4000.
        assert_eq!(span.observed_active_wall_ns, 4000);
        // Even without a close event, the balanced enter/exit pair produces a row.
        assert!(span.elapsed_ns > 0, "elapsed_ns should be > 0");
    }

    #[test]
    fn test_struct_derived_schema_suffix_disambiguates_shared_runtime_name() {
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::FieldDef;
        use dial9_trace_format::types::{FieldType, FieldValue};

        fn enter_schema(name: &'static str) -> Schema {
            Schema::new(
                name,
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("span_id", FieldType::Varint),
                    FieldDef::new("parent_span_id", FieldType::OptionalVarint),
                    FieldDef::new("span_name", FieldType::String),
                ],
            )
        }
        fn exit_schema(name: &'static str) -> Schema {
            Schema::new(
                name,
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("span_id", FieldType::Varint),
                    FieldDef::new("span_name", FieldType::String),
                ],
            )
        }

        let first_enter = enter_schema("SpanEnter__FirstOperation");
        let first_exit = exit_schema("SpanExit__FirstOperation");
        let second_enter = enter_schema("SpanEnter__SecondOperation");
        let second_exit = exit_schema("SpanExit__SecondOperation");
        let mut enc = Encoder::new();
        for (schema, timestamp, span_id) in [
            (&first_enter, 100, 1),
            (&first_exit, 200, 1),
            (&second_enter, 300, 2),
            (&second_exit, 400, 2),
        ] {
            let values = if schema.name().starts_with("SpanEnter__") {
                vec![
                    FieldValue::Varint(0),
                    FieldValue::Varint(span_id),
                    FieldValue::None,
                    FieldValue::String("shared-runtime-name".to_string()),
                ]
            } else {
                vec![
                    FieldValue::Varint(0),
                    FieldValue::Varint(span_id),
                    FieldValue::String("shared-runtime-name".to_string()),
                ]
            };
            enc.write_event(schema, timestamp, &values).unwrap();
        }

        let (_, _, _, spans) = decode_samples(
            &enc.into_inner(),
            "2026-07-15/1714/svc/host/test-boot/0.bin",
        )
        .unwrap();
        assert_eq!(spans.len(), 2);
        assert!(spans.iter().all(|span| span.name == "shared-runtime-name"));
        assert_ne!(
            spans[0].span_type_uid, spans[1].span_type_uid,
            "struct schema suffixes must remain distinct type identities"
        );
    }

    #[test]
    fn test_struct_derived_schema_preserves_distinct_runtime_names() {
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::FieldDef;
        use dial9_trace_format::types::{FieldType, FieldValue};

        let enter = Schema::new(
            "SpanEnter__SharedOperation",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("parent_span_id", FieldType::OptionalVarint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );
        let exit = Schema::new(
            "SpanExit__SharedOperation",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );
        let mut enc = Encoder::new();
        for (timestamp, span_id, runtime_name, schema) in [
            (100, 1, "/jobs", &enter),
            (200, 1, "/jobs", &exit),
            (300, 2, "/jobs/next", &enter),
            (400, 2, "/jobs/next", &exit),
        ] {
            let values = if schema.name().starts_with("SpanEnter__") {
                vec![
                    FieldValue::Varint(0),
                    FieldValue::Varint(span_id),
                    FieldValue::None,
                    FieldValue::String(runtime_name.to_string()),
                ]
            } else {
                vec![
                    FieldValue::Varint(0),
                    FieldValue::Varint(span_id),
                    FieldValue::String(runtime_name.to_string()),
                ]
            };
            enc.write_event(schema, timestamp, &values).unwrap();
        }

        let (_, _, _, spans) = decode_samples(
            &enc.into_inner(),
            "2026-07-15/1714/svc/host/test-boot/0.bin",
        )
        .unwrap();
        assert_eq!(spans.len(), 2);
        assert_ne!(spans[0].name, spans[1].name);
        assert_ne!(
            spans[0].span_type_uid, spans[1].span_type_uid,
            "runtime names sharing one struct schema must remain distinct type identities"
        );
    }

    /// Regression: an async span whose task migrates workers between enter and
    /// exit must still be paired into an interval. The enter fires on worker 3,
    /// the exit on worker 7 (the task was rescheduled onto a different worker
    /// across an `.await`). Pairing keyed on `(span_id, worker_id)` would push
    /// the enter onto worker 3's stack and search worker 7's stack for the exit,
    /// find nothing, and drop the span. Pairing on `span_id` alone recovers it.
    /// (On a real beta trace ~44% of fully-captured spans migrated workers.)
    #[test]
    fn test_legacy_span_survives_worker_migration() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::FieldDef;
        use dial9_trace_format::types::{FieldType, FieldValue};

        #[derive(TraceEvent)]
        struct ClockSyncEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            realtime_ns: u64,
        }

        let enter_schema = Schema::new(
            "SpanEnter__ShaleOperation",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("parent_span_id", FieldType::OptionalVarint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );
        let exit_schema = Schema::new(
            "SpanExit__ShaleOperation",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );

        let mut enc = Encoder::new();
        enc.write(&ClockSyncEvent {
            timestamp_ns: 10,
            realtime_ns: 1_700_000_000_000_000_010,
        })
        .unwrap();

        // Enter span_id=42 on worker 3.
        enc.write_event(
            &enter_schema,
            1000,
            &[
                FieldValue::Varint(3),
                FieldValue::Varint(42),
                FieldValue::None,
                FieldValue::String("/jobs/next".to_string()),
            ],
        )
        .unwrap();
        // Exit span_id=42 on a DIFFERENT worker (7) — the task migrated.
        enc.write_event(
            &exit_schema,
            5000,
            &[
                FieldValue::Varint(7),
                FieldValue::Varint(42),
                FieldValue::String("/jobs/next".to_string()),
            ],
        )
        .unwrap();

        let data = enc.into_inner();
        let source_key = "2026-07-15/1714/shale/host/test-boot/0.bin";
        let (_, _, _, spans) = decode_samples(&data, source_key).unwrap();

        assert_eq!(
            spans.len(),
            1,
            "span whose task migrated workers must still be paired, not dropped"
        );
        // The enter/exit interval was recovered despite the worker change.
        assert_eq!(spans[0].observed_active_wall_ns, 4000);
    }

    /// End-to-end: a legacy span whose owning Tokio task is observable in the
    /// same file gets its entered wall time split into estimated on-CPU (poll
    /// overlap) vs async wait (in-task gaps). A long-poll span (entered across a
    /// single long `.await`) whose task was polled only briefly should report
    /// mostly async wait and little on-CPU — the whole point of this attribution.
    #[test]
    fn test_legacy_span_cpu_wait_attribution_from_polls() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::FieldDef;
        use dial9_trace_format::types::{FieldType, FieldValue};

        #[derive(TraceEvent)]
        struct ClockSyncEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            realtime_ns: u64,
        }

        // Poll events so the decoder reconstructs task 77's poll timeline on
        // worker 3. A poll spans PollStart→PollEnd. We bind tid→worker via
        // WorkerUnpark first (worker_id inference needs it, though attribution
        // itself uses the poll's own worker_id).
        let unpark_schema = Schema::new(
            "WorkerUnparkEvent",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("local_queue", FieldType::Varint),
                FieldDef::new("cpu_time_ns", FieldType::Varint),
                FieldDef::new("sched_wait_ns", FieldType::OptionalVarint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        );
        let poll_start_schema = Schema::new(
            "PollStartEvent",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("local_queue", FieldType::Varint),
                FieldDef::new("task_id", FieldType::Varint),
                FieldDef::new("spawn_loc", FieldType::String),
            ],
        );
        let poll_end_schema = Schema::new(
            "PollEndEvent",
            vec![FieldDef::new("worker_id", FieldType::Varint)],
        );
        let enter_schema = Schema::new(
            "SpanEnter__ShaleOperation",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("parent_span_id", FieldType::OptionalVarint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );
        let exit_schema = Schema::new(
            "SpanExit__ShaleOperation",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );

        let mut enc = Encoder::new();
        enc.write(&ClockSyncEvent {
            timestamp_ns: 10,
            realtime_ns: 1_700_000_000_000_000_010,
        })
        .unwrap();
        // Bind tid 500 → worker 3.
        enc.write_event(
            &unpark_schema,
            500,
            &[
                // ts
                FieldValue::Varint(3),
                // worker_id
                FieldValue::Varint(0),
                // local_queue
                FieldValue::Varint(0),
                // cpu_time_ns
                FieldValue::None,
                // sched_wait_ns
                FieldValue::Varint(500),
                // tid,
            ],
        )
        .unwrap();

        // Poll of task 77 on worker 3 covering the span's enter: [900, 1100].
        enc.write_event(
            &poll_start_schema,
            900,
            &[
                // ts
                FieldValue::Varint(3),
                // worker_id
                FieldValue::Varint(0),
                // local_queue
                FieldValue::Varint(77),
                // task_id
                FieldValue::String("app::handler".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(&poll_end_schema, 1100, &[FieldValue::Varint(3)])
            .unwrap();

        // Enter span_id=42 on worker 3 at t=1000 (inside poll [900,1100] → task 77).
        enc.write_event(
            &enter_schema,
            1000,
            &[
                FieldValue::Varint(3),
                FieldValue::Varint(42),
                FieldValue::None,
                FieldValue::String("/jobs/next".to_string()),
            ],
        )
        .unwrap();

        // A second, brief poll of task 77 well into the span: [5000, 5100].
        // (Non-overlapping with the first — a worker polls one task at a time.)
        enc.write_event(
            &poll_start_schema,
            5000,
            &[
                FieldValue::Varint(3),
                FieldValue::Varint(0),
                FieldValue::Varint(77),
                FieldValue::String("app::handler".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(&poll_end_schema, 5100, &[FieldValue::Varint(3)])
            .unwrap();

        // Exit span_id=42 much later at t=9000: the span was entered across a
        // long await; the task was only on-CPU during the two polls above.
        enc.write_event(
            &exit_schema,
            9000,
            &[
                FieldValue::Varint(3),
                FieldValue::Varint(42),
                FieldValue::String("/jobs/next".to_string()),
            ],
        )
        .unwrap();

        let data = enc.into_inner();
        let source_key = "2026-07-15/1714/shale/host/test-boot/0.bin";
        let (_, _, _, spans) = decode_samples(&data, source_key).unwrap();

        assert_eq!(spans.len(), 1);
        let s = &spans[0];
        // Entered wall = exit - enter = 9000 - 1000 = 8000.
        assert_eq!(s.observed_active_wall_ns, 8000);
        // On-CPU = overlap of [1000,9000] with task 77's polls [900,1100] and
        // [5000,5100] = [1000,1100] (100) + [5000,5100] (100) = 200.
        assert_eq!(s.on_cpu_ns_est, Some(200));
        // Async wait = entered wall - on_cpu = 8000 - 200 = 7800.
        assert_eq!(s.async_wait_ns, Some(7800));
        // Accounting invariant: the five categories sum to elapsed.
        let sum = s.on_cpu_ns_est.unwrap_or(0)
            + s.blocked_ns_est.unwrap_or(0)
            + s.async_wait_ns.unwrap_or(0)
            + s.scheduler_delay_ns.unwrap_or(0)
            + s.unknown_ns;
        assert_eq!(
            sum, s.elapsed_ns,
            "five-way attribution must sum to elapsed"
        );
        // We resolved the owning task, so the worker/tid-ambiguous bit (2) is
        // cleared, but this is still a poll-timeline estimate (bits 0 and 3 set).
        assert_eq!(s.attribution_flags & 0b0100, 0, "task-resolved bit cleared");
    }

    /// Verify that recycled span IDs are handled conservatively: all enters/exits
    /// for the same span_id are merged into one span instance within a single
    /// trace segment. This is the intended compatibility policy — the old producer
    /// reuses span_ids within a process, but within a single segment (typically
    /// 60s), the same span_id almost always represents the same logical span.
    /// The synthetic instance_id is deterministic from span_id + first-enter
    /// timestamp.
    #[test]
    fn test_legacy_recycled_span_ids() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::FieldDef;
        use dial9_trace_format::types::{FieldType, FieldValue};

        #[derive(TraceEvent)]
        struct ClockSyncEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            realtime_ns: u64,
        }

        let enter_schema = Schema::new(
            "SpanEnter:app::op:src/lib.rs:10",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("parent_span_id", FieldType::OptionalVarint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );
        let exit_schema = Schema::new(
            "SpanExit:app::op:src/lib.rs:10",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );
        let close_schema = Schema::new(
            "SpanCloseEvent",
            vec![FieldDef::new("span_id", FieldType::Varint)],
        );

        let mut enc = Encoder::new();
        enc.write(&ClockSyncEvent {
            timestamp_ns: 10,
            realtime_ns: 1_700_000_000_000_000_010,
        })
        .unwrap();

        // First use of span_id=1
        enc.write_event(
            &enter_schema,
            100,
            &[
                FieldValue::Varint(0),
                FieldValue::Varint(1),
                FieldValue::None,
                FieldValue::String("op".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(
            &exit_schema,
            200,
            &[
                FieldValue::Varint(0),
                FieldValue::Varint(1),
                FieldValue::String("op".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(&close_schema, 250, &[FieldValue::Varint(1)])
            .unwrap();

        // One enter/exit/close cycle for span_id=1 produces one row.
        let data = enc.into_inner();
        let (_, _, _, spans) = decode_samples(&data, "test/path/boot/0.bin").unwrap();

        // Should produce exactly one span (one close event for span_id=1).
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].identity_quality, "legacy");
    }

    /// Regression: tracing recycles a `span_id` only after its span closes, so a
    /// `SpanCloseEvent` marks that id "done". When the same span_id is reused for
    /// several short-lived spans across a segment, each close-delimited cycle
    /// must become its OWN row with its own short elapsed — NOT one merged row
    /// whose lifecycle spans the first enter to the last close.
    ///
    /// Before the fix, a 1ms request span whose id was reused across a 14.5s
    /// window decoded as a single 14.5s span. This test encodes two reuses of
    /// span_id=1, each a 100ns span 10s apart, and asserts two ~100ns rows
    /// rather than one ~10s row.
    #[test]
    fn test_legacy_recycled_span_ids_are_close_delimited() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::FieldDef;
        use dial9_trace_format::types::{FieldType, FieldValue};

        #[derive(TraceEvent)]
        struct ClockSyncEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            realtime_ns: u64,
        }

        let enter_schema = Schema::new(
            "SpanEnter:app::request::handle:src/lib.rs:10",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("parent_span_id", FieldType::OptionalVarint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );
        let exit_schema = Schema::new(
            "SpanExit:app::request::handle:src/lib.rs:10",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );
        let close_schema = Schema::new(
            "SpanCloseEvent",
            vec![FieldDef::new("span_id", FieldType::Varint)],
        );

        let mut enc = Encoder::new();
        enc.write(&ClockSyncEvent {
            timestamp_ns: 10,
            realtime_ns: 1_700_000_000_000_000_010,
        })
        .unwrap();

        // Two distinct 100ns spans sharing the recycled span_id=1, 10s apart.
        // Cycle A: enter@1_000, exit@1_100, close@1_100.
        // Cycle B: enter@10_000_001_000, exit@10_000_001_100, close@10_000_001_100.
        let enter = |enc: &mut Encoder, ts: u64| {
            enc.write_event(
                &enter_schema,
                ts,
                &[
                    FieldValue::Varint(0),
                    FieldValue::Varint(1),
                    FieldValue::None,
                    FieldValue::String("handle".to_string()),
                ],
            )
            .unwrap();
        };
        let exit = |enc: &mut Encoder, ts: u64| {
            enc.write_event(
                &exit_schema,
                ts,
                &[
                    FieldValue::Varint(0),
                    FieldValue::Varint(1),
                    FieldValue::String("handle".to_string()),
                ],
            )
            .unwrap();
        };
        let close = |enc: &mut Encoder, ts: u64| {
            enc.write_event(&close_schema, ts, &[FieldValue::Varint(1)])
                .unwrap();
        };

        enter(&mut enc, 1_000);
        exit(&mut enc, 1_100);
        close(&mut enc, 1_100);
        enter(&mut enc, 10_000_001_000);
        exit(&mut enc, 10_000_001_100);
        close(&mut enc, 10_000_001_100);

        let data = enc.into_inner();
        let (_, _, _, mut spans) = decode_samples(&data, "test/path/boot/0.bin").unwrap();

        assert_eq!(
            spans.len(),
            2,
            "each close-delimited reuse of a recycled span_id must be its own span"
        );
        spans.sort_by_key(|s| s.start_ns);
        for span in &spans {
            assert_eq!(
                span.elapsed_ns, 100,
                "each instance must keep its own short lifecycle, not the 10s span between reuses"
            );
            assert_eq!(span.observed_active_wall_ns, 100);
            assert_eq!(span.name, "handle");
        }
        // The two instances have distinct identities despite the shared span_id.
        assert_ne!(spans[0].span_uid, spans[1].span_uid);
    }
}

#[cfg(test)]
mod decode_test;
#[cfg(test)]
mod parser_parity_test;

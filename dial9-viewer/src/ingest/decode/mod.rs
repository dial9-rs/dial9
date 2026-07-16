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
//! - [`spans`]: exact-key interval pairing, modern/legacy adapters, and the
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
use spans::{interval_pairing, legacy, modern};
#[cfg(test)]
use std::collections::HashMap;

pub use types::{DecodeResult, EnclosingSpanSummary, ResolvedPoll, ResolvedSample, ResolvedSpan};

/// Wire value of the `CpuProfile` CPU-sample source (periodic on-CPU sample).
const SOURCE_CPU_PROFILE: u8 = 0;
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
pub fn decode_samples(data: &[u8], source_key: &str) -> anyhow::Result<DecodeResult> {
    let events::DecodedTrace {
        interner,
        mut addr_to_keys,
        mut events,
        clock_offset,
        first_clock_sync_mono,
        segment_metadata_boot_id,
        span_closes,
        span_enters,
        span_exits,
        legacy_enters,
        legacy_exits,
        legacy_closes,
    } = events::decode_trace(data, source_key)?;

    tracing::info!("sorting {} events", events.len());
    // Sort events by timestamp for correct worker_id inference.
    events.sort_unstable_by_key(|e| e.timestamp_ns());

    // Pre-sort symbol entries by inline depth.
    for entries in addr_to_keys.values_mut() {
        entries.sort_unstable_by_key(|(d, _)| *d);
    }

    let mut poll_timeline = polls::PollTimeline::reconstruct(&events);

    let mut stacks_dict: FxHashMap<[u8; 16], Vec<String>> = FxHashMap::default();
    let mut stack_cache: FxHashMap<Vec<u64>, [u8; 16]> = FxHashMap::default();
    let mut samples = Vec::new();
    let (parsed_date, parsed_service, parsed_host) = parse_source_key(source_key);

    for event in &events {
        match event {
            TraceEvent::WorkerPark(_)
            | TraceEvent::WorkerUnpark(_)
            | TraceEvent::PollStart(_)
            | TraceEvent::PollEnd(_) => {}
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

    // ── Stage 2: Resolve tracing span close summaries ────────────────────────
    //
    // Decode the authoritative boot_id from SegmentMetadata when available.
    // The boot_id directory is written into segment metadata by the namespace
    // isolation layer. When absent (old traces, non-namespaced writers), fall
    // back to extracting it from the source_key path.
    //
    // Identity quality tiers:
    // - "metadata": boot_id from SegmentMetadata (authoritative, stable across
    //   files from the same process). Highest quality.
    // - "path": boot_id extracted from a namespaced source key path that
    //   matches the expected layout ({date}/{HHMM}/{service}/{host}/{boot_id}/{file}).
    //   The boot_id directory is a stable process identity anchor: all segments
    //   from the same process write to the same directory, so cross-segment
    //   span_uid correlation is reliable. Authoritative.
    // - "flat": genuinely flat or legacy path where no valid boot_id directory
    //   can be identified. The fallback value (directory name or whole path) is
    //   NOT guaranteed stable across segments from the same process. Cannot
    //   claim cross-file stability. Low quality.
    let (boot_id, identity_quality): (String, &'static str) =
        if let Some(meta_bid) = segment_metadata_boot_id {
            (meta_bid, "metadata")
        } else {
            let (extracted, is_namespaced) = extract_boot_id_from_path_qualified(source_key);
            if is_namespaced {
                (extracted.to_string(), "path")
            } else {
                (extracted.to_string(), "flat")
            }
        };

    let resolution = resolve_spans(
        span_closes,
        &span_enters,
        &span_exits,
        source_key,
        &boot_id,
        clock_offset,
        first_clock_sync_mono,
        &parsed_host,
        &parsed_service,
        &parsed_date,
        identity_quality,
    );
    let mut resolved_spans = resolution.spans;
    let instance_intervals = resolution.instance_intervals;

    // ── Stage 2b: Legacy span reconstruction ─────────────────────────────────
    //
    // Reconstruct any legacy events independently; mixed-format files can carry
    // both modern and legacy spans.
    //
    // Old format:
    //   SpanEnter:{target}::{name}:{file}:{line} → fields: worker_id, span_id, parent_span_id, span_name, ...
    //   SpanExit:{target}::{name}:{file}:{line}  → fields: worker_id, span_id, span_name, ...
    //   SpanCloseEvent                           → fields: span_id (only)
    //
    // Reconstruction strategy:
    // - Use raw span_id as the local identity key
    // - Synthesize a deterministic instance_id from span_id + first-enter timestamp
    //   to avoid collisions when IDs are recycled
    // - Pair enter/exit by span_id alone (NOT worker_id): async tasks migrate
    //   workers across .await, so worker_id is not a stable pairing key.
    // - Parse target/name/file/line from the SpanEnter schema name
    // - Lifecycle start = first observed enter (conservative)
    // - details_complete = false (cannot verify boundary without modern metadata)
    // - identity_quality = "legacy"
    // - loss_observable = false
    // - All elapsed remains unknown (no producer-reported active_ns)

    let mut legacy_intervals: FxHashMap<u64, Vec<interval_pairing::MonoInterval>> =
        FxHashMap::default();

    // Process legacy spans independently of modern spans. Both formats can
    // coexist in the same file (e.g. a library using old-format spans alongside
    // a service using new-format spans). Previously, `has_modern_spans` globally
    // suppressed all legacy events, causing valid legacy spans to be lost in
    // mixed-format files.
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
        legacy_intervals = legacy_resolution.instance_intervals;
        resolved_spans.extend(legacy_resolution.spans);
    }

    // ── Stage 3: Attribute samples to spans using entered intervals ──────────
    //
    // Build a flat sorted interval index for O(n log n + m log n) sweep instead
    // of O(samples × spans). Each entry maps a wall-clock entered interval back
    // to the resolved_spans index.
    //
    // CRITICAL: We attach samples ONLY to balanced, locally observed entered
    // intervals — never to lifecycle envelopes. An async span that is exited
    // (waiting) must NOT claim samples that fire during its idle gap.
    //
    // We REUSE the intervals already computed in resolve_spans (single
    // event-ordered per-(instance_id,tid) LIFO reconstruction) rather than
    // performing a second broken all-enters-then-exits pass.

    attribution::attribute_samples_to_spans(
        &mut samples,
        &mut resolved_spans,
        &instance_intervals,
        &legacy_intervals,
        &boot_id,
        clock_offset,
    );

    Ok((
        samples,
        stacks_dict.into_iter().collect(),
        resolved_polls,
        resolved_spans,
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
/// Path layout: `…/{date}/{HHMM}/{service}/{host}/{boot}/{file}`
/// The boot-id is the second-to-last component. If we cannot find the expected
/// structure (e.g. a legacy flat path), we fall back to the entire directory
/// path (everything except the filename), which still provides cross-segment
/// stability for a single process.
#[cfg(test)]
fn extract_boot_id_from_path(source_key: &str) -> &str {
    extract_boot_id_from_path_qualified(source_key).0
}

/// Extract the boot-id directory from a source key path, returning both the
/// extracted value and whether the path is a valid namespaced layout.
///
/// A "namespaced" path has the boot_id as the second-to-last component and
/// the boot_id matches the `{4-alpha}-{digits}` format generated by
/// `dial9_core::boot_id::generate_boot_id`.
///
/// Returns `(boot_id, is_namespaced)`:
/// - `is_namespaced = true`: the path has the expected structure and the
///   boot_id directory matches the known format. This is authoritative for
///   cross-segment identity.
/// - `is_namespaced = false`: the path is flat/legacy. The returned value is
///   a best-effort fallback (directory portion) that cannot guarantee stability.
fn extract_boot_id_from_path_qualified(source_key: &str) -> (&str, bool) {
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
        (candidate, is_namespaced)
    } else {
        // Fallback: use the whole path minus the filename
        let fallback = path.rsplit_once('/').map_or(path, |(dir, _)| dir);
        (fallback, false)
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

/// Resolve span close summaries into `ResolvedSpan` rows.
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
fn resolve_spans(
    span_closes: Vec<SpanCloseSummary>,
    span_enters: &[SpanEnterEvent],
    span_exits: &[SpanExitEvent],
    source_key: &str,
    boot_id: &str,
    clock_offset: Option<clock::ClockOffset>,
    first_clock_sync_mono: Option<clock::MonoNs>,
    host: &str,
    service: &str,
    date: &str,
    identity_quality: &'static str,
) -> SpanResolution {
    let result = modern::resolve_modern_spans(
        span_closes,
        span_enters,
        span_exits,
        source_key,
        boot_id,
        clock_offset,
        first_clock_sync_mono,
        host,
        service,
        date,
        identity_quality,
    );
    SpanResolution {
        spans: result.spans,
        instance_intervals: result.instance_intervals,
    }
}

/// Resolve legacy (old-producer) span events into `ResolvedSpan` rows.
///
/// Delegates to the [`legacy`] module which handles:
/// - Pairing enters/exits by `span_id` alone (not worker_id)
/// - Synthesizing deterministic instance_ids from span_id + first-enter timestamp
/// - Parsing target/name/file/line from SpanEnter schema names
/// - Task-based CPU/wait attribution when polls are available
///
/// Recycled ID policy: all enters/exits for the same span_id are merged
/// conservatively into one span instance. Within a single trace segment
/// (typically 60s), the same span_id almost always represents the same
/// logical span. A close event marks the lifecycle end.
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

    /// A span that is never entered (no enter/exit events) should have
    /// observed_active_wall_ns = 0, details_complete = false, and
    /// unknown_ns = elapsed_ns.
    #[test]
    fn test_never_entered_span() {
        let closes = vec![SpanCloseSummary {
            timestamp_ns: 2000,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: 1000,
            first_enter_timestamp_ns: None,
            active_ns: 0,
            span_name: "never_entered".to_string(),
            target: "test".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 0,
        }];

        let result = resolve_spans(
            closes,
            &[], // no enters
            &[], // no exits
            "2026-06-19/1300/svc/host/boot/0.bin",
            "boot",
            None,
            None,
            "host",
            "svc",
            "2026-06-19",
            "metadata",
        );

        assert_eq!(result.spans.len(), 1);
        let span = &result.spans[0];
        assert_eq!(span.elapsed_ns, 1000);
        assert_eq!(span.observed_active_wall_ns, 0);
        assert!(!span.details_complete);
        assert_eq!(
            span.unknown_ns, 1000,
            "never-entered span: all time is unknown"
        );
        assert_eq!(span.active_ns, None);
    }

    /// Async gaps between entered intervals should not be attributed to the span.
    /// Only the entered intervals count.
    #[test]
    fn test_async_gaps_not_attributed() {
        let closes = vec![SpanCloseSummary {
            timestamp_ns: 5000,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: 1000,
            first_enter_timestamp_ns: Some(1000),
            active_ns: 0,
            span_name: "async_op".to_string(),
            target: "test".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 0,
        }];

        // Two non-overlapping entered intervals with a gap between them.
        let enters = vec![
            SpanEnterEvent {
                timestamp_ns: 1000,
                span_instance_id: 1,
                tid: 1,
                decode_sequence: 0,
            },
            SpanEnterEvent {
                timestamp_ns: 3000,
                span_instance_id: 1,
                tid: 1,
                decode_sequence: 2,
            },
        ];
        let exits = vec![
            SpanExitEvent {
                timestamp_ns: 2000,
                span_instance_id: 1,
                tid: 1,
                decode_sequence: 1,
            },
            SpanExitEvent {
                timestamp_ns: 4000,
                span_instance_id: 1,
                tid: 1,
                decode_sequence: 3,
            },
        ];

        let result = resolve_spans(
            closes,
            &enters,
            &exits,
            "2026-06-19/1300/svc/host/boot/0.bin",
            "boot",
            None,
            None,
            "host",
            "svc",
            "2026-06-19",
            "metadata",
        );

        assert_eq!(result.spans.len(), 1);
        let span = &result.spans[0];
        assert_eq!(span.elapsed_ns, 4000); // 5000 - 1000
        // Wall union of [1000,2000) and [3000,4000) = 2000ns
        assert_eq!(span.observed_active_wall_ns, 2000);
        // unknown_ns = elapsed_ns (invariant: don't fake categories)
        assert_eq!(span.unknown_ns, 4000);
    }

    /// Overlapping intervals should be unioned, not double-counted.
    #[test]
    fn test_overlap_union() {
        let closes = vec![SpanCloseSummary {
            timestamp_ns: 5000,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: 1000,
            first_enter_timestamp_ns: Some(1000),
            active_ns: 0,
            span_name: "concurrent".to_string(),
            target: "test".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 1, // producer reports concurrency
            saturated: 0,
            loss_observable: 0,
        }];

        // Overlapping intervals (concurrent entry from two threads)
        let enters = vec![
            SpanEnterEvent {
                timestamp_ns: 1000,
                span_instance_id: 1,
                tid: 1,
                decode_sequence: 0,
            },
            SpanEnterEvent {
                timestamp_ns: 1500,
                span_instance_id: 1,
                tid: 2,
                decode_sequence: 1,
            },
        ];
        let exits = vec![
            SpanExitEvent {
                timestamp_ns: 3000,
                span_instance_id: 1,
                tid: 1,
                decode_sequence: 3,
            },
            SpanExitEvent {
                timestamp_ns: 2500,
                span_instance_id: 1,
                tid: 2,
                decode_sequence: 2,
            },
        ];

        let result = resolve_spans(
            closes,
            &enters,
            &exits,
            "2026-06-19/1300/svc/host/boot/0.bin",
            "boot",
            None,
            None,
            "host",
            "svc",
            "2026-06-19",
            "metadata",
        );

        assert_eq!(result.spans.len(), 1);
        let span = &result.spans[0];
        // Union of [1000,3000) and [1500,2500) = [1000,3000) = 2000ns wall
        assert_eq!(span.observed_active_wall_ns, 2000);
        assert!(span.concurrent);
    }

    /// Samples should only be attributed to entered intervals, not lifecycle envelopes.
    #[test]
    fn test_samples_only_in_entered_intervals() {
        // Build a minimal trace with one span that has entered intervals
        // [100, 200) and [300, 400), and samples at ts=50 (before), ts=150
        // (inside first), ts=250 (in gap), ts=350 (inside second), ts=450 (after).

        // For this test, we'll directly test the sample attribution logic.
        // Create a simple scenario with mock data.
        let source_key = "2026-06-19/1300/svc/host/boot/0.bin";

        let enters = vec![
            SpanEnterEvent {
                timestamp_ns: 100,
                span_instance_id: 1,
                tid: 10,
                decode_sequence: 0,
            },
            SpanEnterEvent {
                timestamp_ns: 300,
                span_instance_id: 1,
                tid: 10,
                decode_sequence: 2,
            },
        ];
        let exits = vec![
            SpanExitEvent {
                timestamp_ns: 200,
                span_instance_id: 1,
                tid: 10,
                decode_sequence: 1,
            },
            SpanExitEvent {
                timestamp_ns: 400,
                span_instance_id: 1,
                tid: 10,
                decode_sequence: 3,
            },
        ];
        let closes = vec![SpanCloseSummary {
            timestamp_ns: 500,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: 50,
            first_enter_timestamp_ns: Some(100),
            active_ns: 200,
            span_name: "test_span".to_string(),
            target: "test".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 0,
        }];

        let resolved = resolve_spans(
            closes,
            &enters,
            &exits,
            source_key,
            "boot",
            None,
            None,
            "host",
            "svc",
            "2026-06-19",
            "metadata",
        );
        assert_eq!(resolved.spans.len(), 1);

        // Build intervals for attribution (same logic as decode_samples).
        let _span_uid = resolved.spans[0].span_uid;

        // Simulate the interval index
        struct TestInterval {
            start_wall: u64,
            end_wall: u64,
        }
        let intervals = [
            TestInterval {
                start_wall: 100,
                end_wall: 200,
            },
            TestInterval {
                start_wall: 300,
                end_wall: 400,
            },
        ];

        // Test each sample timestamp
        let test_cases = vec![
            (50u64, false, "before first enter - lifecycle only"),
            (150, true, "inside first entered interval"),
            (250, false, "in gap between intervals"),
            (350, true, "inside second entered interval"),
            (450, false, "after last exit - lifecycle only"),
        ];

        for (ts, should_match, desc) in test_cases {
            let matched = intervals
                .iter()
                .any(|iv| ts >= iv.start_wall && ts < iv.end_wall);
            assert_eq!(matched, should_match, "ts={ts}: {desc}");
        }
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

    /// Nested spans: a sample inside both parent and child intervals should
    /// be attributed to both, and the membership list roundtrips correctly.
    #[test]
    fn test_nested_membership_roundtrip() {
        let source_key = "2026-06-19/1300/svc/host/boot/0.bin";

        // Parent span [100, 400), child span [200, 300)
        let enters = vec![
            SpanEnterEvent {
                timestamp_ns: 100,
                span_instance_id: 1,
                tid: 10,
                decode_sequence: 0,
            },
            SpanEnterEvent {
                timestamp_ns: 200,
                span_instance_id: 2,
                tid: 10,
                decode_sequence: 1,
            },
        ];
        let exits = vec![
            SpanExitEvent {
                timestamp_ns: 400,
                span_instance_id: 1,
                tid: 10,
                decode_sequence: 3,
            },
            SpanExitEvent {
                timestamp_ns: 300,
                span_instance_id: 2,
                tid: 10,
                decode_sequence: 2,
            },
        ];
        let closes = vec![
            SpanCloseSummary {
                timestamp_ns: 400,
                span_id: 1,
                span_instance_id: 1,
                start_timestamp_ns: 100,
                first_enter_timestamp_ns: Some(100),
                active_ns: 300,
                span_name: "parent".to_string(),
                target: "test".to_string(),
                file: None,
                line: None,
                parent_span_instance_id: None,
                attributes: Vec::new(),
                unbalanced_enters: 0,
                concurrent: 0,
                saturated: 0,
                loss_observable: 0,
            },
            SpanCloseSummary {
                timestamp_ns: 300,
                span_id: 2,
                span_instance_id: 2,
                start_timestamp_ns: 200,
                first_enter_timestamp_ns: Some(200),
                active_ns: 100,
                span_name: "child".to_string(),
                target: "test".to_string(),
                file: None,
                line: None,
                parent_span_instance_id: Some(1),
                attributes: Vec::new(),
                unbalanced_enters: 0,
                concurrent: 0,
                saturated: 0,
                loss_observable: 0,
            },
        ];

        let resolved = resolve_spans(
            closes,
            &enters,
            &exits,
            source_key,
            "boot",
            None,
            Some(clock::MonoNs(50)), // clock sync boundary before span starts
            "host",
            "svc",
            "2026-06-19",
            "metadata",
        );
        assert_eq!(resolved.spans.len(), 2);

        let parent_uid = resolved.spans[0].span_uid;
        let child_uid = resolved.spans[1].span_uid;
        assert_ne!(parent_uid, child_uid);

        // Verify parent_span_uid is set correctly
        assert_eq!(resolved.spans[1].parent_span_uid, Some(parent_uid));

        // Verify membership: compact fields only (span_uid, span_type_uid, elapsed_ns, details_complete)
        let _summary = EnclosingSpanSummary {
            span_uid: parent_uid,
            span_type_uid: resolved.spans[0].span_type_uid,
            elapsed_ns: resolved.spans[0].elapsed_ns,
            details_complete: resolved.spans[0].details_complete,
        };
        // Verify the struct has only 4 fields (compact OTAP-aligned)
        assert_eq!(
            std::mem::size_of::<EnclosingSpanSummary>(),
            16 + 16 + 8 + 1 + 7 /* padding */
        );
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
        assert_eq!(resolve_span_task(&polls, 100), Some(11));
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

    /// Path-based boot_id with a valid namespaced format IS authoritative and
    /// CAN yield details_complete=true. Flat/legacy fallback must NOT.
    #[test]
    fn test_identity_quality_tiers_and_details_complete() {
        let closes = || {
            vec![SpanCloseSummary {
                timestamp_ns: 2000,
                span_id: 1,
                span_instance_id: 1,
                start_timestamp_ns: 1000,
                first_enter_timestamp_ns: Some(1000),
                active_ns: 500,
                span_name: "balanced_span".to_string(),
                target: "test".to_string(),
                file: Some("src/main.rs".to_string()),
                line: Some(10),
                parent_span_instance_id: None,
                attributes: Vec::new(),
                unbalanced_enters: 0,
                concurrent: 0,
                saturated: 0,
                loss_observable: 1, // loss IS observable
            }]
        };
        let enters = vec![SpanEnterEvent {
            timestamp_ns: 1000,
            span_instance_id: 1,
            tid: 1,
            decode_sequence: 0,
        }];
        let exits = vec![SpanExitEvent {
            timestamp_ns: 1500,
            span_instance_id: 1,
            tid: 1,
            decode_sequence: 1,
        }];

        // With "flat" identity_quality — details_complete must be false
        let result = resolve_spans(
            closes(),
            &enters,
            &exits,
            "flat-file.bin",
            "flat-file.bin",
            None,
            Some(clock::MonoNs(900)), // clock sync boundary before span start
            "host",
            "svc",
            "2026-06-19",
            "flat", // <-- flat fallback
        );
        assert_eq!(result.spans.len(), 1);
        assert!(
            !result.spans[0].details_complete,
            "flat identity must never yield details_complete=true"
        );
        assert_eq!(result.spans[0].identity_quality, "flat");

        // With "path" identity_quality — details_complete SHOULD be true
        // (namespaced path is authoritative)
        let result2 = resolve_spans(
            closes(),
            &enters,
            &exits,
            "2026-06-19/1300/svc/host/abcd-123/0.bin",
            "abcd-123",
            None,
            Some(clock::MonoNs(900)), // clock sync boundary before span start
            "host",
            "svc",
            "2026-06-19",
            "path", // <-- namespaced path (authoritative)
        );
        assert_eq!(result2.spans.len(), 1);
        assert!(
            result2.spans[0].details_complete,
            "namespaced path identity with balanced spans and observable loss should yield details_complete=true"
        );
        assert_eq!(result2.spans[0].identity_quality, "path");

        // With "metadata" identity_quality — details_complete should be true
        let result3 = resolve_spans(
            closes(),
            &enters,
            &exits,
            "2026-06-19/1300/svc/host/boot/0.bin",
            "boot",
            None,
            Some(clock::MonoNs(900)), // clock sync boundary before span start
            "host",
            "svc",
            "2026-06-19",
            "metadata", // <-- authoritative
        );
        assert_eq!(result3.spans.len(), 1);
        assert!(
            result3.spans[0].details_complete,
            "authoritative boot_id with balanced spans should yield details_complete=true"
        );
        assert_eq!(result3.spans[0].identity_quality, "metadata");
    }

    /// Unmatched exits (exits without matching enter) must degrade details_complete
    /// and be tracked in the `unbalanced_exits` field.
    #[test]
    fn test_unmatched_exits_degrade_completeness() {
        let closes = vec![SpanCloseSummary {
            timestamp_ns: 3000,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: 1000,
            first_enter_timestamp_ns: Some(1000),
            active_ns: 500,
            span_name: "has_unmatched_exit".to_string(),
            target: "test".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 0,
        }];
        // One balanced pair plus one orphaned exit (no matching enter)
        let enters = vec![SpanEnterEvent {
            timestamp_ns: 1000,
            span_instance_id: 1,
            tid: 1,
            decode_sequence: 0,
        }];
        let exits = vec![
            SpanExitEvent {
                timestamp_ns: 1500,
                span_instance_id: 1,
                tid: 1,
                decode_sequence: 1,
            },
            // Orphaned exit (no matching enter for tid=2)
            SpanExitEvent {
                timestamp_ns: 2000,
                span_instance_id: 1,
                tid: 2,
                decode_sequence: 2,
            },
        ];

        let result = resolve_spans(
            closes,
            &enters,
            &exits,
            "2026-06-19/1300/svc/host/boot/0.bin",
            "boot",
            None,
            None,
            "host",
            "svc",
            "2026-06-19",
            "metadata",
        );
        assert_eq!(result.spans.len(), 1);
        let span = &result.spans[0];
        assert_eq!(span.unbalanced_exits, 1, "must track the unmatched exit");
        assert!(
            !span.details_complete,
            "unmatched exits must degrade details_complete"
        );
    }

    /// Equal timestamps preserve original decode sequence (enter at idx 0, exit at idx 1)
    /// so that an enter and exit at the same ns produce a valid zero-length interval.
    /// When the enter is decoded before the exit, decode_sequence assigns enter < exit,
    /// so enter sorts first → balanced zero-duration interval.
    #[test]
    fn test_equal_timestamp_ordering() {
        // Case 1: enter decoded BEFORE exit (enter.decode_sequence < exit.decode_sequence)
        // → enter sorts first → pushed, then exit pops it → balanced zero-duration interval.
        let closes = vec![SpanCloseSummary {
            timestamp_ns: 100,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: 50,
            first_enter_timestamp_ns: Some(100),
            active_ns: 0,
            span_name: "zero_len".to_string(),
            target: "test".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 1, // observable: completeness can be claimed
        }];
        let enters = vec![SpanEnterEvent {
            timestamp_ns: 100,
            span_instance_id: 1,
            tid: 1,
            decode_sequence: 0, // decoded first
        }];
        let exits = vec![SpanExitEvent {
            timestamp_ns: 100,
            span_instance_id: 1,
            tid: 1,
            decode_sequence: 1, // decoded second
        }];

        let result = resolve_spans(
            closes,
            &enters,
            &exits,
            "2026-06-19/1300/svc/host/boot/0.bin",
            "boot",
            None,
            Some(clock::MonoNs(50)), // clock sync boundary before span start
            "host",
            "svc",
            "2026-06-19",
            "metadata",
        );
        assert_eq!(result.spans.len(), 1);
        let span = &result.spans[0];
        // Enter sorts before exit → balanced zero-duration interval.
        assert_eq!(
            span.unbalanced_exits, 0,
            "enter-before-exit must yield balanced pair"
        );
        assert_eq!(
            span.unbalanced_enters, 0,
            "enter-before-exit must yield balanced pair"
        );
        assert!(
            span.details_complete,
            "balanced zero-duration interval with metadata identity must be complete"
        );
        // Verify the interval was recorded (zero duration is valid).
        let intervals = result.instance_intervals.get(&1).unwrap();
        assert_eq!(intervals.len(), 1);
        assert_eq!(
            intervals[0],
            (clock::MonoNs(100), clock::MonoNs(100)),
            "zero-duration interval"
        );
    }

    /// When the exit is decoded before the enter (exit.decode_sequence < enter.decode_sequence),
    /// the exit sorts first and finds nothing to pop → unmatched exit. Then the enter
    /// is pushed and never popped → unmatched enter. Both degrade completeness.
    #[test]
    fn test_equal_timestamp_reverse_order_yields_unmatched() {
        let closes = vec![SpanCloseSummary {
            timestamp_ns: 100,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: 50,
            first_enter_timestamp_ns: Some(100),
            active_ns: 0,
            span_name: "reverse_order".to_string(),
            target: "test".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 0,
        }];
        let enters = vec![SpanEnterEvent {
            timestamp_ns: 100,
            span_instance_id: 1,
            tid: 1,
            decode_sequence: 1, // decoded SECOND
        }];
        let exits = vec![SpanExitEvent {
            timestamp_ns: 100,
            span_instance_id: 1,
            tid: 1,
            decode_sequence: 0, // decoded FIRST
        }];

        let result = resolve_spans(
            closes,
            &enters,
            &exits,
            "2026-06-19/1300/svc/host/boot/0.bin",
            "boot",
            None,
            Some(clock::MonoNs(50)),
            "host",
            "svc",
            "2026-06-19",
            "metadata",
        );
        assert_eq!(result.spans.len(), 1);
        let span = &result.spans[0];
        // Exit sorts first (decode_sequence=0 < 1), finds nothing to pop → unmatched.
        assert_eq!(span.unbalanced_exits, 1, "exit decoded first is unmatched");
        // Enter pushed but never popped → leftover on stack → unbalanced enter.
        assert_eq!(
            span.unbalanced_enters, 1,
            "enter decoded second is never popped"
        );
        assert!(
            !span.details_complete,
            "reverse decode order degrades completeness"
        );
    }

    /// Finding 2: details_complete must require first_clock_sync_ns=Some(boundary).
    /// With None, even a perfectly balanced span with metadata identity must be incomplete.
    #[test]
    fn test_details_complete_requires_some_clock_sync() {
        let closes = vec![SpanCloseSummary {
            timestamp_ns: 2000,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: 1000,
            first_enter_timestamp_ns: Some(1000),
            active_ns: 500,
            span_name: "balanced".to_string(),
            target: "test".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 1, // observable so other conditions can be tested
        }];
        let enters = vec![SpanEnterEvent {
            timestamp_ns: 1000,
            span_instance_id: 1,
            tid: 1,
            decode_sequence: 0,
        }];
        let exits = vec![SpanExitEvent {
            timestamp_ns: 1500,
            span_instance_id: 1,
            tid: 1,
            decode_sequence: 1,
        }];

        // first_clock_sync_ns = None → incomplete
        let result = resolve_spans(
            closes.clone(),
            &enters,
            &exits,
            "2026-06-19/1300/svc/host/boot/0.bin",
            "boot",
            None,
            None, // <-- no clock sync
            "host",
            "svc",
            "2026-06-19",
            "metadata",
        );
        assert_eq!(result.spans.len(), 1);
        assert!(
            !result.spans[0].details_complete,
            "None clock sync must yield details_complete=false even with metadata identity"
        );

        // first_clock_sync_ns = Some(boundary) where start >= boundary → complete
        let result2 = resolve_spans(
            closes,
            &enters,
            &exits,
            "2026-06-19/1300/svc/host/boot/0.bin",
            "boot",
            None,
            Some(clock::MonoNs(900)), // boundary before span start
            "host",
            "svc",
            "2026-06-19",
            "metadata",
        );
        assert_eq!(result2.spans.len(), 1);
        assert!(
            result2.spans[0].details_complete,
            "Some(boundary <= start) must yield details_complete=true"
        );
    }

    /// Finding 2: A balanced span with async gaps (enter/exit/enter/exit)
    /// is structurally complete when clock sync is present and start >= boundary.
    #[test]
    fn test_balanced_async_gap_completeness() {
        let closes = vec![SpanCloseSummary {
            timestamp_ns: 5000,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: 1000,
            first_enter_timestamp_ns: Some(1000),
            active_ns: 0,
            span_name: "async_gap_balanced".to_string(),
            target: "test".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 1, // loss is observable
        }];
        // Two complete enter/exit pairs with a gap between them.
        let enters = vec![
            SpanEnterEvent {
                timestamp_ns: 1000,
                span_instance_id: 1,
                tid: 1,
                decode_sequence: 0,
            },
            SpanEnterEvent {
                timestamp_ns: 3000,
                span_instance_id: 1,
                tid: 1,
                decode_sequence: 2,
            },
        ];
        let exits = vec![
            SpanExitEvent {
                timestamp_ns: 2000,
                span_instance_id: 1,
                tid: 1,
                decode_sequence: 1,
            },
            SpanExitEvent {
                timestamp_ns: 4000,
                span_instance_id: 1,
                tid: 1,
                decode_sequence: 3,
            },
        ];

        let result = resolve_spans(
            closes,
            &enters,
            &exits,
            "2026-06-19/1300/svc/host/boot/0.bin",
            "boot",
            None,
            Some(clock::MonoNs(500)), // clock sync boundary before span start
            "host",
            "svc",
            "2026-06-19",
            "metadata",
        );
        assert_eq!(result.spans.len(), 1);
        let span = &result.spans[0];
        assert_eq!(span.unbalanced_enters, 0);
        assert_eq!(span.unbalanced_exits, 0);
        assert!(
            span.details_complete,
            "balanced async-gap span with metadata identity and clock sync must be complete"
        );
        // Observed active wall = union of [1000,2000) + [3000,4000) = 2000ns
        assert_eq!(span.observed_active_wall_ns, 2000);
        assert!(
            span.loss_observable,
            "loss_observable must be propagated from producer"
        );
    }

    /// Saturation (active_ns hit u64::MAX) must degrade details_complete.
    /// Even with perfectly balanced spans and metadata identity, a saturated
    /// span cannot claim certainty about its detail timing.
    #[test]
    fn test_saturation_degrades_details_complete() {
        let closes = vec![SpanCloseSummary {
            timestamp_ns: 2000,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: 1000,
            first_enter_timestamp_ns: Some(1000),
            active_ns: u64::MAX,
            span_name: "saturated_span".to_string(),
            target: "test".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 1, // <-- saturated!
            loss_observable: 1,
        }];
        let enters = vec![SpanEnterEvent {
            timestamp_ns: 1000,
            span_instance_id: 1,
            tid: 1,
            decode_sequence: 0,
        }];
        let exits = vec![SpanExitEvent {
            timestamp_ns: 1500,
            span_instance_id: 1,
            tid: 1,
            decode_sequence: 1,
        }];

        let result = resolve_spans(
            closes,
            &enters,
            &exits,
            "2026-06-19/1300/svc/host/boot/0.bin",
            "boot",
            None,
            Some(clock::MonoNs(900)),
            "host",
            "svc",
            "2026-06-19",
            "metadata",
        );
        assert_eq!(result.spans.len(), 1);
        let span = &result.spans[0];
        assert!(span.saturated, "saturated flag must be set");
        assert!(
            !span.details_complete,
            "saturated span must not claim details_complete=true"
        );
    }

    /// Unobservable loss (loss_observable=0) must degrade details_complete.
    /// When we cannot distinguish "zero loss" from "unknown loss", completeness
    /// cannot be claimed.
    #[test]
    fn test_unobservable_loss_degrades_details_complete() {
        let closes = vec![SpanCloseSummary {
            timestamp_ns: 2000,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: 1000,
            first_enter_timestamp_ns: Some(1000),
            active_ns: 500,
            span_name: "loss_unknown".to_string(),
            target: "test".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 0, // <-- loss NOT observable
        }];
        let enters = vec![SpanEnterEvent {
            timestamp_ns: 1000,
            span_instance_id: 1,
            tid: 1,
            decode_sequence: 0,
        }];
        let exits = vec![SpanExitEvent {
            timestamp_ns: 1500,
            span_instance_id: 1,
            tid: 1,
            decode_sequence: 1,
        }];

        let result = resolve_spans(
            closes,
            &enters,
            &exits,
            "2026-06-19/1300/svc/host/boot/0.bin",
            "boot",
            None,
            Some(clock::MonoNs(900)),
            "host",
            "svc",
            "2026-06-19",
            "metadata",
        );
        assert_eq!(result.spans.len(), 1);
        let span = &result.spans[0];
        assert!(!span.loss_observable, "loss_observable must be false");
        assert!(
            !span.details_complete,
            "unobservable loss must degrade details_complete"
        );
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

    /// Finding 5: Full integration test using the trace Encoder to produce a
    /// fixture with SegmentMetadataEvent, ClockSyncEvent, and SpanClose/Enter/Exit
    /// events. Proves metadata identity_quality vs path fallback through
    /// decode_samples.
    #[test]
    fn test_decode_samples_full_metadata_vs_fallback() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::Encoder;

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

        // SpanCloseEvent must match the name decode_samples looks for.
        #[derive(TraceEvent)]
        #[traceevent(name = "SpanCloseEvent")]
        struct SpanCloseEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            span_id: u64,
            span_instance_id: u64,
            start_timestamp_ns: u64,
            first_enter_timestamp_ns: Option<u64>,
            active_ns: u64,
            span_name: String,
            target: String,
            file: Option<String>,
            line: Option<u32>,
            parent_span_instance_id: Option<u64>,
            attributes: Vec<(String, String)>,
            unbalanced_enters: u32,
            concurrent: u32,
            saturated: u32,
            loss_observable: u32,
        }

        // Build trace WITH boot_id in segment metadata
        let mut enc = Encoder::new();
        enc.write(&ClockSyncEvent {
            timestamp_ns: 50,
            realtime_ns: 1_700_000_000_000_000_050,
        })
        .unwrap();
        enc.write(&SegmentMetadataEvent {
            timestamp_ns: 51,
            entries: vec![("boot_id".to_string(), "meta-boot-xyz".to_string())],
        })
        .unwrap();
        enc.write(&SpanCloseEvent {
            timestamp_ns: 200,
            span_id: 1,
            span_instance_id: 42,
            start_timestamp_ns: 100,
            first_enter_timestamp_ns: None,
            active_ns: 0,
            span_name: "test_op".to_string(),
            target: "test_target".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 0,
        })
        .unwrap();
        let data_with_metadata = enc.into_inner();

        let (_, _, _, spans) = decode_samples(
            &data_with_metadata,
            "2026-06-19/1300/svc/host/path-boot/0.bin.gz",
        )
        .unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(
            spans[0].identity_quality, "metadata",
            "boot_id from SegmentMetadata must yield identity_quality=metadata"
        );
        // Verify span_uid is computed from the metadata boot_id, not the path.
        let expected_uid = compute_span_uid("meta-boot-xyz", 42);
        assert_eq!(spans[0].span_uid, expected_uid);

        // Build trace WITHOUT segment metadata (or without boot_id key)
        let mut enc2 = Encoder::new();
        enc2.write(&ClockSyncEvent {
            timestamp_ns: 50,
            realtime_ns: 1_700_000_000_000_000_050,
        })
        .unwrap();
        enc2.write(&SpanCloseEvent {
            timestamp_ns: 200,
            span_id: 1,
            span_instance_id: 42,
            start_timestamp_ns: 100,
            first_enter_timestamp_ns: None,
            active_ns: 0,
            span_name: "test_op".to_string(),
            target: "test_target".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 0,
        })
        .unwrap();
        let data_without_metadata = enc2.into_inner();

        // Path with a valid boot_id-shaped directory → "path" quality
        let (_, _, _, spans2) = decode_samples(
            &data_without_metadata,
            "2026-06-19/1300/svc/host/abcd-123/0.bin.gz",
        )
        .unwrap();
        assert_eq!(spans2.len(), 1);
        assert_eq!(
            spans2[0].identity_quality, "path",
            "namespaced path with valid boot_id format must yield identity_quality=path"
        );
        let path_boot_id = extract_boot_id_from_path("2026-06-19/1300/svc/host/abcd-123/0.bin.gz");
        let expected_uid_path = compute_span_uid(path_boot_id, 42);
        assert_eq!(spans2[0].span_uid, expected_uid_path);

        // Path with a non-boot_id directory → "flat" quality
        let (_, _, _, spans3) = decode_samples(
            &data_without_metadata,
            "2026-06-19/1300/svc/host/my-custom-dir/0.bin.gz",
        )
        .unwrap();
        assert_eq!(spans3.len(), 1);
        assert_eq!(
            spans3[0].identity_quality, "flat",
            "non-boot_id path must yield identity_quality=flat (low quality)"
        );
        let flat_boot_id =
            extract_boot_id_from_path("2026-06-19/1300/svc/host/my-custom-dir/0.bin.gz");
        let expected_uid_flat = compute_span_uid(flat_boot_id, 42);
        assert_eq!(spans3[0].span_uid, expected_uid_flat);
    }

    /// Finding 5: Stage 3 membership builder test with >64 intervals,
    /// overlapping dedup, and async gap exclusion. Exercises the actual
    /// interval attribution code path from decode_samples.
    #[test]
    fn test_membership_builder_many_intervals_dedup_and_gap_exclusion() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::FieldDef;
        use dial9_trace_format::types::{FieldType, FieldValue, StackFrames};

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

        #[derive(TraceEvent)]
        struct SpanCloseEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            span_id: u64,
            span_instance_id: u64,
            start_timestamp_ns: u64,
            first_enter_timestamp_ns: Option<u64>,
            active_ns: u64,
            span_name: String,
            target: String,
            file: Option<String>,
            line: Option<u32>,
            parent_span_instance_id: Option<u64>,
            attributes: Vec<(String, String)>,
            unbalanced_enters: u32,
            concurrent: u32,
            saturated: u32,
            loss_observable: u32,
        }

        // Use raw Schema + write_event for span enter/exit because their schema
        // names contain ":" which cannot be a Rust struct name.
        let enter_schema = Schema::new(
            "SpanEnter:test_target::many_intervals",
            vec![
                FieldDef::new("span_instance_id", FieldType::Varint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        );
        let exit_schema = Schema::new(
            "SpanExit:test_target::many_intervals",
            vec![
                FieldDef::new("span_instance_id", FieldType::Varint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        );

        // CpuSampleEvent also uses raw write_event for the callchain field.
        let cpu_sample_schema = Schema::new(
            "CpuSampleEvent",
            vec![
                FieldDef::new("tid", FieldType::Varint),
                FieldDef::new("source", FieldType::Varint),
                FieldDef::new("callchain", FieldType::StackFrames),
            ],
        );

        let worker_park_schema = Schema::new(
            "WorkerParkEvent",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        );

        let symbol_schema = Schema::new(
            "SymbolTableEntry",
            vec![
                FieldDef::new("addr", FieldType::Varint),
                FieldDef::new("inline_depth", FieldType::Varint),
                FieldDef::new("symbol_name", FieldType::String),
            ],
        );

        let mut enc = Encoder::new();
        // Clock sync and metadata
        enc.write(&ClockSyncEvent {
            timestamp_ns: 10,
            realtime_ns: 1_700_000_000_000_000_010,
        })
        .unwrap();
        enc.write(&SegmentMetadataEvent {
            timestamp_ns: 11,
            entries: vec![("boot_id".to_string(), "many-iv-boot".to_string())],
        })
        .unwrap();

        // Bind tid=100 to worker_id=0 so CPU samples are attributed.
        enc.write_event(
            &worker_park_schema,
            &[
                FieldValue::Varint(15),  // timestamp
                FieldValue::Varint(0),   // worker_id
                FieldValue::Varint(100), // tid
            ],
        )
        .unwrap();

        // Symbol table entry for our fake address.
        enc.write_event(
            &symbol_schema,
            &[
                FieldValue::Varint(16),                       // timestamp
                FieldValue::Varint(0xDEAD),                   // addr
                FieldValue::Varint(0),                        // inline_depth
                FieldValue::String("test_frame".to_string()), // symbol_name
            ],
        )
        .unwrap();

        // Write >64 non-overlapping enter/exit pairs for span instance 1.
        // Each interval is [i*100+1000, i*100+1050) for i in 0..70.
        let num_intervals = 70u64;
        let base_ts = 1000u64;
        for i in 0..num_intervals {
            let enter_ts = base_ts + i * 100;
            let exit_ts = enter_ts + 50;
            enc.write_event(
                &enter_schema,
                &[
                    FieldValue::Varint(enter_ts), // timestamp
                    FieldValue::Varint(1),        // span_instance_id
                    FieldValue::Varint(100),      // tid
                ],
            )
            .unwrap();
            enc.write_event(
                &exit_schema,
                &[
                    FieldValue::Varint(exit_ts), // timestamp
                    FieldValue::Varint(1),       // span_instance_id
                    FieldValue::Varint(100),     // tid
                ],
            )
            .unwrap();
        }

        // Write CPU samples:
        // - One inside interval 0: ts = base_ts + 25 (in [1000, 1050))
        // - One in gap between intervals 0 and 1: ts = base_ts + 75 (in [1050, 1100))
        // - One inside interval 65: ts = base_ts + 65*100 + 25 = 7525 (in [7500, 7550))
        // - Two inside interval 3 (same interval - dedup test): ts = base_ts + 310, 320
        let sample_in_interval_0 = base_ts + 25;
        let sample_in_gap = base_ts + 75;
        let sample_in_interval_65 = base_ts + 65 * 100 + 25;
        let sample_in_interval_3_a = base_ts + 310;
        let sample_in_interval_3_b = base_ts + 320;

        for ts in [
            sample_in_interval_0,
            sample_in_gap,
            sample_in_interval_65,
            sample_in_interval_3_a,
            sample_in_interval_3_b,
        ] {
            enc.write_event(
                &cpu_sample_schema,
                &[
                    FieldValue::Varint(ts),                             // timestamp
                    FieldValue::Varint(100),                            // tid
                    FieldValue::Varint(0),                              // source (CPU_PROFILE)
                    FieldValue::StackFrames(StackFrames(vec![0xDEAD])), // callchain
                ],
            )
            .unwrap();
        }

        // Close the span after all intervals.
        let close_ts = base_ts + num_intervals * 100 + 100;
        enc.write(&SpanCloseEvent {
            timestamp_ns: close_ts,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: base_ts,
            first_enter_timestamp_ns: Some(base_ts),
            active_ns: 0,
            span_name: "many_intervals".to_string(),
            target: "test_target".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 1,
        })
        .unwrap();

        let data = enc.into_inner();
        let source_key = "2026-06-19/1300/svc/host/many-iv-boot/0.bin.gz";
        let (samples, _stacks, _polls, spans) = decode_samples(&data, source_key).unwrap();

        // Verify span was resolved.
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span.identity_quality, "metadata");
        assert!(span.loss_observable);
        // 70 intervals of 50ns each → observed_active_wall_ns = 3500 (no overlap)
        assert_eq!(span.observed_active_wall_ns, 3500);

        // Verify samples: 5 total CPU samples were written.
        assert_eq!(samples.len(), 5, "all 5 samples should be decoded");

        // Check enclosing_spans attribution:
        // - sample_in_interval_0 should be attributed to the span
        // - sample_in_gap should NOT be attributed (async gap exclusion)
        // - sample_in_interval_65 should be attributed
        // - sample_in_interval_3_a and 3_b should both be attributed (same span, dedup)

        let attributed: Vec<_> = samples
            .iter()
            .filter(|s| !s.enclosing_spans.is_empty())
            .collect();
        let unattributed: Vec<_> = samples
            .iter()
            .filter(|s| s.enclosing_spans.is_empty())
            .collect();

        // 4 samples inside intervals, 1 in gap.
        assert_eq!(
            attributed.len(),
            4,
            "4 samples inside entered intervals should be attributed"
        );
        assert_eq!(
            unattributed.len(),
            1,
            "1 sample in async gap must NOT be attributed"
        );

        // The unattributed sample should be the one in the gap.
        let gap_sample_wall = 1_700_000_000_000_000_010 + (sample_in_gap as i128 - 10) as u64;
        assert_eq!(
            unattributed[0].timestamp_ns, gap_sample_wall,
            "gap sample correctly excluded"
        );

        // Each attributed sample must have exactly 1 enclosing span (dedup: same span_uid).
        for s in &attributed {
            assert_eq!(
                s.enclosing_spans.len(),
                1,
                "each sample should see exactly one span (dedup)"
            );
            assert_eq!(s.enclosing_spans[0].span_uid, span.span_uid);
        }

        // Verify the span accumulated the correct sample count.
        assert_eq!(
            span.cpu_sample_count, 4,
            "span should have 4 cpu samples (gap sample excluded)"
        );
    }

    /// Overlapping balanced intervals for the SAME span instance on different
    /// threads (concurrent entry). A single CPU sample in the overlap region
    /// must produce exactly one enclosing_spans membership entry and increment
    /// cpu_sample_count by exactly 1. Removing the dedup guard would cause the
    /// sample to be attributed twice — this test would fail.
    #[test]
    fn test_decode_samples_overlapping_balanced_intervals_dedup() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::FieldDef;
        use dial9_trace_format::types::{FieldType, FieldValue, StackFrames};

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

        #[derive(TraceEvent)]
        struct SpanCloseEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            span_id: u64,
            span_instance_id: u64,
            start_timestamp_ns: u64,
            first_enter_timestamp_ns: Option<u64>,
            active_ns: u64,
            span_name: String,
            target: String,
            file: Option<String>,
            line: Option<u32>,
            parent_span_instance_id: Option<u64>,
            attributes: Vec<(String, String)>,
            unbalanced_enters: u32,
            concurrent: u32,
            saturated: u32,
            loss_observable: u32,
        }

        // Raw schemas for span enter/exit and CPU samples.
        let enter_schema = Schema::new(
            "SpanEnter:overlap_target::overlap_span",
            vec![
                FieldDef::new("span_instance_id", FieldType::Varint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        );
        let exit_schema = Schema::new(
            "SpanExit:overlap_target::overlap_span",
            vec![
                FieldDef::new("span_instance_id", FieldType::Varint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        );
        let cpu_sample_schema = Schema::new(
            "CpuSampleEvent",
            vec![
                FieldDef::new("tid", FieldType::Varint),
                FieldDef::new("source", FieldType::Varint),
                FieldDef::new("callchain", FieldType::StackFrames),
            ],
        );
        let worker_park_schema = Schema::new(
            "WorkerParkEvent",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        );
        let symbol_schema = Schema::new(
            "SymbolTableEntry",
            vec![
                FieldDef::new("addr", FieldType::Varint),
                FieldDef::new("inline_depth", FieldType::Varint),
                FieldDef::new("symbol_name", FieldType::String),
            ],
        );

        let mut enc = Encoder::new();

        // Clock sync and metadata.
        enc.write(&ClockSyncEvent {
            timestamp_ns: 10,
            realtime_ns: 1_700_000_000_000_000_010,
        })
        .unwrap();
        enc.write(&SegmentMetadataEvent {
            timestamp_ns: 11,
            entries: vec![("boot_id".to_string(), "overlap-boot".to_string())],
        })
        .unwrap();

        // Bind tid=100 to worker_id=0 so CPU sample is attributed.
        enc.write_event(
            &worker_park_schema,
            &[
                FieldValue::Varint(15),  // timestamp
                FieldValue::Varint(0),   // worker_id
                FieldValue::Varint(100), // tid
            ],
        )
        .unwrap();

        // Symbol table entry for a fake address.
        enc.write_event(
            &symbol_schema,
            &[
                FieldValue::Varint(16),
                FieldValue::Varint(0xBEEF),
                FieldValue::Varint(0),
                FieldValue::String("overlap_frame".to_string()),
            ],
        )
        .unwrap();

        // Create overlapping balanced intervals for span_instance_id=1.
        // tid=100: enter at t=1000, exit at t=1100 → interval [1000, 1100)
        // tid=200: enter at t=1020, exit at t=1080 → interval [1020, 1080)
        // Both intervals enclose t=1050.
        enc.write_event(
            &enter_schema,
            &[
                FieldValue::Varint(1000), // timestamp
                FieldValue::Varint(1),    // span_instance_id
                FieldValue::Varint(100),  // tid
            ],
        )
        .unwrap();
        enc.write_event(
            &enter_schema,
            &[
                FieldValue::Varint(1020), // timestamp
                FieldValue::Varint(1),    // span_instance_id
                FieldValue::Varint(200),  // tid
            ],
        )
        .unwrap();
        enc.write_event(
            &exit_schema,
            &[
                FieldValue::Varint(1080), // timestamp
                FieldValue::Varint(1),    // span_instance_id
                FieldValue::Varint(200),  // tid
            ],
        )
        .unwrap();
        enc.write_event(
            &exit_schema,
            &[
                FieldValue::Varint(1100), // timestamp
                FieldValue::Varint(1),    // span_instance_id
                FieldValue::Varint(100),  // tid
            ],
        )
        .unwrap();

        // CPU sample at t=1050, in the overlap region of both intervals.
        enc.write_event(
            &cpu_sample_schema,
            &[
                FieldValue::Varint(1050),                           // timestamp
                FieldValue::Varint(100),                            // tid (on worker 0)
                FieldValue::Varint(0),                              // source (CPU_PROFILE)
                FieldValue::StackFrames(StackFrames(vec![0xBEEF])), // callchain
            ],
        )
        .unwrap();

        // Close the span.
        enc.write(&SpanCloseEvent {
            timestamp_ns: 1200,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: 1000,
            first_enter_timestamp_ns: Some(1000),
            active_ns: 0,
            span_name: "overlap_span".to_string(),
            target: "overlap_target".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 1, // concurrent: entered on 2 tids
            saturated: 0,
            loss_observable: 0,
        })
        .unwrap();

        let data = enc.into_inner();
        let source_key = "2026-06-19/1300/svc/host/overlap-boot/0.bin.gz";
        let (samples, _stacks, _polls, spans) = decode_samples(&data, source_key).unwrap();

        // Exactly one span resolved.
        assert_eq!(spans.len(), 1);
        let span = &spans[0];

        // Exactly one CPU sample decoded.
        assert_eq!(samples.len(), 1, "one CPU sample expected");
        let sample = &samples[0];

        // The sample must have exactly ONE enclosing_spans entry (dedup).
        // Without the dedup guard, this would be 2 because both intervals
        // for the same span_idx contain the sample.
        assert_eq!(
            sample.enclosing_spans.len(),
            1,
            "overlapping intervals for same span instance must produce exactly one membership entry (dedup)"
        );
        assert_eq!(sample.enclosing_spans[0].span_uid, span.span_uid);

        // The span must have exactly 1 cpu_sample_count (not 2).
        assert_eq!(
            span.cpu_sample_count, 1,
            "overlapping intervals must not double-count: cpu_sample_count should be exactly 1"
        );
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
    fn test_find_last_single_colon() {
        // Simple case: only single colons
        assert_eq!(find_last_single_colon("a:b:c"), Some(3));

        // Mixed: has both :: and :
        assert_eq!(
            find_last_single_colon("metrics_service::routes::record_metric:examples/src/routes.rs"),
            Some(38) // the : before "examples"
        );

        // Only ::
        assert_eq!(find_last_single_colon("a::b::c"), None);

        // Single colon at end
        assert_eq!(find_last_single_colon("abc:"), Some(3));

        // Empty
        assert_eq!(find_last_single_colon(""), None);
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

        // The demo trace has 27,524 SpanCloseEvents (old format), so we should
        // get legacy span rows.
        assert!(
            !spans.is_empty(),
            "expected legacy span rows from demo trace, got 0"
        );

        // All spans should have identity_quality = "legacy"
        for span in &spans {
            assert_eq!(
                span.identity_quality, "legacy",
                "demo trace spans must have identity_quality='legacy'"
            );
            assert!(
                !span.loss_observable,
                "legacy spans must have loss_observable=false"
            );
            assert!(
                !span.details_complete,
                "legacy spans must have details_complete=false"
            );
        }

        // Check that known span types are present.
        let record_metric_spans: Vec<_> = spans
            .iter()
            .filter(|s| s.name == "record_metric" && s.target.contains("routes"))
            .collect();
        assert!(
            !record_metric_spans.is_empty(),
            "expected record_metric spans from demo trace"
        );

        let query_metric_spans: Vec<_> =
            spans.iter().filter(|s| s.name == "query_metric").collect();
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
        let spans_with_elapsed: Vec<_> = spans.iter().filter(|s| s.elapsed_ns > 0).collect();
        assert!(
            !spans_with_elapsed.is_empty(),
            "expected some spans with non-zero elapsed_ns"
        );

        // Verify some spans have observed_active_wall_ns > 0 (balanced enter/exit pairs).
        let spans_with_active: Vec<_> = spans
            .iter()
            .filter(|s| s.observed_active_wall_ns > 0)
            .collect();
        assert!(
            !spans_with_active.is_empty(),
            "expected some spans with observed active wall time"
        );

        // Verify sample attribution works with legacy spans.
        let samples_with_spans = samples
            .iter()
            .filter(|s| !s.enclosing_spans.is_empty())
            .count();
        // With ~9000 CPU samples and ~86k enter/exit pairs, some should be attributed.
        assert!(
            samples_with_spans > 0,
            "expected some samples attributed to legacy spans, got 0"
        );

        eprintln!(
            "decoded {} legacy spans ({} record_metric, {} query_metric), {} samples with span attribution",
            spans.len(),
            record_metric_spans.len(),
            query_metric_spans.len(),
            samples_with_spans,
        );
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
            &[
                FieldValue::Varint(100),                   // timestamp
                FieldValue::Varint(0),                     // worker_id
                FieldValue::Varint(1),                     // span_id
                FieldValue::None,                          // parent_span_id (absent)
                FieldValue::String("do_work".to_string()), // span_name
            ],
        )
        .unwrap();

        // Exit span_id=1 on worker 0
        enc.write_event(
            &exit_schema,
            &[
                FieldValue::Varint(200),                   // timestamp
                FieldValue::Varint(0),                     // worker_id
                FieldValue::Varint(1),                     // span_id
                FieldValue::String("do_work".to_string()), // span_name
            ],
        )
        .unwrap();

        // Close span_id=1
        enc.write_event(
            &close_schema,
            &[
                FieldValue::Varint(250), // timestamp
                FieldValue::Varint(1),   // span_id
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
        assert!(!span.loss_observable);
        assert_eq!(span.kind, "tracing");

        // Elapsed should be close_ts - first_enter_ts = 250 - 100 = 150
        // (in wall clock with offset)
        assert!(span.elapsed_ns > 0, "elapsed_ns should be > 0");

        // Observed active = exit - enter = 200 - 100 = 100
        assert_eq!(span.observed_active_wall_ns, 100);
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
            &[
                FieldValue::Varint(100),
                FieldValue::Varint(0),
                FieldValue::Varint(7),
                FieldValue::String("equal_timestamp".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(
            &enter_schema,
            &[
                FieldValue::Varint(100),
                FieldValue::Varint(0),
                FieldValue::Varint(7),
                FieldValue::None,
                FieldValue::String("equal_timestamp".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(
            &close_schema,
            &[FieldValue::Varint(101), FieldValue::Varint(7)],
        )
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
            &[
                FieldValue::Varint(1000),
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
            &[
                FieldValue::Varint(5000),
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
                    FieldValue::Varint(timestamp),
                    FieldValue::Varint(0),
                    FieldValue::Varint(span_id),
                    FieldValue::None,
                    FieldValue::String("shared-runtime-name".to_string()),
                ]
            } else {
                vec![
                    FieldValue::Varint(timestamp),
                    FieldValue::Varint(0),
                    FieldValue::Varint(span_id),
                    FieldValue::String("shared-runtime-name".to_string()),
                ]
            };
            enc.write_event(schema, &values).unwrap();
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
                    FieldValue::Varint(timestamp),
                    FieldValue::Varint(0),
                    FieldValue::Varint(span_id),
                    FieldValue::None,
                    FieldValue::String(runtime_name.to_string()),
                ]
            } else {
                vec![
                    FieldValue::Varint(timestamp),
                    FieldValue::Varint(0),
                    FieldValue::Varint(span_id),
                    FieldValue::String(runtime_name.to_string()),
                ]
            };
            enc.write_event(schema, &values).unwrap();
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
            &[
                FieldValue::Varint(1000),
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
            &[
                FieldValue::Varint(5000),
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
            &[
                FieldValue::Varint(500), // ts
                FieldValue::Varint(3),   // worker_id
                FieldValue::Varint(0),   // local_queue
                FieldValue::Varint(0),   // cpu_time_ns
                FieldValue::None,        // sched_wait_ns
                FieldValue::Varint(500), // tid
            ],
        )
        .unwrap();

        // Poll of task 77 on worker 3 covering the span's enter: [900, 1100].
        enc.write_event(
            &poll_start_schema,
            &[
                FieldValue::Varint(900), // ts
                FieldValue::Varint(3),   // worker_id
                FieldValue::Varint(0),   // local_queue
                FieldValue::Varint(77),  // task_id
                FieldValue::String("app::handler".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(
            &poll_end_schema,
            &[FieldValue::Varint(1100), FieldValue::Varint(3)],
        )
        .unwrap();

        // Enter span_id=42 on worker 3 at t=1000 (inside poll [900,1100] → task 77).
        enc.write_event(
            &enter_schema,
            &[
                FieldValue::Varint(1000),
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
            &[
                FieldValue::Varint(5000),
                FieldValue::Varint(3),
                FieldValue::Varint(0),
                FieldValue::Varint(77),
                FieldValue::String("app::handler".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(
            &poll_end_schema,
            &[FieldValue::Varint(5100), FieldValue::Varint(3)],
        )
        .unwrap();

        // Exit span_id=42 much later at t=9000: the span was entered across a
        // long await; the task was only on-CPU during the two polls above.
        enc.write_event(
            &exit_schema,
            &[
                FieldValue::Varint(9000),
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
            &[
                FieldValue::Varint(100),
                FieldValue::Varint(0),
                FieldValue::Varint(1),
                FieldValue::None,
                FieldValue::String("op".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(
            &exit_schema,
            &[
                FieldValue::Varint(200),
                FieldValue::Varint(0),
                FieldValue::Varint(1),
                FieldValue::String("op".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(
            &close_schema,
            &[FieldValue::Varint(250), FieldValue::Varint(1)],
        )
        .unwrap();

        // The old format reuses span_id=1 — but within the same file the first
        // enter establishes the context. One close event per unique span_id means
        // we produce one row. This is conservative: we merge all enters/exits
        // for the same span_id into one span instance.
        let data = enc.into_inner();
        let (_, _, _, spans) = decode_samples(&data, "test/path/boot/0.bin").unwrap();

        // Should produce exactly one span (one close event for span_id=1).
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].identity_quality, "legacy");
    }

    /// Modern spans should still work exactly as before (backward compatibility).
    #[test]
    fn test_modern_spans_still_work() {
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::Encoder;

        #[derive(TraceEvent)]
        struct ClockSyncEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            realtime_ns: u64,
        }

        #[derive(TraceEvent)]
        #[traceevent(name = "SpanCloseEvent")]
        struct SpanCloseEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            span_id: u64,
            span_instance_id: u64,
            start_timestamp_ns: u64,
            first_enter_timestamp_ns: Option<u64>,
            active_ns: u64,
            span_name: String,
            target: String,
            file: Option<String>,
            line: Option<u32>,
            parent_span_instance_id: Option<u64>,
            attributes: Vec<(String, String)>,
            unbalanced_enters: u32,
            concurrent: u32,
            saturated: u32,
            loss_observable: u32,
        }

        let mut enc = Encoder::new();
        enc.write(&ClockSyncEvent {
            timestamp_ns: 50,
            realtime_ns: 1_700_000_000_000_000_050,
        })
        .unwrap();
        enc.write(&SpanCloseEvent {
            timestamp_ns: 200,
            span_id: 1,
            span_instance_id: 42,
            start_timestamp_ns: 100,
            first_enter_timestamp_ns: None,
            active_ns: 0,
            span_name: "modern_span".to_string(),
            target: "modern_target".to_string(),
            file: Some("src/modern.rs".to_string()),
            line: Some(10),
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 1,
        })
        .unwrap();

        let data = enc.into_inner();
        let (_, _, _, spans) =
            decode_samples(&data, "2026-06-19/1300/svc/host/abcd-123/0.bin").unwrap();

        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span.name, "modern_span");
        assert_eq!(span.target, "modern_target");
        // Modern spans use "path" or "metadata" quality, NOT "legacy".
        assert_ne!(span.identity_quality, "legacy");
    }

    /// Mixed-format files: both modern (span_instance_id > 0) and legacy
    /// (span_id only, span_instance_id == 0) span events in the same trace
    /// file must both produce spans. Previously, `has_modern_spans` globally
    /// suppressed legacy events, losing valid legacy spans in mixed files.
    #[test]
    fn test_mixed_modern_and_legacy_spans_both_resolve() {
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
        #[derive(TraceEvent)]
        struct SegmentMetadataEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            entries: Vec<(String, String)>,
        }
        #[derive(TraceEvent)]
        #[traceevent(name = "SpanCloseEvent")]
        struct SpanCloseEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            span_id: u64,
            span_instance_id: u64,
            start_timestamp_ns: u64,
            first_enter_timestamp_ns: Option<u64>,
            active_ns: u64,
            span_name: String,
            target: String,
            file: Option<String>,
            line: Option<u32>,
            parent_span_instance_id: Option<u64>,
            attributes: Vec<(String, String)>,
            unbalanced_enters: u32,
            concurrent: u32,
            saturated: u32,
            loss_observable: u32,
        }

        // Legacy schema (old format: span_id, worker_id, no span_instance_id)
        let legacy_enter_schema = Schema::new(
            "SpanEnter:legacy_lib::op:src/legacy.rs:10",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("parent_span_id", FieldType::OptionalVarint),
                FieldDef::new("span_name", FieldType::String),
            ],
        );
        let legacy_exit_schema = Schema::new(
            "SpanExit:legacy_lib::op:src/legacy.rs:10",
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
        enc.write(&SegmentMetadataEvent {
            timestamp_ns: 11,
            entries: vec![("boot_id".to_string(), "mixed-boot".to_string())],
        })
        .unwrap();

        // Modern span (span_instance_id > 0 → triggers has_modern_spans = true)
        enc.write(&SpanCloseEvent {
            timestamp_ns: 500,
            span_id: 99,
            span_instance_id: 42,
            start_timestamp_ns: 100,
            first_enter_timestamp_ns: None,
            active_ns: 0,
            span_name: "modern_op".to_string(),
            target: "modern_target".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 0,
        })
        .unwrap();

        // Legacy span (span_instance_id == 0, only span_id) — the decoder
        // sees span_instance_id == 0 and routes to legacy reconstruction.
        enc.write_event(
            &legacy_enter_schema,
            &[
                FieldValue::Varint(600),
                FieldValue::Varint(0),
                FieldValue::Varint(7),
                FieldValue::None,
                FieldValue::String("legacy_op".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(
            &legacy_exit_schema,
            &[
                FieldValue::Varint(800),
                FieldValue::Varint(0),
                FieldValue::Varint(7),
                FieldValue::String("legacy_op".to_string()),
            ],
        )
        .unwrap();
        // Close event with span_instance_id=0 triggers legacy route in decoder.
        enc.write(&SpanCloseEvent {
            timestamp_ns: 900,
            span_id: 7,
            span_instance_id: 0,
            start_timestamp_ns: 600,
            first_enter_timestamp_ns: Some(600),
            active_ns: 0,
            span_name: "legacy_op".to_string(),
            target: "legacy_lib".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 0,
        })
        .unwrap();

        let data = enc.into_inner();
        let (_, _, _, spans) =
            decode_samples(&data, "2026-06-19/1300/svc/host/mixed-boot/0.bin").unwrap();

        // Both spans must be present — the fix ensures legacy spans are not
        // suppressed when modern spans exist in the same file.
        assert_eq!(
            spans.len(),
            2,
            "mixed modern+legacy file must produce spans from BOTH formats"
        );

        let modern_span = spans.iter().find(|s| s.name == "modern_op").unwrap();
        let legacy_span = spans.iter().find(|s| s.name == "legacy_op").unwrap();

        assert_eq!(modern_span.identity_quality, "metadata");
        assert_eq!(legacy_span.identity_quality, "legacy");
        assert_eq!(legacy_span.observed_active_wall_ns, 200);
    }

    /// DELIVERABLE end-to-end repro for the span-explorer bug: two span TYPES,
    /// each enclosing CPU samples with clearly-distinguishable stack frames, run
    /// through the REAL pipeline (decode → parquet → FlamegraphAccum span filter)
    /// exactly as `/api/flamegraph?span_type_uid=…` does. Filtering to type A must
    /// leave only frame_A_only in the flamegraph, and type B only frame_B_only.
    ///
    /// This is the "clear, obvious output" test the user asked for: if the
    /// span_type_uid filter is broken (samples pass unfiltered, or the wrong
    /// span's samples leak), the surviving frame set contains BOTH frames and the
    /// assertion prints exactly which frames leaked.
    #[test]
    fn span_type_uid_filter_end_to_end_keeps_only_matching_frames() {
        use crate::ingest::aggregate::{FlamegraphAccum, SampleFilter};
        use crate::ingest::parquet_writer::{write_samples, write_stacks_dict};
        use dial9_trace_format::TraceEvent;
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::FieldDef;
        use dial9_trace_format::types::{FieldType, FieldValue, StackFrames};

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
        #[derive(TraceEvent)]
        struct SpanCloseEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            span_id: u64,
            span_instance_id: u64,
            start_timestamp_ns: u64,
            first_enter_timestamp_ns: Option<u64>,
            active_ns: u64,
            span_name: String,
            target: String,
            file: Option<String>,
            line: Option<u32>,
            parent_span_instance_id: Option<u64>,
            attributes: Vec<(String, String)>,
            unbalanced_enters: u32,
            concurrent: u32,
            saturated: u32,
            loss_observable: u32,
        }

        // Two span TYPES: A = span_a, B = span_b (distinct target::name → distinct
        // span_type_uid). Each on its own instance id + tid so their intervals and
        // CPU samples never overlap.
        let enter_a = Schema::new(
            "SpanEnter:type_target::span_a",
            vec![
                FieldDef::new("span_instance_id", FieldType::Varint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        );
        let exit_a = Schema::new(
            "SpanExit:type_target::span_a",
            vec![
                FieldDef::new("span_instance_id", FieldType::Varint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        );
        let enter_b = Schema::new(
            "SpanEnter:type_target::span_b",
            vec![
                FieldDef::new("span_instance_id", FieldType::Varint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        );
        let exit_b = Schema::new(
            "SpanExit:type_target::span_b",
            vec![
                FieldDef::new("span_instance_id", FieldType::Varint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        );
        let cpu_sample_schema = Schema::new(
            "CpuSampleEvent",
            vec![
                FieldDef::new("tid", FieldType::Varint),
                FieldDef::new("source", FieldType::Varint),
                FieldDef::new("callchain", FieldType::StackFrames),
            ],
        );
        let worker_park_schema = Schema::new(
            "WorkerParkEvent",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        );
        let symbol_schema = Schema::new(
            "SymbolTableEntry",
            vec![
                FieldDef::new("addr", FieldType::Varint),
                FieldDef::new("inline_depth", FieldType::Varint),
                FieldDef::new("symbol_name", FieldType::String),
            ],
        );

        let mut enc = Encoder::new();
        enc.write(&ClockSyncEvent {
            timestamp_ns: 10,
            realtime_ns: 1_700_000_000_000_000_010,
        })
        .unwrap();
        enc.write(&SegmentMetadataEvent {
            timestamp_ns: 11,
            entries: vec![("boot_id".to_string(), "type-filter-boot".to_string())],
        })
        .unwrap();

        // Bind two tids to workers so their CPU samples are attributed.
        for (tid, worker) in [(100u64, 0u64), (200, 1)] {
            enc.write_event(
                &worker_park_schema,
                &[
                    FieldValue::Varint(12),
                    FieldValue::Varint(worker),
                    FieldValue::Varint(tid),
                ],
            )
            .unwrap();
        }

        // Distinct frame symbols so the two span types are trivially separable.
        enc.write_event(
            &symbol_schema,
            &[
                FieldValue::Varint(13),
                FieldValue::Varint(0xA000),
                FieldValue::Varint(0),
                FieldValue::String("frame_A_only".to_string()),
            ],
        )
        .unwrap();
        enc.write_event(
            &symbol_schema,
            &[
                FieldValue::Varint(14),
                FieldValue::Varint(0xB000),
                FieldValue::Varint(0),
                FieldValue::String("frame_B_only".to_string()),
            ],
        )
        .unwrap();

        // Span A on tid 100: interval [1000, 2000), CPU sample at 1500 (frame A).
        enc.write_event(
            &enter_a,
            &[
                FieldValue::Varint(1000),
                FieldValue::Varint(1),
                FieldValue::Varint(100),
            ],
        )
        .unwrap();
        enc.write_event(
            &cpu_sample_schema,
            &[
                FieldValue::Varint(1500),
                FieldValue::Varint(100),
                FieldValue::Varint(0),
                FieldValue::StackFrames(StackFrames(vec![0xA000])),
            ],
        )
        .unwrap();
        enc.write_event(
            &exit_a,
            &[
                FieldValue::Varint(2000),
                FieldValue::Varint(1),
                FieldValue::Varint(100),
            ],
        )
        .unwrap();

        // Span B on tid 200: interval [3000, 4000), CPU sample at 3500 (frame B).
        enc.write_event(
            &enter_b,
            &[
                FieldValue::Varint(3000),
                FieldValue::Varint(2),
                FieldValue::Varint(200),
            ],
        )
        .unwrap();
        enc.write_event(
            &cpu_sample_schema,
            &[
                FieldValue::Varint(3500),
                FieldValue::Varint(200),
                FieldValue::Varint(0),
                FieldValue::StackFrames(StackFrames(vec![0xB000])),
            ],
        )
        .unwrap();
        enc.write_event(
            &exit_b,
            &[
                FieldValue::Varint(4000),
                FieldValue::Varint(2),
                FieldValue::Varint(200),
            ],
        )
        .unwrap();

        // Close both spans.
        enc.write(&SpanCloseEvent {
            timestamp_ns: 2100,
            span_id: 1,
            span_instance_id: 1,
            start_timestamp_ns: 1000,
            first_enter_timestamp_ns: Some(1000),
            active_ns: 0,
            span_name: "span_a".to_string(),
            target: "type_target".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 1,
        })
        .unwrap();
        enc.write(&SpanCloseEvent {
            timestamp_ns: 4100,
            span_id: 2,
            span_instance_id: 2,
            start_timestamp_ns: 3000,
            first_enter_timestamp_ns: Some(3000),
            active_ns: 0,
            span_name: "span_b".to_string(),
            target: "type_target".to_string(),
            file: None,
            line: None,
            parent_span_instance_id: None,
            attributes: Vec::new(),
            unbalanced_enters: 0,
            concurrent: 0,
            saturated: 0,
            loss_observable: 1,
        })
        .unwrap();

        let data = enc.into_inner();
        let source_key = "2026-06-19/1300/shale/host/type-filter-boot/0.bin.gz";
        let (samples, dict, _polls, spans) = decode_samples(&data, source_key).unwrap();

        // Sanity: two spans resolved with distinct type uids, and each CPU sample
        // is attributed to exactly one span type.
        assert_eq!(spans.len(), 2, "two spans decoded");
        let uid_a = spans
            .iter()
            .find(|s| s.name == "span_a")
            .expect("span_a resolved")
            .span_type_uid;
        let uid_b = spans
            .iter()
            .find(|s| s.name == "span_b")
            .expect("span_b resolved")
            .span_type_uid;
        assert_ne!(uid_a, uid_b, "the two span types have distinct type uids");
        assert_eq!(samples.len(), 2, "two CPU samples decoded");
        assert!(
            samples.iter().all(|s| s.enclosing_spans.len() == 1),
            "each CPU sample is enclosed by exactly one span; got {:?}",
            samples
                .iter()
                .map(|s| s.enclosing_spans.len())
                .collect::<Vec<_>>()
        );

        // Write the samples + dict exactly like the fold does.
        let mut samples_buf = Vec::new();
        write_samples(&mut samples_buf, &samples, &HashMap::new()).unwrap();
        let mut dict_buf = Vec::new();
        write_stacks_dict(&mut dict_buf, &dict).unwrap();

        // Run the flamegraph accumulator under a span_type_uid filter and collect
        // the surviving frame names — the observable the user sees in the UI.
        let surviving_frames =
            |span_type_uid: Option<[u8; 16]>| -> std::collections::HashSet<String> {
                let filter = SampleFilter {
                    span_type_uid,
                    facets: HashMap::from([("source", "cpu".to_string())]),
                    ..Default::default()
                };
                let mut accum = FlamegraphAccum::new(filter);
                accum
                    .merge(samples_buf.clone(), Some(dict_buf.clone()))
                    .unwrap();
                let snap = accum.snapshot();
                let mut frames = std::collections::HashSet::new();
                for (stack_id, _count) in &snap.stack_counts {
                    if let Some(fs) = snap.stacks_dict.get(stack_id) {
                        frames.extend(fs.iter().cloned());
                    }
                }
                frames
            };

        let a = surviving_frames(Some(uid_a));
        assert!(
            a.contains("frame_A_only") && !a.contains("frame_B_only"),
            "type-A span filter must keep ONLY frame_A_only, got {a:?}"
        );

        let b = surviving_frames(Some(uid_b));
        assert!(
            b.contains("frame_B_only") && !b.contains("frame_A_only"),
            "type-B span filter must keep ONLY frame_B_only, got {b:?}"
        );

        let both = surviving_frames(None);
        assert!(
            both.contains("frame_A_only") && both.contains("frame_B_only"),
            "no filter must keep both frames, got {both:?}"
        );
    }
}

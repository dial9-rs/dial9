//! Wire-event decoding types and legacy schema-name parsing.
//!
//! These structs intentionally deserialize only fields the aggregation decoder
//! consumes. Raw timestamp fields are converted to clock-domain newtypes as
//! soon as orchestration leaves the wire edge.

use dial9_trace_format::decoder::Decoder;
use lasso::{Rodeo, Spur};
use rustc_hash::FxHashMap;
use serde::Deserialize;

use super::clock::ClockOffset;

// ─── Lightweight serde structs (select only needed fields) ───────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct CpuSample {
    pub(crate) timestamp_ns: u64,
    pub(crate) tid: u32,
    pub(crate) source: u64,
    pub(crate) callchain: Vec<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkerPark {
    pub(crate) timestamp_ns: u64,
    pub(crate) worker_id: u64,
    pub(crate) tid: u32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkerUnpark {
    pub(crate) timestamp_ns: u64,
    pub(crate) worker_id: u64,
    pub(crate) tid: u32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PollStart {
    pub(crate) timestamp_ns: u64,
    pub(crate) worker_id: u64,
    pub(crate) task_id: u64,
    #[serde(default)]
    pub(crate) spawn_loc: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PollEnd {
    pub(crate) timestamp_ns: u64,
    pub(crate) worker_id: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClockSync {
    pub(crate) timestamp_ns: u64,
    pub(crate) realtime_ns: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SymbolEntry {
    pub(crate) addr: u64,
    pub(crate) inline_depth: u64,
    pub(crate) symbol_name: String,
}

/// Legacy span enter event from old producers.
///
/// `worker_id` is on the wire but intentionally unused: spans are paired
/// enter↔exit by `span_id` alone, because a task can migrate workers between
/// enter and exit (see `resolve_legacy_spans`).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct LegacySpanEnterEvent {
    pub(crate) timestamp_ns: u64,
    #[serde(default)]
    pub(crate) worker_id: u64,
    #[serde(default)]
    pub(crate) span_id: u64,
    #[serde(default)]
    pub(crate) parent_span_id: Option<u64>,
    #[serde(default)]
    pub(crate) span_name: Option<String>,
    /// Wire decode order for deterministic equal-timestamp pairing.
    #[serde(skip)]
    pub(crate) decode_sequence: u64,
}

/// Legacy span exit event from old producers.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct LegacySpanExitEvent {
    pub(crate) timestamp_ns: u64,
    #[serde(default)]
    pub(crate) worker_id: u64,
    #[serde(default)]
    pub(crate) span_id: u64,
    #[serde(default)]
    pub(crate) span_name: Option<String>,
    /// Wire decode order for deterministic equal-timestamp pairing.
    #[serde(skip)]
    pub(crate) decode_sequence: u64,
}

/// Legacy span close event from old producers (only has span_id).
#[derive(Debug, Deserialize)]
pub(crate) struct LegacySpanCloseEvent {
    pub(crate) timestamp_ns: u64,
    #[serde(default)]
    pub(crate) span_id: u64,
}

/// Metadata parsed from the SpanEnter schema name for old-format events.
/// Schema name format: `SpanEnter:{target}::{name}:{file}:{line}`
#[derive(Debug, Clone)]
pub(crate) struct LegacySpanSchemaInfo {
    pub(crate) target: String,
    pub(crate) name: String,
    pub(crate) file: Option<String>,
    pub(crate) line: Option<u32>,
}

/// Parse type/callsite identity from an old-format SpanEnter or SpanExit schema
/// name. Dynamic schemas use
/// `Span{Enter|Exit}:{target}::{name}:{file}:{line}`; struct-derived schemas use
/// `Span{Enter|Exit}__{Type}` and provide only the stable type suffix.
///
/// The format is `{prefix}:{target}::{name}:{file}:{line}` where:
/// - prefix is "SpanEnter" or "SpanExit"
/// - target is the module path (may contain `::`)
/// - name is the span name (after the LAST `::` before the file)
/// - file is the source file path (may contain `:`)
/// - line is the numeric line number at the end
pub(crate) fn parse_legacy_span_schema_name(schema_name: &str) -> Option<LegacySpanSchemaInfo> {
    // Struct-derived schemas cannot contain `:`. Their `__` suffix is the only
    // stable type discriminator available when different event structs reuse
    // the same runtime span_name.
    if let Some(name) = schema_name
        .strip_prefix("SpanEnter__")
        .or_else(|| schema_name.strip_prefix("SpanExit__"))
        .filter(|name| !name.is_empty())
    {
        return Some(LegacySpanSchemaInfo {
            target: String::new(),
            name: name.to_string(),
            file: None,
            line: None,
        });
    }

    // Strip the "SpanEnter:" or "SpanExit:" prefix
    let rest = schema_name
        .strip_prefix("SpanEnter:")
        .or_else(|| schema_name.strip_prefix("SpanExit:"))?;

    // The format after prefix is: {target}::{name}:{file}:{line}
    // We need to find the last `:` that's followed by only digits (the line number),
    // then the `:` before that separates file from name::target.
    //
    // Strategy: split from the right. The line number is the last segment after `:`.
    // The file path is everything between the second-to-last `:` and the line `:`.
    // But file paths can contain `:` on Windows, so we use a different approach:
    //
    // Find the last `:digits` suffix for line number. Then the segment before that
    // (back to the previous `:`) is the file. Everything before that is target::name.

    // Find the last `:` followed by only digits
    let line_colon_pos = rest.rfind(':')?;
    let line_str = &rest[line_colon_pos + 1..];
    let line: u32 = line_str.parse().ok()?;

    let before_line = &rest[..line_colon_pos];

    // The file is the segment between the previous `:` and line_colon_pos
    // But we need to be careful: the format is `target::name:file:line`
    // where `target::name` uses `::` as separator.
    // After stripping line, we have `target::name:file`.
    // The file is separated from name by a single `:` (not `::`).
    // Find the last single `:` that is NOT part of a `::`.
    let file_colon_pos = find_last_single_colon(before_line)?;

    let target_name = &before_line[..file_colon_pos];
    let file = &before_line[file_colon_pos + 1..];

    // Split target_name on the LAST `::` to get target and name.
    let (target, name) = if let Some(last_dcolon) = target_name.rfind("::") {
        (&target_name[..last_dcolon], &target_name[last_dcolon + 2..])
    } else {
        // No `::` found — treat the whole thing as the name with empty target
        ("", target_name)
    };

    Some(LegacySpanSchemaInfo {
        target: target.to_string(),
        name: name.to_string(),
        file: if file.is_empty() {
            None
        } else {
            Some(file.to_string())
        },
        line: Some(line),
    })
}

/// Find the last `:` in `s` that is NOT part of a `::` sequence.
/// Returns the byte offset of the single colon, or None if not found.
pub(crate) fn find_last_single_colon(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        if bytes[i] == b':' {
            // Check it's not part of `::`
            let preceded_by_colon = i > 0 && bytes[i - 1] == b':';
            let followed_by_colon = i + 1 < bytes.len() && bytes[i + 1] == b':';
            if !preceded_by_colon && !followed_by_colon {
                return Some(i);
            }
            // If it's part of `::`, skip both colons
            if preceded_by_colon {
                i -= 1; // skip the preceding `:`
            }
        }
    }
    None
}

// ─── Parsed event enum (sorted by timestamp) ────────────────────────────────

pub(crate) enum TraceEvent {
    CpuSample(CpuSample),
    WorkerPark(WorkerPark),
    WorkerUnpark(WorkerUnpark),
    PollStart(PollStart),
    PollEnd(PollEnd),
}

impl TraceEvent {
    pub(crate) fn timestamp_ns(&self) -> u64 {
        match self {
            Self::CpuSample(e) => e.timestamp_ns,
            Self::WorkerPark(e) => e.timestamp_ns,
            Self::WorkerUnpark(e) => e.timestamp_ns,
            Self::PollStart(e) => e.timestamp_ns,
            Self::PollEnd(e) => e.timestamp_ns,
        }
    }
}

/// All information collected in one pass over the self-describing wire stream.
pub(crate) struct DecodedTrace {
    pub(crate) interner: Rodeo,
    pub(crate) addr_to_keys: FxHashMap<u64, Vec<(u64, Spur)>>,
    pub(crate) events: Vec<TraceEvent>,
    pub(crate) clock_offset: Option<ClockOffset>,
    pub(crate) segment_metadata_boot_id: Option<String>,
    pub(crate) legacy_enters: Vec<(String, LegacySpanEnterEvent)>,
    pub(crate) legacy_exits: Vec<(String, LegacySpanExitEvent)>,
    pub(crate) legacy_closes: Vec<LegacySpanCloseEvent>,
}

/// Decode relevant wire events without imposing downstream ordering or attribution.
pub(crate) fn decode_trace(data: &[u8], source_key: &str) -> anyhow::Result<DecodedTrace> {
    let mut decoder = Decoder::new(data).ok_or_else(|| anyhow::anyhow!("invalid trace header"))?;

    let mut interner = Rodeo::default();
    let mut addr_to_keys: FxHashMap<u64, Vec<(u64, Spur)>> = FxHashMap::default();
    let mut events: Vec<TraceEvent> = Vec::new();
    let mut clock_offset: Option<ClockOffset> = None;
    // Authoritative boot_id decoded from SegmentMetadata entries. When the
    // namespace isolation layer is active, the writer stamps a
    // `boot_id` key in segment metadata. This is the stable cross-segment
    // process identity anchor. When absent, we fall back to the path-based
    // extraction which cannot claim cross-file stability.
    let mut segment_metadata_boot_id: Option<String> = None;
    // Monotonically increasing counter assigned to span enter/exit events in
    // wire decode order. Used to break timestamp ties deterministically.
    let mut span_decode_sequence: u64 = 0;

    // Span events: SpanEnter/Exit with span_id + worker_id and SpanCloseEvent
    // with only span_id. These are reconstructed into ResolvedSpan rows using
    // local context (`resolve_legacy_spans`).
    let mut legacy_enters: Vec<(String, LegacySpanEnterEvent)> = Vec::new(); // (schema_name, event)
    let mut legacy_exits: Vec<(String, LegacySpanExitEvent)> = Vec::new();
    let mut legacy_closes: Vec<LegacySpanCloseEvent> = Vec::new();
    let mut legacy_enter_decode_errors: u64 = 0;
    let mut legacy_exit_decode_errors: u64 = 0;
    let mut legacy_close_decode_errors: u64 = 0;
    decoder
        .for_each_event(|ev| match ev.name {
            "ClockSyncEvent" => {
                if let Ok(cs) = ev.deserialize::<ClockSync>()
                    && cs.realtime_ns > 0
                    && cs.timestamp_ns > 0
                    && clock_offset.is_none()
                {
                    clock_offset = Some(ClockOffset::from_clock_sync(
                        cs.realtime_ns,
                        cs.timestamp_ns,
                    ));
                }
            }
            "SegmentMetadataEvent" => {
                // Decode segment metadata to extract the authoritative boot_id.
                // SegmentMetadataEvent has entries: HashMap<String, String>.
                #[derive(serde::Deserialize)]
                struct SegmentMeta {
                    #[serde(default)]
                    entries: std::collections::HashMap<String, String>,
                }
                if let Ok(meta) = ev.deserialize::<SegmentMeta>()
                    && segment_metadata_boot_id.is_none()
                    && let Some(bid) = meta.entries.get("boot_id")
                    && !bid.is_empty()
                {
                    segment_metadata_boot_id = Some(bid.clone());
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
            // Span close: the old producer's `SpanCloseEvent` carries only
            // `span_id`. Also accept the struct-derived `SpanClose__{Type}`
            // convention (a Rust identifier cannot contain `:`).
            name if name == "SpanCloseEvent" || name.starts_with("SpanClose__") => {
                match ev.deserialize::<LegacySpanCloseEvent>() {
                    Ok(lc) if lc.span_id > 0 => {
                        legacy_closes.push(lc);
                    }
                    _ => {
                        legacy_close_decode_errors += 1;
                    }
                }
            }
            // Span enter/exit events for local interval tracking.
            //
            // Two on-wire naming conventions carry the same events:
            //   - `SpanEnter:{target}::{name}:{file}:{line}` — the dynamic schema
            //     name emitted by the tracing layer (colon-separated).
            //   - `SpanEnter__{Type}` — a struct-derived event (e.g.
            //     `SpanEnter__ShaleOperation`). A Rust identifier cannot contain
            //     `:`, so struct-named events use the `__` separator instead.
            // We accept both, mirroring the viewer's `buildSpanData`.
            name if name.starts_with("SpanEnter:") || name.starts_with("SpanEnter__") => {
                match ev.deserialize::<LegacySpanEnterEvent>() {
                    Ok(mut le) if le.span_id > 0 => {
                        le.decode_sequence = span_decode_sequence;
                        span_decode_sequence += 1;
                        legacy_enters.push((name.to_string(), le));
                    }
                    _ => {
                        legacy_enter_decode_errors += 1;
                    }
                }
            }
            name if name.starts_with("SpanExit:") || name.starts_with("SpanExit__") => {
                match ev.deserialize::<LegacySpanExitEvent>() {
                    Ok(mut le) if le.span_id > 0 => {
                        le.decode_sequence = span_decode_sequence;
                        span_decode_sequence += 1;
                        legacy_exits.push((name.to_string(), le));
                    }
                    _ => {
                        legacy_exit_decode_errors += 1;
                    }
                }
            }
            _ => {}
        })
        .map_err(|e| anyhow::anyhow!("decode error: {e}"))?;

    if legacy_close_decode_errors > 0
        || legacy_enter_decode_errors > 0
        || legacy_exit_decode_errors > 0
    {
        use dial9_core::rate_limited;
        rate_limited!(std::time::Duration::from_secs(60), {
            tracing::warn!(
                source_key,
                legacy_enter_errors = legacy_enter_decode_errors,
                legacy_exit_errors = legacy_exit_decode_errors,
                legacy_close_errors = legacy_close_decode_errors,
                "skipped malformed span event(s) during decode"
            );
        });
    }

    Ok(DecodedTrace {
        interner,
        addr_to_keys,
        events,
        clock_offset,
        segment_metadata_boot_id,
        legacy_enters,
        legacy_exits,
        legacy_closes,
    })
}

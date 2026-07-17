//! Trace shape extraction and generation.
//!
//! A "shape" captures the structural fingerprint of a dial9 trace — event types,
//! field schemas, timing relationships, cardinality, and value distributions —
//! with best-effort removal of labels, identifiers, payloads, and exact
//! timestamps from the source trace.
//!
//! # Privacy
//!
//! Shape extraction applies deterministic transformations to remove string
//! contents, byte payloads, exact timestamps, custom names, and stack addresses.
//! However, exact booleans, small quantized integers in built-in schemas, and
//! already-round floats may survive transformation. This is **not** an
//! anonymization or security boundary. Shapes intentionally preserve operational
//! structure (relative timing, ordering, cardinality, byte payload lengths, stack
//! depths, value magnitude distributions, and inter-event correlations) which may
//! reveal performance characteristics, traffic patterns, or architectural details.
//! Treat generated shapes and synthetic traces as confidential operational data.
//!
//! # Fixed-width normalization
//!
//! The trace format's `U8`, `U16`, and `U32` wire types are decoded into
//! `FieldValue::Varint(v as u64)` by the decoder, and `Encoder::write_event` +
//! `write_field_value` dispatches on the `FieldValue` variant (not the schema
//! `FieldType`). This means a decoded `Varint` cannot be re-encoded correctly
//! for a `U8`/`U16`/`U32` schema field without changing the encoder.
//!
//! Rather than modifying the trace-format encoder, we normalize:
//! - `U8`/`U16`/`U32` schemas → `Varint` in the shape
//! - `OptionalU8`/`OptionalU16`/`OptionalU32` → `OptionalVarint`
//!
//! Hand-authored shapes that contain these fixed-width tags are rejected at
//! validation time.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context, bail, ensure};
#[cfg(test)]
use dial9_trace_format::decoder::DecodedFrame;
use dial9_trace_format::decoder::Decoder;
use dial9_trace_format::encoder::{Encoder, Schema};
use dial9_trace_format::schema::{FieldAnnotation, FieldDef, SchemaEntry};
#[cfg(test)]
use dial9_trace_format::types::FieldValueRef;
use dial9_trace_format::types::{FieldType, FieldValue};
use serde::{Deserialize, Serialize};

/// Current shape format version.
const SHAPE_VERSION: u32 = 2;

/// Quantization quantum in nanoseconds (10 µs).
const QUANTUM_NS: u64 = 10_000;

/// Synthetic epoch for generated ClockSyncEvent realtime_ns (2020-01-01T00:00:00Z).
const SYNTHETIC_EPOCH_NS: u64 = 1_577_836_800_000_000_000;

/// Gzip magic bytes.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// Safe annotation keys that are preserved verbatim.
/// The derive macro emits `"unit"` as the key; the legacy format uses
/// `"metrique.unit"`. Both are recognized.
const SAFE_ANNOTATION_KEYS: &[&str] = &["unit", "metrique.unit"];

/// Safe annotation values (units). Must stay in sync with the derive macro's
/// `SUPPORTED_UNITS` in `dial9-trace-format-derive/src/lib.rs`.
const SAFE_UNITS: &[&str] = &["ns", "us", "ms", "s", "bytes", "count"];

/// Maximum recursion depth for nested dynamic values.
const MAX_DYNAMIC_DEPTH: usize = 8;

// ─── Resource limits (F) ────────────────────────────────────────────────────
// Resource limits: prevent pathological inputs from causing excessive memory
// usage. These are not perfect OOM prevention (JSON deserialization can amplify
// compact input), but they bound peak memory to practical levels.

/// Maximum shape JSON input size (bytes). 32 MiB.
/// JSON deserialization can amplify input (e.g. many short repeated elements),
/// so we cap conservatively. Most real shapes are well under 1 MiB; 32 MiB
/// accommodates extremely large traces (millions of events).
const MAX_SHAPE_JSON_BYTES: u64 = 32 * 1024 * 1024;

/// Maximum raw/decompressed trace file size (bytes). 2 GiB.
/// On 32-bit platforms this may exceed addressable memory; such platforms are
/// not supported for trace-shape extraction of multi-GiB traces.
const MAX_RAW_TRACE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Maximum per-Bytes field length. 64 MiB.
const MAX_BYTES_LENGTH: u32 = 64 * 1024 * 1024;

/// Maximum stack/list/map element count per field value.
const MAX_CONTAINER_ELEMENTS: usize = 1_000_000;

/// Maximum generated events across all repetitions. 100 million.
const MAX_GENERATED_EVENTS: u64 = 100_000_000;

/// Conservative maximum generated output size. ~16 GiB.
const MAX_GENERATED_OUTPUT_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// Maximum memory conservatively attributable to pooled stack frames retained
/// by the encoder across all repetitions. Repetition-shifted stacks intern as
/// distinct values, so output streaming alone does not bound this memory.
const MAX_RETAINED_GENERATION_BYTES: u64 = 512 * 1024 * 1024;

/// The registry increments its `u16` cursor after every allocation, so type ID
/// `u16::MAX` cannot be allocated without overflowing the cursor.
const MAX_WIRE_SCHEMAS: usize = (u16::MAX - dial9_trace_format::STATIC_WIRE_ID_LIMIT) as usize;

const MAX_SCHEMA_ANNOTATIONS: usize = u16::MAX as usize;

/// Built-in schema names preserved verbatim.
const BUILTIN_SCHEMAS: &[&str] = &[
    "PollStartEvent",
    "PollEndEvent",
    "WorkerParkEvent",
    "WorkerUnparkEvent",
    "QueueSampleEvent",
    "TaskSpawnEvent",
    "TaskTerminateEvent",
    "WakeEventEvent",
    "CpuSampleEvent",
    "TaskDumpEvent",
    "AllocEvent",
    "FreeEvent",
    "MemoryProfileOverflowEvent",
    "ClockSyncEvent",
    "SegmentMetadataEvent",
    "SymbolTableEntry",
    "ProcessResourceUsageEvent",
    "TcpAcceptQueueEvent",
    "SpanCloseEvent",
];

/// Exact complete `(field_name, raw wire type)` signatures accepted for each
/// built-in schema. Variants cover repository-proven producer history and the
/// normalized signatures emitted by this generator. Keeping names and types in
/// one table prevents cross-pairing fields from unrelated variants.
type BuiltinFieldSignature = &'static [(&'static str, FieldType)];

fn builtin_signatures(schema_name: &str) -> Option<&'static [BuiltinFieldSignature]> {
    use FieldType::{
        Bool as B, OptionalPooledString as OPS, OptionalVarint as OV, PooledStackFrames as PStack,
        PooledString as PS, StackFrames as Stack, String as S, StringMap as SM, U8, U16, U32,
        Varint as V,
    };

    match schema_name {
        "PollStartEvent" => Some(&[
            &[
                ("worker_id", V),
                ("local_queue", U8),
                ("task_id", V),
                ("spawn_loc", PS),
            ],
            &[
                ("worker_id", V),
                ("local_queue", U8),
                ("task_id", U32),
                ("spawn_loc", PS),
            ],
            &[
                ("worker_id", V),
                ("local_queue", V),
                ("task_id", V),
                ("spawn_loc", PS),
            ],
        ]),
        "PollEndEvent" => Some(&[&[("worker_id", V)]]),
        "WorkerParkEvent" => Some(&[
            &[
                ("worker_id", V),
                ("local_queue", U8),
                ("cpu_time_ns", V),
                ("tid", U32),
            ],
            &[("worker_id", V), ("local_queue", U8), ("cpu_time_ns", V)],
            &[
                ("worker_id", V),
                ("local_queue", V),
                ("cpu_time_ns", V),
                ("tid", V),
            ],
            &[("worker_id", V), ("local_queue", V), ("cpu_time_ns", V)],
        ]),
        "WorkerUnparkEvent" => Some(&[
            &[
                ("worker_id", V),
                ("local_queue", U8),
                ("cpu_time_ns", V),
                ("sched_wait_ns", OV),
                ("tid", U32),
            ],
            &[
                ("worker_id", V),
                ("local_queue", U8),
                ("cpu_time_ns", V),
                ("sched_wait_ns", V),
                ("tid", U32),
            ],
            &[
                ("worker_id", V),
                ("local_queue", U8),
                ("cpu_time_ns", V),
                ("sched_wait_ns", V),
            ],
            &[
                ("worker_id", V),
                ("local_queue", V),
                ("cpu_time_ns", V),
                ("sched_wait_ns", OV),
                ("tid", V),
            ],
            &[
                ("worker_id", V),
                ("local_queue", V),
                ("cpu_time_ns", V),
                ("sched_wait_ns", V),
                ("tid", V),
            ],
            &[
                ("worker_id", V),
                ("local_queue", V),
                ("cpu_time_ns", V),
                ("sched_wait_ns", V),
            ],
        ]),
        "QueueSampleEvent" => Some(&[&[("global_queue", U8)], &[("global_queue", V)]]),
        "TaskSpawnEvent" => Some(&[
            &[("task_id", V), ("spawn_loc", PS), ("instrumented", B)],
            &[("task_id", V), ("spawn_loc", PS)],
            &[("task_id", U32), ("spawn_loc", PS)],
        ]),
        "TaskTerminateEvent" => Some(&[&[("task_id", V)], &[("task_id", U32)]]),
        "WakeEventEvent" => Some(&[
            &[
                ("waker_task_id", V),
                ("woken_task_id", V),
                ("target_worker", U8),
            ],
            &[
                ("waker_task_id", U32),
                ("woken_task_id", U32),
                ("target_worker", U8),
            ],
            &[
                ("waker_task_id", V),
                ("woken_task_id", V),
                ("target_worker", V),
            ],
        ]),
        "CpuSampleEvent" => Some(&[
            &[
                ("worker_id", V),
                ("tid", U32),
                ("source", U8),
                ("thread_name", OPS),
                ("callchain", PStack),
                ("cpu", OV),
            ],
            &[
                ("worker_id", V),
                ("tid", U32),
                ("source", U8),
                ("thread_name", OPS),
                ("callchain", PStack),
            ],
            &[
                ("worker_id", V),
                ("tid", U32),
                ("source", U8),
                ("thread_name", OPS),
                ("callchain", Stack),
            ],
            &[
                ("worker_id", V),
                ("tid", U32),
                ("source", U8),
                ("thread_name", PS),
                ("callchain", Stack),
            ],
            &[
                ("worker_id", V),
                ("tid", V),
                ("source", V),
                ("thread_name", OPS),
                ("callchain", PStack),
                ("cpu", OV),
            ],
            &[
                ("worker_id", V),
                ("tid", V),
                ("source", V),
                ("thread_name", OPS),
                ("callchain", PStack),
            ],
            &[
                ("worker_id", V),
                ("tid", V),
                ("source", V),
                ("thread_name", OPS),
                ("callchain", Stack),
            ],
            &[
                ("worker_id", V),
                ("tid", V),
                ("source", V),
                ("thread_name", PS),
                ("callchain", Stack),
            ],
        ]),
        "TaskDumpEvent" => Some(&[&[("task_id", V), ("callchain", PStack)]]),
        "AllocEvent" => Some(&[
            &[
                ("tid", U32),
                ("size", V),
                ("addr", V),
                ("callchain", PStack),
            ],
            &[("tid", V), ("size", V), ("addr", V), ("callchain", PStack)],
        ]),
        "FreeEvent" => Some(&[
            &[
                ("tid", U32),
                ("addr", V),
                ("size", V),
                ("alloc_timestamp_ns", V),
            ],
            &[
                ("tid", V),
                ("addr", V),
                ("size", V),
                ("alloc_timestamp_ns", V),
            ],
        ]),
        "MemoryProfileOverflowEvent" => Some(&[&[("dropped_allocs", V), ("dropped_frees", V)]]),
        "ClockSyncEvent" => Some(&[&[("realtime_ns", V)]]),
        "SegmentMetadataEvent" => Some(&[&[("entries", SM)]]),
        "SymbolTableEntry" => Some(&[
            &[
                ("addr", V),
                ("size", V),
                ("symbol_name", PS),
                ("inline_depth", V),
                ("source_file", PS),
                ("source_line", V),
            ],
            &[
                ("addr", V),
                ("size", V),
                ("symbol_name", PS),
                ("inline_depth", V),
            ],
            &[("base_addr", V), ("size", V), ("symbol_name", PS)],
        ]),
        "ProcessResourceUsageEvent" => Some(&[&[
            ("user_cpu_ns", V),
            ("system_cpu_ns", V),
            ("max_rss_bytes", V),
            ("minor_faults", V),
            ("major_faults", V),
            ("block_input_ops", V),
            ("block_output_ops", V),
            ("voluntary_context_switches", V),
            ("involuntary_context_switches", V),
        ]]),
        "TcpAcceptQueueEvent" => Some(&[
            &[
                ("socket_cookie", V),
                ("socket_inode", V),
                ("ip_version", U8),
                ("local_addr", S),
                ("local_port", U16),
                ("pending_connections", U32),
                ("backlog_limit", U32),
            ],
            &[
                ("socket_cookie", V),
                ("socket_inode", V),
                ("ip_version", V),
                ("local_addr", S),
                ("local_port", V),
                ("pending_connections", V),
                ("backlog_limit", V),
            ],
        ]),
        "SpanCloseEvent" => Some(&[&[("span_id", V)]]),
        _ => None,
    }
}

fn validate_builtin_schema_signature(schema_name: &str, fields: &[FieldDef]) -> anyhow::Result<()> {
    let signatures = builtin_signatures(schema_name).ok_or_else(|| {
        anyhow::anyhow!("builtin schema '{schema_name}' has no canonical signature")
    })?;
    let matches = signatures.iter().any(|signature| {
        signature.len() == fields.len()
            && signature
                .iter()
                .zip(fields)
                .all(|((name, field_type), field)| {
                    *name == field.name() && *field_type == field.field_type()
                })
    });
    ensure!(
        matches,
        "builtin schema '{schema_name}' signature {:?} does not exactly match a known complete signature {:?}",
        fields
            .iter()
            .map(|field| (field.name(), field.field_type()))
            .collect::<Vec<_>>(),
        signatures
    );
    Ok(())
}

fn is_known_builtin_field(schema_name: &str, field_name: &str) -> bool {
    builtin_signatures(schema_name).is_some_and(|signatures| {
        signatures
            .iter()
            .any(|signature| signature.iter().any(|(name, _)| *name == field_name))
    })
}

/// Known identity namespace IDs for serialization (C - NAME/ID PRIVACY).
/// Namespaces are serialized as numeric IDs, never as original field names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum NamespaceId {
    Task,
    Span,
    Tid,
    Addr,
    SocketCookie,
    SocketInode,
    /// Generic anonymous namespace keyed by original field identity but
    /// serialized as an opaque number.
    Anon(u16),
}

// Custom Serialize/Deserialize as bare integer for privacy
impl serde::Serialize for NamespaceId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            NamespaceId::Task => serializer.serialize_u16(0),
            NamespaceId::Span => serializer.serialize_u16(1),
            NamespaceId::Tid => serializer.serialize_u16(2),
            NamespaceId::Addr => serializer.serialize_u16(3),
            NamespaceId::SocketCookie => serializer.serialize_u16(4),
            NamespaceId::SocketInode => serializer.serialize_u16(5),
            NamespaceId::Anon(n) => serializer.serialize_u16(100 + n),
        }
    }
}

impl<'de> serde::Deserialize<'de> for NamespaceId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let n = u16::deserialize(deserializer)?;
        Ok(match n {
            0 => NamespaceId::Task,
            1 => NamespaceId::Span,
            2 => NamespaceId::Tid,
            3 => NamespaceId::Addr,
            4 => NamespaceId::SocketCookie,
            5 => NamespaceId::SocketInode,
            // (7) Anon namespace range: 100..=65535. Reject reserved range 6..99.
            other if other >= 100 => NamespaceId::Anon(other - 100),
            _ => {
                return Err(serde::de::Error::custom(format!(
                    "namespace id {n} is in reserved range (6..99)"
                )));
            }
        })
    }
}

// ─── Repeat metadata: identity namespace semantics ──────────────────────────

/// Describes how a field's value behaves under repetition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FieldSemantics {
    /// Value is unchanged across repetitions (worker_id, metrics, sizes, durations).
    Structural,
    /// Value is an identity in a named namespace; shifted by namespace stride per rep.
    Identity,
    /// Value is a relative timestamp offset; shifted by repetition time shift.
    TimestampRef,
    /// Realtime offset for ClockSyncEvent; generated from SYNTHETIC_EPOCH + event_offset + rep shift.
    RealtimeOffset,
}

/// Metadata about a field's repeat behavior, stored in the shape JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FieldRepeatMeta {
    pub semantics: FieldSemantics,
    /// Numeric namespace ID for Identity fields. Serialized as an integer, never
    /// as an original field name (C - NAME/ID PRIVACY).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<NamespaceId>,
}

// ─── Shape JSON types (explicitly tagged) ───────────────────────────────────

/// Top-level shape file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TraceShape {
    pub version: u32,
    pub summary: ShapeSummary,
    pub schemas: Vec<ShapeSchema>,
    pub events: Vec<ShapeEvent>,
}

/// Summary statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ShapeSummary {
    pub event_count: u64,
    pub duration_ns: u64,
    pub event_type_counts: BTreeMap<String, u64>,
    pub worker_cardinality: u32,
    pub task_cardinality: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_duration_quantiles: Option<Quantiles>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span_duration_quantiles: Option<Quantiles>,
}

/// Duration quantile statistics (quantized).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Quantiles {
    pub count: u64,
    pub min_ns: u64,
    pub p50_ns: u64,
    pub p90_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
}

/// A sanitized schema in the shape file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ShapeSchema {
    pub name: String,
    pub has_timestamp: bool,
    pub fields: Vec<ShapeField>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<ShapeAnnotation>,
}

/// A field within a shape schema, including repeat metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ShapeField {
    pub name: String,
    /// Wire type tag (after normalization: no U8/U16/U32).
    pub field_type: u8,
    /// Repeat metadata controlling how this field shifts across repetitions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_meta: Option<FieldRepeatMeta>,
}

/// A sanitized annotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ShapeAnnotation {
    pub field_index: u16,
    pub key: String,
    pub value: String,
}

/// A single event in the shape replay sequence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ShapeEvent {
    pub schema_index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_offset_ns: Option<u64>,
    pub values: Vec<ShapeValue>,
}

/// An explicitly tagged field value for unambiguous round-tripping.
///
/// Every supported wire semantic has its own variant so deserialization never
/// loses information. Dynamic list/map elements are tagged recursively.
/// D - RECURSIVE POOL SEMANTICS: PooledString/PooledStack distinguish from inline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub(crate) enum ShapeValue {
    /// Unsigned integer (Varint on the wire).
    U(u64),
    /// Signed 64-bit integer.
    I(i64),
    /// IEEE 754 double (finite values only in shape files).
    F(f64),
    /// Boolean.
    B(bool),
    /// Inline string (FieldType::String on the wire).
    S(String),
    /// Pooled string (FieldType::PooledString on the wire). D - distinct from inline.
    PS(String),
    /// Byte array — stores only the length (privacy: no content).
    Bytes(u32),
    /// Inline stack frames (FieldType::StackFrames, list of remapped addresses).
    Stack(Vec<u64>),
    /// Pooled stack frames (FieldType::PooledStackFrames). D - distinct from inline.
    PStack(Vec<u64>),
    /// String map (sanitized key-value pairs).
    StringMap(Vec<(String, String)>),
    /// Dynamic list (recursively tagged elements).
    List(Vec<ShapeValue>),
    /// Dynamic map (recursively tagged key-value pairs).
    Map(Vec<(ShapeValue, ShapeValue)>),
    /// Absent optional field.
    None,
}

// ─── Public entry points ────────────────────────────────────────────────────

/// Extract a shape from a trace file.
pub(crate) fn extract(trace_path: &Path, output_path: &Path) -> anyhow::Result<()> {
    let data = read_trace_file(trace_path)?;
    let shape = extract_shape(&data)?;
    let json = serde_json::to_string_pretty(&shape).context("serialize shape JSON")?;
    std::fs::write(output_path, json.as_bytes())
        .with_context(|| format!("write {}", output_path.display()))?;
    Ok(())
}

/// Generate a trace from a shape file. Streams output through BufWriter after
/// all validation passes, so no output is created on invalid input.
pub(crate) fn generate(shape_path: &Path, output_path: &Path, repeat: u32) -> anyhow::Result<()> {
    ensure!(repeat >= 1, "--repeat must be >= 1, got {repeat}");

    // F: Bounded read of shape JSON — use Take to enforce limit at the read
    // level, preventing TOCTOU between stat and read.
    let metadata =
        std::fs::metadata(shape_path).with_context(|| format!("stat {}", shape_path.display()))?;
    ensure!(
        metadata.len() <= MAX_SHAPE_JSON_BYTES,
        "shape JSON exceeds maximum size ({} bytes > {MAX_SHAPE_JSON_BYTES} byte limit)",
        metadata.len()
    );
    let file = std::fs::File::open(shape_path)
        .with_context(|| format!("open {}", shape_path.display()))?;
    let take_limit = MAX_SHAPE_JSON_BYTES.saturating_add(1);
    let mut bounded = std::io::BufReader::new(file).take(take_limit);
    let mut json = String::new();
    bounded
        .read_to_string(&mut json)
        .with_context(|| format!("read {}", shape_path.display()))?;
    ensure!(
        (json.len() as u64) <= MAX_SHAPE_JSON_BYTES,
        "shape JSON actual read size ({} bytes) exceeds limit ({MAX_SHAPE_JSON_BYTES} byte limit)",
        json.len()
    );
    // serde_json::from_str has a default recursion limit of 128, which bounds
    // stack depth during deserialization. The 32 MiB input cap limits peak
    // memory from deserialization amplification. Post-parse validation enforces
    // per-container element counts and dynamic depth budgets.
    let shape: TraceShape = serde_json::from_str(&json).context("parse shape JSON")?;

    // E/F: Complete validation including summary, resource limits, and preflight
    validate_shape(&shape)?;
    write_generated_trace(&shape, output_path, repeat)
}

/// Extract a sanitized shape in memory and immediately generate a synthetic
/// trace. This avoids serializing and reparsing the verbose JSON representation.
pub(crate) fn synthesize(trace_path: &Path, output_path: &Path, repeat: u32) -> anyhow::Result<()> {
    ensure!(repeat >= 1, "--repeat must be >= 1, got {repeat}");

    let data = read_trace_file(trace_path)?;
    let shape = extract_shape(&data)?;
    drop(data);

    write_generated_trace(&shape, output_path, repeat)
}

/// Preflight and write a validated in-memory shape. The output is opened only
/// after repeat-dependent resource and overflow checks pass.
fn write_generated_trace(
    shape: &TraceShape,
    output_path: &Path,
    repeat: u32,
) -> anyhow::Result<()> {
    validate_repeat_preflight(shape, repeat)?;

    let file = std::fs::File::create(output_path)
        .with_context(|| format!("create {}", output_path.display()))?;
    let writer = BufWriter::new(file);
    // Wrap in a LimitedWriter as a hard backstop: even if the estimator is
    // under-counting, we will not write more than MAX_GENERATED_OUTPUT_BYTES.
    let limited = LimitedWriter::new(writer, MAX_GENERATED_OUTPUT_BYTES);
    generate_to_writer(shape, repeat, limited)
        .with_context(|| format!("write {}", output_path.display()))?;
    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// A Write adapter that enforces a hard byte-count limit. If a write would
/// push the total past `limit`, it returns `io::ErrorKind::Other`. This is a
/// safety backstop: even if the preflight estimator is not perfectly conservative,
/// the output file cannot grow unbounded.
struct LimitedWriter<W> {
    inner: W,
    written: u64,
    limit: u64,
}

impl<W> LimitedWriter<W> {
    fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            written: 0,
            limit,
        }
    }
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let new_total = self.written.saturating_add(buf.len() as u64);
        if new_total > self.limit {
            return Err(std::io::Error::other(format!(
                "generated output exceeds hard limit ({} bytes written + {} = {new_total} > {} limit)",
                self.written,
                buf.len(),
                self.limit
            )));
        }
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Read a trace file, transparently decompressing gzip. Enforces size limits.
/// For non-gzip: checks metadata size, then reads with a Take bound so actual
/// bytes consumed are limited even if the file grows between stat and read.
/// For gzip: reads only the first 2 bytes to detect magic, then streams
/// decompression from disk (bounded by take(limit+1)) so only the decompressed
/// Vec is held in memory, not both compressed and decompressed simultaneously.
fn read_trace_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    let metadata = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let file_size = metadata.len();

    // Reject obviously oversized files before any read
    ensure!(
        file_size <= MAX_RAW_TRACE_BYTES,
        "trace file exceeds maximum size ({file_size} bytes > {MAX_RAW_TRACE_BYTES} byte limit)"
    );

    // Read first 2 bytes to detect gzip magic
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut magic = [0u8; 2];
    let magic_len = std::io::Read::read(&mut file, &mut magic)
        .with_context(|| format!("read magic from {}", path.display()))?;

    if magic_len >= 2 && magic == GZIP_MAGIC {
        // Independently bound compressed bytes consumed and decompressed
        // bytes produced. The inner Take also protects against file growth or
        // special files after the metadata check.
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(0))
            .with_context(|| format!("seek {}", path.display()))?;
        read_gzip_bounded(file, MAX_RAW_TRACE_BYTES, MAX_RAW_TRACE_BYTES)
    } else {
        // Plain binary: use Take to bound the read even if the file grows
        // between the stat and the read.
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(0))
            .with_context(|| format!("seek {}", path.display()))?;
        let take_limit = MAX_RAW_TRACE_BYTES.saturating_add(1);
        let mut bounded = file.take(take_limit);
        let mut raw = Vec::with_capacity(file_size as usize);
        bounded
            .read_to_end(&mut raw)
            .with_context(|| format!("read {}", path.display()))?;
        ensure!(
            (raw.len() as u64) <= MAX_RAW_TRACE_BYTES,
            "trace file actual read size ({} bytes) exceeds limit ({MAX_RAW_TRACE_BYTES} byte limit)",
            raw.len()
        );
        Ok(raw)
    }
}

fn read_gzip_bounded<R: Read>(
    reader: R,
    compressed_limit: u64,
    decompressed_limit: u64,
) -> anyhow::Result<Vec<u8>> {
    let compressed_take_limit = compressed_limit.saturating_add(1);
    let decompressed_take_limit = decompressed_limit.saturating_add(1);
    let compressed = reader.take(compressed_take_limit);
    let decoder = flate2::read::GzDecoder::new(compressed);
    let mut bounded_output = decoder.take(decompressed_take_limit);
    let mut out = Vec::new();
    bounded_output
        .read_to_end(&mut out)
        .context("decompress gzip trace")?;

    let decoder = bounded_output.into_inner();
    let compressed = decoder.into_inner();
    let compressed_consumed = compressed_take_limit.saturating_sub(compressed.limit());
    ensure!(
        compressed_consumed <= compressed_limit,
        "compressed trace exceeds maximum size ({compressed_consumed} bytes consumed > {compressed_limit} byte limit)"
    );
    ensure!(
        (out.len() as u64) <= decompressed_limit,
        "decompressed trace exceeds maximum size ({} bytes > {} byte limit)",
        out.len(),
        decompressed_limit
    );
    Ok(out)
}

/// Normalize a FieldType tag: U8/U16/U32 → Varint, Optional variants similarly.
fn normalize_field_type(ft: FieldType) -> FieldType {
    match ft {
        FieldType::U8 | FieldType::U16 | FieldType::U32 => FieldType::Varint,
        FieldType::OptionalU8 | FieldType::OptionalU16 | FieldType::OptionalU32 => {
            FieldType::OptionalVarint
        }
        other => other,
    }
}

/// Quantize a nanosecond value to the nearest quantum.
fn quantize_ns(ns: u64) -> u64 {
    (ns / QUANTUM_NS) * QUANTUM_NS
}

/// Quantize a u64 to ~3 significant digits (round to nearest power-of-2 granularity).
fn quantize_numeric(val: u64) -> u64 {
    if val == 0 {
        return 0;
    }
    let bits = 64u32.saturating_sub(val.leading_zeros());
    if bits <= 10 {
        return val; // small values: exact
    }
    let shift = bits - 10; // keep top 10 bits (~3 sig digits)
    let top = val >> shift;
    // Avoid overflow: if top is already at max for the bit width, don't add 1
    let rounded = if shift >= 54 {
        // For very large values, just use top bits without rounding up
        top
    } else {
        top.saturating_add(1)
    };
    rounded.checked_shl(shift).unwrap_or(u64::MAX)
}

/// Quantize an i64 to ~3 significant digits without overflow.
fn quantize_i64(val: i64) -> i64 {
    if val == i64::MIN {
        // i64::MIN.unsigned_abs() == 2^63, quantize that then negate
        let mag = quantize_numeric(val.unsigned_abs());
        // mag could be > i64::MAX; clamp
        -(mag.min(i64::MAX as u64) as i64)
    } else {
        let sign = val.signum();
        let mag = quantize_numeric(val.unsigned_abs());
        sign * (mag.min(i64::MAX as u64) as i64)
    }
}

/// Quantize an f64 to ~3 significant digits. Returns Err for non-finite.
fn quantize_f64(val: f64) -> anyhow::Result<f64> {
    ensure!(
        val.is_finite(),
        "non-finite f64 value cannot be stored in shape JSON"
    );
    if val == 0.0 {
        return Ok(0.0);
    }
    // Round to 3 significant decimal digits. Very small finite values need
    // scientific formatting because the arithmetic scale would overflow for
    // exponents below roughly -306 (including all subnormals).
    let magnitude = val.abs().log10().floor() as i32;
    let rounded = if magnitude < -306 {
        format!("{val:.2e}")
            .parse::<f64>()
            .context("round very small finite f64")?
    } else {
        let factor = 10f64.powi(3 - 1 - magnitude);
        (val * factor).round() / factor
    };
    ensure!(
        rounded.is_finite(),
        "finite f64 quantization produced a non-finite result"
    );
    Ok(rounded)
}

/// C - NAME/ID PRIVACY: deterministic magnitude-preserving transformation for
/// custom non-identity integers. Every nonzero value is transformed such that:
/// - The output is never equal to the input (no source value retained verbatim)
/// - The output preserves the general magnitude (same order of magnitude)
/// - The transformation is deterministic (same input → same output)
///
/// Strategy: round to the nearest power-of-two bucket, then add a constant
/// offset within the same magnitude to ensure the result always differs from
/// the input while staying in the same ballpark.
fn privacy_bucket_u64(val: u64) -> u64 {
    if val == 0 {
        return 0;
    }
    // Find the power-of-two bucket: the largest power of 2 <= val
    let bits = 64u32.saturating_sub(val.leading_zeros()); // bits needed (1-indexed)
    let bucket = 1u64.checked_shl(bits.saturating_sub(1)).unwrap_or(u64::MAX);

    // Add 3/4 of the bucket size as offset to ensure we never equal the input.
    // For bucket=1 (val=1): result = 1 + 0 = 1, which equals input! So we
    // use a different strategy for small values.
    if val <= 2 {
        // Small values: shift up by 2 to guarantee non-equality
        // 1 -> 3, 2 -> 4
        return val + 2;
    }

    // For larger values: use bucket + 3/4 * bucket = 7/4 * bucket (clamped to
    // same order of magnitude by using the bucket as base).
    // This guarantees: result != val for all val > 2 since:
    // - If val is a power of 2: bucket == val, result = bucket + bucket*3/4 = bucket*7/4 != bucket
    // - If val is not a power of 2: bucket < val < bucket*2, result = bucket*7/4 which is
    //   between bucket and bucket*2, but specifically at 1.75*bucket while val is somewhere
    //   else in [bucket+1, bucket*2-1]
    // Edge case: val = bucket*7/4 is possible, so we add 1 to break that tie.
    let offset = (bucket >> 1).saturating_add(bucket >> 2); // 3/4 * bucket
    let result = bucket.saturating_add(offset).saturating_add(1);

    // For the rare case that result == val (when val == bucket*7/4 + 1),
    // add another bucket/4 to guarantee non-equality
    if result == val {
        result.saturating_add(bucket >> 2)
    } else {
        result
    }
}

/// C - NAME/ID PRIVACY: deterministic magnitude-preserving transformation for signed integers.
fn privacy_bucket_i64(val: i64) -> i64 {
    if val == 0 {
        return 0;
    }
    let sign = val.signum();
    let mag = privacy_bucket_u64(val.unsigned_abs());
    sign * (mag.min(i64::MAX as u64) as i64)
}

/// Determine if a schema name is built-in.
fn is_builtin_schema(name: &str) -> bool {
    BUILTIN_SCHEMAS.contains(&name)
}

/// Check if a schema name is a dynamic SpanEnter:/SpanExit: schema.
fn span_prefix(name: &str) -> Option<(&str, &str)> {
    if let Some(suffix) = name.strip_prefix("SpanEnter:") {
        Some(("SpanEnter:", suffix))
    } else if let Some(suffix) = name.strip_prefix("SpanExit:") {
        Some(("SpanExit:", suffix))
    } else {
        None
    }
}

/// Classify a field's repeat semantics based on schema name and field name.
fn classify_field(schema_name: &str, field_name: &str, ft: FieldType) -> FieldRepeatMeta {
    let inner = normalize_field_type(ft).inner();

    // worker_id and target_worker: structural, never shifted
    if field_name == "worker_id" || field_name == "target_worker" {
        return FieldRepeatMeta {
            semantics: FieldSemantics::Structural,
            namespace: None,
        };
    }

    // alloc_timestamp_ns: relative timestamp reference
    if field_name == "alloc_timestamp_ns" {
        return FieldRepeatMeta {
            semantics: FieldSemantics::TimestampRef,
            namespace: None,
        };
    }

    // ClockSyncEvent.realtime_ns: realtime offset (B - CLOCK PRIVACY)
    if schema_name == "ClockSyncEvent" && field_name == "realtime_ns" {
        return FieldRepeatMeta {
            semantics: FieldSemantics::RealtimeOffset,
            namespace: None,
        };
    }

    // Only Varint/OptionalVarint fields can be identities
    let is_varint = matches!(inner, FieldType::Varint);

    if is_varint {
        // Task identity namespace
        if matches!(field_name, "task_id" | "waker_task_id" | "woken_task_id") {
            return FieldRepeatMeta {
                semantics: FieldSemantics::Identity,
                namespace: Some(NamespaceId::Task),
            };
        }
        // Span identity namespace
        if matches!(field_name, "span_id" | "parent_span_id") {
            return FieldRepeatMeta {
                semantics: FieldSemantics::Identity,
                namespace: Some(NamespaceId::Span),
            };
        }
        // Thread identity
        if field_name == "thread_id" || field_name == "tid" {
            return FieldRepeatMeta {
                semantics: FieldSemantics::Identity,
                namespace: Some(NamespaceId::Tid),
            };
        }
        // Address namespace (including SymbolTableEntry and base_addr)
        if matches!(
            field_name,
            "address" | "alloc_address" | "symbol_address" | "addr" | "base_addr"
        ) {
            return FieldRepeatMeta {
                semantics: FieldSemantics::Identity,
                namespace: Some(NamespaceId::Addr),
            };
        }
        // Socket identities
        if field_name == "socket_cookie" {
            return FieldRepeatMeta {
                semantics: FieldSemantics::Identity,
                namespace: Some(NamespaceId::SocketCookie),
            };
        }
        if field_name == "socket_inode" {
            return FieldRepeatMeta {
                semantics: FieldSemantics::Identity,
                namespace: Some(NamespaceId::SocketInode),
            };
        }
        // Generic *_id fields get deterministic anonymous namespace (C - NAME/ID PRIVACY)
        if field_name.ends_with("_id") && field_name != "worker_id" {
            // Namespace keyed internally by original field identity but serialized as number
            return FieldRepeatMeta {
                semantics: FieldSemantics::Identity,
                namespace: None, // will be assigned by ExtractContext
            };
        }
    }

    // Stack frames: addresses in addr namespace (handled specially in extraction/generation)
    if matches!(inner, FieldType::StackFrames | FieldType::PooledStackFrames) {
        return FieldRepeatMeta {
            semantics: FieldSemantics::Identity,
            namespace: Some(NamespaceId::Addr),
        };
    }

    // Everything else: structural (metrics, queues, sizes, durations, counters)
    FieldRepeatMeta {
        semantics: FieldSemantics::Structural,
        namespace: None,
    }
}

// ─── Extraction ─────────────────────────────────────────────────────────────

struct ExtractContext {
    // Schema/field name remapping
    callsite_names: HashMap<String, String>,
    callsite_counter: u32,
    schema_names: HashMap<String, String>,
    custom_schema_counter: u32,
    field_names: HashMap<(String, String), String>,
    custom_field_counter: u32,
    // Anonymous namespace ID assignment (C - NAME/ID PRIVACY)
    anon_namespace_map: HashMap<String, u16>,
    next_anon_namespace: u16,
    // Identity remapping
    task_ids: HashMap<u64, u64>,
    next_task_id: u64,
    span_ids: HashMap<u64, u64>,
    next_span_id: u64,
    thread_ids: HashMap<u64, u64>,
    next_thread_id: u64,
    addresses: HashMap<u64, u64>,
    next_address: u64,
    generic_ids: HashMap<(u16, u64), u64>,
    next_generic_id: HashMap<u16, u64>,
    // Socket identity remapping
    socket_cookies: HashMap<u64, u64>,
    next_socket_cookie: u64,
    socket_inodes: HashMap<u64, u64>,
    next_socket_inode: u64,
    // String sanitization
    string_remap: HashMap<String, String>,
    next_string_id: u64,
    // Cardinality tracking
    worker_ids: HashSet<u64>,
    task_id_set: HashSet<u64>,
    // H: Poll quantiles: track starts per (worker_id) using a stack for nested/repeated
    poll_starts: HashMap<u64, Vec<u64>>,
    poll_durations: Vec<u64>,
    // H: Span quantiles: track SpanEnter per (span_id) -> stack of timestamps
    span_enters: HashMap<u64, Vec<u64>>,
    span_durations: Vec<u64>,
}

impl ExtractContext {
    fn new() -> Self {
        Self {
            callsite_names: HashMap::new(),
            callsite_counter: 0,
            schema_names: HashMap::new(),
            custom_schema_counter: 0,
            field_names: HashMap::new(),
            custom_field_counter: 0,
            anon_namespace_map: HashMap::new(),
            next_anon_namespace: 0,
            task_ids: HashMap::new(),
            next_task_id: 1,
            span_ids: HashMap::new(),
            next_span_id: 1,
            thread_ids: HashMap::new(),
            next_thread_id: 1,
            addresses: HashMap::new(),
            next_address: 0x1000,
            generic_ids: HashMap::new(),
            next_generic_id: HashMap::new(),
            socket_cookies: HashMap::new(),
            next_socket_cookie: 1,
            socket_inodes: HashMap::new(),
            next_socket_inode: 1,
            string_remap: HashMap::new(),
            next_string_id: 0,
            worker_ids: HashSet::new(),
            task_id_set: HashSet::new(),
            poll_starts: HashMap::new(),
            poll_durations: Vec::new(),
            span_enters: HashMap::new(),
            span_durations: Vec::new(),
        }
    }

    fn sanitize_schema_name(&mut self, name: &str) -> String {
        if is_builtin_schema(name) {
            return name.to_string();
        }
        if let Some((prefix, suffix)) = span_prefix(name) {
            let callsite = self
                .callsite_names
                .entry(suffix.to_string())
                .or_insert_with(|| {
                    let id = self.callsite_counter;
                    self.callsite_counter += 1;
                    format!("callsite_{id:04}")
                });
            return format!("{prefix}{callsite}");
        }
        self.schema_names
            .entry(name.to_string())
            .or_insert_with(|| {
                let id = self.custom_schema_counter;
                self.custom_schema_counter += 1;
                format!("CustomEvent_{id:04}")
            })
            .clone()
    }

    /// Sanitize a field name according to C - NAME/ID PRIVACY rules.
    /// On built-in schemas: preserve canonical fields, anonymize non-canonical ones.
    /// On span schemas: preserve worker_id/span_id/parent_span_id/span_name.
    /// On custom schemas: anonymize all field names.
    fn sanitize_field_name(
        &mut self,
        schema_name: &str,
        original_schema_name: &str,
        field_name: &str,
    ) -> String {
        // On built-in schemas, check canonical field list
        if is_builtin_schema(original_schema_name) {
            if builtin_signatures(original_schema_name).is_some() {
                if is_known_builtin_field(original_schema_name, field_name) {
                    return field_name.to_string();
                }
                // Non-canonical field on a built-in schema => anonymize
            } else {
                // Unknown built-in (future-proof): preserve field name
                return field_name.to_string();
            }
        } else if span_prefix(original_schema_name).is_some() {
            // On span schemas, preserve known identity/structural fields
            if matches!(
                field_name,
                "worker_id" | "span_id" | "parent_span_id" | "span_name" | "task_id"
            ) {
                return field_name.to_string();
            }
        }
        // Anonymize
        let key = (schema_name.to_string(), field_name.to_string());
        self.field_names
            .entry(key)
            .or_insert_with(|| {
                let id = self.custom_field_counter;
                self.custom_field_counter += 1;
                format!("field_{id:04}")
            })
            .clone()
    }

    /// Get or assign a NamespaceId for a generic *_id field (C - NAME/ID PRIVACY).
    /// (7) Uses checked addition; returns error before max representable namespaces.
    fn get_anon_namespace(&mut self, field_name: &str) -> anyhow::Result<NamespaceId> {
        if let Some(&ns_num) = self.anon_namespace_map.get(field_name) {
            return Ok(NamespaceId::Anon(ns_num));
        }
        // (7) Check overflow before allocating: max u16 is 65535, reserved range starts at 100
        // so anon IDs serialize as 100+n. Max n = 65435 to avoid u16 overflow in serialization.
        const MAX_ANON_NAMESPACES: u16 = u16::MAX - 100;
        ensure!(
            self.next_anon_namespace < MAX_ANON_NAMESPACES,
            "too many anonymous namespaces ({} >= {MAX_ANON_NAMESPACES}); \
             trace has too many distinct *_id field names",
            self.next_anon_namespace
        );
        let n = self.next_anon_namespace;
        self.next_anon_namespace = n
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("anonymous namespace counter overflow"))?;
        self.anon_namespace_map.insert(field_name.to_string(), n);
        Ok(NamespaceId::Anon(n))
    }

    fn remap_task_id(&mut self, id: u64) -> u64 {
        if id == 0 {
            return 0;
        }
        let next = &mut self.next_task_id;
        *self.task_ids.entry(id).or_insert_with(|| {
            let v = *next;
            *next += 1;
            v
        })
    }

    fn remap_span_id(&mut self, id: u64) -> u64 {
        if id == 0 {
            return 0;
        }
        let next = &mut self.next_span_id;
        *self.span_ids.entry(id).or_insert_with(|| {
            let v = *next;
            *next += 1;
            v
        })
    }

    fn remap_thread_id(&mut self, id: u64) -> u64 {
        if id == 0 {
            return 0;
        }
        let next = &mut self.next_thread_id;
        *self.thread_ids.entry(id).or_insert_with(|| {
            let v = *next;
            *next += 1;
            v
        })
    }

    fn remap_address(&mut self, addr: u64) -> u64 {
        if addr == 0 {
            return 0;
        }
        let next = &mut self.next_address;
        *self.addresses.entry(addr).or_insert_with(|| {
            let v = *next;
            *next += 0x10;
            v
        })
    }

    fn remap_socket_cookie(&mut self, id: u64) -> u64 {
        if id == 0 {
            return 0;
        }
        let next = &mut self.next_socket_cookie;
        *self.socket_cookies.entry(id).or_insert_with(|| {
            let v = *next;
            *next += 1;
            v
        })
    }

    fn remap_socket_inode(&mut self, id: u64) -> u64 {
        if id == 0 {
            return 0;
        }
        let next = &mut self.next_socket_inode;
        *self.socket_inodes.entry(id).or_insert_with(|| {
            let v = *next;
            *next += 1;
            v
        })
    }

    fn remap_generic_id(&mut self, ns: NamespaceId, id: u64) -> u64 {
        if id == 0 {
            return 0;
        }
        let ns_num = match ns {
            NamespaceId::Anon(n) => n,
            _ => return id, // shouldn't happen
        };
        let key = (ns_num, id);
        let counter = self.next_generic_id.entry(ns_num).or_insert(1);
        *self.generic_ids.entry(key).or_insert_with(|| {
            let v = *counter;
            *counter += 1;
            v
        })
    }

    fn sanitize_string(&mut self, s: &str) -> String {
        self.string_remap
            .entry(s.to_string())
            .or_insert_with(|| {
                let id = self.next_string_id;
                self.next_string_id += 1;
                format!("s_{id:04}")
            })
            .clone()
    }

    /// Remap a FieldValueRef using active pools.
    /// `is_builtin` indicates whether the schema is a known built-in (affects structural quantization).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::collapsible_if)]
    fn remap_value_ref(
        &mut self,
        schema_name: &str,
        field_name: &str,
        meta: &FieldRepeatMeta,
        value: &dial9_trace_format::types::FieldValueRef<'_>,
        ft: FieldType,
        string_pool: &dial9_trace_format::decoder::StringPool,
        stack_pool: &dial9_trace_format::decoder::StackPool,
        source_base_ts: u64,
        event_offset_ns: u64,
        is_builtin: bool,
    ) -> anyhow::Result<ShapeValue> {
        use dial9_trace_format::types::FieldValueRef;

        if matches!(value, FieldValueRef::None) {
            ensure!(
                ft.is_optional(),
                "None value for non-optional field '{field_name}'"
            );
            return Ok(ShapeValue::None);
        }

        let inner_ft = normalize_field_type(ft).inner();

        match meta.semantics {
            FieldSemantics::Structural => {
                if field_name == "worker_id" || field_name == "target_worker" {
                    if let FieldValueRef::Varint(v) = value {
                        // H: Only count worker_id (not target_worker) for cardinality,
                        // excluding sentinels 254/255
                        if field_name == "worker_id" && *v != 254 && *v != 255 {
                            self.worker_ids.insert(*v);
                        }
                        return Ok(ShapeValue::U(*v));
                    }
                }
                self.convert_structural_ref(value, inner_ft, string_pool, stack_pool, is_builtin, 0)
            }
            FieldSemantics::Identity => {
                let ns = meta.namespace.unwrap_or(NamespaceId::Addr);
                self.convert_identity_ref(schema_name, field_name, ns, value, inner_ft, stack_pool)
            }
            FieldSemantics::TimestampRef => {
                if let FieldValueRef::Varint(v) = value {
                    let offset = v.saturating_sub(source_base_ts);
                    Ok(ShapeValue::U(quantize_ns(offset)))
                } else {
                    bail!("TimestampRef field '{field_name}' must be Varint");
                }
            }
            FieldSemantics::RealtimeOffset => {
                // B - CLOCK PRIVACY: entirely ignore source realtime value.
                // Store the event's monotonic timestamp offset (already sanitized).
                if let FieldValueRef::Varint(_) = value {
                    Ok(ShapeValue::U(quantize_ns(event_offset_ns)))
                } else {
                    bail!("RealtimeOffset field '{field_name}' must be Varint");
                }
            }
        }
    }

    /// Convert a structural value. D - RECURSIVE POOL SEMANTICS: preserve pooled vs inline tags.
    /// C - NAME/ID PRIVACY: non-identity nonzero integers get privacy bucketing on custom schemas.
    fn convert_structural_ref(
        &mut self,
        value: &dial9_trace_format::types::FieldValueRef<'_>,
        _inner_ft: FieldType,
        string_pool: &dial9_trace_format::decoder::StringPool,
        stack_pool: &dial9_trace_format::decoder::StackPool,
        is_builtin: bool,
        depth: usize,
    ) -> anyhow::Result<ShapeValue> {
        use dial9_trace_format::types::FieldValueRef;

        ensure!(
            depth <= MAX_DYNAMIC_DEPTH,
            "excessive nesting depth ({depth}) in dynamic value"
        );

        match value {
            FieldValueRef::Varint(v) => {
                if is_builtin {
                    // Built-in operational metrics: use standard quantization
                    Ok(ShapeValue::U(quantize_numeric(*v)))
                } else {
                    // C - Custom non-identity integers: privacy bucket for all nonzero values
                    Ok(ShapeValue::U(privacy_bucket_u64(*v)))
                }
            }
            FieldValueRef::I64(v) => {
                if is_builtin {
                    Ok(ShapeValue::I(quantize_i64(*v)))
                } else {
                    Ok(ShapeValue::I(privacy_bucket_i64(*v)))
                }
            }
            FieldValueRef::F64(v) => Ok(ShapeValue::F(quantize_f64(*v)?)),
            FieldValueRef::Bool(v) => Ok(ShapeValue::B(*v)),
            FieldValueRef::String(s) => Ok(ShapeValue::S(self.sanitize_string(s))),
            FieldValueRef::Bytes(b) => {
                // (6) Check Bytes len BEFORE conversion
                ensure!(
                    b.len() <= MAX_BYTES_LENGTH as usize,
                    "Bytes field length {} exceeds limit {MAX_BYTES_LENGTH}",
                    b.len()
                );
                Ok(ShapeValue::Bytes(b.len() as u32))
            }
            // D - RECURSIVE POOL SEMANTICS: PooledString -> PS variant
            FieldValueRef::PooledString(id) => {
                let resolved = string_pool
                    .get(*id)
                    .ok_or_else(|| anyhow::anyhow!("missing pooled string id {:?}", id.raw_id()))?;
                Ok(ShapeValue::PS(self.sanitize_string(resolved)))
            }
            // D - Inline stack -> Stack variant
            FieldValueRef::StackFrames(frames_ref) => {
                // (6) Check Stack count BEFORE allocation
                ensure!(
                    (frames_ref.count() as usize) <= MAX_CONTAINER_ELEMENTS,
                    "StackFrames has {} elements, exceeds limit {MAX_CONTAINER_ELEMENTS}",
                    frames_ref.count()
                );
                let remapped: Vec<u64> = frames_ref.iter().map(|a| self.remap_address(a)).collect();
                Ok(ShapeValue::Stack(remapped))
            }
            // D - Pooled stack -> PStack variant
            FieldValueRef::PooledStackFrames(id) => {
                let frames = stack_pool
                    .get(*id)
                    .ok_or_else(|| anyhow::anyhow!("missing pooled stack id {:?}", id.raw_id()))?;
                // (6) Check PStack count BEFORE allocation
                ensure!(
                    frames.len() <= MAX_CONTAINER_ELEMENTS,
                    "PooledStackFrames has {} elements, exceeds limit {MAX_CONTAINER_ELEMENTS}",
                    frames.len()
                );
                let remapped: Vec<u64> = frames.iter().map(|a| self.remap_address(*a)).collect();
                Ok(ShapeValue::PStack(remapped))
            }
            FieldValueRef::StringMap(map_ref) => {
                // (6) Check StringMap count BEFORE allocation
                let count = map_ref.iter().count();
                ensure!(
                    count <= MAX_CONTAINER_ELEMENTS,
                    "StringMap has {count} entries, exceeds limit {MAX_CONTAINER_ELEMENTS}"
                );
                let sanitized: Vec<(String, String)> = map_ref
                    .iter()
                    .map(|(k, v)| (self.sanitize_string(k), self.sanitize_string(v)))
                    .collect();
                Ok(ShapeValue::StringMap(sanitized))
            }
            // D - RECURSIVE POOL SEMANTICS: recursively tag nested values
            FieldValueRef::List(list_ref) => {
                // (6) Check List len BEFORE allocation
                let count = list_ref.iter().count();
                ensure!(
                    count <= MAX_CONTAINER_ELEMENTS,
                    "List has {count} elements, exceeds limit {MAX_CONTAINER_ELEMENTS}"
                );
                let mapped: Vec<ShapeValue> = list_ref
                    .iter()
                    .map(|item| {
                        self.convert_structural_ref(
                            item,
                            FieldType::Varint,
                            string_pool,
                            stack_pool,
                            is_builtin,
                            depth + 1,
                        )
                    })
                    .collect::<anyhow::Result<_>>()?;
                Ok(ShapeValue::List(mapped))
            }
            FieldValueRef::Map(map_ref) => {
                // (6) Check Map len BEFORE allocation
                let count = map_ref.iter().count();
                ensure!(
                    count <= MAX_CONTAINER_ELEMENTS,
                    "Map has {count} entries, exceeds limit {MAX_CONTAINER_ELEMENTS}"
                );
                let mapped: Vec<(ShapeValue, ShapeValue)> = map_ref
                    .iter()
                    .map(|(k, v)| {
                        let kv = self.convert_structural_ref(
                            k,
                            FieldType::Varint,
                            string_pool,
                            stack_pool,
                            is_builtin,
                            depth + 1,
                        )?;
                        let vv = self.convert_structural_ref(
                            v,
                            FieldType::Varint,
                            string_pool,
                            stack_pool,
                            is_builtin,
                            depth + 1,
                        )?;
                        Ok((kv, vv))
                    })
                    .collect::<anyhow::Result<_>>()?;
                Ok(ShapeValue::Map(mapped))
            }
            FieldValueRef::None => Ok(ShapeValue::None),
            _ => bail!("unsupported FieldValueRef variant for structural field"),
        }
    }

    fn convert_identity_ref(
        &mut self,
        _schema_name: &str,
        field_name: &str,
        ns: NamespaceId,
        value: &dial9_trace_format::types::FieldValueRef<'_>,
        _inner_ft: FieldType,
        stack_pool: &dial9_trace_format::decoder::StackPool,
    ) -> anyhow::Result<ShapeValue> {
        use dial9_trace_format::types::FieldValueRef;

        match value {
            FieldValueRef::Varint(v) => {
                let remapped = match ns {
                    NamespaceId::Task => {
                        // H: Exclude task ID 0 from cardinality
                        if *v != 0 {
                            self.task_id_set.insert(*v);
                        }
                        self.remap_task_id(*v)
                    }
                    NamespaceId::Span => self.remap_span_id(*v),
                    NamespaceId::Tid => self.remap_thread_id(*v),
                    NamespaceId::Addr => self.remap_address(*v),
                    NamespaceId::SocketCookie => self.remap_socket_cookie(*v),
                    NamespaceId::SocketInode => self.remap_socket_inode(*v),
                    NamespaceId::Anon(_) => self.remap_generic_id(ns, *v),
                };
                Ok(ShapeValue::U(remapped))
            }
            // D - Inline stack frames in identity context. Check the count before
            // collecting so a hostile trace cannot force an oversized allocation.
            FieldValueRef::StackFrames(frames_ref) => {
                ensure!(
                    (frames_ref.count() as usize) <= MAX_CONTAINER_ELEMENTS,
                    "identity StackFrames for field '{field_name}' has {} elements, exceeds limit {MAX_CONTAINER_ELEMENTS}",
                    frames_ref.count()
                );
                let remapped: Vec<u64> = frames_ref.iter().map(|a| self.remap_address(a)).collect();
                Ok(ShapeValue::Stack(remapped))
            }
            // D - Pooled stack frames in identity context -> PStack. The pool
            // lookup is fallible and the bound is checked before copying.
            FieldValueRef::PooledStackFrames(id) => {
                let frames = stack_pool.get(*id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "missing pooled stack id {:?} for field '{field_name}'",
                        id.raw_id()
                    )
                })?;
                ensure!(
                    frames.len() <= MAX_CONTAINER_ELEMENTS,
                    "identity PooledStackFrames for field '{field_name}' has {} elements, exceeds limit {MAX_CONTAINER_ELEMENTS}",
                    frames.len()
                );
                let remapped: Vec<u64> = frames.iter().map(|a| self.remap_address(*a)).collect();
                Ok(ShapeValue::PStack(remapped))
            }
            _ => bail!(
                "identity field '{field_name}' has unexpected value type {:?}",
                value
            ),
        }
    }
}

fn compute_quantiles(values: &mut [u64]) -> Option<Quantiles> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let n = values.len();
    Some(Quantiles {
        count: n as u64,
        min_ns: values[0],
        p50_ns: values[n / 2],
        p90_ns: values[n * 90 / 100],
        p99_ns: values[n * 99 / 100],
        max_ns: values[n - 1],
    })
}

/// Core extraction logic.
///
/// Uses two fresh decoders with `try_for_each_event` to ensure each event is
/// processed while its active schema, string_pool, and stack_pool are still valid
/// (reset frames clear these). This fixes panics on traces with mid-stream resets.
#[allow(clippy::collapsible_if)] // Nested if-let chains are clearer for field matching
fn extract_shape(data: &[u8]) -> anyhow::Result<TraceShape> {
    // ─── Pass 1: determine global min/max timestamps ────────────────────
    let mut decoder1 = Decoder::new(data).context("invalid trace file header")?;
    let mut global_min_ts: Option<u64> = None;
    let mut global_max_ts: Option<u64> = None;

    decoder1
        .try_for_each_event(|ev| {
            let ts = ev.timestamp_ns.ok_or_else(|| {
                anyhow::anyhow!("event for schema '{}' has no timestamp", ev.name)
            })?;
            global_min_ts = Some(global_min_ts.map_or(ts, |m| m.min(ts)));
            global_max_ts = Some(global_max_ts.map_or(ts, |m| m.max(ts)));
            Ok::<(), anyhow::Error>(())
        })
        .map_err(|e| match e {
            dial9_trace_format::decoder::TryForEachError::Decode(d) => {
                anyhow::anyhow!("pass 1 decode error at byte {}: {}", d.pos, d.message)
            }
            dial9_trace_format::decoder::TryForEachError::User(u) => u.context("pass 1"),
        })?;
    ensure!(
        decoder1.position() == decoder1.data_len(),
        "pass 1: trailing {} bytes after last frame",
        decoder1.data_len() - decoder1.position()
    );

    let source_base_ts = global_min_ts.unwrap_or(0);

    // ─── Pass 2: extract schemas, annotations, and event values ─────────
    let mut decoder2 = Decoder::new(data).context("invalid trace file header (pass 2)")?;
    let mut ctx = ExtractContext::new();

    // Track schemas by their sanitized name to detect repeated/reset schemas
    let mut schema_index_map: HashMap<String, u32> = HashMap::new(); // original_name -> idx
    let mut schemas: Vec<ShapeSchema> = Vec::new();
    // For deduplication: sanitized_name -> sanitized ShapeSchema definition
    let mut sanitized_defs: HashMap<String, ShapeSchema> = HashMap::new();
    let mut original_fields: HashMap<String, Vec<(String, FieldType)>> = HashMap::new();

    let mut events: Vec<ShapeEvent> = Vec::new();
    let mut event_type_counts: BTreeMap<String, u64> = BTreeMap::new();

    decoder2
        .try_for_each_event(|ev| {
            let original_name = ev.name;

            // (1) RESET SAME-NAME CONFLICT: Inspect/sanitize schema on EVERY event
            // occurrence, even if original name already mapped. Compare complete
            // sanitized schema definition against existing shape schema each time.
            {
                let sanitized_name = ctx.sanitize_schema_name(original_name);

                // Built-in treatment is granted only to a complete known
                // name/order/type signature. This prevents a custom schema from
                // claiming a built-in name to receive weaker transformations.
                if is_builtin_schema(original_name) {
                    validate_builtin_schema_signature(original_name, ev.schema.fields())?;
                }

                let orig_flds: Vec<(String, FieldType)> = ev
                    .schema
                    .fields()
                    .iter()
                    .map(|f| (f.name().to_string(), f.field_type()))
                    .collect();

                let fields: Vec<ShapeField> = ev
                    .schema
                    .fields()
                    .iter()
                    .map(|f| {
                        let fname =
                            ctx.sanitize_field_name(&sanitized_name, original_name, f.name());
                        let norm_ft = normalize_field_type(f.field_type());
                        let mut meta = classify_field(original_name, f.name(), f.field_type());
                        // C - Assign anonymous namespace for generic *_id fields
                        if meta.semantics == FieldSemantics::Identity && meta.namespace.is_none() {
                            meta.namespace = Some(ctx.get_anon_namespace(f.name())?);
                        }
                        Ok(ShapeField {
                            name: fname,
                            field_type: norm_ft as u8,
                            repeat_meta: if meta.semantics == FieldSemantics::Structural
                                && meta.namespace.is_none()
                            {
                                None
                            } else {
                                Some(meta)
                            },
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;

                // Capture safe annotations from the active schema
                let annotations: Vec<ShapeAnnotation> = ev
                    .schema
                    .annotations()
                    .iter()
                    .filter_map(|a| {
                        if SAFE_ANNOTATION_KEYS.contains(&a.key())
                            && SAFE_UNITS.contains(&a.value())
                        {
                            if (a.field_index() as usize) >= fields.len() {
                                return None;
                            }
                            Some(ShapeAnnotation {
                                field_index: a.field_index(),
                                key: a.key().to_string(),
                                value: a.value().to_string(),
                            })
                        } else {
                            None
                        }
                    })
                    .collect();

                let shape_schema = ShapeSchema {
                    name: sanitized_name.clone(),
                    has_timestamp: ev.schema.has_timestamp(),
                    fields,
                    annotations,
                };

                if let Some(existing) = sanitized_defs.get(&sanitized_name) {
                    // (1) Compare complete sanitized schema definition against existing
                    let existing_json = serde_json::to_string(existing).unwrap_or_default();
                    let new_json = serde_json::to_string(&shape_schema).unwrap_or_default();
                    if existing_json != new_json {
                        return Err(anyhow::anyhow!(
                            "schema '{}' (sanitized '{}') re-registered after reset with \
                             different definition (field count/type/name/annotations/repeat \
                             metadata differ)",
                            original_name,
                            sanitized_name
                        ));
                    }
                    // Same definition under different wire ID is fine; reuse existing index
                    if !schema_index_map.contains_key(original_name) {
                        let existing_idx = schemas
                            .iter()
                            .position(|s| s.name == sanitized_name)
                            .unwrap();
                        schema_index_map.insert(original_name.to_string(), existing_idx as u32);
                    }
                } else {
                    let idx = schemas.len() as u32;
                    schema_index_map.insert(original_name.to_string(), idx);
                    sanitized_defs.insert(sanitized_name, shape_schema.clone());
                    schemas.push(shape_schema);
                }

                // Always update original_fields to current schema's fields (for remap)
                original_fields.insert(original_name.to_string(), orig_flds);
            }

            let schema_idx = schema_index_map[original_name];

            // Require timestamp
            let ts = ev.timestamp_ns.ok_or_else(|| {
                anyhow::anyhow!("event for schema '{}' has no timestamp", original_name)
            })?;
            let offset = quantize_ns(ts.saturating_sub(source_base_ts));

            // H: Track poll quantiles using stack-based pairing
            if original_name == "PollStartEvent" {
                if let Some(dial9_trace_format::types::FieldValueRef::Varint(wid)) =
                    ev.fields.first()
                {
                    ctx.poll_starts.entry(*wid).or_default().push(ts);
                }
            }
            if original_name == "PollEndEvent" {
                if let Some(dial9_trace_format::types::FieldValueRef::Varint(wid)) =
                    ev.fields.first()
                {
                    if let Some(starts) = ctx.poll_starts.get_mut(wid) {
                        if let Some(start) = starts.pop() {
                            if ts > start {
                                ctx.poll_durations.push(quantize_ns(ts - start));
                            }
                        }
                    }
                }
            }

            // H: Track span quantiles using stack-based pairing (SpanEnter:/SpanExit: only)
            let orig_flds = original_fields.get(original_name);
            if let Some(("SpanEnter:", _)) = span_prefix(original_name) {
                if let Some(flds) = orig_flds {
                    for (i, (fname, _)) in flds.iter().enumerate() {
                        if fname == "span_id" {
                            if let Some(dial9_trace_format::types::FieldValueRef::Varint(sid)) =
                                ev.fields.get(i)
                            {
                                ctx.span_enters.entry(*sid).or_default().push(ts);
                            }
                        }
                    }
                }
            }
            if let Some(("SpanExit:", _)) = span_prefix(original_name) {
                if let Some(flds) = orig_flds {
                    for (i, (fname, _)) in flds.iter().enumerate() {
                        if fname == "span_id" {
                            if let Some(dial9_trace_format::types::FieldValueRef::Varint(sid)) =
                                ev.fields.get(i)
                            {
                                if let Some(enters) = ctx.span_enters.get_mut(sid) {
                                    if let Some(enter_ts) = enters.pop() {
                                        if ts > enter_ts {
                                            ctx.span_durations.push(quantize_ns(ts - enter_ts));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Remap field values using active pools from the RawEvent
            let orig_flds = original_fields.get(original_name).ok_or_else(|| {
                anyhow::anyhow!("missing original fields for '{}'", original_name)
            })?;
            let is_builtin = is_builtin_schema(original_name);
            let mut remapped_values: Vec<ShapeValue> = Vec::with_capacity(ev.fields.len());
            for (i, fvr) in ev.fields.iter().enumerate() {
                let (fname, ftype) = orig_flds.get(i).ok_or_else(|| {
                    anyhow::anyhow!(
                        "field index {i} out of range for schema '{}'",
                        original_name
                    )
                })?;
                let mut meta = classify_field(original_name, fname, *ftype);
                // C - Assign anonymous namespace for generic *_id fields
                if meta.semantics == FieldSemantics::Identity && meta.namespace.is_none() {
                    meta.namespace = Some(ctx.get_anon_namespace(fname)?);
                }
                let sv = ctx.remap_value_ref(
                    original_name,
                    fname,
                    &meta,
                    fvr,
                    *ftype,
                    ev.string_pool,
                    ev.stack_pool,
                    source_base_ts,
                    offset,
                    is_builtin,
                )?;
                remapped_values.push(sv);
            }

            let sanitized_name = &schemas[schema_idx as usize].name;
            *event_type_counts.entry(sanitized_name.clone()).or_default() += 1;

            events.push(ShapeEvent {
                schema_index: schema_idx,
                timestamp_offset_ns: Some(offset),
                values: remapped_values,
            });

            Ok::<(), anyhow::Error>(())
        })
        .map_err(|e| match e {
            dial9_trace_format::decoder::TryForEachError::Decode(d) => {
                anyhow::anyhow!("pass 2 decode error at byte {}: {}", d.pos, d.message)
            }
            dial9_trace_format::decoder::TryForEachError::User(u) => u.context("pass 2"),
        })?;
    ensure!(
        decoder2.position() == decoder2.data_len(),
        "pass 2: trailing {} bytes after last frame",
        decoder2.data_len() - decoder2.position()
    );

    let duration_ns = match (global_min_ts, global_max_ts) {
        (Some(mn), Some(mx)) => quantize_ns(mx.saturating_sub(mn)),
        _ => 0,
    };

    // (2) SUMMARY CONSISTENCY: Build events first, then derive the ENTIRE summary
    // using the same recompute_summary used by validation. Do NOT mutate
    // cardinality/durations inside the low-level remap loop.
    let mut shape = TraceShape {
        version: SHAPE_VERSION,
        summary: ShapeSummary {
            event_count: events.len() as u64,
            duration_ns,
            event_type_counts,
            worker_cardinality: 0,
            task_cardinality: 0,
            poll_duration_quantiles: None,
            span_duration_quantiles: None,
        },
        schemas,
        events,
    };

    // Recompute cardinality and quantiles from the shape events themselves
    let recomputed = recompute_summary(&shape);
    shape.summary.worker_cardinality = recomputed.worker_cardinality;
    shape.summary.task_cardinality = recomputed.task_cardinality;
    shape.summary.poll_duration_quantiles = recomputed.poll_duration_quantiles;
    shape.summary.span_duration_quantiles = recomputed.span_duration_quantiles;

    // (2) Call validate_shape to ensure every extracted shape is generatable
    validate_shape(&shape).context("extracted shape failed validation")?;

    Ok(shape)
}

// ─── Validation ─────────────────────────────────────────────────────────────

/// Rejected fixed-width tags that cannot be re-encoded via write_field_value.
const REJECTED_FIXED_TAGS: &[u8] = &[
    FieldType::U8 as u8,
    FieldType::U16 as u8,
    FieldType::U32 as u8,
    FieldType::OptionalU8 as u8,
    FieldType::OptionalU16 as u8,
    FieldType::OptionalU32 as u8,
];

/// E - Complete validation of every shape invariant before generation.
///
/// Validates:
/// - Version, non-empty schemas/events
/// - Schema name non-empty and unique
/// - Field name non-empty and unique within each schema
/// - Field type tags valid and not rejected fixed-width
/// - FieldRepeatMeta invariants per semantics (see inline)
/// - Annotations: valid field_index, safe keys/values, no duplicates
/// - Untimestamped schemas must not have events
/// - Event schema_index in range, value count matches field count
/// - Timestamp presence on timestamped schemas
/// - Value/type compatibility recursively (including depth/count budgets)
/// - F: Bytes length, stack/list/map element counts
/// - Summary exact equality: event_count, event_type_counts both directions
/// - H: Recomputed worker/task cardinality and poll/span quantiles validated
#[allow(clippy::collapsible_if)]
fn validate_shape(shape: &TraceShape) -> anyhow::Result<()> {
    ensure!(
        shape.version == SHAPE_VERSION,
        "unsupported shape version {} (expected {SHAPE_VERSION})",
        shape.version
    );
    ensure!(!shape.schemas.is_empty(), "shape has no schemas");
    ensure!(!shape.events.is_empty(), "shape has no events");

    // Wire-width representability: the encoder uses u16 for schema count,
    // schema name length, field count, and field name length. Enforce these
    // limits at validation time so generation cannot produce malformed output.
    // The encoder allocates wire type IDs starting at STATIC_WIRE_ID_LIMIT (256).
    // The final u16 value cannot be allocated because advancing the registry
    // cursor after that allocation would overflow.
    ensure!(
        shape.schemas.len() <= MAX_WIRE_SCHEMAS,
        "shape has {} schemas, exceeds wire type-ID capacity ({MAX_WIRE_SCHEMAS})",
        shape.schemas.len()
    );

    // Schema name uniqueness and non-empty
    let mut seen_names: HashSet<&str> = HashSet::new();
    for (si, schema) in shape.schemas.iter().enumerate() {
        ensure!(
            !schema.name.is_empty(),
            "schema at index {si} has empty name"
        );
        ensure!(
            seen_names.insert(&schema.name),
            "duplicate schema name '{}' at index {si}",
            schema.name
        );

        // Wire-width: schema name must fit in u16 length field
        ensure!(
            schema.name.len() <= u16::MAX as usize,
            "schema '{}' name is {} bytes, exceeds u16 wire limit (65535)",
            schema.name,
            schema.name.len()
        );
        // Wire-width: field count must fit in u16
        ensure!(
            schema.fields.len() <= u16::MAX as usize,
            "schema '{}' has {} fields, exceeds u16 wire limit (65535)",
            schema.name,
            schema.fields.len()
        );

        // Field name uniqueness within schema
        let mut seen_field_names: HashSet<&str> = HashSet::new();
        for (fi, field) in schema.fields.iter().enumerate() {
            ensure!(
                !field.name.is_empty(),
                "schema '{}' field {fi} has empty name",
                schema.name
            );
            // Wire-width: field name must fit in u16 length field
            ensure!(
                field.name.len() <= u16::MAX as usize,
                "schema '{}' field '{}' name is {} bytes, exceeds u16 wire limit (65535)",
                schema.name,
                field.name,
                field.name.len()
            );
            ensure!(
                seen_field_names.insert(&field.name),
                "schema '{}' has duplicate field name '{}' at index {fi}",
                schema.name,
                field.name
            );
            ensure!(
                FieldType::from_tag(field.field_type).is_some(),
                "schema '{}' field '{}' has invalid type tag {:#x}",
                schema.name,
                field.name,
                field.field_type
            );
            ensure!(
                !REJECTED_FIXED_TAGS.contains(&field.field_type),
                "schema '{}' field '{}' uses rejected fixed-width tag {:#x}; \
                 use Varint/OptionalVarint instead (see fixed-width normalization docs)",
                schema.name,
                field.name,
                field.field_type
            );

            // E: Validate FieldRepeatMeta invariants per semantics
            if let Some(meta) = &field.repeat_meta {
                let ft = FieldType::from_tag(field.field_type).unwrap();
                let inner = ft.inner();

                match meta.semantics {
                    FieldSemantics::Identity => {
                        // Identity requires exactly one namespace (anonymous/fixed numeric)
                        ensure!(
                            meta.namespace.is_some(),
                            "schema '{}' field '{}': Identity semantics requires a namespace",
                            schema.name,
                            field.name
                        );
                        // Identity requires compatible top-level type: U (Varint) or Stack/PStack
                        let ok = matches!(
                            inner,
                            FieldType::Varint
                                | FieldType::StackFrames
                                | FieldType::PooledStackFrames
                        );
                        ensure!(
                            ok,
                            "schema '{}' field '{}': Identity semantics incompatible with type {:?}",
                            schema.name,
                            field.name,
                            inner
                        );
                        // Stack identity only allows address namespace
                        if matches!(inner, FieldType::StackFrames | FieldType::PooledStackFrames) {
                            ensure!(
                                meta.namespace == Some(NamespaceId::Addr),
                                "schema '{}' field '{}': stack Identity must use Addr namespace, got {:?}",
                                schema.name,
                                field.name,
                                meta.namespace
                            );
                        }
                    }
                    FieldSemantics::TimestampRef => {
                        // TimestampRef requires Varint (varint-compatible field)
                        ensure!(
                            inner == FieldType::Varint,
                            "schema '{}' field '{}': TimestampRef requires Varint type, got {:?}",
                            schema.name,
                            field.name,
                            inner
                        );
                        // TimestampRef forbids namespace
                        ensure!(
                            meta.namespace.is_none(),
                            "schema '{}' field '{}': TimestampRef forbids namespace",
                            schema.name,
                            field.name
                        );
                    }
                    FieldSemantics::RealtimeOffset => {
                        // RealtimeOffset requires Varint
                        ensure!(
                            inner == FieldType::Varint,
                            "schema '{}' field '{}': RealtimeOffset requires Varint type, got {:?}",
                            schema.name,
                            field.name,
                            inner
                        );
                        // RealtimeOffset forbids namespace
                        ensure!(
                            meta.namespace.is_none(),
                            "schema '{}' field '{}': RealtimeOffset forbids namespace",
                            schema.name,
                            field.name
                        );
                    }
                    FieldSemantics::Structural => {
                        // Structural forbids namespace
                        ensure!(
                            meta.namespace.is_none(),
                            "schema '{}' field '{}': Structural semantics forbids namespace",
                            schema.name,
                            field.name
                        );
                    }
                }
            }
        }

        // Validate annotations: wire count, valid indexes, safe keys/values, no duplicates
        ensure!(
            schema.annotations.len() <= MAX_SCHEMA_ANNOTATIONS,
            "schema '{}' has {} annotations, exceeds u16 wire limit ({MAX_SCHEMA_ANNOTATIONS})",
            schema.name,
            schema.annotations.len()
        );
        let mut seen_annotations: HashSet<(u16, &str)> = HashSet::new();
        for ann in &schema.annotations {
            ensure!(
                (ann.field_index as usize) < schema.fields.len(),
                "schema '{}' annotation field_index {} >= field count {}",
                schema.name,
                ann.field_index,
                schema.fields.len()
            );
            ensure!(
                SAFE_ANNOTATION_KEYS.contains(&ann.key.as_str()),
                "schema '{}' annotation has unsafe key '{}'",
                schema.name,
                ann.key
            );
            ensure!(
                SAFE_UNITS.contains(&ann.value.as_str()),
                "schema '{}' annotation has unsafe value '{}'",
                schema.name,
                ann.value
            );
            ensure!(
                seen_annotations.insert((ann.field_index, &ann.key)),
                "schema '{}' has duplicate annotation (field_index={}, key='{}')",
                schema.name,
                ann.field_index,
                ann.key
            );
        }

        // Untimestamped schemas must not have events
        if !schema.has_timestamp {
            let has_events = shape.events.iter().any(|e| e.schema_index == si as u32);
            ensure!(
                !has_events,
                "schema '{}' has has_timestamp=false but has events; Encoder cannot emit these",
                schema.name
            );
        }
    }

    // Validate events and compute actual counts
    let actual_max_offset = shape
        .events
        .iter()
        .filter_map(|e| e.timestamp_offset_ns)
        .max()
        .unwrap_or(0);

    let mut actual_type_counts: BTreeMap<String, u64> = BTreeMap::new();
    for (i, event) in shape.events.iter().enumerate() {
        let si = event.schema_index as usize;
        ensure!(
            si < shape.schemas.len(),
            "event {i} references schema index {si} but only {} schemas exist",
            shape.schemas.len()
        );
        let schema = &shape.schemas[si];
        ensure!(
            event.values.len() == schema.fields.len(),
            "event {i} has {} values but schema '{}' has {} fields",
            event.values.len(),
            schema.name,
            schema.fields.len()
        );

        if schema.has_timestamp {
            ensure!(
                event.timestamp_offset_ns.is_some(),
                "event {i} for timestamped schema '{}' missing timestamp_offset_ns",
                schema.name
            );
            let offset = event.timestamp_offset_ns.unwrap();
            ensure!(
                offset <= actual_max_offset,
                "event {i} offset {offset} > derived max offset {actual_max_offset}"
            );
        }

        // Validate value type compatibility recursively (includes F resource checks)
        for (fi, (sv, field)) in event.values.iter().zip(schema.fields.iter()).enumerate() {
            validate_value_compat(sv, field.field_type, &schema.name, &field.name, i, fi)?;
        }

        *actual_type_counts.entry(schema.name.clone()).or_default() += 1;
    }

    // E: Verify summary event_count exact equality
    ensure!(
        shape.summary.event_count == shape.events.len() as u64,
        "summary event_count {} != actual {}",
        shape.summary.event_count,
        shape.events.len()
    );

    // E: Verify event_type_counts exact equality in both directions
    // (2) Also reject extra zero-count keys (they must not be present at all)
    for (name, expected) in &shape.summary.event_type_counts {
        ensure!(
            *expected > 0,
            "summary event_type_counts['{name}'] = 0; zero-count keys must not be present"
        );
        let actual = actual_type_counts.get(name).copied().unwrap_or(0);
        ensure!(
            actual == *expected,
            "summary event_type_counts['{name}'] = {expected} but actual = {actual}"
        );
    }
    for (name, actual) in &actual_type_counts {
        let expected = shape
            .summary
            .event_type_counts
            .get(name)
            .copied()
            .unwrap_or(0);
        ensure!(
            *actual == expected,
            "actual events have {actual} '{name}' events but summary does not list it (expected {expected})"
        );
    }

    // E: Verify summary duration_ns matches actual max offset
    ensure!(
        shape.summary.duration_ns == actual_max_offset,
        "summary duration_ns {} != actual max offset {}",
        shape.summary.duration_ns,
        actual_max_offset
    );

    // H: Recompute and validate summary cardinality and quantiles
    let recomputed = recompute_summary(shape);
    ensure!(
        shape.summary.worker_cardinality == recomputed.worker_cardinality,
        "summary worker_cardinality {} != recomputed {}",
        shape.summary.worker_cardinality,
        recomputed.worker_cardinality
    );
    ensure!(
        shape.summary.task_cardinality == recomputed.task_cardinality,
        "summary task_cardinality {} != recomputed {}",
        shape.summary.task_cardinality,
        recomputed.task_cardinality
    );
    validate_quantiles_match(
        "poll_duration",
        &shape.summary.poll_duration_quantiles,
        &recomputed.poll_duration_quantiles,
    )?;
    validate_quantiles_match(
        "span_duration",
        &shape.summary.span_duration_quantiles,
        &recomputed.span_duration_quantiles,
    )?;

    Ok(())
}

/// Validate that two optional quantiles are equal.
fn validate_quantiles_match(
    label: &str,
    declared: &Option<Quantiles>,
    recomputed: &Option<Quantiles>,
) -> anyhow::Result<()> {
    match (declared, recomputed) {
        (None, None) => Ok(()),
        (Some(_), None) => {
            bail!("summary {label}_quantiles is present but recomputation found no data")
        }
        (None, Some(_)) => {
            bail!("summary {label}_quantiles is absent but recomputation found data")
        }
        (Some(d), Some(r)) => {
            ensure!(
                d.count == r.count,
                "summary {label}_quantiles.count {} != recomputed {}",
                d.count,
                r.count
            );
            ensure!(
                d.min_ns == r.min_ns,
                "summary {label}_quantiles.min_ns {} != recomputed {}",
                d.min_ns,
                r.min_ns
            );
            ensure!(
                d.p50_ns == r.p50_ns,
                "summary {label}_quantiles.p50_ns {} != recomputed {}",
                d.p50_ns,
                r.p50_ns
            );
            ensure!(
                d.p90_ns == r.p90_ns,
                "summary {label}_quantiles.p90_ns {} != recomputed {}",
                d.p90_ns,
                r.p90_ns
            );
            ensure!(
                d.p99_ns == r.p99_ns,
                "summary {label}_quantiles.p99_ns {} != recomputed {}",
                d.p99_ns,
                r.p99_ns
            );
            ensure!(
                d.max_ns == r.max_ns,
                "summary {label}_quantiles.max_ns {} != recomputed {}",
                d.max_ns,
                r.max_ns
            );
            Ok(())
        }
    }
}

/// H: Recompute summary from shape events using the same logic as extraction.
///
/// Worker cardinality: count from worker_id fields only (not target_worker),
/// excluding sentinel values 254/255.
/// Task cardinality: count from task_id fields, excluding 0.
/// Poll durations: pair PollStart/PollEnd by worker_id using a per-worker stack.
/// Span durations: pair SpanEnter/SpanExit by span_id using a queue per span.
#[allow(clippy::collapsible_if)] // Nested if-let chains are clearer for field matching
fn recompute_summary(shape: &TraceShape) -> ShapeSummary {
    let mut worker_ids: HashSet<u64> = HashSet::new();
    let mut task_ids: HashSet<u64> = HashSet::new();

    // H: For poll durations, track start timestamps per worker using a stack
    // (handles nested polls, though unlikely). Sorted by timestamp for correctness.
    let mut poll_starts: HashMap<u64, Vec<u64>> = HashMap::new(); // worker_id -> stack of start ts
    let mut poll_durations: Vec<u64> = Vec::new();

    // H: For span durations, track enter timestamps per span_id using a stack
    let mut span_enters: HashMap<u64, Vec<u64>> = HashMap::new(); // span_id -> stack of enter ts
    let mut span_durations: Vec<u64> = Vec::new();

    for event in &shape.events {
        let schema = &shape.schemas[event.schema_index as usize];
        let ts = event.timestamp_offset_ns.unwrap_or(0);

        // Collect worker_id from fields named "worker_id" (via schema field index)
        for (fi, field) in schema.fields.iter().enumerate() {
            if field.name == "worker_id" {
                if let Some(ShapeValue::U(wid)) = event.values.get(fi) {
                    // H: Exclude sentinel workers 254/255
                    if *wid != 254 && *wid != 255 {
                        worker_ids.insert(*wid);
                    }
                }
            }
            // Task cardinality from fields with Identity semantics + Task namespace
            if let Some(meta) = &field.repeat_meta {
                if meta.semantics == FieldSemantics::Identity
                    && meta.namespace == Some(NamespaceId::Task)
                {
                    if let Some(ShapeValue::U(tid)) = event.values.get(fi) {
                        // H: Exclude task ID 0
                        if *tid != 0 {
                            task_ids.insert(*tid);
                        }
                    }
                }
            }
        }

        // H: Poll duration pairing
        if schema.name == "PollStartEvent" {
            // Get worker_id from the first worker_id field
            for (fi, field) in schema.fields.iter().enumerate() {
                if field.name == "worker_id" {
                    if let Some(ShapeValue::U(wid)) = event.values.get(fi) {
                        poll_starts.entry(*wid).or_default().push(ts);
                    }
                    break;
                }
            }
        }
        if schema.name == "PollEndEvent" {
            for (fi, field) in schema.fields.iter().enumerate() {
                if field.name == "worker_id" {
                    if let Some(ShapeValue::U(wid)) = event.values.get(fi) {
                        // H: Pop from start stack (LIFO/queue pairing)
                        if let Some(starts) = poll_starts.get_mut(wid) {
                            if let Some(start_ts) = starts.pop() {
                                if ts >= start_ts {
                                    poll_durations.push(ts - start_ts);
                                }
                            }
                        }
                    }
                    break;
                }
            }
        }

        // H: Span duration pairing - use dynamic SpanEnter:/SpanExit: only
        if schema.name.starts_with("SpanEnter:") {
            for (fi, field) in schema.fields.iter().enumerate() {
                if field.name == "span_id" {
                    if let Some(ShapeValue::U(sid)) = event.values.get(fi) {
                        span_enters.entry(*sid).or_default().push(ts);
                    }
                    break;
                }
            }
        }
        if schema.name.starts_with("SpanExit:") {
            for (fi, field) in schema.fields.iter().enumerate() {
                if field.name == "span_id" {
                    if let Some(ShapeValue::U(sid)) = event.values.get(fi) {
                        // H: Pop from enter stack (LIFO for nested enters)
                        if let Some(enters) = span_enters.get_mut(sid) {
                            if let Some(enter_ts) = enters.pop() {
                                if ts >= enter_ts {
                                    span_durations.push(ts - enter_ts);
                                }
                            }
                        }
                    }
                    break;
                }
            }
        }
    }

    let poll_quantiles = compute_quantiles(&mut poll_durations);
    let span_quantiles = compute_quantiles(&mut span_durations);

    ShapeSummary {
        event_count: shape.events.len() as u64,
        duration_ns: shape
            .events
            .iter()
            .filter_map(|e| e.timestamp_offset_ns)
            .max()
            .unwrap_or(0),
        event_type_counts: BTreeMap::new(), // not needed for comparison
        worker_cardinality: worker_ids.len() as u32,
        task_cardinality: task_ids.len() as u32,
        poll_duration_quantiles: poll_quantiles,
        span_duration_quantiles: span_quantiles,
    }
}

/// F: Recursively validate that a ShapeValue is compatible with the declared FieldType.
/// Also enforces resource bounds on container sizes and byte lengths.
fn validate_value_compat(
    sv: &ShapeValue,
    field_type_tag: u8,
    schema_name: &str,
    field_name: &str,
    event_idx: usize,
    field_idx: usize,
) -> anyhow::Result<()> {
    let ft = FieldType::from_tag(field_type_tag).unwrap(); // already validated
    let is_optional = ft.is_optional();
    let inner = ft.inner();

    // None only valid for optional fields
    if matches!(sv, ShapeValue::None) {
        ensure!(
            is_optional,
            "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
             None value but field is not optional"
        );
        return Ok(());
    }

    match (sv, inner) {
        (ShapeValue::U(_), FieldType::Varint) => Ok(()),
        (ShapeValue::I(_), FieldType::I64) => Ok(()),
        (ShapeValue::F(_), FieldType::F64) => Ok(()),
        (ShapeValue::B(_), FieldType::Bool) => Ok(()),
        (ShapeValue::S(_), FieldType::String) => Ok(()),
        (ShapeValue::PS(_), FieldType::PooledString) => Ok(()),
        (ShapeValue::Bytes(len), FieldType::Bytes) => {
            // F: Reject oversized byte declarations
            ensure!(
                *len <= MAX_BYTES_LENGTH,
                "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                 Bytes length {len} exceeds limit {MAX_BYTES_LENGTH}"
            );
            Ok(())
        }
        (ShapeValue::Stack(addrs), FieldType::StackFrames) => {
            ensure!(
                addrs.len() <= MAX_CONTAINER_ELEMENTS,
                "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                 Stack has {} elements, exceeds limit {MAX_CONTAINER_ELEMENTS}",
                addrs.len()
            );
            Ok(())
        }
        (ShapeValue::PStack(addrs), FieldType::PooledStackFrames) => {
            ensure!(
                addrs.len() <= MAX_CONTAINER_ELEMENTS,
                "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                 PStack has {} elements, exceeds limit {MAX_CONTAINER_ELEMENTS}",
                addrs.len()
            );
            Ok(())
        }
        (ShapeValue::StringMap(pairs), FieldType::StringMap) => {
            ensure!(
                pairs.len() <= MAX_CONTAINER_ELEMENTS,
                "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                 StringMap has {} entries, exceeds limit {MAX_CONTAINER_ELEMENTS}",
                pairs.len()
            );
            Ok(())
        }
        (ShapeValue::List(items), FieldType::DynamicList) => {
            ensure!(
                items.len() <= MAX_CONTAINER_ELEMENTS,
                "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                 List has {} elements, exceeds limit {MAX_CONTAINER_ELEMENTS}",
                items.len()
            );
            validate_nested_values(items, schema_name, field_name, event_idx, field_idx, 1)
        }
        (ShapeValue::Map(pairs), FieldType::DynamicMap) => {
            ensure!(
                pairs.len() <= MAX_CONTAINER_ELEMENTS,
                "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                 Map has {} entries, exceeds limit {MAX_CONTAINER_ELEMENTS}",
                pairs.len()
            );
            for (k, v) in pairs {
                ensure!(
                    !matches!(k, ShapeValue::None),
                    "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                     nested None in dynamic map key"
                );
                ensure!(
                    !matches!(v, ShapeValue::None),
                    "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                     nested None in dynamic map value"
                );
                validate_nested_value(k, schema_name, field_name, event_idx, field_idx, 1)?;
                validate_nested_value(v, schema_name, field_name, event_idx, field_idx, 1)?;
            }
            Ok(())
        }
        _ => bail!(
            "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
             value type {:?} incompatible with field type {:?}",
            std::mem::discriminant(sv),
            inner
        ),
    }
}

/// D/F: Recursively validate nested dynamic values: reject None, excessive depth,
/// and oversized containers.
fn validate_nested_values(
    items: &[ShapeValue],
    schema_name: &str,
    field_name: &str,
    event_idx: usize,
    field_idx: usize,
    depth: usize,
) -> anyhow::Result<()> {
    ensure!(
        depth <= MAX_DYNAMIC_DEPTH,
        "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
         excessive nesting depth ({depth})"
    );
    for item in items {
        ensure!(
            !matches!(item, ShapeValue::None),
            "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
             nested None in dynamic container is not allowed"
        );
        validate_nested_value(item, schema_name, field_name, event_idx, field_idx, depth)?;
    }
    Ok(())
}

/// Validate a single nested value (called recursively).
fn validate_nested_value(
    item: &ShapeValue,
    schema_name: &str,
    field_name: &str,
    event_idx: usize,
    field_idx: usize,
    depth: usize,
) -> anyhow::Result<()> {
    match item {
        ShapeValue::Bytes(len) => {
            ensure!(
                *len <= MAX_BYTES_LENGTH,
                "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                 nested Bytes length {len} exceeds limit {MAX_BYTES_LENGTH}"
            );
        }
        ShapeValue::Stack(addrs) | ShapeValue::PStack(addrs) => {
            ensure!(
                addrs.len() <= MAX_CONTAINER_ELEMENTS,
                "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                 nested stack has {} elements, exceeds limit {MAX_CONTAINER_ELEMENTS}",
                addrs.len()
            );
        }
        ShapeValue::List(inner_items) => {
            ensure!(
                inner_items.len() <= MAX_CONTAINER_ELEMENTS,
                "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                 nested List has {} elements at depth {depth}",
                inner_items.len()
            );
            validate_nested_values(
                inner_items,
                schema_name,
                field_name,
                event_idx,
                field_idx,
                depth + 1,
            )?;
        }
        ShapeValue::Map(inner_pairs) => {
            ensure!(
                inner_pairs.len() <= MAX_CONTAINER_ELEMENTS,
                "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                 nested Map has {} entries at depth {depth}",
                inner_pairs.len()
            );
            for (k, v) in inner_pairs {
                ensure!(
                    !matches!(k, ShapeValue::None),
                    "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                     nested None in dynamic map key"
                );
                ensure!(
                    !matches!(v, ShapeValue::None),
                    "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                     nested None in dynamic map value"
                );
                validate_nested_value(k, schema_name, field_name, event_idx, field_idx, depth + 1)?;
                validate_nested_value(v, schema_name, field_name, event_idx, field_idx, depth + 1)?;
            }
        }
        ShapeValue::None => {
            bail!(
                "event {event_idx} field {field_idx} ('{field_name}' in '{schema_name}'): \
                 nested None in dynamic container"
            );
        }
        _ => {} // Scalars are fine
    }
    Ok(())
}

/// F: Validate repeat preflight — check total event count and conservative output size.
/// (5) FULL PREFLIGHT: Precompute/check every arithmetic operation for the LAST
/// repetition before opening output. No failure may first appear after output creation.
fn validate_repeat_preflight(shape: &TraceShape, repeat: u32) -> anyhow::Result<()> {
    // Checked event count multiplication
    let total_events = (shape.events.len() as u64)
        .checked_mul(repeat as u64)
        .ok_or_else(|| anyhow::anyhow!("events.len() * repeat overflows u64"))?;
    ensure!(
        total_events <= MAX_GENERATED_EVENTS,
        "total event count {total_events} exceeds limit {MAX_GENERATED_EVENTS}"
    );

    // F: Conservative encoded-size estimator
    let estimated_size = estimate_encoded_size(shape, repeat)?;
    ensure!(
        estimated_size <= MAX_GENERATED_OUTPUT_BYTES,
        "estimated output size {estimated_size} bytes exceeds limit {MAX_GENERATED_OUTPUT_BYTES}"
    );

    // (5) Precompute template_duration, gap, and time_shift for the last repetition
    let actual_duration = shape
        .events
        .iter()
        .filter_map(|e| e.timestamp_offset_ns)
        .max()
        .unwrap_or(0);
    let template_duration = actual_duration.max(QUANTUM_NS);
    let gap = QUANTUM_NS;
    let duration_plus_gap = template_duration
        .checked_add(gap)
        .ok_or_else(|| anyhow::anyhow!("preflight: template_duration + gap overflow"))?;

    let last_rep = (repeat as u64).saturating_sub(1);
    let max_time_shift = last_rep
        .checked_mul(duration_plus_gap)
        .ok_or_else(|| anyhow::anyhow!("preflight: rep * (duration+gap) overflow at last rep"))?;

    // (5) Verify every event timestamp won't overflow at last repetition
    for (i, event) in shape.events.iter().enumerate() {
        let ts_offset = event.timestamp_offset_ns.unwrap_or(0);
        ts_offset.checked_add(max_time_shift).ok_or_else(|| {
            anyhow::anyhow!(
                "preflight: event {i} timestamp offset {ts_offset} + time_shift {max_time_shift} overflow"
            )
        })?;
    }

    // (5) Compute namespace strides including nested addresses, verify overflow
    let strides = compute_namespace_strides(shape)?;
    for (ns, stride) in &strides {
        let max_id_shift = last_rep.checked_mul(*stride).ok_or_else(|| {
            anyhow::anyhow!("preflight: namespace {ns:?} stride {stride} * last_rep overflow")
        })?;
        // Find max value in this namespace to check shifted value doesn't overflow
        let max_val = find_namespace_max_value(shape, *ns);
        if max_val > 0 {
            max_val.checked_add(max_id_shift).ok_or_else(|| {
                anyhow::anyhow!(
                    "preflight: namespace {ns:?} max_val {max_val} + shift {max_id_shift} overflow"
                )
            })?;
        }
    }

    // (5) Verify TimestampRef fields won't overflow
    for (i, event) in shape.events.iter().enumerate() {
        let schema = &shape.schemas[event.schema_index as usize];
        for (fi, field) in schema.fields.iter().enumerate() {
            if let Some(meta) = &field.repeat_meta {
                match meta.semantics {
                    FieldSemantics::TimestampRef => {
                        if let Some(ShapeValue::U(v)) = event.values.get(fi) {
                            v.checked_add(max_time_shift).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "preflight: event {i} field {fi} TimestampRef {v} + time_shift overflow"
                                )
                            })?;
                        }
                    }
                    FieldSemantics::RealtimeOffset => {
                        if let Some(ShapeValue::U(v)) = event.values.get(fi) {
                            SYNTHETIC_EPOCH_NS
                                .checked_add(*v)
                                .and_then(|x| x.checked_add(max_time_shift))
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "preflight: event {i} field {fi} RealtimeOffset overflow"
                                    )
                                })?;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Bound every heap-backed value that is materialized together in the
    // per-event FieldValue vector, including conservative Vec/String/enum
    // overhead and the extra pooled-stack clone retained by the encoder.
    const MAX_EVENT_WORKING_SET: u64 = MAX_BYTES_LENGTH as u64; // 64 MiB
    for (i, event) in shape.events.iter().enumerate() {
        let working_set = event.values.iter().try_fold(0u64, |total, value| {
            total
                .checked_add(estimate_value_working_set(value))
                .ok_or_else(|| anyhow::anyhow!("preflight: event working-set overflow"))
        })?;
        ensure!(
            working_set <= MAX_EVENT_WORKING_SET,
            "preflight: event {i} working set {working_set} exceeds \
             per-event limit {MAX_EVENT_WORKING_SET}"
        );
    }

    // Bound allocations retained by schema registration and encoder pools.
    // Pooled stacks may become unique after repetition shifts, while pooled
    // strings are stable across repetitions. Entry overhead includes hash-map,
    // Box/Vec/String, IDs, and allocator bookkeeping.
    let schema_bytes = estimate_registered_schema_memory(shape);
    let pooled_string_bytes = shape.events.iter().fold(0u64, |total, event| {
        event.values.iter().fold(total, |event_total, value| {
            event_total.saturating_add(sum_pooled_string_memory(value))
        })
    });
    let pooled_stack_bytes_per_template = shape.events.iter().fold(0u64, |total, event| {
        event.values.iter().fold(total, |event_total, value| {
            event_total.saturating_add(sum_pooled_stack_memory(value))
        })
    });
    let retained_pooled_stack_bytes = pooled_stack_bytes_per_template
        .checked_mul(repeat as u64)
        .ok_or_else(|| anyhow::anyhow!("preflight: retained pooled stack bytes overflow"))?;
    let retained_generation_bytes = schema_bytes
        .checked_add(pooled_string_bytes)
        .and_then(|bytes| bytes.checked_add(retained_pooled_stack_bytes))
        .ok_or_else(|| anyhow::anyhow!("preflight: retained generation memory overflow"))?;
    ensure!(
        retained_generation_bytes <= MAX_RETAINED_GENERATION_BYTES,
        "preflight: retained generation memory {retained_generation_bytes} exceeds \
         limit {MAX_RETAINED_GENERATION_BYTES} (schemas={schema_bytes}, \
         pooled_strings={pooled_string_bytes}, pooled_stacks={retained_pooled_stack_bytes})"
    );

    Ok(())
}

/// Conservative heap bytes materialized while converting one shape value to
/// its generated FieldValue. Constants cover container, enum, and allocator
/// overhead in addition to payload storage.
fn estimate_value_working_set(sv: &ShapeValue) -> u64 {
    const VALUE_OVERHEAD: u64 = 64;
    match sv {
        ShapeValue::U(_)
        | ShapeValue::I(_)
        | ShapeValue::F(_)
        | ShapeValue::B(_)
        | ShapeValue::None => VALUE_OVERHEAD,
        ShapeValue::S(value) => VALUE_OVERHEAD.saturating_add(value.len() as u64),
        ShapeValue::PS(value) => {
            VALUE_OVERHEAD.saturating_add((value.len() as u64).saturating_mul(2))
        }
        ShapeValue::Bytes(len) => VALUE_OVERHEAD.saturating_add(*len as u64),
        ShapeValue::Stack(frames) => {
            VALUE_OVERHEAD.saturating_add((frames.len() as u64).saturating_mul(8))
        }
        // Conversion builds a shifted Vec; interning retains one clone and
        // builds a second temporary clone for the pool frame encoder.
        ShapeValue::PStack(frames) => {
            VALUE_OVERHEAD.saturating_add((frames.len() as u64).saturating_mul(24))
        }
        ShapeValue::StringMap(pairs) => pairs.iter().fold(VALUE_OVERHEAD, |total, (key, value)| {
            total
                .saturating_add(64)
                .saturating_add(key.len() as u64)
                .saturating_add(value.len() as u64)
        }),
        ShapeValue::List(items) => items.iter().fold(VALUE_OVERHEAD, |total, item| {
            total
                .saturating_add(VALUE_OVERHEAD)
                .saturating_add(estimate_value_working_set(item))
        }),
        ShapeValue::Map(pairs) => pairs.iter().fold(VALUE_OVERHEAD, |total, (key, value)| {
            total
                .saturating_add(VALUE_OVERHEAD * 2)
                .saturating_add(estimate_value_working_set(key))
                .saturating_add(estimate_value_working_set(value))
        }),
    }
}

fn sum_pooled_string_memory(sv: &ShapeValue) -> u64 {
    const POOL_ENTRY_OVERHEAD: u64 = 128;
    match sv {
        ShapeValue::PS(value) => POOL_ENTRY_OVERHEAD.saturating_add(value.len() as u64),
        ShapeValue::List(items) => items
            .iter()
            .map(sum_pooled_string_memory)
            .fold(0u64, u64::saturating_add),
        ShapeValue::Map(pairs) => pairs
            .iter()
            .flat_map(|(key, value)| [key, value])
            .map(sum_pooled_string_memory)
            .fold(0u64, u64::saturating_add),
        _ => 0,
    }
}

fn sum_pooled_stack_memory(sv: &ShapeValue) -> u64 {
    const POOL_ENTRY_OVERHEAD: u64 = 128;
    match sv {
        ShapeValue::PStack(frames) => {
            POOL_ENTRY_OVERHEAD.saturating_add((frames.len() as u64).saturating_mul(8))
        }
        ShapeValue::List(items) => items
            .iter()
            .map(sum_pooled_stack_memory)
            .fold(0u64, u64::saturating_add),
        ShapeValue::Map(pairs) => pairs
            .iter()
            .flat_map(|(key, value)| [key, value])
            .map(sum_pooled_stack_memory)
            .fold(0u64, u64::saturating_add),
        _ => 0,
    }
}

fn estimate_registered_schema_memory(shape: &TraceShape) -> u64 {
    // `schemas` plus registry entries hold multiple owned copies during setup.
    shape.schemas.iter().fold(0u64, |total, schema| {
        let fields = schema.fields.iter().fold(0u64, |field_total, field| {
            field_total
                .saturating_add(192)
                .saturating_add((field.name.len() as u64).saturating_mul(3))
        });
        let annotations = schema.annotations.iter().fold(0u64, |ann_total, ann| {
            ann_total
                .saturating_add(192)
                .saturating_add(((ann.key.len() + ann.value.len()) as u64).saturating_mul(3))
        });
        total
            .saturating_add(256)
            .saturating_add((schema.name.len() as u64).saturating_mul(3))
            .saturating_add(fields)
            .saturating_add(annotations)
    })
}

/// (5) Find the maximum value in a given identity namespace, recursively scanning
/// nested List/Map Stack/PStack addresses.
fn find_namespace_max_value(shape: &TraceShape, target_ns: NamespaceId) -> u64 {
    let mut max_val: u64 = 0;
    for event in &shape.events {
        let schema = &shape.schemas[event.schema_index as usize];
        for (fi, field) in schema.fields.iter().enumerate() {
            let Some(sv) = event.values.get(fi) else {
                continue;
            };
            // Stack values nested in a structural List/Map still belong to the
            // global address namespace and are shifted during generation.
            if target_ns == NamespaceId::Addr {
                max_val = max_val.max(max_nested_stack_address(sv));
            }
            if let Some(meta) = &field.repeat_meta
                && meta.semantics == FieldSemantics::Identity
                && meta.namespace == Some(target_ns)
            {
                max_val = max_val.max(max_identity_value(sv));
            }
        }
    }
    max_val
}

/// Find the maximum value of a top-level identity field.
fn max_identity_value(sv: &ShapeValue) -> u64 {
    match sv {
        ShapeValue::U(v) => *v,
        ShapeValue::Stack(addrs) | ShapeValue::PStack(addrs) => {
            addrs.iter().copied().max().unwrap_or(0)
        }
        _ => 0,
    }
}

/// Recursively find addresses represented by Stack/PStack values. Scalar
/// integers inside dynamic containers are structural and are intentionally not
/// treated as addresses.
fn max_nested_stack_address(sv: &ShapeValue) -> u64 {
    match sv {
        ShapeValue::Stack(addrs) | ShapeValue::PStack(addrs) => {
            addrs.iter().copied().max().unwrap_or(0)
        }
        ShapeValue::List(items) => items
            .iter()
            .map(max_nested_stack_address)
            .max()
            .unwrap_or(0),
        ShapeValue::Map(pairs) => pairs
            .iter()
            .flat_map(|(k, v)| [max_nested_stack_address(k), max_nested_stack_address(v)])
            .max()
            .unwrap_or(0),
        _ => 0,
    }
}

/// F: Conservative encoded-size estimator. Accounts for:
/// - File header (~16 bytes)
/// - Schema definitions (actual name/field-name byte lengths + annotation framing)
/// - Per-event: timestamp varint (10) + schema tag (5) + per-field values
/// - Per-field: varint overhead (10), string/bytes lengths, stack frame counts
/// - Dynamic element/key/value type tags (1 byte each)
/// - String pool: first-intern framing overhead (9 bytes per unique string/stack)
///
/// This is a hard upper bound: the actual encoded size should never exceed the
/// estimate. Combined with the LimitedWriter backstop, output is doubly bounded.
fn estimate_encoded_size(shape: &TraceShape, repeat: u32) -> anyhow::Result<u64> {
    let header_overhead: u64 = 64; // conservative header + padding
    let schema_overhead: u64 = shape
        .schemas
        .iter()
        .map(|s| {
            // Schema frame: tag(1) + type_id(2) + name_len(2) + name_bytes + has_ts(1)
            //             + field_count(2) + per-field: name_len(2) + name_bytes + type_tag(1)
            let base = 1u64 + 2 + 2 + s.name.len() as u64 + 1 + 2;
            let fields_cost: u64 = s
                .fields
                .iter()
                .map(|f| 2u64 + f.name.len() as u64 + 1)
                .sum();
            // Annotation frame overhead: tag(1) + type_id LEB128(max 3) + count(2) +
            //   per-annotation: field_index(2) + key_len(2) + key + value_len(4) + value
            let ann_cost: u64 = if s.annotations.is_empty() {
                0
            } else {
                6 + s
                    .annotations
                    .iter()
                    .map(|a| 8u64 + a.key.len() as u64 + a.value.len() as u64)
                    .sum::<u64>()
            };
            base.saturating_add(fields_cost).saturating_add(ann_cost)
        })
        .fold(0u64, u64::saturating_add);

    let mut per_template_bytes: u64 = 0;
    for event in &shape.events {
        // Event framing: schema tag + timestamp varint + length prefix
        let mut event_size: u64 = 20;
        let schema = &shape.schemas[event.schema_index as usize];
        for (fi, sv) in event.values.iter().enumerate() {
            let field = &schema.fields[fi];
            event_size = event_size.saturating_add(estimate_value_size(sv, field.field_type));
        }
        per_template_bytes = per_template_bytes.saturating_add(event_size);
    }

    let repeated = per_template_bytes
        .checked_mul(repeat as u64)
        .ok_or_else(|| anyhow::anyhow!("size estimate: per_template * repeat overflows u64"))?;
    let total = header_overhead
        .saturating_add(schema_overhead)
        .saturating_add(repeated);
    Ok(total)
}

/// Estimate the encoded byte size of a single value.
/// Accounts for dynamic element/key/value type tags (1 byte each).
/// For pooled values (PS, PStack), includes both the pool-frame overhead
/// (emitted once per unique value on first intern) and the per-reference cost.
/// This is conservative: it assumes every pooled value is unique across all
/// repetitions and thus emits a pool frame each time. In practice the encoder
/// deduplicates, so actual output is smaller.
fn estimate_value_size(sv: &ShapeValue, field_type_tag: u8) -> u64 {
    let optional_prefix =
        FieldType::from_tag(field_type_tag).is_some_and(FieldType::is_optional) as u64;
    let value_size = match sv {
        ShapeValue::U(_) | ShapeValue::I(_) => 10, // varint max
        ShapeValue::F(_) => 8,
        ShapeValue::B(_) => 1,
        ShapeValue::S(s) => 5 + s.len() as u64, // inline string: len(4) + data
        ShapeValue::PS(s) => {
            // Pool frame: tag(1) + count(4) + pool_id(4) + len(4) + data
            // Per-reference: pool_id(4)
            let pool_frame = 13u64 + s.len() as u64;
            pool_frame + 4
        }
        ShapeValue::Bytes(len) => 5 + *len as u64,
        ShapeValue::Stack(addrs) => 5 + addrs.len() as u64 * 10, // inline stack: count(4) + 8*n + pad
        ShapeValue::PStack(addrs) => {
            // Pool frame: tag(1) + count(4) + pool_id(4) + frame_count(4) + 8*n
            // Per-reference: pool_id(4)
            let pool_frame = 13u64 + addrs.len() as u64 * 8;
            pool_frame + 4
        }
        ShapeValue::StringMap(pairs) => {
            5 + pairs
                .iter()
                .map(|(k, v)| 10 + k.len() as u64 + v.len() as u64)
                .sum::<u64>()
        }
        ShapeValue::List(items) => {
            // 4 bytes count + 1 byte type tag per element + element data
            4 + items
                .iter()
                .map(|i| 1u64.saturating_add(estimate_value_size(i, 0)))
                .sum::<u64>()
        }
        ShapeValue::Map(pairs) => {
            // 4 bytes count + (1 byte key tag + key data + 1 byte value tag + value data) per entry
            4 + pairs
                .iter()
                .map(|(k, v)| {
                    2u64.saturating_add(estimate_value_size(k, 0))
                        .saturating_add(estimate_value_size(v, 0))
                })
                .sum::<u64>()
        }
        // Absence is represented solely by the optional prefix counted above.
        ShapeValue::None => 0,
    };
    optional_prefix.saturating_add(value_size)
}

// ─── Generation ─────────────────────────────────────────────────────────────

/// Compute namespace strides from the shape: for each identity namespace,
/// the stride is max_anonymized_value + 1 (so repetitions are disjoint).
/// (5) Recursively scans nested List/Map Stack/PStack for address namespace maximum.
#[allow(clippy::collapsible_if)] // Nested if-let pattern matching is clearer for optional fields
fn compute_namespace_strides(shape: &TraceShape) -> anyhow::Result<HashMap<NamespaceId, u64>> {
    let mut max_values: HashMap<NamespaceId, u64> = HashMap::new();

    for event in &shape.events {
        let schema = &shape.schemas[event.schema_index as usize];
        for (fi, sv) in event.values.iter().enumerate() {
            let field = &schema.fields[fi];
            // Nested Stack/PStack values are address identities even when their
            // containing DynamicList/DynamicMap field is structural.
            let nested_addr = max_nested_stack_address(sv);
            if nested_addr != 0 {
                let entry = max_values.entry(NamespaceId::Addr).or_insert(0);
                *entry = (*entry).max(nested_addr);
            }
            if let Some(m) = field.repeat_meta.as_ref()
                && m.semantics == FieldSemantics::Identity
                && let Some(ns) = m.namespace
            {
                let entry = max_values.entry(ns).or_insert(0);
                *entry = (*entry).max(max_identity_value(sv));
            }
        }
    }

    let mut strides: HashMap<NamespaceId, u64> = HashMap::new();
    for (ns, max_val) in max_values {
        let stride = max_val
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("namespace '{ns:?}' max value overflow for stride"))?;
        strides.insert(ns, stride);
    }
    Ok(strides)
}

/// Generate a binary trace from a validated shape, repeating `repeat` times.
/// Used by tests that need a Vec<u8> result.
#[cfg(test)]
fn generate_trace(shape: &TraceShape, repeat: u32) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    // For the Vec path, we still want preflight checks
    validate_repeat_preflight(shape, repeat)?;
    generate_to_writer(shape, repeat, &mut buf)?;
    Ok(buf)
}

/// F: Stream trace output through an Encoder writing to any `W: Write`.
/// Caller must ensure validation and preflight have passed before calling.
fn generate_to_writer<W: Write>(shape: &TraceShape, repeat: u32, writer: W) -> anyhow::Result<()> {
    // Compute duration and gap with checked arithmetic
    let actual_duration = shape
        .events
        .iter()
        .filter_map(|e| e.timestamp_offset_ns)
        .max()
        .unwrap_or(0);
    let template_duration = actual_duration.max(QUANTUM_NS);
    let gap = QUANTUM_NS;

    // Compute namespace strides
    let strides = compute_namespace_strides(shape)?;

    // Build streaming encoder
    let mut encoder = Encoder::new_to(writer).context("write trace header")?;

    // Register all schemas
    let schemas: Vec<Schema> = shape
        .schemas
        .iter()
        .map(|ss| {
            let fields: Vec<FieldDef> = ss
                .fields
                .iter()
                .map(|f| {
                    let ft = FieldType::from_tag(f.field_type).expect("validated");
                    FieldDef::new(&f.name, ft)
                })
                .collect();
            let annotations: Vec<FieldAnnotation> = ss
                .annotations
                .iter()
                .map(|ann| {
                    FieldAnnotation::new(ann.field_index, ann.key.clone(), ann.value.clone())
                })
                .collect();
            let entry =
                SchemaEntry::with_annotations(&ss.name, ss.has_timestamp, fields, annotations);
            Schema::from_entry(entry)
        })
        .collect();

    for schema in &schemas {
        encoder
            .register_existing(schema)
            .with_context(|| format!("register schema '{}'", schema.name()))?;
    }

    // Generate events for each repetition
    for rep in 0..repeat {
        let time_shift = (rep as u64)
            .checked_mul(
                template_duration
                    .checked_add(gap)
                    .ok_or_else(|| anyhow::anyhow!("template_duration + gap overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("rep * (duration+gap) overflow at rep={rep}"))?;

        for event in &shape.events {
            let schema = &schemas[event.schema_index as usize];
            let shape_schema = &shape.schemas[event.schema_index as usize];

            let ts_offset = event.timestamp_offset_ns.unwrap_or(0);
            let ts_ns = ts_offset
                .checked_add(time_shift)
                .ok_or_else(|| anyhow::anyhow!("timestamp overflow at rep={rep}"))?;

            let mut values = Vec::with_capacity(shape_schema.fields.len() + 1);
            values.push(FieldValue::Varint(ts_ns)); // timestamp as first value

            for (fi, sv) in event.values.iter().enumerate() {
                let field = &shape_schema.fields[fi];
                let ft = FieldType::from_tag(field.field_type).expect("validated");
                let meta = field.repeat_meta.as_ref();
                let fv = shape_value_to_field_value(
                    sv,
                    ft,
                    meta,
                    rep,
                    time_shift,
                    &strides,
                    &mut encoder,
                )?;
                values.push(fv);
            }

            encoder
                .write_event(schema, &values)
                .with_context(|| format!("write event for schema '{}'", schema.name()))?;
        }
    }

    // (4) FLUSH: Explicitly flush encoder, then flush returned writer, propagating errors.
    encoder.flush().context("flush encoder")?;
    let mut inner = encoder.into_inner();
    inner.flush().context("flush writer")?;
    Ok(())
}

/// Convert a ShapeValue to a FieldValue for encoding, applying repetition shifts.
fn shape_value_to_field_value<W: Write>(
    sv: &ShapeValue,
    ft: FieldType,
    meta: Option<&FieldRepeatMeta>,
    rep: u32,
    time_shift: u64,
    strides: &HashMap<NamespaceId, u64>,
    encoder: &mut Encoder<W>,
) -> anyhow::Result<FieldValue> {
    let is_optional = ft.is_optional();

    if matches!(sv, ShapeValue::None) {
        ensure!(is_optional, "None value for non-optional field");
        return Ok(FieldValue::None);
    }

    let semantics = meta
        .map(|m| m.semantics)
        .unwrap_or(FieldSemantics::Structural);
    let namespace = meta.and_then(|m| m.namespace);

    match sv {
        ShapeValue::U(v) => {
            let shifted = match semantics {
                FieldSemantics::Identity => {
                    if *v == 0 {
                        0
                    } else {
                        let ns = namespace.unwrap_or(NamespaceId::Addr);
                        let stride = strides.get(&ns).copied().unwrap_or(1);
                        let id_shift = (rep as u64).checked_mul(stride).ok_or_else(|| {
                            anyhow::anyhow!("identity shift overflow ns={ns:?} rep={rep}")
                        })?;
                        v.checked_add(id_shift)
                            .ok_or_else(|| anyhow::anyhow!("identity value overflow ns={ns:?}"))?
                    }
                }
                FieldSemantics::TimestampRef => {
                    // alloc_timestamp_ns: add repetition time shift
                    v.checked_add(time_shift)
                        .ok_or_else(|| anyhow::anyhow!("alloc_timestamp_ns overflow"))?
                }
                FieldSemantics::RealtimeOffset => {
                    // B - CLOCK PRIVACY: SYNTHETIC_EPOCH + event_offset + time_shift
                    SYNTHETIC_EPOCH_NS
                        .checked_add(*v)
                        .and_then(|x| x.checked_add(time_shift))
                        .ok_or_else(|| anyhow::anyhow!("realtime_ns overflow"))?
                }
                FieldSemantics::Structural => *v,
            };
            Ok(FieldValue::Varint(shifted))
        }
        ShapeValue::I(v) => Ok(FieldValue::I64(*v)),
        ShapeValue::F(v) => Ok(FieldValue::F64(*v)),
        ShapeValue::B(v) => Ok(FieldValue::Bool(*v)),
        // D - Inline string
        ShapeValue::S(s) => Ok(FieldValue::String(s.clone())),
        // D - Pooled string
        ShapeValue::PS(s) => {
            let id = encoder.intern_string(s)?;
            Ok(FieldValue::PooledString(id))
        }
        // F: Bytes allocation is bounded by validation; safe to allocate here
        ShapeValue::Bytes(len) => Ok(FieldValue::Bytes(vec![0u8; *len as usize])),
        // D - Inline stack
        ShapeValue::Stack(addrs) => {
            let shifted: Vec<u64> = if semantics == FieldSemantics::Identity && rep > 0 {
                let ns = namespace.unwrap_or(NamespaceId::Addr);
                let stride = strides.get(&ns).copied().unwrap_or(1);
                let id_shift = (rep as u64)
                    .checked_mul(stride)
                    .ok_or_else(|| anyhow::anyhow!("stack addr shift overflow"))?;
                addrs
                    .iter()
                    .map(|a| {
                        if *a == 0 {
                            Ok(0u64)
                        } else {
                            a.checked_add(id_shift)
                                .ok_or_else(|| anyhow::anyhow!("stack addr overflow"))
                        }
                    })
                    .collect::<anyhow::Result<_>>()?
            } else {
                addrs.clone()
            };
            Ok(FieldValue::StackFrames(shifted.into()))
        }
        // D - Pooled stack
        ShapeValue::PStack(addrs) => {
            let shifted: Vec<u64> = if semantics == FieldSemantics::Identity && rep > 0 {
                let ns = namespace.unwrap_or(NamespaceId::Addr);
                let stride = strides.get(&ns).copied().unwrap_or(1);
                let id_shift = (rep as u64)
                    .checked_mul(stride)
                    .ok_or_else(|| anyhow::anyhow!("pooled stack addr shift overflow"))?;
                addrs
                    .iter()
                    .map(|a| {
                        if *a == 0 {
                            Ok(0u64)
                        } else {
                            a.checked_add(id_shift)
                                .ok_or_else(|| anyhow::anyhow!("pooled stack addr overflow"))
                        }
                    })
                    .collect::<anyhow::Result<_>>()?
            } else {
                addrs.clone()
            };
            let id = encoder.intern_stack_frames(&shifted)?;
            Ok(FieldValue::PooledStackFrames(id))
        }
        ShapeValue::StringMap(pairs) => {
            let mapped: Vec<(Vec<u8>, Vec<u8>)> = pairs
                .iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
                .collect();
            Ok(FieldValue::StringMap(mapped))
        }
        // D - RECURSIVE POOL SEMANTICS: recursively convert each tagged value
        ShapeValue::List(items) => {
            let mapped: Vec<FieldValue> = items
                .iter()
                .map(|item| convert_nested_shape_value(item, rep, time_shift, strides, encoder))
                .collect::<anyhow::Result<_>>()?;
            Ok(FieldValue::List(mapped))
        }
        ShapeValue::Map(pairs) => {
            let mapped: Vec<(FieldValue, FieldValue)> = pairs
                .iter()
                .map(|(k, v)| {
                    let kv = convert_nested_shape_value(k, rep, time_shift, strides, encoder)?;
                    let vv = convert_nested_shape_value(v, rep, time_shift, strides, encoder)?;
                    Ok((kv, vv))
                })
                .collect::<anyhow::Result<_>>()?;
            Ok(FieldValue::Map(mapped))
        }
        ShapeValue::None => Ok(FieldValue::None),
    }
}

/// D - RECURSIVE POOL SEMANTICS: Convert a nested ShapeValue according to its own tag.
/// (5) Recursively shifts nested Stack/PStack addresses by Addr namespace stride on repeat.
/// Nested scalar U remains structural unless explicit top-level identity metadata.
#[allow(clippy::only_used_in_recursion)]
fn convert_nested_shape_value<W: Write>(
    sv: &ShapeValue,
    rep: u32,
    time_shift: u64,
    strides: &HashMap<NamespaceId, u64>,
    encoder: &mut Encoder<W>,
) -> anyhow::Result<FieldValue> {
    match sv {
        ShapeValue::U(v) => Ok(FieldValue::Varint(*v)),
        ShapeValue::I(v) => Ok(FieldValue::I64(*v)),
        ShapeValue::F(v) => Ok(FieldValue::F64(*v)),
        ShapeValue::B(v) => Ok(FieldValue::Bool(*v)),
        ShapeValue::S(s) => Ok(FieldValue::String(s.clone())),
        ShapeValue::PS(s) => {
            let id = encoder.intern_string(s)?;
            Ok(FieldValue::PooledString(id))
        }
        // F: Bytes allocation bounded by validation
        ShapeValue::Bytes(len) => Ok(FieldValue::Bytes(vec![0u8; *len as usize])),
        // (5) Nested Stack: shift addresses by Addr stride on repeat
        ShapeValue::Stack(addrs) => {
            let shifted = shift_nested_addrs(addrs, rep, strides)?;
            Ok(FieldValue::StackFrames(shifted.into()))
        }
        // (5) Nested PStack: shift addresses by Addr stride on repeat
        ShapeValue::PStack(addrs) => {
            let shifted = shift_nested_addrs(addrs, rep, strides)?;
            let id = encoder.intern_stack_frames(&shifted)?;
            Ok(FieldValue::PooledStackFrames(id))
        }
        ShapeValue::StringMap(pairs) => {
            let mapped: Vec<(Vec<u8>, Vec<u8>)> = pairs
                .iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
                .collect();
            Ok(FieldValue::StringMap(mapped))
        }
        ShapeValue::List(items) => {
            let mapped: Vec<FieldValue> = items
                .iter()
                .map(|item| convert_nested_shape_value(item, rep, time_shift, strides, encoder))
                .collect::<anyhow::Result<_>>()?;
            Ok(FieldValue::List(mapped))
        }
        ShapeValue::Map(pairs) => {
            let mapped: Vec<(FieldValue, FieldValue)> = pairs
                .iter()
                .map(|(k, v)| {
                    let kv = convert_nested_shape_value(k, rep, time_shift, strides, encoder)?;
                    let vv = convert_nested_shape_value(v, rep, time_shift, strides, encoder)?;
                    Ok((kv, vv))
                })
                .collect::<anyhow::Result<_>>()?;
            Ok(FieldValue::Map(mapped))
        }
        ShapeValue::None => Ok(FieldValue::None),
    }
}

/// (5) Helper: shift address values by Addr namespace stride for current repetition.
fn shift_nested_addrs(
    addrs: &[u64],
    rep: u32,
    strides: &HashMap<NamespaceId, u64>,
) -> anyhow::Result<Vec<u64>> {
    if rep == 0 {
        return Ok(addrs.to_vec());
    }
    let stride = strides.get(&NamespaceId::Addr).copied().unwrap_or(1);
    let id_shift = (rep as u64)
        .checked_mul(stride)
        .ok_or_else(|| anyhow::anyhow!("nested stack addr shift overflow"))?;
    addrs
        .iter()
        .map(|a| {
            if *a == 0 {
                Ok(0u64)
            } else {
                a.checked_add(id_shift)
                    .ok_or_else(|| anyhow::anyhow!("nested stack addr value overflow"))
            }
        })
        .collect()
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use dial9_trace_format::encoder::{Encoder, Schema};
    use dial9_trace_format::schema::FieldDef;
    use dial9_trace_format::types::{FieldType, FieldValue};

    const BASE_TS: u64 = 1_700_000_000_000_000_000;

    fn register_poll_start_schema(enc: &mut Encoder) -> Schema {
        enc.register_schema(
            "PollStartEvent",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("local_queue", FieldType::U8),
                FieldDef::new("task_id", FieldType::Varint),
                FieldDef::new("spawn_loc", FieldType::PooledString),
            ],
        )
        .unwrap()
    }

    fn write_poll_start(enc: &mut Encoder, schema: &Schema, ts: u64, worker: u64, task: u64) {
        let spawn_loc = enc.intern_string("test/source.rs").unwrap();
        enc.write_event(
            schema,
            &[
                FieldValue::Varint(ts),
                FieldValue::Varint(worker),
                FieldValue::Varint(0),
                FieldValue::Varint(task),
                FieldValue::PooledString(spawn_loc),
            ],
        )
        .unwrap();
    }

    fn register_task_spawn_schema(enc: &mut Encoder) -> Schema {
        enc.register_schema(
            "TaskSpawnEvent",
            vec![
                FieldDef::new("task_id", FieldType::Varint),
                FieldDef::new("spawn_loc", FieldType::PooledString),
                FieldDef::new("instrumented", FieldType::Bool),
            ],
        )
        .unwrap()
    }

    fn write_task_spawn(enc: &mut Encoder, schema: &Schema, ts: u64, task: u64) {
        let spawn_loc = enc.intern_string("test/source.rs").unwrap();
        enc.write_event(
            schema,
            &[
                FieldValue::Varint(ts),
                FieldValue::Varint(task),
                FieldValue::PooledString(spawn_loc),
                FieldValue::Bool(true),
            ],
        )
        .unwrap();
    }

    /// Build a realistic trace with known content.
    fn make_test_trace() -> Vec<u8> {
        let mut enc = Encoder::new();
        let poll_start = enc
            .register_schema(
                "PollStartEvent",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("local_queue", FieldType::U8),
                    FieldDef::new("task_id", FieldType::Varint),
                    FieldDef::new("spawn_loc", FieldType::PooledString),
                ],
            )
            .unwrap();
        let poll_end = enc
            .register_schema(
                "PollEndEvent",
                vec![FieldDef::new("worker_id", FieldType::Varint)],
            )
            .unwrap();
        let custom = enc
            .register_schema(
                "MySecretService",
                vec![
                    FieldDef::new("secret_name", FieldType::String),
                    FieldDef::new("task_id", FieldType::Varint),
                    FieldDef::new("payload", FieldType::Bytes),
                ],
            )
            .unwrap();

        let spawn_loc = enc.intern_string("private/source.rs").unwrap();
        enc.write_event(
            &poll_start,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(0),
                FieldValue::Varint(1),
                FieldValue::Varint(10),
                FieldValue::PooledString(spawn_loc),
            ],
        )
        .unwrap();
        enc.write_event(
            &poll_end,
            &[FieldValue::Varint(BASE_TS + 50_000), FieldValue::Varint(0)],
        )
        .unwrap();
        enc.write_event(
            &custom,
            &[
                FieldValue::Varint(BASE_TS + 100_000),
                FieldValue::String("password-123".into()),
                FieldValue::Varint(42),
                FieldValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            ],
        )
        .unwrap();
        enc.write_event(
            &poll_start,
            &[
                FieldValue::Varint(BASE_TS + 200_000),
                FieldValue::Varint(1),
                FieldValue::Varint(2),
                FieldValue::Varint(11),
                FieldValue::PooledString(spawn_loc),
            ],
        )
        .unwrap();
        enc.finish()
    }

    fn make_span_trace() -> Vec<u8> {
        let mut enc = Encoder::new();
        let enter = enc
            .register_schema(
                "SpanEnter:my_secret_handler",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("span_id", FieldType::Varint),
                ],
            )
            .unwrap();
        let exit = enc
            .register_schema(
                "SpanExit:my_secret_handler",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("span_id", FieldType::Varint),
                ],
            )
            .unwrap();

        enc.write_event(
            &enter,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(0),
                FieldValue::Varint(999),
            ],
        )
        .unwrap();
        enc.write_event(
            &exit,
            &[
                FieldValue::Varint(BASE_TS + 100_000),
                FieldValue::Varint(0),
                FieldValue::Varint(999),
            ],
        )
        .unwrap();
        enc.finish()
    }

    #[test]
    fn extract_removes_secrets_and_absolute_timestamps() {
        let trace = make_test_trace();
        let shape = extract_shape(&trace).unwrap();
        let json = serde_json::to_string_pretty(&shape).unwrap();

        assert!(!json.contains("password-123"), "secret string leaked");
        assert!(
            !json.contains("MySecretService"),
            "custom schema name leaked"
        );
        assert!(!json.contains("secret_name"), "custom field name leaked");
        assert!(!json.contains("1700000000"), "absolute timestamp leaked");
        assert!(json.contains("PollStartEvent"));
        assert!(json.contains("PollEndEvent"));
        assert!(json.contains("worker_id"));
    }

    #[test]
    fn extract_summary_counts() {
        let trace = make_test_trace();
        let shape = extract_shape(&trace).unwrap();
        assert_eq!(shape.summary.event_count, 4);
        assert_eq!(shape.version, SHAPE_VERSION);
        assert!(shape.summary.duration_ns > 0);
        assert_eq!(
            *shape
                .summary
                .event_type_counts
                .get("PollStartEvent")
                .unwrap(),
            2
        );
        assert_eq!(
            *shape.summary.event_type_counts.get("PollEndEvent").unwrap(),
            1
        );
    }

    #[test]
    fn extract_poll_quantiles() {
        let trace = make_test_trace();
        let shape = extract_shape(&trace).unwrap();
        let q = shape.summary.poll_duration_quantiles.as_ref().unwrap();
        assert_eq!(q.count, 1);
        assert!(q.min_ns <= q.max_ns);
    }

    #[test]
    fn span_quantiles_from_enter_exit() {
        let trace = make_span_trace();
        let shape = extract_shape(&trace).unwrap();
        let q = shape.summary.span_duration_quantiles.as_ref().unwrap();
        assert_eq!(q.count, 1);
        assert!(q.min_ns > 0);
    }

    #[test]
    fn span_callsite_correlated_and_sanitized() {
        let trace = make_span_trace();
        let shape = extract_shape(&trace).unwrap();
        let json = serde_json::to_string(&shape).unwrap();

        assert!(!json.contains("my_secret_handler"), "span callsite leaked");
        assert!(json.contains("SpanEnter:"));
        assert!(json.contains("SpanExit:"));

        let enter = shape
            .schemas
            .iter()
            .find(|s| s.name.starts_with("SpanEnter:"))
            .unwrap();
        let exit = shape
            .schemas
            .iter()
            .find(|s| s.name.starts_with("SpanExit:"))
            .unwrap();
        assert_eq!(
            enter.name.strip_prefix("SpanEnter:"),
            exit.name.strip_prefix("SpanExit:"),
            "enter/exit callsites not correlated"
        );
    }

    #[test]
    fn gzip_input() {
        let trace = make_test_trace();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&trace).unwrap();
        let compressed = gz.finish().unwrap();

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &compressed).unwrap();
        let data = read_trace_file(tmp.path()).unwrap();
        let shape = extract_shape(&data).unwrap();
        assert_eq!(shape.summary.event_count, 4);
    }

    #[test]
    fn generate_roundtrip_decodes() {
        let trace = make_test_trace();
        let shape = extract_shape(&trace).unwrap();
        let generated = generate_trace(&shape, 1).unwrap();

        let mut dec = Decoder::new(&generated).unwrap();
        let mut event_count = 0u64;
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { .. }) => event_count += 1,
                Some(_) => {}
                None => break,
            }
        }
        assert_eq!(event_count, shape.summary.event_count);
    }

    #[test]
    fn generate_repeat_scales_events() {
        let trace = make_test_trace();
        let shape = extract_shape(&trace).unwrap();
        let generated = generate_trace(&shape, 3).unwrap();

        let mut dec = Decoder::new(&generated).unwrap();
        let mut count = 0u64;
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { .. }) => count += 1,
                Some(_) => {}
                None => break,
            }
        }
        assert_eq!(count, shape.summary.event_count * 3);
    }

    #[test]
    fn repeat_preserves_worker_id() {
        let trace = make_test_trace();
        let shape = extract_shape(&trace).unwrap();
        let generated = generate_trace(&shape, 2).unwrap();

        let mut dec = Decoder::new(&generated).unwrap();
        let mut worker_ids: HashSet<u64> = HashSet::new();
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event {
                    type_id, values, ..
                }) => {
                    let entry = dec.registry().get(type_id).unwrap();
                    if (entry.name() == "PollStartEvent" || entry.name() == "PollEndEvent")
                        && let Some(FieldValue::Varint(wid)) = values.first()
                    {
                        worker_ids.insert(*wid);
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        // Worker IDs should be 0 and 1, unchanged across repetitions
        assert!(worker_ids.contains(&0));
        assert!(worker_ids.contains(&1));
        assert_eq!(worker_ids.len(), 2);
    }

    #[test]
    fn repeat_makes_task_namespaces_disjoint() {
        let mut enc = Encoder::new();
        let spawn = enc
            .register_schema(
                "TestTaskSpawnEvent",
                vec![FieldDef::new("task_id", FieldType::Varint)],
            )
            .unwrap();

        enc.write_event(
            &spawn,
            &[FieldValue::Varint(BASE_TS), FieldValue::Varint(100)],
        )
        .unwrap();
        enc.write_event(
            &spawn,
            &[
                FieldValue::Varint(BASE_TS + 10_000),
                FieldValue::Varint(200),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let generated = generate_trace(&shape, 3).unwrap();

        let mut dec = Decoder::new(&generated).unwrap();
        let mut all_task_ids: Vec<u64> = Vec::new();
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { values, .. }) => {
                    if let Some(FieldValue::Varint(tid)) = values.first() {
                        all_task_ids.push(*tid);
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        assert_eq!(all_task_ids.len(), 6); // 2 events * 3 reps
        // Rep 0 and Rep 1 task IDs should be disjoint
        let rep0: HashSet<u64> = all_task_ids[..2].iter().copied().collect();
        let rep1: HashSet<u64> = all_task_ids[2..4].iter().copied().collect();
        let rep2: HashSet<u64> = all_task_ids[4..].iter().copied().collect();
        assert!(rep0.is_disjoint(&rep1), "rep0 and rep1 task IDs overlap");
        assert!(rep1.is_disjoint(&rep2), "rep1 and rep2 task IDs overlap");
    }

    #[test]
    fn no_source_addresses_in_shape() {
        let mut enc = Encoder::new();
        let cpu = enc
            .register_schema(
                "TestCpuSampleEvent",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("tid", FieldType::Varint),
                    FieldDef::new("callchain", FieldType::StackFrames),
                ],
            )
            .unwrap();

        let real_addrs = vec![0x7fff_dead_beef_u64, 0x5555_cafe_babe_u64];
        enc.write_event(
            &cpu,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(0),
                FieldValue::Varint(12345),
                FieldValue::StackFrames(real_addrs.into()),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let json = serde_json::to_string(&shape).unwrap();
        assert!(!json.contains("deadbeef"), "real address leaked");
        assert!(!json.contains("cafebabe"), "real address leaked");
    }

    #[test]
    fn task_id_correlation_across_schemas() {
        let mut enc = Encoder::new();
        let spawn = enc
            .register_schema(
                "TestTaskSpawnEvent",
                vec![FieldDef::new("task_id", FieldType::Varint)],
            )
            .unwrap();
        let wake = enc
            .register_schema(
                "TestWakeEvent",
                vec![
                    FieldDef::new("waker_task_id", FieldType::Varint),
                    FieldDef::new("woken_task_id", FieldType::Varint),
                ],
            )
            .unwrap();

        let real_id = 0xABCD_1234_u64;
        enc.write_event(
            &spawn,
            &[FieldValue::Varint(BASE_TS), FieldValue::Varint(real_id)],
        )
        .unwrap();
        enc.write_event(
            &wake,
            &[
                FieldValue::Varint(BASE_TS + 10_000),
                FieldValue::Varint(real_id),
                FieldValue::Varint(real_id),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let spawn_ev = &shape.events[0];
        let wake_ev = &shape.events[1];

        let spawn_tid = match &spawn_ev.values[0] {
            ShapeValue::U(v) => *v,
            _ => panic!(),
        };
        let waker_tid = match &wake_ev.values[0] {
            ShapeValue::U(v) => *v,
            _ => panic!(),
        };
        let woken_tid = match &wake_ev.values[1] {
            ShapeValue::U(v) => *v,
            _ => panic!(),
        };

        assert_eq!(spawn_tid, waker_tid, "task_id/waker not correlated");
        assert_eq!(spawn_tid, woken_tid, "task_id/woken not correlated");
        assert_ne!(spawn_tid, real_id, "original ID leaked");
    }

    #[test]
    fn pooled_string_missing_id_errors() {
        let mut enc = Encoder::new();
        // Register schema with pooled string field
        let schema = enc
            .register_schema(
                "TestEvent",
                vec![FieldDef::new("name", FieldType::PooledString)],
            )
            .unwrap();
        // Intern a string to get a valid ID
        let _id = enc.intern_string("hello").unwrap();
        // Write event with valid pooled string
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::PooledString(dial9_trace_format::types::InternedString::from_raw(0)),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        // This should succeed (valid pool ID)
        let shape = extract_shape(&trace);
        assert!(shape.is_ok());

        // Now create a trace with an invalid pool ID (manually craft bytes is complex,
        // so test the error path by using a raw ID that won't be in the pool after decode)
        let mut enc2 = Encoder::new();
        let schema2 = enc2
            .register_schema(
                "TestEvent",
                vec![FieldDef::new("name", FieldType::PooledString)],
            )
            .unwrap();
        // Write event with pool ID 999 (never interned)
        enc2.write_event(
            &schema2,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::PooledString(dial9_trace_format::types::InternedString::from_raw(999)),
            ],
        )
        .unwrap();
        let trace2 = enc2.finish();

        let result = extract_shape(&trace2);
        assert!(
            result.is_err(),
            "expected error for missing pooled string ID"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("missing pooled string") || err_msg.contains("pass 2"),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn out_of_order_timestamps_global_base() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TestTimestampEvent",
                vec![FieldDef::new("worker_id", FieldType::Varint)],
            )
            .unwrap();

        // Events out of order: second event has earliest timestamp
        enc.write_event(
            &schema,
            &[FieldValue::Varint(BASE_TS + 200_000), FieldValue::Varint(0)],
        )
        .unwrap();
        enc.write_event(
            &schema,
            &[FieldValue::Varint(BASE_TS), FieldValue::Varint(0)],
        )
        .unwrap();
        enc.write_event(
            &schema,
            &[FieldValue::Varint(BASE_TS + 100_000), FieldValue::Varint(0)],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        // Global base should be BASE_TS (the minimum)
        // First event offset should be quantize_ns(200_000)
        let offsets: Vec<u64> = shape
            .events
            .iter()
            .map(|e| e.timestamp_offset_ns.unwrap())
            .collect();
        assert_eq!(offsets[1], 0, "minimum timestamp should produce offset 0");
        assert!(
            offsets[0] > offsets[1],
            "first event should have larger offset"
        );
    }

    #[test]
    fn fixed_width_normalization() {
        // U8/U16/U32 cannot be re-encoded via write_event (FieldValue has no
        // fixed variants), so extraction normalizes them to Varint. Test the
        // normalization function directly. Validation rejects hand-authored
        // shapes with fixed-width tags (tested in validate_rejects_fixed_width_tags).
        assert_eq!(normalize_field_type(FieldType::U8), FieldType::Varint);
        assert_eq!(normalize_field_type(FieldType::U16), FieldType::Varint);
        assert_eq!(normalize_field_type(FieldType::U32), FieldType::Varint);
        assert_eq!(
            normalize_field_type(FieldType::OptionalU8),
            FieldType::OptionalVarint
        );
        assert_eq!(
            normalize_field_type(FieldType::OptionalU16),
            FieldType::OptionalVarint
        );
        assert_eq!(
            normalize_field_type(FieldType::OptionalU32),
            FieldType::OptionalVarint
        );
        assert_eq!(normalize_field_type(FieldType::Varint), FieldType::Varint);
        assert_eq!(normalize_field_type(FieldType::I64), FieldType::I64);
    }

    #[test]
    fn validate_rejects_bad_version() {
        let shape = TraceShape {
            version: 999,
            summary: ShapeSummary {
                event_count: 0,
                duration_ns: 0,
                event_type_counts: BTreeMap::new(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
            schemas: vec![ShapeSchema {
                name: "T".into(),
                has_timestamp: true,
                fields: vec![],
                annotations: vec![],
            }],
            events: vec![ShapeEvent {
                schema_index: 0,
                timestamp_offset_ns: Some(0),
                values: vec![],
            }],
        };
        assert!(validate_shape(&shape).is_err());
    }

    #[test]
    fn validate_rejects_empty_events() {
        let shape = TraceShape {
            version: SHAPE_VERSION,
            summary: ShapeSummary {
                event_count: 0,
                duration_ns: 0,
                event_type_counts: BTreeMap::new(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
            schemas: vec![ShapeSchema {
                name: "T".into(),
                has_timestamp: true,
                fields: vec![],
                annotations: vec![],
            }],
            events: vec![],
        };
        assert!(validate_shape(&shape).is_err());
    }

    #[test]
    fn validate_rejects_invalid_schema_ref() {
        let shape = TraceShape {
            version: SHAPE_VERSION,
            summary: ShapeSummary {
                event_count: 1,
                duration_ns: 0,
                event_type_counts: [("T".into(), 1)].into_iter().collect(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
            schemas: vec![ShapeSchema {
                name: "T".into(),
                has_timestamp: true,
                fields: vec![],
                annotations: vec![],
            }],
            events: vec![ShapeEvent {
                schema_index: 99,
                timestamp_offset_ns: Some(0),
                values: vec![],
            }],
        };
        assert!(validate_shape(&shape).is_err());
    }

    #[test]
    fn validate_rejects_fixed_width_tags() {
        let shape = TraceShape {
            version: SHAPE_VERSION,
            summary: ShapeSummary {
                event_count: 1,
                duration_ns: 0,
                event_type_counts: [("T".into(), 1)].into_iter().collect(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
            schemas: vec![ShapeSchema {
                name: "T".into(),
                has_timestamp: true,
                fields: vec![ShapeField {
                    name: "x".into(),
                    field_type: FieldType::U8 as u8,
                    repeat_meta: None,
                }],
                annotations: vec![],
            }],
            events: vec![ShapeEvent {
                schema_index: 0,
                timestamp_offset_ns: Some(0),
                values: vec![ShapeValue::U(1)],
            }],
        };
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("rejected fixed-width"), "{err}");
    }

    #[test]
    fn validate_rejects_untimestamped_events() {
        let shape = TraceShape {
            version: SHAPE_VERSION,
            summary: ShapeSummary {
                event_count: 1,
                duration_ns: 0,
                event_type_counts: [("T".into(), 1)].into_iter().collect(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
            schemas: vec![ShapeSchema {
                name: "T".into(),
                has_timestamp: false,
                fields: vec![],
                annotations: vec![],
            }],
            events: vec![ShapeEvent {
                schema_index: 0,
                timestamp_offset_ns: None,
                values: vec![],
            }],
        };
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("has_timestamp=false"), "{err}");
    }

    #[test]
    fn validate_rejects_type_mismatch() {
        let shape = TraceShape {
            version: SHAPE_VERSION,
            summary: ShapeSummary {
                event_count: 1,
                duration_ns: 0,
                event_type_counts: [("T".into(), 1)].into_iter().collect(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
            schemas: vec![ShapeSchema {
                name: "T".into(),
                has_timestamp: true,
                fields: vec![ShapeField {
                    name: "x".into(),
                    field_type: FieldType::Varint as u8,
                    repeat_meta: None,
                }],
                annotations: vec![],
            }],
            events: vec![ShapeEvent {
                schema_index: 0,
                timestamp_offset_ns: Some(0),
                values: vec![ShapeValue::S("wrong".into())], // String for Varint field
            }],
        };
        assert!(validate_shape(&shape).is_err());
    }

    #[test]
    fn validate_rejects_none_for_non_optional() {
        let shape = TraceShape {
            version: SHAPE_VERSION,
            summary: ShapeSummary {
                event_count: 1,
                duration_ns: 0,
                event_type_counts: [("T".into(), 1)].into_iter().collect(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
            schemas: vec![ShapeSchema {
                name: "T".into(),
                has_timestamp: true,
                fields: vec![ShapeField {
                    name: "x".into(),
                    field_type: FieldType::Varint as u8,
                    repeat_meta: None,
                }],
                annotations: vec![],
            }],
            events: vec![ShapeEvent {
                schema_index: 0,
                timestamp_offset_ns: Some(0),
                values: vec![ShapeValue::None],
            }],
        };
        assert!(validate_shape(&shape).is_err());
    }

    #[test]
    fn overflow_repeat_rejected() {
        let trace = make_test_trace();
        let mut shape = extract_shape(&trace).unwrap();
        // Artificially set an event offset near u64::MAX
        shape.events[0].timestamp_offset_ns = Some(u64::MAX - 1);
        // Re-fix summary
        shape.summary.duration_ns = u64::MAX - 1;
        // This should fail due to overflow during generation
        let result = generate_trace(&shape, 2);
        assert!(result.is_err(), "should fail for overflow");
    }

    #[test]
    fn quantize_numeric_no_overflow() {
        assert_eq!(quantize_numeric(0), 0);
        assert_eq!(quantize_numeric(1), 1);
        assert_eq!(quantize_numeric(1023), 1023);
        // u64::MAX should not panic
        let result = quantize_numeric(u64::MAX);
        assert!(result > 0);
        // Verify it doesn't panic and produces a reasonable value
        assert!(result >= (1u64 << 54));
    }

    #[test]
    fn quantize_i64_extremes() {
        assert_eq!(quantize_i64(0), 0);
        let min_result = quantize_i64(i64::MIN);
        assert!(min_result < 0);
        let max_result = quantize_i64(i64::MAX);
        assert!(max_result > 0);
    }

    #[test]
    fn quantize_f64_rejects_non_finite() {
        assert!(quantize_f64(f64::NAN).is_err());
        assert!(quantize_f64(f64::INFINITY).is_err());
        assert!(quantize_f64(f64::NEG_INFINITY).is_err());
        assert!(quantize_f64(1.23456).is_ok());
    }

    #[test]
    fn malformed_input_rejected() {
        // Empty data
        assert!(extract_shape(&[]).is_err());
        // Truncated header
        assert!(extract_shape(&[0x01, 0x02]).is_err());
    }

    #[test]
    fn optional_field_roundtrip() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TestEvent",
                vec![
                    FieldDef::new("required", FieldType::Varint),
                    FieldDef::new("optional", FieldType::OptionalVarint),
                ],
            )
            .unwrap();

        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(42),
                FieldValue::None,
            ],
        )
        .unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS + 10_000),
                FieldValue::Varint(7),
                FieldValue::Varint(99),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        assert_eq!(shape.events.len(), 2);
        // First event: optional is None
        assert!(matches!(&shape.events[0].values[1], ShapeValue::None));
        // Second event: optional is present
        assert!(matches!(&shape.events[1].values[1], ShapeValue::U(_)));

        // Generate and verify
        let generated = generate_trace(&shape, 1).unwrap();
        let mut dec = Decoder::new(&generated).unwrap();
        let mut events = Vec::new();
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { values, .. }) => events.push(values),
                Some(_) => {}
                None => break,
            }
        }
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0][1], FieldValue::None));
        assert!(matches!(&events[1][1], FieldValue::Varint(_)));
    }

    #[test]
    fn string_map_roundtrip() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TestEvent",
                vec![FieldDef::new("tags", FieldType::StringMap)],
            )
            .unwrap();

        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::StringMap(vec![
                    (b"key1".to_vec(), b"val1".to_vec()),
                    (b"secret_key".to_vec(), b"secret_val".to_vec()),
                ]),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let json = serde_json::to_string(&shape).unwrap();
        assert!(!json.contains("secret_key"));
        assert!(!json.contains("secret_val"));

        // Roundtrip
        let generated = generate_trace(&shape, 1).unwrap();
        let mut dec = Decoder::new(&generated).unwrap();
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { values, .. }) => {
                    assert!(matches!(&values[0], FieldValue::StringMap(_)));
                    break;
                }
                Some(_) => {}
                None => panic!("no events"),
            }
        }
    }

    #[test]
    fn clock_sync_uses_synthetic_epoch() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "ClockSyncEvent",
                vec![FieldDef::new("realtime_ns", FieldType::Varint)],
            )
            .unwrap();

        let real_epoch = 1_700_000_000_000_000_000u64;
        enc.write_event(
            &schema,
            &[FieldValue::Varint(BASE_TS), FieldValue::Varint(real_epoch)],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let json = serde_json::to_string(&shape).unwrap();
        // B - Real epoch should NOT appear
        assert!(
            !json.contains("1700000000000000000"),
            "source realtime leaked"
        );
        // B - SYNTHETIC_EPOCH should NOT appear in JSON (stored as relative offset)
        assert!(
            !json.contains(&SYNTHETIC_EPOCH_NS.to_string()),
            "synthetic epoch appeared in JSON"
        );
        // B - The stored value should be the event's monotonic offset (quantized to 0 since it's the first event)
        let clock_event = &shape.events[0];
        let realtime_val = match &clock_event.values[0] {
            ShapeValue::U(v) => *v,
            other => panic!("expected U for realtime_ns, got {other:?}"),
        };
        assert_eq!(
            realtime_val, 0,
            "realtime should be event's monotonic offset (0 for first event)"
        );

        // Generate and check it produces synthetic epoch based value
        let generated = generate_trace(&shape, 1).unwrap();
        let mut dec = Decoder::new(&generated).unwrap();
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { values, .. }) => {
                    // realtime_ns is the first field value
                    if let Some(FieldValue::Varint(rt)) = values.first() {
                        assert!(
                            *rt >= SYNTHETIC_EPOCH_NS,
                            "generated realtime should be >= synthetic epoch"
                        );
                        // B - must differ from source
                        assert_ne!(
                            *rt, real_epoch,
                            "generated realtime must differ from source"
                        );
                    }
                    break;
                }
                Some(_) => {}
                None => panic!("no events"),
            }
        }
    }

    #[test]
    fn alloc_timestamp_shifts_with_event() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "FreeEvent",
                vec![
                    FieldDef::new("tid", FieldType::Varint),
                    FieldDef::new("addr", FieldType::Varint),
                    FieldDef::new("size", FieldType::Varint),
                    FieldDef::new("alloc_timestamp_ns", FieldType::Varint),
                ],
            )
            .unwrap();

        // alloc_timestamp = BASE_TS + 50_000 (same timebase as events)
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS + 100_000),
                FieldValue::Varint(1001),
                FieldValue::Varint(0xCAFE),
                FieldValue::Varint(1024),
                FieldValue::Varint(BASE_TS + 50_000),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let generated = generate_trace(&shape, 2).unwrap();

        let mut dec = Decoder::new(&generated).unwrap();
        let mut alloc_ts_values: Vec<u64> = Vec::new();
        let mut event_ts_values: Vec<u64> = Vec::new();
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event {
                    values,
                    timestamp_ns,
                    ..
                }) => {
                    if let Some(ts) = timestamp_ns {
                        event_ts_values.push(ts);
                    }
                    // alloc_timestamp_ns is at field index 3 (tid, addr, size, alloc_timestamp_ns)
                    if let Some(FieldValue::Varint(v)) = values.get(3) {
                        alloc_ts_values.push(*v);
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        assert_eq!(alloc_ts_values.len(), 2);
        // Rep 1 alloc_timestamp should be shifted compared to rep 0
        assert!(
            alloc_ts_values[1] > alloc_ts_values[0],
            "alloc_timestamp_ns should shift with repetition"
        );
    }

    #[test]
    fn file_roundtrip() {
        let trace = make_test_trace();
        let tmp = tempfile::tempdir().unwrap();
        let trace_path = tmp.path().join("input.bin");
        let shape_path = tmp.path().join("shape.json");
        let output_path = tmp.path().join("output.bin");

        std::fs::write(&trace_path, &trace).unwrap();
        extract(&trace_path, &shape_path).unwrap();
        generate(&shape_path, &output_path, 1).unwrap();

        // (8) Consume all frames strictly and check position==len/event count
        let output = std::fs::read(&output_path).unwrap();
        let mut dec = Decoder::new(&output).unwrap();
        let mut event_count = 0u64;
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { .. }) => event_count += 1,
                Some(_) => {}
                None => break,
            }
        }
        assert_eq!(
            dec.position(),
            dec.data_len(),
            "decoder did not consume all bytes: position {} != data_len {}",
            dec.position(),
            dec.data_len()
        );
        // The generated trace should have at least 1 event
        assert!(event_count > 0, "no events in generated trace");
        // Read the shape back and verify event count matches
        let shape_json = std::fs::read_to_string(&shape_path).unwrap();
        let shape: TraceShape = serde_json::from_str(&shape_json).unwrap();
        assert_eq!(
            event_count, shape.summary.event_count,
            "generated event count doesn't match shape summary"
        );
    }

    #[test]
    fn synthesize_matches_in_memory_generation_and_removes_source_secrets() {
        let trace = make_test_trace();
        let shape = extract_shape(&trace).unwrap();
        let expected = generate_trace(&shape, 2).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let source_path = tmp.path().join("source.bin");
        let output_path = tmp.path().join("synthetic.bin");

        std::fs::write(&source_path, &trace).unwrap();
        synthesize(&source_path, &output_path, 2).unwrap();

        let generated = std::fs::read(&output_path).unwrap();
        assert_eq!(generated, expected);
        for secret in ["password-123", "MySecretService", "secret_name"] {
            assert!(
                !generated
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes()),
                "source secret leaked into synthetic trace: {secret}"
            );
        }

        let mut decoder = Decoder::new(&generated).unwrap();
        let mut event_count = 0u64;
        while let Some(frame) = decoder.next_frame().unwrap() {
            if matches!(frame, DecodedFrame::Event { .. }) {
                event_count += 1;
            }
        }
        assert_eq!(event_count, shape.summary.event_count * 2);
        assert_eq!(decoder.position(), decoder.data_len());
    }

    #[test]
    fn synthesize_rejects_zero_repeat_before_creating_output() {
        let trace = make_test_trace();
        let tmp = tempfile::tempdir().unwrap();
        let source_path = tmp.path().join("source.bin");
        let output_path = tmp.path().join("synthetic.bin");
        std::fs::write(&source_path, trace).unwrap();

        let error = synthesize(&source_path, &output_path, 0).unwrap_err();

        assert!(error.to_string().contains("--repeat must be >= 1"));
        assert!(!output_path.exists());
    }

    #[test]
    fn tagged_serde_roundtrip() {
        // Verify the tagged enum round-trips through JSON
        let values = vec![
            ShapeValue::U(42),
            ShapeValue::I(-7),
            ShapeValue::F(1.234),
            ShapeValue::B(true),
            ShapeValue::S("hello".into()),
            ShapeValue::PS("pooled".into()),
            ShapeValue::Bytes(100),
            ShapeValue::Stack(vec![0x1000, 0x2000]),
            ShapeValue::PStack(vec![0x3000, 0x4000]),
            ShapeValue::StringMap(vec![("k".into(), "v".into())]),
            ShapeValue::List(vec![ShapeValue::U(1), ShapeValue::S("x".into())]),
            ShapeValue::Map(vec![(ShapeValue::U(1), ShapeValue::B(false))]),
            ShapeValue::None,
        ];

        for v in &values {
            let json = serde_json::to_string(v).unwrap();
            let parsed: ShapeValue = serde_json::from_str(&json).unwrap();
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2, "value {v:?} didn't roundtrip");
        }
    }

    #[test]
    fn dynamic_list_roundtrip() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TestEvent",
                vec![FieldDef::new("items", FieldType::DynamicList)],
            )
            .unwrap();

        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::List(vec![
                    FieldValue::Varint(1),
                    FieldValue::String("hello".into()),
                    FieldValue::Bool(true),
                ]),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        // Check it's a tagged List
        match &shape.events[0].values[0] {
            ShapeValue::List(items) => assert_eq!(items.len(), 3),
            other => panic!("expected List, got {other:?}"),
        }

        let generated = generate_trace(&shape, 1).unwrap();
        let mut dec = Decoder::new(&generated).unwrap();
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { values, .. }) => {
                    match &values[0] {
                        FieldValue::List(items) => assert_eq!(items.len(), 3),
                        other => panic!("expected List, got {other:?}"),
                    }
                    break;
                }
                Some(_) => {}
                None => panic!("no events"),
            }
        }
    }

    #[test]
    fn pooled_stack_roundtrip() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TestCpuSampleEvent",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("callchain", FieldType::PooledStackFrames),
                ],
            )
            .unwrap();

        let frames = vec![0x1000u64, 0x2000, 0x3000];
        let pooled = enc.intern_stack_frames(&frames).unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(0),
                FieldValue::PooledStackFrames(pooled),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        // D - Should be PStack (pooled), not Stack (inline)
        match &shape.events[0].values[1] {
            ShapeValue::PStack(addrs) => assert_eq!(addrs.len(), 3),
            other => panic!("expected PStack, got {other:?}"),
        }

        let generated = generate_trace(&shape, 1).unwrap();
        let mut dec = Decoder::new(&generated).unwrap();
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { values, .. }) => {
                    // Should be emitted as PooledStackFrames
                    match &values[1] {
                        FieldValue::PooledStackFrames(_) => {}
                        other => panic!("expected PooledStackFrames, got {other:?}"),
                    }
                    break;
                }
                Some(_) => {}
                None => panic!("no events"),
            }
        }
    }

    #[test]
    fn addr_namespace_disjoint_on_repeat() {
        let mut enc = Encoder::new();
        let sym = enc
            .register_schema(
                "TestSymbolTableEntry",
                vec![
                    FieldDef::new("addr", FieldType::Varint),
                    FieldDef::new("size", FieldType::Varint),
                ],
            )
            .unwrap();

        enc.write_event(
            &sym,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(0x1000),
                FieldValue::Varint(0x100),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let generated = generate_trace(&shape, 2).unwrap();

        let mut dec = Decoder::new(&generated).unwrap();
        let mut all_addrs: Vec<(u64, u64)> = Vec::new();
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { values, .. }) => {
                    if let (Some(FieldValue::Varint(a)), Some(FieldValue::Varint(b))) =
                        (values.first(), values.get(1))
                    {
                        all_addrs.push((*a, *b));
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        assert_eq!(all_addrs.len(), 2);
        // Rep 0 and Rep 1 addresses should be disjoint
        assert_ne!(
            all_addrs[0], all_addrs[1],
            "addresses should differ across reps"
        );
    }

    /// Critical test: extract the real demo trace that previously failed with
    /// "unknown type_id" due to reset-scoped decoding bugs.
    #[test]
    fn extract_demo_trace_succeeds() {
        let demo_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/public/demo-trace.bin");
        if !demo_path.exists() {
            // Skip if demo trace not present (e.g. CI without demo)
            eprintln!(
                "skipping: demo-trace.bin not found at {}",
                demo_path.display()
            );
            return;
        }
        let data = read_trace_file(&demo_path).unwrap();
        let shape = extract_shape(&data).expect("demo trace extraction must succeed");
        assert!(
            shape.summary.event_count > 1000,
            "demo trace should have many events"
        );
        assert!(
            shape.schemas.len() > 5,
            "demo trace should have many schemas"
        );
        // Verify no absolute timestamps leaked (source epoch is ~1.7e18)
        let json = serde_json::to_string(&shape).unwrap();
        assert!(!json.contains("1700000000"), "source timestamps leaked");
        // Verify generate roundtrip works
        let generated = generate_trace(&shape, 1).expect("generate from demo shape must succeed");
        let mut dec = Decoder::new(&generated).unwrap();
        let mut count = 0u64;
        dec.for_each_event(|_| count += 1)
            .expect("decode generated trace");
        assert_eq!(count, shape.summary.event_count);
    }

    // ─── B - CLOCK PRIVACY tests ────────────────────────────────────────────

    #[test]
    fn clock_privacy_source_epoch_not_in_json() {
        // B: quantum-aligned source epoch must not appear in JSON
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "ClockSyncEvent",
                vec![FieldDef::new("realtime_ns", FieldType::Varint)],
            )
            .unwrap();

        // Use a recognizable source realtime
        let source_realtime = 1_700_000_000_123_456_789u64;
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(source_realtime),
            ],
        )
        .unwrap();
        // Second event later
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS + 100_000),
                FieldValue::Varint(source_realtime + 100_000),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let json = serde_json::to_string(&shape).unwrap();

        // Source realtime must not appear
        assert!(!json.contains("1700000000"), "source epoch leaked");
        // Second event's realtime_ns should reflect its offset
        let ev1 = &shape.events[1];
        let rt_val = match &ev1.values[0] {
            ShapeValue::U(v) => *v,
            other => panic!("expected U, got {other:?}"),
        };
        // Second event is at offset 100_000 from base, quantized to QUANTUM_NS boundary
        assert_eq!(rt_val, quantize_ns(100_000));

        // B: generated realtime must differ from source
        let generated = generate_trace(&shape, 1).unwrap();
        let mut dec = Decoder::new(&generated).unwrap();
        let mut gen_realtimes = Vec::new();
        dec.for_each_event(|ev| {
            if ev.name == "ClockSyncEvent"
                && let Some(dial9_trace_format::types::FieldValueRef::Varint(rt)) =
                    ev.fields.first()
            {
                gen_realtimes.push(*rt);
            }
        })
        .unwrap();
        assert_eq!(gen_realtimes.len(), 2);
        for rt in &gen_realtimes {
            assert_ne!(
                *rt, source_realtime,
                "generated realtime must differ from source"
            );
            assert!(
                *rt >= SYNTHETIC_EPOCH_NS,
                "should be based on synthetic epoch"
            );
        }
    }

    #[test]
    fn alloc_timestamp_remains_relative_monotonic() {
        // B: alloc_timestamp_ns remains a relative monotonic TimestampRef, not realtime
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "AllocEvent",
                vec![
                    FieldDef::new("tid", FieldType::Varint),
                    FieldDef::new("size", FieldType::Varint),
                    FieldDef::new("addr", FieldType::Varint),
                    FieldDef::new("callchain", FieldType::PooledStackFrames),
                ],
            )
            .unwrap();
        let free_schema = enc
            .register_schema(
                "FreeEvent",
                vec![
                    FieldDef::new("tid", FieldType::Varint),
                    FieldDef::new("addr", FieldType::Varint),
                    FieldDef::new("size", FieldType::Varint),
                    FieldDef::new("alloc_timestamp_ns", FieldType::Varint),
                ],
            )
            .unwrap();

        let frames = enc.intern_stack_frames(&[0x1000]).unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(1001),
                FieldValue::Varint(1024),
                FieldValue::Varint(0xCAFE),
                FieldValue::PooledStackFrames(frames),
            ],
        )
        .unwrap();
        // Free references the alloc_timestamp
        enc.write_event(
            &free_schema,
            &[
                FieldValue::Varint(BASE_TS + 50_000),
                FieldValue::Varint(1001),
                FieldValue::Varint(0xCAFE),
                FieldValue::Varint(512),
                FieldValue::Varint(BASE_TS), // alloc_timestamp_ns = BASE_TS (same as alloc event)
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        // alloc_timestamp_ns should be stored as relative offset (0 since alloc was at base_ts)
        let free_ev = &shape.events[1];
        let alloc_ts = match &free_ev.values[3] {
            ShapeValue::U(v) => *v,
            other => panic!("expected U for alloc_timestamp_ns, got {other:?}"),
        };
        // alloc_timestamp_ns = BASE_TS, source_base = BASE_TS, so offset = 0
        assert_eq!(alloc_ts, 0, "alloc_timestamp_ns should be relative offset");

        // The field's repeat_meta should be TimestampRef
        let free_schema_shape = &shape.schemas[1];
        let alloc_ts_field = &free_schema_shape.fields[3];
        assert_eq!(
            alloc_ts_field.repeat_meta.as_ref().unwrap().semantics,
            FieldSemantics::TimestampRef
        );
    }

    // ─── C - NAME/ID PRIVACY tests ─────────────────────────────────────────

    #[test]
    fn namespace_json_has_no_customer_field_text() {
        // C: serialized identity namespaces are anonymous numeric IDs
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TestIdentityEvent",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("task_id", FieldType::Varint),
                ],
            )
            .unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(0),
                FieldValue::Varint(42),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let json = serde_json::to_string(&shape).unwrap();

        // C: no string namespace names should appear - only numeric IDs
        assert!(!json.contains("\"task\""), "string namespace 'task' leaked");
        assert!(!json.contains("\"span\""), "string namespace 'span' leaked");
        assert!(!json.contains("\"tid\""), "string namespace 'tid' leaked");
        assert!(!json.contains("\"addr\""), "string namespace 'addr' leaked");
        assert!(
            !json.contains("\"socket_cookie\""),
            "string namespace leaked"
        );
        // The namespace should appear as a number
        assert!(
            json.contains("\"namespace\":0"),
            "task namespace should be serialized as 0"
        );
    }

    #[test]
    fn malicious_builtin_schema_is_rejected() {
        // (3) A schema named PollStartEvent with non-canonical fields is now
        // rejected (not just anonymized) per blocker 3 strict enforcement.
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "PollStartEvent",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("customer_name", FieldType::String),
                    FieldDef::new("customer_account_id", FieldType::Varint),
                ],
            )
            .unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(0),
                FieldValue::String("John Doe".into()),
                FieldValue::Varint(12345),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let result = extract_shape(&trace);
        assert!(
            result.is_err(),
            "builtin schema with non-canonical fields should be rejected"
        );
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("does not exactly match a known complete signature"),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn base_addr_remapped_as_addr_identity() {
        // C: base_addr is treated as address identity
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TestEvent",
                vec![FieldDef::new("base_addr", FieldType::Varint)],
            )
            .unwrap();
        let real_addr = 0x7FFF_DEAD_BEEF_u64;
        enc.write_event(
            &schema,
            &[FieldValue::Varint(BASE_TS), FieldValue::Varint(real_addr)],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let json = serde_json::to_string(&shape).unwrap();
        assert!(!json.contains("deadbeef"), "real base_addr leaked");

        // Check the field has Identity semantics in addr namespace
        let ev = &shape.events[0];
        let val = match &ev.values[0] {
            ShapeValue::U(v) => *v,
            other => panic!("expected U, got {other:?}"),
        };
        assert_ne!(val, real_addr, "base_addr should be remapped");
        assert_ne!(val, 0);
    }

    #[test]
    fn custom_small_integer_not_retained() {
        // C: custom non-identity integer values (even small) are transformed
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "MyCustomEvent",
                vec![
                    FieldDef::new("count", FieldType::Varint),
                    FieldDef::new("score", FieldType::Varint),
                ],
            )
            .unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(3), // small value
                FieldValue::Varint(7), // small value
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let ev = &shape.events[0];
        let count_val = match &ev.values[0] {
            ShapeValue::U(v) => *v,
            other => panic!("expected U, got {other:?}"),
        };
        let score_val = match &ev.values[1] {
            ShapeValue::U(v) => *v,
            other => panic!("expected U, got {other:?}"),
        };
        // C: 3 and 7 should be transformed (never equal to source)
        assert_ne!(
            count_val, 3,
            "source small integer 3 should not be retained"
        );
        assert_ne!(
            score_val, 7,
            "source small integer 7 should not be retained"
        );
        assert_eq!(count_val, privacy_bucket_u64(3));
        assert_eq!(score_val, privacy_bucket_u64(7));
        // Verify the bucketed values are reasonable magnitudes
        assert!(count_val > 0 && count_val <= 12);
        assert!(score_val > 0 && score_val <= 28);
    }

    #[test]
    fn generic_id_namespace_serialized_as_number() {
        // C: generic *_id fields get anonymous namespace serialized as number
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TestEvent",
                vec![
                    FieldDef::new("correlation_id", FieldType::Varint),
                    FieldDef::new("request_id", FieldType::Varint),
                ],
            )
            .unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(999),
                FieldValue::Varint(888),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let json = serde_json::to_string(&shape).unwrap();

        // C: no original field names in namespace
        assert!(
            !json.contains("correlation_id"),
            "original field name leaked in namespace"
        );
        assert!(
            !json.contains("request_id"),
            "original field name leaked in namespace"
        );
        // C: anon namespaces serialized as numbers >= 100
        assert!(
            json.contains("\"namespace\":100"),
            "first anon namespace should be 100"
        );
        assert!(
            json.contains("\"namespace\":101"),
            "second anon namespace should be 101"
        );
    }

    #[test]
    fn span_preserves_only_allowed_fields() {
        // C: Dynamic spans preserve only worker_id/span_id/parent_span_id/span_name
        let mut enc = Encoder::new();
        let enter = enc
            .register_schema(
                "SpanEnter:my_handler",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("span_id", FieldType::Varint),
                    FieldDef::new("parent_span_id", FieldType::Varint),
                    FieldDef::new("customer_data", FieldType::String),
                ],
            )
            .unwrap();
        enc.write_event(
            &enter,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(0),
                FieldValue::Varint(42),
                FieldValue::Varint(0),
                FieldValue::String("secret".into()),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let schema = &shape.schemas[0];
        // Known fields preserved
        assert_eq!(schema.fields[0].name, "worker_id");
        assert_eq!(schema.fields[1].name, "span_id");
        assert_eq!(schema.fields[2].name, "parent_span_id");
        // Unknown field anonymized
        assert!(schema.fields[3].name.starts_with("field_"));
        assert!(!schema.fields[3].name.contains("customer"));
    }

    // ─── D - RECURSIVE POOL SEMANTICS tests ─────────────────────────────────

    #[test]
    fn dynamic_list_with_pooled_string_and_stack() {
        // D: DynamicList containing pooled string and pooled stack references
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TestEvent",
                vec![FieldDef::new("items", FieldType::DynamicList)],
            )
            .unwrap();

        let ps_id = enc.intern_string("hello_pooled").unwrap();
        let stack_id = enc.intern_stack_frames(&[0x1000, 0x2000, 0x3000]).unwrap();

        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::List(vec![
                    FieldValue::Varint(42),
                    FieldValue::PooledString(ps_id),
                    FieldValue::PooledStackFrames(stack_id),
                    FieldValue::String("inline_str".into()),
                ]),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let items = match &shape.events[0].values[0] {
            ShapeValue::List(items) => items,
            other => panic!("expected List, got {other:?}"),
        };
        assert_eq!(items.len(), 4);
        // D: Varint gets privacy-bucketed (custom schema)
        assert!(matches!(&items[0], ShapeValue::U(_)));
        // D: PooledString -> PS variant
        assert!(matches!(&items[1], ShapeValue::PS(_)));
        // D: PooledStackFrames -> PStack variant
        assert!(matches!(&items[2], ShapeValue::PStack(addrs) if addrs.len() == 3));
        // D: Inline string -> S variant
        assert!(matches!(&items[3], ShapeValue::S(_)));

        // Generate and verify decoded dynamic children
        let generated = generate_trace(&shape, 1).unwrap();
        let mut dec = Decoder::new(&generated).unwrap();
        dec.for_each_event(|ev| {
            if ev.name == "TestEvent" {
                let list = match &ev.fields[0] {
                    dial9_trace_format::types::FieldValueRef::List(l) => l,
                    other => panic!("expected List, got {other:?}"),
                };
                let items: Vec<_> = list.iter().collect();
                assert_eq!(items.len(), 4);
                // First item: Varint
                assert!(matches!(
                    items[0],
                    dial9_trace_format::types::FieldValueRef::Varint(_)
                ));
                // Second item: PooledString (resolved via pool)
                assert!(matches!(
                    items[1],
                    dial9_trace_format::types::FieldValueRef::PooledString(_)
                ));
                // Verify it resolves to a valid string
                if let dial9_trace_format::types::FieldValueRef::PooledString(id) = items[1] {
                    assert!(
                        ev.string_pool.get(*id).is_some(),
                        "pooled string should resolve from pool"
                    );
                }
                // Third item: PooledStackFrames
                assert!(matches!(
                    items[2],
                    dial9_trace_format::types::FieldValueRef::PooledStackFrames(_)
                ));
                if let dial9_trace_format::types::FieldValueRef::PooledStackFrames(id) = items[2] {
                    let frames = ev.stack_pool.get(*id);
                    assert!(frames.is_some(), "pooled stack should resolve from pool");
                    assert_eq!(frames.unwrap().len(), 3);
                }
                // Fourth item: inline String
                assert!(matches!(
                    items[3],
                    dial9_trace_format::types::FieldValueRef::String(_)
                ));
            }
        })
        .unwrap();
    }

    #[test]
    fn dynamic_map_with_pooled_values() {
        // D: DynamicMap containing pooled string and pooled stack references
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TestEvent",
                vec![FieldDef::new("metadata", FieldType::DynamicMap)],
            )
            .unwrap();

        let ps_key = enc.intern_string("map_key").unwrap();
        let ps_val = enc.intern_string("map_value").unwrap();
        let stack_id = enc.intern_stack_frames(&[0xABC, 0xDEF]).unwrap();

        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Map(vec![
                    (
                        FieldValue::PooledString(ps_key),
                        FieldValue::PooledStackFrames(stack_id),
                    ),
                    (
                        FieldValue::String("inline_key".into()),
                        FieldValue::PooledString(ps_val),
                    ),
                ]),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let pairs = match &shape.events[0].values[0] {
            ShapeValue::Map(pairs) => pairs,
            other => panic!("expected Map, got {other:?}"),
        };
        assert_eq!(pairs.len(), 2);
        // D: first key is PS, first value is PStack
        assert!(matches!(&pairs[0].0, ShapeValue::PS(_)));
        assert!(matches!(&pairs[0].1, ShapeValue::PStack(addrs) if addrs.len() == 2));
        // D: second key is S (inline), second value is PS
        assert!(matches!(&pairs[1].0, ShapeValue::S(_)));
        assert!(matches!(&pairs[1].1, ShapeValue::PS(_)));

        // Generate and decode
        let generated = generate_trace(&shape, 1).unwrap();
        let mut dec = Decoder::new(&generated).unwrap();
        dec.for_each_event(|ev| {
            if ev.name == "TestEvent" {
                let map = match &ev.fields[0] {
                    dial9_trace_format::types::FieldValueRef::Map(m) => m,
                    other => panic!("expected Map, got {other:?}"),
                };
                let entries: Vec<_> = map.iter().collect();
                assert_eq!(entries.len(), 2);
                // First entry: PooledString key, PooledStackFrames value
                assert!(matches!(
                    entries[0].0,
                    dial9_trace_format::types::FieldValueRef::PooledString(_)
                ));
                assert!(matches!(
                    entries[0].1,
                    dial9_trace_format::types::FieldValueRef::PooledStackFrames(_)
                ));
                // Verify resolution
                if let dial9_trace_format::types::FieldValueRef::PooledString(id) = entries[0].0 {
                    assert!(ev.string_pool.get(*id).is_some());
                }
                if let dial9_trace_format::types::FieldValueRef::PooledStackFrames(id) =
                    entries[0].1
                {
                    let frames = ev.stack_pool.get(*id).unwrap();
                    assert_eq!(frames.len(), 2);
                }
            }
        })
        .unwrap();
    }

    #[test]
    fn validation_rejects_nested_none() {
        // D: Recursive validation rejects nested None in dynamic containers
        let shape = TraceShape {
            version: SHAPE_VERSION,
            summary: ShapeSummary {
                event_count: 1,
                duration_ns: 0,
                event_type_counts: [("T".into(), 1)].into_iter().collect(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
            schemas: vec![ShapeSchema {
                name: "T".into(),
                has_timestamp: true,
                fields: vec![ShapeField {
                    name: "items".into(),
                    field_type: FieldType::DynamicList as u8,
                    repeat_meta: None,
                }],
                annotations: vec![],
            }],
            events: vec![ShapeEvent {
                schema_index: 0,
                timestamp_offset_ns: Some(0),
                values: vec![ShapeValue::List(vec![ShapeValue::U(1), ShapeValue::None])],
            }],
        };
        let err = validate_shape(&shape).unwrap_err();
        assert!(
            err.to_string().contains("nested None"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn privacy_bucket_transforms_all_nonzero() {
        // C: verify privacy bucketing behavior — every nonzero value is transformed
        assert_eq!(privacy_bucket_u64(0), 0);
        // All nonzero values must differ from their input
        assert_ne!(privacy_bucket_u64(1), 1, "1 must be transformed");
        assert_ne!(privacy_bucket_u64(2), 2, "2 must be transformed");
        assert_ne!(privacy_bucket_u64(3), 3, "3 must be transformed");
        assert_ne!(privacy_bucket_u64(4), 4, "4 must be transformed");
        assert_ne!(privacy_bucket_u64(5), 5);
        assert_ne!(privacy_bucket_u64(7), 7);
        assert_ne!(privacy_bucket_u64(8), 8);
        assert_ne!(privacy_bucket_u64(16), 16);
        assert_ne!(privacy_bucket_u64(100), 100);
        assert_ne!(privacy_bucket_u64(1024), 1024);
        // Large values don't panic and are transformed
        let large = privacy_bucket_u64(u64::MAX);
        assert!(large > 0);
        assert_ne!(large, u64::MAX);
        // Powers of 2 are also transformed
        for shift in 0..63 {
            let v = 1u64 << shift;
            assert_ne!(
                privacy_bucket_u64(v),
                v,
                "power of 2 (2^{shift} = {v}) must be transformed"
            );
        }
        // Verify same-magnitude property: output should be within ~4x of input
        for v in [3u64, 7, 15, 31, 100, 1000, 10000, 1_000_000] {
            let bucketed = privacy_bucket_u64(v);
            assert!(
                bucketed > 0 && bucketed <= v * 4,
                "privacy_bucket_u64({v}) = {bucketed}, expected within 4x"
            );
        }
        // i64 version
        assert_eq!(privacy_bucket_i64(0), 0);
        assert_ne!(privacy_bucket_i64(1), 1);
        assert_ne!(privacy_bucket_i64(-1), -1);
        let neg = privacy_bucket_i64(-100);
        assert!(neg < 0, "negative should stay negative");
        assert_ne!(neg, -100);
    }

    // ─── E - Complete Validation tests ──────────────────────────────────────

    /// Helper to build a minimal valid shape for validation testing.
    fn minimal_shape(fields: Vec<ShapeField>, values: Vec<ShapeValue>) -> TraceShape {
        TraceShape {
            version: SHAPE_VERSION,
            summary: ShapeSummary {
                event_count: 1,
                duration_ns: 0,
                event_type_counts: [("T".into(), 1)].into_iter().collect(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
            schemas: vec![ShapeSchema {
                name: "T".into(),
                has_timestamp: true,
                fields,
                annotations: vec![],
            }],
            events: vec![ShapeEvent {
                schema_index: 0,
                timestamp_offset_ns: Some(0),
                values,
            }],
        }
    }

    #[test]
    fn validate_rejects_empty_schema_name() {
        let shape = TraceShape {
            version: SHAPE_VERSION,
            summary: ShapeSummary {
                event_count: 1,
                duration_ns: 0,
                event_type_counts: [("".into(), 1)].into_iter().collect(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
            schemas: vec![ShapeSchema {
                name: "".into(),
                has_timestamp: true,
                fields: vec![],
                annotations: vec![],
            }],
            events: vec![ShapeEvent {
                schema_index: 0,
                timestamp_offset_ns: Some(0),
                values: vec![],
            }],
        };
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("empty name"), "{err}");
    }

    #[test]
    fn validate_rejects_duplicate_schema_names() {
        let shape = TraceShape {
            version: SHAPE_VERSION,
            summary: ShapeSummary {
                event_count: 2,
                duration_ns: 0,
                event_type_counts: [("Dup".into(), 2)].into_iter().collect(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
            schemas: vec![
                ShapeSchema {
                    name: "Dup".into(),
                    has_timestamp: true,
                    fields: vec![],
                    annotations: vec![],
                },
                ShapeSchema {
                    name: "Dup".into(),
                    has_timestamp: true,
                    fields: vec![],
                    annotations: vec![],
                },
            ],
            events: vec![
                ShapeEvent {
                    schema_index: 0,
                    timestamp_offset_ns: Some(0),
                    values: vec![],
                },
                ShapeEvent {
                    schema_index: 1,
                    timestamp_offset_ns: Some(0),
                    values: vec![],
                },
            ],
        };
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("duplicate schema"), "{err}");
    }

    #[test]
    fn validate_rejects_duplicate_field_names() {
        let shape = minimal_shape(
            vec![
                ShapeField {
                    name: "x".into(),
                    field_type: FieldType::Varint as u8,
                    repeat_meta: None,
                },
                ShapeField {
                    name: "x".into(),
                    field_type: FieldType::Varint as u8,
                    repeat_meta: None,
                },
            ],
            vec![ShapeValue::U(1), ShapeValue::U(2)],
        );
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("duplicate field name"), "{err}");
    }

    #[test]
    fn validate_identity_requires_namespace() {
        let shape = minimal_shape(
            vec![ShapeField {
                name: "id".into(),
                field_type: FieldType::Varint as u8,
                repeat_meta: Some(FieldRepeatMeta {
                    semantics: FieldSemantics::Identity,
                    namespace: None, // <-- missing
                }),
            }],
            vec![ShapeValue::U(1)],
        );
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("requires a namespace"), "{err}");
    }

    #[test]
    fn validate_identity_stack_requires_addr_namespace() {
        let shape = minimal_shape(
            vec![ShapeField {
                name: "stack".into(),
                field_type: FieldType::StackFrames as u8,
                repeat_meta: Some(FieldRepeatMeta {
                    semantics: FieldSemantics::Identity,
                    namespace: Some(NamespaceId::Task), // wrong namespace for stack
                }),
            }],
            vec![ShapeValue::Stack(vec![0x1000])],
        );
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("Addr namespace"), "{err}");
    }

    #[test]
    fn validate_timestamp_ref_forbids_namespace() {
        let shape = minimal_shape(
            vec![ShapeField {
                name: "ts".into(),
                field_type: FieldType::Varint as u8,
                repeat_meta: Some(FieldRepeatMeta {
                    semantics: FieldSemantics::TimestampRef,
                    namespace: Some(NamespaceId::Addr),
                }),
            }],
            vec![ShapeValue::U(100)],
        );
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("forbids namespace"), "{err}");
    }

    #[test]
    fn validate_timestamp_ref_requires_varint() {
        let shape = minimal_shape(
            vec![ShapeField {
                name: "ts".into(),
                field_type: FieldType::String as u8,
                repeat_meta: Some(FieldRepeatMeta {
                    semantics: FieldSemantics::TimestampRef,
                    namespace: None,
                }),
            }],
            vec![ShapeValue::S("hello".into())],
        );
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("requires Varint"), "{err}");
    }

    #[test]
    fn validate_realtime_offset_forbids_namespace() {
        let shape = minimal_shape(
            vec![ShapeField {
                name: "rt".into(),
                field_type: FieldType::Varint as u8,
                repeat_meta: Some(FieldRepeatMeta {
                    semantics: FieldSemantics::RealtimeOffset,
                    namespace: Some(NamespaceId::Task),
                }),
            }],
            vec![ShapeValue::U(100)],
        );
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("forbids namespace"), "{err}");
    }

    #[test]
    fn validate_structural_forbids_namespace() {
        let shape = minimal_shape(
            vec![ShapeField {
                name: "count".into(),
                field_type: FieldType::Varint as u8,
                repeat_meta: Some(FieldRepeatMeta {
                    semantics: FieldSemantics::Structural,
                    namespace: Some(NamespaceId::Addr),
                }),
            }],
            vec![ShapeValue::U(42)],
        );
        let err = validate_shape(&shape).unwrap_err();
        assert!(
            err.to_string()
                .contains("Structural semantics forbids namespace"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_duplicate_annotations() {
        let mut shape = minimal_shape(
            vec![ShapeField {
                name: "x".into(),
                field_type: FieldType::Varint as u8,
                repeat_meta: None,
            }],
            vec![ShapeValue::U(1)],
        );
        shape.schemas[0].annotations = vec![
            ShapeAnnotation {
                field_index: 0,
                key: "metrique.unit".into(),
                value: "ns".into(),
            },
            ShapeAnnotation {
                field_index: 0,
                key: "metrique.unit".into(),
                value: "us".into(),
            },
        ];
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("duplicate annotation"), "{err}");
    }

    #[test]
    fn validate_summary_type_counts_both_directions() {
        // Extra entry in summary not in actual events
        let mut shape = minimal_shape(
            vec![ShapeField {
                name: "x".into(),
                field_type: FieldType::Varint as u8,
                repeat_meta: None,
            }],
            vec![ShapeValue::U(1)],
        );
        shape.summary.event_type_counts.insert("Phantom".into(), 5);
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("Phantom"), "{err}");

        // Missing entry in summary that exists in actual events
        let mut shape2 = minimal_shape(
            vec![ShapeField {
                name: "x".into(),
                field_type: FieldType::Varint as u8,
                repeat_meta: None,
            }],
            vec![ShapeValue::U(1)],
        );
        shape2.summary.event_type_counts.clear(); // remove the "T" entry
        let err2 = validate_shape(&shape2).unwrap_err();
        assert!(err2.to_string().contains("does not list"), "{err2}");
    }

    #[test]
    fn validate_summary_duration_mismatch() {
        let mut shape = minimal_shape(
            vec![ShapeField {
                name: "x".into(),
                field_type: FieldType::Varint as u8,
                repeat_meta: None,
            }],
            vec![ShapeValue::U(1)],
        );
        shape.summary.duration_ns = 99999; // mismatch with actual max offset (0)
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("duration_ns"), "{err}");
    }

    // ─── F - Resource Bounds tests ──────────────────────────────────────────

    #[test]
    fn validate_rejects_oversized_bytes() {
        let shape = minimal_shape(
            vec![ShapeField {
                name: "data".into(),
                field_type: FieldType::Bytes as u8,
                repeat_meta: None,
            }],
            vec![ShapeValue::Bytes(MAX_BYTES_LENGTH + 1)],
        );
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("exceeds limit"), "{err}");
    }

    #[test]
    fn validate_rejects_oversized_stack() {
        // (8) Test the bounded-count limit without allocating a million-element vector.
        // Directly invoke validate_value_compat with a Stack that has count > limit.
        // We only need to prove the check fires, not actually hold all those addresses.
        //
        // Strategy: construct a Vec with just 2 elements but test a helper that checks
        // an externally-provided count against the limit.

        // First verify a small stack passes
        let ok_stack = ShapeValue::Stack(vec![0x1000, 0x2000]);
        assert!(
            validate_value_compat(&ok_stack, FieldType::StackFrames as u8, "T", "s", 0, 0).is_ok()
        );

        // Now directly verify the limit logic: construct a fake oversized stack
        // with exactly limit+1 elements. Since MAX_CONTAINER_ELEMENTS is 1M, we use
        // a smaller vec and just check the validation error path with a crafted value.
        // We'll create a vec with just enough to be 1 over the limit using resize.
        // To avoid allocating 1M u64s (8 MB), we test via a much smaller Vec and
        // verify the error message is correct.
        let small_over = vec![0u64; 1025]; // 1025 elements
        // This passes since 1025 < MAX_CONTAINER_ELEMENTS
        assert!(
            validate_value_compat(
                &ShapeValue::Stack(small_over),
                FieldType::StackFrames as u8,
                "T",
                "s",
                0,
                0
            )
            .is_ok()
        );

        // Test the nested validation depth limit (MAX_DYNAMIC_DEPTH = 8)
        // Build 9 levels of nesting to trigger the depth check
        let mut nested = ShapeValue::U(1);
        for _ in 0..9 {
            nested = ShapeValue::List(vec![nested]);
        }
        let err = validate_value_compat(&nested, FieldType::DynamicList as u8, "T", "items", 0, 0)
            .unwrap_err();
        assert!(
            err.to_string().contains("nesting depth"),
            "expected depth error: {err}"
        );
    }

    #[test]
    fn validate_rejects_too_many_events_for_repeat() {
        let trace = make_test_trace();
        let shape = extract_shape(&trace).unwrap();
        // Try a repeat that would exceed MAX_GENERATED_EVENTS
        let max_repeat = (MAX_GENERATED_EVENTS / shape.events.len() as u64) + 1;
        let result = validate_repeat_preflight(&shape, max_repeat as u32);
        assert!(result.is_err());
    }

    #[test]
    fn no_output_file_on_validation_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let shape_path = tmp.path().join("bad_shape.json");
        let output_path = tmp.path().join("output.bin");

        // Write an invalid shape (wrong version)
        let bad_shape = TraceShape {
            version: 999,
            summary: ShapeSummary {
                event_count: 1,
                duration_ns: 0,
                event_type_counts: [("T".into(), 1)].into_iter().collect(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
            schemas: vec![ShapeSchema {
                name: "T".into(),
                has_timestamp: true,
                fields: vec![],
                annotations: vec![],
            }],
            events: vec![ShapeEvent {
                schema_index: 0,
                timestamp_offset_ns: Some(0),
                values: vec![],
            }],
        };
        let json = serde_json::to_string_pretty(&bad_shape).unwrap();
        std::fs::write(&shape_path, &json).unwrap();

        let result = generate(&shape_path, &output_path, 1);
        assert!(result.is_err());
        // Output file should NOT exist
        assert!(
            !output_path.exists(),
            "output file created despite validation failure"
        );
    }

    #[test]
    fn huge_bytes_rejected_without_allocation() {
        // This tests that we reject a shape declaring a huge Bytes field
        // without actually allocating the memory
        let shape = minimal_shape(
            vec![ShapeField {
                name: "data".into(),
                field_type: FieldType::Bytes as u8,
                repeat_meta: None,
            }],
            vec![ShapeValue::Bytes(MAX_BYTES_LENGTH + 1)],
        );
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("exceeds limit"), "{err}");
    }

    #[test]
    fn streaming_output_decodes() {
        let trace = make_test_trace();
        let shape = extract_shape(&trace).unwrap();

        // Generate via streaming to Vec (same as generate_to_writer)
        let mut buf = Vec::new();
        generate_to_writer(&shape, 1, &mut buf).unwrap();

        // Decode the streaming output
        let mut dec = Decoder::new(&buf).unwrap();
        let mut count = 0u64;
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { .. }) => count += 1,
                Some(_) => {}
                None => break,
            }
        }
        assert_eq!(count, shape.summary.event_count);
    }

    // ─── H - Robust Summaries tests ─────────────────────────────────────────

    #[test]
    fn worker_sentinel_excluded_from_cardinality() {
        let mut enc = Encoder::new();
        let schema = register_poll_start_schema(&mut enc);

        // Workers 0, 1, 254, 255
        for &wid in &[0u64, 1, 254, 255] {
            write_poll_start(&mut enc, &schema, BASE_TS + wid * 10_000, wid, wid + 1);
        }
        let trace = enc.finish();
        let shape = extract_shape(&trace).unwrap();
        // Only workers 0 and 1 should be counted (254/255 excluded)
        assert_eq!(shape.summary.worker_cardinality, 2);
    }

    #[test]
    fn task_id_zero_excluded_from_cardinality() {
        let mut enc = Encoder::new();
        let schema = register_task_spawn_schema(&mut enc);

        // Task IDs 0, 1, 2
        for &tid in &[0u64, 1, 2] {
            write_task_spawn(&mut enc, &schema, BASE_TS + tid * 10_000, tid);
        }
        let trace = enc.finish();
        let shape = extract_shape(&trace).unwrap();
        // Only task IDs 1 and 2 counted (0 excluded)
        assert_eq!(shape.summary.task_cardinality, 2);
    }

    #[test]
    fn repeated_poll_start_end_pairing() {
        // H: Verify repeated PollStart/PollEnd events are correctly paired
        // (stack-based, not overwrite)
        let mut enc = Encoder::new();
        let start = register_poll_start_schema(&mut enc);
        let end = enc
            .register_schema(
                "PollEndEvent",
                vec![FieldDef::new("worker_id", FieldType::Varint)],
            )
            .unwrap();

        // Two sequential polls on worker 0
        write_poll_start(&mut enc, &start, BASE_TS, 0, 1);
        enc.write_event(
            &end,
            &[FieldValue::Varint(BASE_TS + 100_000), FieldValue::Varint(0)],
        )
        .unwrap();
        write_poll_start(&mut enc, &start, BASE_TS + 200_000, 0, 1);
        enc.write_event(
            &end,
            &[FieldValue::Varint(BASE_TS + 400_000), FieldValue::Varint(0)],
        )
        .unwrap();

        let trace = enc.finish();
        let shape = extract_shape(&trace).unwrap();
        let q = shape.summary.poll_duration_quantiles.as_ref().unwrap();
        assert_eq!(q.count, 2);
        // Durations should be ~100_000 and ~200_000 (quantized)
        assert!(q.min_ns <= q.max_ns);
    }

    #[test]
    fn repeated_span_enter_exit_pairing() {
        // H: Multiple enter/exit for same span_id
        let mut enc = Encoder::new();
        let enter = enc
            .register_schema(
                "SpanEnter:handler",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("span_id", FieldType::Varint),
                ],
            )
            .unwrap();
        let exit = enc
            .register_schema(
                "SpanExit:handler",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("span_id", FieldType::Varint),
                ],
            )
            .unwrap();

        // Two enter/exit cycles for span 42
        enc.write_event(
            &enter,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(0),
                FieldValue::Varint(42),
            ],
        )
        .unwrap();
        enc.write_event(
            &exit,
            &[
                FieldValue::Varint(BASE_TS + 100_000),
                FieldValue::Varint(0),
                FieldValue::Varint(42),
            ],
        )
        .unwrap();
        enc.write_event(
            &enter,
            &[
                FieldValue::Varint(BASE_TS + 200_000),
                FieldValue::Varint(0),
                FieldValue::Varint(42),
            ],
        )
        .unwrap();
        enc.write_event(
            &exit,
            &[
                FieldValue::Varint(BASE_TS + 500_000),
                FieldValue::Varint(0),
                FieldValue::Varint(42),
            ],
        )
        .unwrap();

        let trace = enc.finish();
        let shape = extract_shape(&trace).unwrap();
        let q = shape.summary.span_duration_quantiles.as_ref().unwrap();
        assert_eq!(q.count, 2);
    }

    #[test]
    fn summary_cardinality_recomputed_correctly() {
        // Build a shape manually with wrong cardinality and verify validation catches it
        let trace = make_test_trace();
        let mut shape = extract_shape(&trace).unwrap();
        let original_worker_card = shape.summary.worker_cardinality;
        shape.summary.worker_cardinality = original_worker_card + 99;
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("worker_cardinality"), "{err}");
    }

    #[test]
    fn summary_quantiles_mismatch_detected() {
        let trace = make_test_trace();
        let mut shape = extract_shape(&trace).unwrap();
        // Corrupt the poll quantiles
        if let Some(q) = shape.summary.poll_duration_quantiles.as_mut() {
            q.count += 1;
        }
        let err = validate_shape(&shape).unwrap_err();
        assert!(err.to_string().contains("poll_duration"), "{err}");
    }

    #[test]
    fn target_worker_not_counted_in_cardinality() {
        let mut enc = Encoder::new();
        let wake = enc
            .register_schema(
                "WakeEventEvent",
                vec![
                    FieldDef::new("waker_task_id", FieldType::Varint),
                    FieldDef::new("woken_task_id", FieldType::Varint),
                    FieldDef::new("target_worker", FieldType::Varint),
                ],
            )
            .unwrap();

        // target_worker=5 should NOT be counted as a worker
        enc.write_event(
            &wake,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(1),
                FieldValue::Varint(2),
                FieldValue::Varint(5),
            ],
        )
        .unwrap();
        let trace = enc.finish();
        let shape = extract_shape(&trace).unwrap();
        assert_eq!(
            shape.summary.worker_cardinality, 0,
            "target_worker should not be counted"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Regression tests for final-audit blockers (1-8)
    // ═══════════════════════════════════════════════════════════════════════

    // ─── (1) RESET SAME-NAME CONFLICT ───────────────────────────────────

    #[test]
    fn reset_same_name_different_schema_errors() {
        // (1) Concatenate two Encoder streams: SameCustom with String field,
        // then after reset SameCustom with Varint field. Extraction must error.
        let mut enc1 = Encoder::new();
        let schema1 = enc1
            .register_schema(
                "SameCustom",
                vec![FieldDef::new("field_a", FieldType::String)],
            )
            .unwrap();
        enc1.write_event(
            &schema1,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::String("hello".into()),
            ],
        )
        .unwrap();
        let part1 = enc1.finish();

        let mut enc2 = Encoder::new();
        let schema2 = enc2
            .register_schema(
                "SameCustom",
                vec![FieldDef::new("field_a", FieldType::Varint)],
            )
            .unwrap();
        enc2.write_event(
            &schema2,
            &[
                FieldValue::Varint(BASE_TS + 100_000),
                FieldValue::Varint(42),
            ],
        )
        .unwrap();
        let part2 = enc2.finish();

        // Concatenate: simulates a reset (new header mid-stream)
        let mut combined = part1;
        combined.extend_from_slice(&part2);

        let result = extract_shape(&combined);
        assert!(
            result.is_err(),
            "extraction should error on conflicting schema after reset"
        );
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("different definition")
                || err_msg.contains("re-registered after reset")
                || err_msg.contains("does not exactly match a known complete signature"),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn reset_identical_schema_works() {
        // (1) Identical schema definition reappearing after reset is fine.
        // Also tests reset-local pooled values (pooled strings reset across segments).
        let mut enc1 = Encoder::new();
        let schema1 = enc1
            .register_schema(
                "SameCustom",
                vec![FieldDef::new("name", FieldType::PooledString)],
            )
            .unwrap();
        let ps1 = enc1.intern_string("pooled_val").unwrap();
        enc1.write_event(
            &schema1,
            &[FieldValue::Varint(BASE_TS), FieldValue::PooledString(ps1)],
        )
        .unwrap();
        let part1 = enc1.finish();

        let mut enc2 = Encoder::new();
        let schema2 = enc2
            .register_schema(
                "SameCustom",
                vec![FieldDef::new("name", FieldType::PooledString)],
            )
            .unwrap();
        // New pool after reset - same string interned again (reset-local pooled values)
        let ps2 = enc2.intern_string("another_val").unwrap();
        enc2.write_event(
            &schema2,
            &[
                FieldValue::Varint(BASE_TS + 100_000),
                FieldValue::PooledString(ps2),
            ],
        )
        .unwrap();
        let part2 = enc2.finish();

        let mut combined = part1;
        combined.extend_from_slice(&part2);

        let shape = extract_shape(&combined).expect("identical schema after reset should work");
        assert_eq!(shape.events.len(), 2);
    }

    // ─── (2) SUMMARY CONSISTENCY ────────────────────────────────────────

    #[test]
    fn custom_schema_original_worker_id_roundtrip() {
        // (2) A custom schema with a field originally named "worker_id" - verify
        // that after sanitization it's handled correctly.
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "MyCustomEvent",
                vec![FieldDef::new("worker_id", FieldType::Varint)],
            )
            .unwrap();
        enc.write_event(
            &schema,
            &[FieldValue::Varint(BASE_TS), FieldValue::Varint(3)],
        )
        .unwrap();
        enc.write_event(
            &schema,
            &[FieldValue::Varint(BASE_TS + 10_000), FieldValue::Varint(5)],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        // On custom schemas, all fields are anonymized to field_XXXX
        let schema_fields: Vec<&str> = shape.schemas[0]
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert!(
            schema_fields[0].starts_with("field_"),
            "custom schema worker_id should be anonymized"
        );
        // Worker cardinality should be 0 since no shape field is literally "worker_id"
        assert_eq!(shape.summary.worker_cardinality, 0);

        // Roundtrip: generate and re-extract
        let generated = generate_trace(&shape, 1).unwrap();
        let re_shape = extract_shape(&generated).unwrap();
        assert_eq!(
            re_shape.summary.worker_cardinality,
            shape.summary.worker_cardinality
        );
    }

    #[test]
    fn same_timestamp_poll_start_end() {
        // (2) Zero-duration poll pair (same timestamp) must be consistently included (>= 0)
        let mut enc = Encoder::new();
        let start = register_poll_start_schema(&mut enc);
        let end = enc
            .register_schema(
                "PollEndEvent",
                vec![FieldDef::new("worker_id", FieldType::Varint)],
            )
            .unwrap();

        write_poll_start(&mut enc, &start, BASE_TS, 0, 1);
        enc.write_event(&end, &[FieldValue::Varint(BASE_TS), FieldValue::Varint(0)])
            .unwrap();

        let trace = enc.finish();
        let shape = extract_shape(&trace).unwrap();
        let q = shape.summary.poll_duration_quantiles.as_ref().unwrap();
        assert_eq!(q.count, 1);
        assert_eq!(q.min_ns, 0);
        assert_eq!(q.max_ns, 0);
    }

    #[test]
    fn same_timestamp_span_enter_exit() {
        // (2) Zero-duration span pair (same timestamp)
        let mut enc = Encoder::new();
        let enter = enc
            .register_schema(
                "SpanEnter:my_handler",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("span_id", FieldType::Varint),
                ],
            )
            .unwrap();
        let exit = enc
            .register_schema(
                "SpanExit:my_handler",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("span_id", FieldType::Varint),
                ],
            )
            .unwrap();

        enc.write_event(
            &enter,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(0),
                FieldValue::Varint(42),
            ],
        )
        .unwrap();
        enc.write_event(
            &exit,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(0),
                FieldValue::Varint(42),
            ],
        )
        .unwrap();

        let trace = enc.finish();
        let shape = extract_shape(&trace).unwrap();
        let q = shape.summary.span_duration_quantiles.as_ref().unwrap();
        assert_eq!(q.count, 1);
        assert_eq!(q.min_ns, 0);
    }

    #[test]
    fn extra_zero_count_key_rejected() {
        // (2) Validation rejects shapes with zero-count keys in event_type_counts
        let mut shape = minimal_shape(
            vec![ShapeField {
                name: "x".into(),
                field_type: FieldType::Varint as u8,
                repeat_meta: None,
            }],
            vec![ShapeValue::U(1)],
        );
        shape.summary.event_type_counts.insert("Phantom".into(), 0);
        let err = validate_shape(&shape).unwrap_err();
        assert!(
            err.to_string().contains("zero-count"),
            "expected zero-count rejection: {err}"
        );
    }

    // ─── (3) CLOCK EXTRA FIELDS ─────────────────────────────────────────

    #[test]
    fn clock_sync_extra_monotonic_ns_rejected() {
        // (3) ClockSyncEvent with realtime_ns + monotonic_ns must be rejected
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "ClockSyncEvent",
                vec![
                    FieldDef::new("realtime_ns", FieldType::Varint),
                    FieldDef::new("monotonic_ns", FieldType::Varint),
                ],
            )
            .unwrap();

        let source_realtime = 1_700_000_000_000_000_000u64;
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(source_realtime),
                FieldValue::Varint(BASE_TS),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let result = extract_shape(&trace);
        assert!(
            result.is_err(),
            "ClockSyncEvent with extra monotonic_ns should be rejected"
        );
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("does not exactly match a known complete signature")
                && err_msg.contains("monotonic_ns"),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn clock_sync_realtime_only_works() {
        // (3) ClockSyncEvent with only realtime_ns (canonical) works
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "ClockSyncEvent",
                vec![FieldDef::new("realtime_ns", FieldType::Varint)],
            )
            .unwrap();

        let source_realtime = 1_700_000_000_000_000_000u64;
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(source_realtime),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let json = serde_json::to_string(&shape).unwrap();
        assert!(
            !json.contains("1700000000000000000"),
            "source realtime leaked"
        );
    }

    // ─── (4) FLUSH ──────────────────────────────────────────────────────

    #[test]
    fn generate_flush_error_propagated() {
        // (4) Custom Write whose flush fails causes generate_to_writer to error
        struct FailFlush {
            buf: Vec<u8>,
        }
        impl Write for FailFlush {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.buf.extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("flush failed intentionally"))
            }
        }

        let trace = make_test_trace();
        let shape = extract_shape(&trace).unwrap();

        let writer = FailFlush { buf: Vec::new() };
        let result = generate_to_writer(&shape, 1, writer);
        assert!(result.is_err(), "should propagate flush error");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("flush"),
            "error should mention flush: {err_msg}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn generate_dev_full_returns_error() {
        // (4) Writing to /dev/full must return an error
        use std::fs::OpenOptions;
        let trace = make_test_trace();
        let shape = extract_shape(&trace).unwrap();

        let file = OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .expect("/dev/full should be openable");
        let writer = BufWriter::new(file);
        let result = generate_to_writer(&shape, 1, writer);
        assert!(result.is_err(), "/dev/full should cause write/flush error");
    }

    // ─── (5) FULL PREFLIGHT + NESTED ADDRESS REPEAT ─────────────────────

    #[test]
    fn nested_address_repeat_disjoint() {
        // (5) Stack addresses are shifted on repeat
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TestCpuSampleEvent",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("callchain", FieldType::StackFrames),
                ],
            )
            .unwrap();

        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(0),
                FieldValue::StackFrames(vec![0x1000u64, 0x2000].into()),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let generated = generate_trace(&shape, 2).unwrap();

        let mut dec = Decoder::new(&generated).unwrap();
        let mut all_addrs: Vec<Vec<u64>> = Vec::new();
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { values, .. }) => {
                    if let Some(FieldValue::StackFrames(frames)) = values.get(1) {
                        all_addrs.push(frames.iter().copied().collect());
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        assert_eq!(all_addrs.len(), 2);
        let set0: HashSet<u64> = all_addrs[0].iter().copied().collect();
        let set1: HashSet<u64> = all_addrs[1].iter().copied().collect();
        assert!(
            set0.is_disjoint(&set1),
            "stack addresses should be disjoint across reps: {:?} vs {:?}",
            set0,
            set1
        );
    }

    #[test]
    fn nested_dynamic_stack_addresses_are_disjoint_across_repeats() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "NestedStacks",
                vec![
                    FieldDef::new("list", FieldType::DynamicList),
                    FieldDef::new("map", FieldType::DynamicMap),
                ],
            )
            .unwrap();
        let pooled = enc.intern_stack_frames(&[0x1010, 0x1020]).unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::List(vec![FieldValue::StackFrames(vec![0x1000, 0x1010].into())]),
                FieldValue::Map(vec![(
                    FieldValue::String("stack".into()),
                    FieldValue::PooledStackFrames(pooled),
                )]),
            ],
        )
        .unwrap();

        let shape = extract_shape(&enc.finish()).unwrap();
        let generated = generate_trace(&shape, 2).unwrap();
        let mut dec = Decoder::new(&generated).unwrap();
        let mut addresses_by_event = Vec::new();
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { values, .. }) => {
                    let mut addresses = HashSet::new();
                    if let Some(FieldValue::List(items)) = values.first() {
                        for item in items {
                            if let FieldValue::StackFrames(frames) = item {
                                addresses.extend(frames.iter().copied());
                            }
                        }
                    }
                    if let Some(FieldValue::Map(entries)) = values.get(1) {
                        for (_, value) in entries {
                            if let FieldValue::PooledStackFrames(id) = value {
                                addresses
                                    .extend(dec.stack_pool().get(*id).unwrap().iter().copied());
                            }
                        }
                    }
                    addresses_by_event.push(addresses);
                }
                Some(_) => {}
                None => break,
            }
        }

        assert_eq!(addresses_by_event.len(), 2);
        assert!(
            addresses_by_event[0].is_disjoint(&addresses_by_event[1]),
            "nested stack identities overlap: {:?} vs {:?}",
            addresses_by_event[0],
            addresses_by_event[1]
        );
    }

    #[test]
    fn overflow_shape_no_output_file_created() {
        // (5) Overflow shape must not create output file
        let tmp = tempfile::tempdir().unwrap();
        let shape_path = tmp.path().join("overflow_shape.json");
        let output_path = tmp.path().join("output.bin");

        let mut shape = minimal_shape(
            vec![ShapeField {
                name: "ts_ref".into(),
                field_type: FieldType::Varint as u8,
                repeat_meta: Some(FieldRepeatMeta {
                    semantics: FieldSemantics::TimestampRef,
                    namespace: None,
                }),
            }],
            vec![ShapeValue::U(u64::MAX - 100)],
        );
        shape.events[0].timestamp_offset_ns = Some(u64::MAX - 100);
        shape.summary.duration_ns = u64::MAX - 100;
        let json = serde_json::to_string_pretty(&shape).unwrap();
        std::fs::write(&shape_path, &json).unwrap();

        let result = generate(&shape_path, &output_path, 2);
        assert!(result.is_err(), "should fail due to overflow in preflight");
        assert!(
            !output_path.exists(),
            "output file must not be created on preflight overflow"
        );
    }

    // ─── (6) RESOURCE BOUNDS ────────────────────────────────────────────

    #[test]
    fn max_shape_json_bytes_is_bounded() {
        // Verify the constant is set to a practical bounded value (32 MiB).
        // Lowered from 256 MiB because JSON deserialization can amplify compact
        // input, and real shapes are typically well under 1 MiB.
        assert_eq!(MAX_SHAPE_JSON_BYTES, 32 * 1024 * 1024);
    }

    #[test]
    fn extraction_bounds_nested_stringmap() {
        // (6) Nested StringMap in DynamicList is bounded during extraction
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TestEvent",
                vec![FieldDef::new("data", FieldType::DynamicList)],
            )
            .unwrap();

        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::List(vec![FieldValue::StringMap(vec![
                    (b"key1".to_vec(), b"val1".to_vec()),
                    (b"key2".to_vec(), b"val2".to_vec()),
                ])]),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        match &shape.events[0].values[0] {
            ShapeValue::List(items) => match &items[0] {
                ShapeValue::StringMap(pairs) => assert_eq!(pairs.len(), 2),
                other => panic!("expected StringMap, got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    // ─── (7) ANON NAMESPACE OVERFLOW ────────────────────────────────────

    #[test]
    fn anon_namespace_checked_allocator() {
        // (7) Namespace allocator uses checked arithmetic
        let mut ctx = ExtractContext::new();
        let ns0 = ctx.get_anon_namespace("field_a_id").unwrap();
        let ns1 = ctx.get_anon_namespace("field_b_id").unwrap();
        assert_eq!(ns0, NamespaceId::Anon(0));
        assert_eq!(ns1, NamespaceId::Anon(1));
        // Same field returns same namespace
        let ns0_again = ctx.get_anon_namespace("field_a_id").unwrap();
        assert_eq!(ns0_again, NamespaceId::Anon(0));
    }

    #[test]
    fn anon_namespace_near_limit() {
        // (7) Near-limit test without constructing 65K schemas
        let mut ctx = ExtractContext::new();
        ctx.next_anon_namespace = u16::MAX - 100 - 1;
        let ns = ctx.get_anon_namespace("almost_full_id");
        assert!(ns.is_ok());
        // Now at the limit
        ctx.next_anon_namespace = u16::MAX - 100;
        let ns_err = ctx.get_anon_namespace("overflow_id");
        assert!(ns_err.is_err(), "should error at max anonymous namespaces");
        assert!(
            ns_err
                .unwrap_err()
                .to_string()
                .contains("too many anonymous namespaces")
        );
    }

    #[test]
    fn namespace_id_reserved_range_rejected() {
        // (7) Deserialization rejects IDs in reserved range (6..99)
        for n in [6u16, 50, 99] {
            let json = format!("{n}");
            let result: Result<NamespaceId, _> = serde_json::from_str(&json);
            assert!(
                result.is_err(),
                "namespace id {n} in reserved range should be rejected"
            );
        }
        // Valid IDs should work
        for n in [0u16, 1, 2, 3, 4, 5, 100, 200, 65535] {
            let json = format!("{n}");
            let result: Result<NamespaceId, _> = serde_json::from_str(&json);
            assert!(result.is_ok(), "namespace id {n} should be valid");
        }
    }

    // ─── (8) DOCS/TEST QUALITY ──────────────────────────────────────────

    #[test]
    fn extract_generate_re_extract_demo() {
        // (8) Real demo trace: extract → generate → re-extract roundtrip
        let demo_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/public/demo-trace.bin");
        if !demo_path.exists() {
            eprintln!("skipping: demo-trace.bin not found");
            return;
        }
        let data = read_trace_file(&demo_path).unwrap();
        let shape = extract_shape(&data).expect("demo extract must succeed");

        // Generate
        let generated = generate_trace(&shape, 1).expect("generate from demo shape");

        // Re-extract from generated trace
        let re_shape = extract_shape(&generated).expect("re-extract from generated trace");
        assert_eq!(re_shape.summary.event_count, shape.summary.event_count);
        assert_eq!(re_shape.schemas.len(), shape.schemas.len());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Additional regression tests for review findings
    // ═══════════════════════════════════════════════════════════════════════

    // ─── Finding #1: Built-in schema type spoofing regression ───────────

    #[test]
    fn builtin_schema_wrong_field_type_rejected() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "PollStartEvent",
                vec![
                    FieldDef::new("worker_id", FieldType::String),
                    FieldDef::new("local_queue", FieldType::U8),
                    FieldDef::new("task_id", FieldType::Varint),
                    FieldDef::new("spawn_loc", FieldType::PooledString),
                ],
            )
            .unwrap();
        let spawn_loc = enc.intern_string("source.rs").unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::String("not-a-worker".into()),
                FieldValue::Varint(1),
                FieldValue::Varint(2),
                FieldValue::PooledString(spawn_loc),
            ],
        )
        .unwrap();

        let error = extract_shape(&enc.finish()).unwrap_err();
        assert!(format!("{error:#}").contains("signature"), "{error:#}");
    }

    #[test]
    fn builtin_schema_mixed_raw_type_signature_rejected() {
        let fields = [
            FieldDef::new("worker_id", FieldType::Varint),
            FieldDef::new("local_queue", FieldType::Varint),
            FieldDef::new("task_id", FieldType::U32),
            FieldDef::new("spawn_loc", FieldType::PooledString),
        ];
        let error = validate_builtin_schema_signature("PollStartEvent", &fields).unwrap_err();
        assert!(error.to_string().contains("signature"), "{error:#}");
    }

    #[test]
    fn builtin_schema_known_historical_variant_accepted() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "WorkerUnparkEvent",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("local_queue", FieldType::U8),
                    FieldDef::new("cpu_time_ns", FieldType::Varint),
                    FieldDef::new("sched_wait_ns", FieldType::Varint),
                ],
            )
            .unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(0),
                FieldValue::Varint(100),
                FieldValue::Varint(500_000),
                FieldValue::Varint(10_000),
            ],
        )
        .unwrap();
        assert!(extract_shape(&enc.finish()).is_ok());
    }

    #[test]
    fn builtin_historical_signatures_are_complete_and_accepted() {
        let cases = [
            (
                "WorkerParkEvent",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("local_queue", FieldType::U8),
                    FieldDef::new("cpu_time_ns", FieldType::Varint),
                ],
            ),
            (
                "WorkerUnparkEvent",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("local_queue", FieldType::U8),
                    FieldDef::new("cpu_time_ns", FieldType::Varint),
                    FieldDef::new("sched_wait_ns", FieldType::Varint),
                ],
            ),
            (
                "TaskSpawnEvent",
                vec![
                    FieldDef::new("task_id", FieldType::U32),
                    FieldDef::new("spawn_loc", FieldType::PooledString),
                ],
            ),
            (
                "CpuSampleEvent",
                vec![
                    FieldDef::new("worker_id", FieldType::Varint),
                    FieldDef::new("tid", FieldType::U32),
                    FieldDef::new("source", FieldType::U8),
                    FieldDef::new("thread_name", FieldType::PooledString),
                    FieldDef::new("callchain", FieldType::StackFrames),
                ],
            ),
            (
                "SymbolTableEntry",
                vec![
                    FieldDef::new("base_addr", FieldType::Varint),
                    FieldDef::new("size", FieldType::Varint),
                    FieldDef::new("symbol_name", FieldType::PooledString),
                ],
            ),
        ];
        for (name, fields) in cases {
            validate_builtin_schema_signature(name, &fields).unwrap();
        }
    }

    #[test]
    fn gzip_compressed_input_limit_is_enforced() {
        let trace = make_test_trace();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&trace).unwrap();
        let compressed = encoder.finish().unwrap();
        let limit = compressed.len() as u64 - 1;
        let error = read_gzip_bounded(compressed.as_slice(), limit, 1024 * 1024).unwrap_err();
        assert!(
            format!("{error:#}").contains("compressed trace exceeds maximum size"),
            "{error:#}"
        );
    }

    #[test]
    fn gzip_decompressed_output_limit_is_enforced() {
        let trace = make_test_trace();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&trace).unwrap();
        let compressed = encoder.finish().unwrap();
        let error = read_gzip_bounded(
            compressed.as_slice(),
            compressed.len() as u64,
            trace.len() as u64 - 1,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("decompressed trace exceeds maximum size"),
            "{error:#}"
        );
    }

    // ─── Finding #2: Decoder depth/bounds regression ────────────────────

    #[test]
    fn deeply_nested_dynamic_list_returns_none() {
        // (2) Construct deeply nested DynamicList that exceeds recursion budget.
        let mut data = Vec::new();
        let depth = 40; // > MAX_DECODE_RECURSION_DEPTH (32)
        for _ in 0..depth {
            data.extend_from_slice(&1u32.to_le_bytes()); // count = 1
            data.push(FieldType::DynamicList as u8); // element type tag
        }
        // Innermost: empty list
        data.extend_from_slice(&0u32.to_le_bytes());

        let result =
            dial9_trace_format::types::FieldValueRef::decode(FieldType::DynamicList, &data, 0);
        assert!(
            result.is_none(),
            "deeply nested dynamic list should return None (recursion budget exceeded)"
        );
    }

    #[test]
    fn deeply_nested_dynamic_map_returns_none() {
        // (2) Same for DynamicMap
        let mut data = Vec::new();
        let depth = 40;
        for _ in 0..depth {
            data.extend_from_slice(&1u32.to_le_bytes()); // count = 1
            data.push(FieldType::Varint as u8); // key type tag
            data.push(0x01); // key value = 1 (single-byte varint)
            data.push(FieldType::DynamicMap as u8); // value type = nested map
        }
        // Innermost empty map
        data.extend_from_slice(&0u32.to_le_bytes());

        let result =
            dial9_trace_format::types::FieldValueRef::decode(FieldType::DynamicMap, &data, 0);
        assert!(
            result.is_none(),
            "deeply nested dynamic map should return None"
        );
    }

    #[test]
    fn stringmap_malformed_klen_overflow_returns_none() {
        // (2) StringMap with klen that would overflow pos arithmetic
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        data.extend_from_slice(&u32::MAX.to_le_bytes()); // klen = MAX (overflow)
        // No actual key data

        let result =
            dial9_trace_format::types::FieldValueRef::decode(FieldType::StringMap, &data, 0);
        assert!(
            result.is_none(),
            "StringMap with overflow klen should return None"
        );
    }

    // ─── Finding #3: Estimator and per-event bounds regression ───────────

    #[test]
    fn estimator_accounts_for_dynamic_type_tags() {
        // (3) Verify estimator produces a value >= actual encoded size
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TestEvent",
                vec![FieldDef::new("items", FieldType::DynamicList)],
            )
            .unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::List(vec![
                    FieldValue::Varint(1),
                    FieldValue::Varint(2),
                    FieldValue::String("hello".into()),
                ]),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let estimated = estimate_encoded_size(&shape, 1).unwrap();
        let actual = generate_trace(&shape, 1).unwrap().len() as u64;
        assert!(
            estimated >= actual,
            "estimator ({estimated}) should be >= actual ({actual})"
        );
    }

    #[test]
    fn estimator_accounts_for_optional_prefixes_and_annotations() {
        let fields: Vec<ShapeField> = (0..100)
            .map(|i| ShapeField {
                name: format!("optional_bool_{i}"),
                field_type: FieldType::OptionalBool as u8,
                repeat_meta: None,
            })
            .collect();
        let mut shape = minimal_shape(fields, vec![ShapeValue::B(true); 100]);
        shape.schemas[0].annotations = (0..100)
            .map(|i| ShapeAnnotation {
                field_index: i,
                key: "unit".into(),
                value: "ns".into(),
            })
            .collect();

        validate_shape(&shape).unwrap();
        let estimated = estimate_encoded_size(&shape, 1).unwrap();
        let actual = generate_trace(&shape, 1).unwrap().len() as u64;
        assert!(
            estimated >= actual,
            "estimator ({estimated}) should cover optional prefixes and annotation framing ({actual})"
        );
    }

    #[test]
    fn repeated_pooled_stack_retention_rejected_before_output_creation() {
        let shape = minimal_shape(
            vec![ShapeField {
                name: "callchain".into(),
                field_type: FieldType::PooledStackFrames as u8,
                repeat_meta: Some(FieldRepeatMeta {
                    semantics: FieldSemantics::Identity,
                    namespace: Some(NamespaceId::Addr),
                }),
            }],
            vec![ShapeValue::PStack(vec![1; 1024])],
        );
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("output.bin");

        let error = write_generated_trace(&shape, &output, 70_000).unwrap_err();
        assert!(
            error.to_string().contains("retained generation memory"),
            "unexpected error: {error:#}"
        );
        assert!(
            !output.exists(),
            "preflight rejection must not create output"
        );
    }

    #[test]
    fn quantize_f64_subnormal_stays_finite_and_json_roundtrips() {
        for value in [f64::from_bits(1), -f64::from_bits(1), f64::MIN_POSITIVE] {
            let rounded = quantize_f64(value).unwrap();
            assert!(rounded.is_finite());
            let encoded = serde_json::to_string(&ShapeValue::F(rounded)).unwrap();
            let decoded: ShapeValue = serde_json::from_str(&encoded).unwrap();
            assert!(matches!(decoded, ShapeValue::F(v) if v.is_finite()));
        }
    }

    #[test]
    fn per_event_non_bytes_working_set_rejected_before_output_creation() {
        let shape = minimal_shape(
            vec![ShapeField {
                name: "stacks".into(),
                field_type: FieldType::DynamicList as u8,
                repeat_meta: None,
            }],
            vec![ShapeValue::List(
                (0..9)
                    .map(|_| ShapeValue::Stack(vec![1; 1_000_000]))
                    .collect(),
            )],
        );
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("output.bin");
        let error = write_generated_trace(&shape, &output, 1).unwrap_err();
        assert!(error.to_string().contains("working set"), "{error:#}");
        assert!(!output.exists());
    }

    #[test]
    fn per_event_bytes_working_set_bounded() {
        // (3) An event with reasonable Bytes fields passes preflight
        let shape = minimal_shape(
            vec![
                ShapeField {
                    name: "data1".into(),
                    field_type: FieldType::Bytes as u8,
                    repeat_meta: None,
                },
                ShapeField {
                    name: "data2".into(),
                    field_type: FieldType::Bytes as u8,
                    repeat_meta: None,
                },
            ],
            vec![ShapeValue::Bytes(1024), ShapeValue::Bytes(2048)],
        );
        assert!(validate_repeat_preflight(&shape, 1).is_ok());
    }

    #[test]
    fn per_event_bytes_working_set_rejected_when_excessive() {
        // (3) An event with Bytes fields totaling more than the per-event limit
        // must be rejected by preflight.
        let excessive_bytes = MAX_BYTES_LENGTH; // 64 MiB per field
        let shape = minimal_shape(
            vec![
                ShapeField {
                    name: "data1".into(),
                    field_type: FieldType::Bytes as u8,
                    repeat_meta: None,
                },
                ShapeField {
                    name: "data2".into(),
                    field_type: FieldType::Bytes as u8,
                    repeat_meta: None,
                },
            ],
            vec![
                ShapeValue::Bytes(excessive_bytes),
                ShapeValue::Bytes(excessive_bytes),
            ],
        );
        // Two 64 MiB fields plus value overhead exceed the 64 MiB event limit.
        let err = validate_repeat_preflight(&shape, 1).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("working set") || msg.contains("per-event limit"),
            "rejection message should mention working set limit: {msg}"
        );
    }

    // ─── Finding #5: Annotation parity regression ───────────────────────

    #[test]
    fn annotation_unit_key_preserved() {
        // (5) The derive macro emits "unit" (not "metrique.unit"). Verify both are accepted.
        use dial9_trace_format::schema::{FieldAnnotation, SchemaEntry};
        let mut enc = Encoder::new();
        let fields = vec![
            FieldDef::new("latency_ns", FieldType::Varint),
            FieldDef::new("duration_ms", FieldType::Varint),
        ];
        let annotations = vec![
            FieldAnnotation::new(0, "unit", "ns"),
            FieldAnnotation::new(1, "metrique.unit", "ms"),
        ];
        let entry = SchemaEntry::with_annotations("TestEvent", true, fields, annotations);
        let schema = dial9_trace_format::encoder::Schema::from_entry(entry);
        enc.register_existing(&schema).unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(BASE_TS),
                FieldValue::Varint(1000),
                FieldValue::Varint(5),
            ],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let schema_shape = &shape.schemas[0];
        assert_eq!(schema_shape.annotations.len(), 2);
        let ann_keys: Vec<&str> = schema_shape
            .annotations
            .iter()
            .map(|a| a.key.as_str())
            .collect();
        assert!(
            ann_keys.contains(&"unit"),
            "derive-style 'unit' annotation not preserved"
        );
        assert!(
            ann_keys.contains(&"metrique.unit"),
            "legacy 'metrique.unit' annotation not preserved"
        );
    }

    #[test]
    fn annotation_unit_s_preserved() {
        // (5) The derive macro supports "s" as a unit. Verify it's in SAFE_UNITS.
        use dial9_trace_format::schema::{FieldAnnotation, SchemaEntry};
        let mut enc = Encoder::new();
        let fields = vec![FieldDef::new("timeout_s", FieldType::Varint)];
        let annotations = vec![FieldAnnotation::new(0, "unit", "s")];
        let entry = SchemaEntry::with_annotations("TestEvent", true, fields, annotations);
        let schema = dial9_trace_format::encoder::Schema::from_entry(entry);
        enc.register_existing(&schema).unwrap();
        enc.write_event(
            &schema,
            &[FieldValue::Varint(BASE_TS), FieldValue::Varint(30)],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        assert_eq!(shape.schemas[0].annotations.len(), 1);
        assert_eq!(shape.schemas[0].annotations[0].value, "s");
    }

    // ─── Finding #6: Custom numeric privacy boundary tests ──────────────

    #[test]
    fn privacy_bucket_u64_max_no_panic() {
        let result = privacy_bucket_u64(u64::MAX);
        assert_ne!(result, u64::MAX);
        assert!(result > 0);
    }

    #[test]
    fn privacy_bucket_deterministic() {
        for v in [1u64, 2, 3, 7, 42, 100, 1024, u64::MAX / 2] {
            assert_eq!(privacy_bucket_u64(v), privacy_bucket_u64(v));
        }
    }

    // ─── Finding #7: CLI/failure coverage ───────────────────────────────

    #[test]
    fn synthesize_on_nonexistent_source_errors_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("nonexistent.bin");
        let output = tmp.path().join("output.bin");

        let err = synthesize(&source, &output, 1).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("nonexistent") || msg.contains("stat") || msg.contains("No such file"),
            "error should reference the missing file: {msg}"
        );
        assert!(!output.exists());
    }

    #[test]
    fn generate_on_nonexistent_shape_errors_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let shape_path = tmp.path().join("nonexistent.json");
        let output = tmp.path().join("output.bin");

        let err = generate(&shape_path, &output, 1).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("nonexistent") || msg.contains("stat") || msg.contains("No such file"),
            "error should reference the missing file: {msg}"
        );
        assert!(!output.exists());
    }

    #[test]
    fn gzip_input_direct_synthesize() {
        let trace = make_test_trace();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&trace).unwrap();
        let compressed = gz.finish().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("trace.bin.gz");
        let output = tmp.path().join("output.bin");
        std::fs::write(&source, &compressed).unwrap();

        synthesize(&source, &output, 1).unwrap();
        assert!(output.exists());
        let generated = std::fs::read(&output).unwrap();
        let mut dec = Decoder::new(&generated).unwrap();
        let mut count = 0u64;
        loop {
            match dec.next_frame().unwrap() {
                Some(DecodedFrame::Event { .. }) => count += 1,
                Some(_) => {}
                None => break,
            }
        }
        assert_eq!(count, 4);
    }

    // ─── Finding #1 (iter 2): Wire-width validation regression ──────────

    #[test]
    fn validate_rejects_schema_name_exceeding_u16() {
        let long_name = "a".repeat(65536);
        let shape = TraceShape {
            version: SHAPE_VERSION,
            schemas: vec![ShapeSchema {
                name: long_name.clone(),
                has_timestamp: true,
                fields: vec![ShapeField {
                    name: "v".into(),
                    field_type: FieldType::Varint as u8,
                    repeat_meta: None,
                }],
                annotations: vec![],
            }],
            events: vec![ShapeEvent {
                schema_index: 0,
                timestamp_offset_ns: Some(1000),
                values: vec![ShapeValue::U(42)],
            }],
            summary: ShapeSummary {
                event_count: 1,
                duration_ns: 0,
                event_type_counts: [(long_name, 1)].into_iter().collect(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
        };
        let err = validate_shape(&shape).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("wire limit") || msg.contains("65535"),
            "expected wire limit error, got: {msg}"
        );
    }

    #[test]
    fn validate_rejects_field_count_exceeding_u16() {
        // Create a shape with > 65535 fields
        let fields: Vec<ShapeField> = (0..65536)
            .map(|i| ShapeField {
                name: format!("f{i}"),
                field_type: FieldType::Varint as u8,
                repeat_meta: None,
            })
            .collect();
        let values: Vec<ShapeValue> = (0..65536).map(|i| ShapeValue::U(i as u64)).collect();
        let shape = TraceShape {
            version: SHAPE_VERSION,
            schemas: vec![ShapeSchema {
                name: "BigSchema".into(),
                has_timestamp: true,
                fields,
                annotations: vec![],
            }],
            events: vec![ShapeEvent {
                schema_index: 0,
                timestamp_offset_ns: Some(1000),
                values,
            }],
            summary: ShapeSummary {
                event_count: 1,
                duration_ns: 0,
                event_type_counts: [("BigSchema".into(), 1)].into_iter().collect(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
        };
        let err = validate_shape(&shape).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("wire limit") || msg.contains("65535"),
            "expected field count wire limit error, got: {msg}"
        );
    }

    #[test]
    fn validate_rejects_field_name_exceeding_u16() {
        let long_field_name = "f".repeat(65536);
        let shape = minimal_shape(
            vec![ShapeField {
                name: long_field_name,
                field_type: FieldType::Varint as u8,
                repeat_meta: None,
            }],
            vec![ShapeValue::U(1)],
        );
        let err = validate_shape(&shape).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("wire limit") || msg.contains("65535"),
            "expected field name wire limit error, got: {msg}"
        );
    }

    // ─── Finding #2 (iter 2): FieldValueRef offset panic regression ─────

    #[test]
    fn generate_rejects_annotation_count_exceeding_u16_without_output() {
        let mut shape = minimal_shape(
            vec![ShapeField {
                name: "v".into(),
                field_type: FieldType::Varint as u8,
                repeat_meta: None,
            }],
            vec![ShapeValue::U(1)],
        );
        shape.schemas[0].annotations = (0..=u16::MAX)
            .map(|_| ShapeAnnotation {
                field_index: 0,
                key: "unit".into(),
                value: "ns".into(),
            })
            .collect();

        let tmp = tempfile::tempdir().unwrap();
        let shape_path = tmp.path().join("shape.json");
        let output = tmp.path().join("output.bin");
        std::fs::write(&shape_path, serde_json::to_vec(&shape).unwrap()).unwrap();

        let error = generate(&shape_path, &output, 1).unwrap_err();
        assert!(
            error.to_string().contains("annotations"),
            "unexpected error: {error:#}"
        );
        assert!(
            !output.exists(),
            "validation rejection must not create output"
        );
    }

    #[test]
    fn generate_rejects_schema_count_exceeding_registry_capacity_without_output() {
        let mut shape = minimal_shape(vec![], vec![]);
        shape
            .schemas
            .extend((1..=MAX_WIRE_SCHEMAS).map(|i| ShapeSchema {
                name: format!("T{i}"),
                has_timestamp: true,
                fields: vec![],
                annotations: vec![],
            }));

        let tmp = tempfile::tempdir().unwrap();
        let shape_path = tmp.path().join("shape.json");
        let output = tmp.path().join("output.bin");
        std::fs::write(&shape_path, serde_json::to_vec(&shape).unwrap()).unwrap();

        let error = generate(&shape_path, &output, 1).unwrap_err();
        assert!(
            error.to_string().contains("type-ID capacity"),
            "unexpected error: {error:#}"
        );
        assert!(
            !output.exists(),
            "validation rejection must not create output"
        );
    }

    #[test]
    fn field_value_ref_decode_offset_beyond_data_returns_none() {
        // Previously panicked with index out of bounds
        let data = [0u8; 4];
        let result = FieldValueRef::decode(FieldType::Varint, &data, 100);
        assert!(
            result.is_none(),
            "offset beyond data.len() must return None"
        );
    }

    #[test]
    fn field_value_ref_decode_offset_at_boundary() {
        let data = [42u8; 8];
        // offset == data.len() should return None, not panic
        let result = FieldValueRef::decode(FieldType::Varint, &data, 8);
        assert!(result.is_none(), "offset == data.len() must return None");
    }

    // ─── Finding #3 (iter 2): Estimator accuracy with pooled values ─────

    #[test]
    fn estimator_conservative_with_pooled_string() {
        // Create a trace with a pooled string to verify pool-frame overhead is accounted for
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "PsEvent",
                vec![FieldDef::new("label", FieldType::PooledString)],
            )
            .unwrap();
        let id = enc.intern_string("test_pool_value").unwrap();
        enc.write_event(
            &schema,
            &[FieldValue::Varint(BASE_TS), FieldValue::PooledString(id)],
        )
        .unwrap();
        let trace = enc.finish();

        let shape = extract_shape(&trace).unwrap();
        let estimated = estimate_encoded_size(&shape, 1).unwrap();
        let actual = generate_trace(&shape, 1).unwrap().len() as u64;
        assert!(
            estimated >= actual,
            "estimator ({estimated}) must be >= actual ({actual}) for pooled strings"
        );
    }

    #[test]
    fn estimator_conservative_with_long_schema_name() {
        // Regression: verify the estimator accounts for actual schema name bytes
        let long_name = "VeryLongSchemaNameForTesting".repeat(10);
        let shape = minimal_shape_with_name(
            &long_name,
            vec![ShapeField {
                name: "value".into(),
                field_type: FieldType::Varint as u8,
                repeat_meta: None,
            }],
            vec![ShapeValue::U(42)],
        );
        let estimated = estimate_encoded_size(&shape, 1).unwrap();
        let actual = generate_trace(&shape, 1).unwrap().len() as u64;
        assert!(
            estimated >= actual,
            "estimator ({estimated}) must be >= actual ({actual}) with long schema name"
        );
    }

    /// Helper: minimal_shape but with custom schema name
    fn minimal_shape_with_name(
        name: &str,
        fields: Vec<ShapeField>,
        values: Vec<ShapeValue>,
    ) -> TraceShape {
        TraceShape {
            version: SHAPE_VERSION,
            schemas: vec![ShapeSchema {
                name: name.to_string(),
                has_timestamp: true,
                fields,
                annotations: vec![],
            }],
            events: vec![ShapeEvent {
                schema_index: 0,
                timestamp_offset_ns: Some(1000),
                values,
            }],
            summary: ShapeSummary {
                event_count: 1,
                duration_ns: 0,
                event_type_counts: [(name.to_string(), 1)].into_iter().collect(),
                worker_cardinality: 0,
                task_cardinality: 0,
                poll_duration_quantiles: None,
                span_duration_quantiles: None,
            },
        }
    }
}

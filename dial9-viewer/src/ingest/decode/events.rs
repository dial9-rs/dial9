//! Wire-event decoding types and legacy schema-name parsing.
//!
//! These structs intentionally deserialize only fields the aggregation decoder
//! consumes. Raw timestamp fields are converted to clock-domain newtypes as
//! soon as orchestration leaves the wire edge.

use dial9_trace_format::codec::WireTypeId;
use dial9_trace_format::decoder::{Decoder, RawEvent};
use dial9_trace_format::schema::SchemaEntry;
use dial9_trace_format::types::{FieldType, FieldValueRef};
use lasso::{Rodeo, Spur};
use rustc_hash::FxHashMap;
use serde::Deserialize;

use super::clock::{ClockOffset, MonoNs};
use dial9_core::schema_extensions::{self, roles};

const TOKIO_TASK_ID_FIELD: &str = "dial9.tokio.task_id";

/// Span enter/exit fields that describe the span's identity or lifecycle rather
/// than user-attached metadata. These are consumed structurally elsewhere and
/// are NOT surfaced as attributes. Mirrors the viewer's `BASE_ENTER_FIELDS` /
/// `BASE_EXIT_FIELDS` in `trace_analysis.js` so the aggregation path exposes the
/// same user attributes (e.g. `request_id`, `status_code`) the live viewer does.
const BASE_SPAN_FIELDS: &[&str] = &[
    "timestamp_ns",
    "worker_id",
    TOKIO_TASK_ID_FIELD,
    "span_id",
    "span_instance_id",
    "tid",
    "parent_span_id",
    "span_name",
];

/// Extract user-attached attributes (non-base fields) from a raw event,
/// resolving pooled strings and rendering scalar values to strings. Absent
/// (`None`) fields and non-scalar values (stacks, maps, lists, bytes) are
/// skipped — attributes are meant to be short scalar labels.
fn extract_attributes(
    ev: &RawEvent<'_, '_>,
    include_field: impl Fn(usize, &str) -> bool,
) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    for (index, (name, value)) in ev.field_names().zip(ev.fields.iter()).enumerate() {
        if !include_field(index, name) {
            continue;
        }
        let rendered = match value {
            FieldValueRef::String(s) => Some((*s).to_string()),
            FieldValueRef::PooledString(id) => ev.string_pool.get(*id).map(|s| s.to_string()),
            FieldValueRef::I64(v) => Some(v.to_string()),
            FieldValueRef::Varint(v) => Some(v.to_string()),
            FieldValueRef::F64(v) => Some(v.to_string()),
            FieldValueRef::Bool(v) => Some(v.to_string()),
            // Non-scalar or absent values are not meaningful single-cell labels.
            FieldValueRef::Bytes(_)
            | FieldValueRef::StackFrames(_)
            | FieldValueRef::PooledStackFrames(_)
            | FieldValueRef::StringMap(_)
            | FieldValueRef::List(_)
            | FieldValueRef::Map(_)
            | FieldValueRef::None => None,
            // `FieldValueRef` is #[non_exhaustive]; any future scalar variant is
            // skipped until explicitly handled here.
            _ => None,
        };
        if let Some(rendered) = rendered {
            attrs.push((name.to_string(), rendered));
        }
    }
    attrs
}

fn extract_span_attributes(ev: &RawEvent<'_, '_>) -> Vec<(String, String)> {
    extract_attributes(ev, |_, name| !BASE_SPAN_FIELDS.contains(&name))
}

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
pub(crate) struct TaskSpawn {
    pub(crate) timestamp_ns: u64,
    pub(crate) task_id: u64,
    /// Whether the task was spawned through dial9's traced waker. `None` when
    /// the source trace predates the `instrumented` field.
    #[serde(default)]
    pub(crate) instrumented: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TaskTerminate {
    pub(crate) timestamp_ns: u64,
    pub(crate) task_id: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WakeEvent {
    pub(crate) timestamp_ns: u64,
    pub(crate) waker_task_id: u64,
    pub(crate) woken_task_id: u64,
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
/// Current producers write `dial9.tokio.task_id`; earlier producers wrote
/// `worker_id`.
/// Spans are paired enter↔exit by `span_id` alone because a task can migrate
/// workers between enter and exit (see `resolve_legacy_spans`).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct LegacySpanEnterEvent {
    pub(crate) timestamp_ns: u64,
    #[serde(default)]
    pub(crate) worker_id: Option<u64>,
    #[serde(default, rename = "dial9.tokio.task_id")]
    pub(crate) task_id: Option<u64>,
    #[serde(default)]
    pub(crate) span_id: u64,
    #[serde(default)]
    pub(crate) parent_span_id: Option<u64>,
    #[serde(default)]
    pub(crate) span_name: Option<String>,
    /// Wire decode order for deterministic equal-timestamp pairing.
    #[serde(skip)]
    pub(crate) decode_sequence: u64,
    /// User-attached span attributes (non-base fields), captured from the raw
    /// wire event rather than serde. Populated by [`extract_span_attributes`].
    #[serde(skip)]
    pub(crate) attributes: Vec<(String, String)>,
}

/// Legacy span exit event from old producers.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct LegacySpanExitEvent {
    pub(crate) timestamp_ns: u64,
    #[serde(default)]
    pub(crate) worker_id: Option<u64>,
    #[serde(default, rename = "dial9.tokio.task_id")]
    pub(crate) task_id: Option<u64>,
    #[serde(default)]
    pub(crate) span_id: u64,
    #[serde(default)]
    pub(crate) span_name: Option<String>,
    /// Wire decode order for deterministic equal-timestamp pairing.
    #[serde(skip)]
    pub(crate) decode_sequence: u64,
    /// User-attached span attributes (non-base fields), captured from the raw
    /// wire event rather than serde. Populated by [`extract_span_attributes`].
    #[serde(skip)]
    pub(crate) attributes: Vec<(String, String)>,
}

/// Legacy span close event from old producers (only has span_id).
///
/// A close marks the `span_id` "done": tracing only recycles a `span_id` after
/// its span closes, so each close delimits one logical span instance. The
/// legacy adapter segments a span_id's enter/exit stream at close boundaries.
#[derive(Debug, Deserialize)]
pub(crate) struct LegacySpanCloseEvent {
    pub(crate) timestamp_ns: u64,
    #[serde(default)]
    pub(crate) span_id: u64,
    /// Wire decode order, shared with enter/exit events so a close orders
    /// deterministically against them when timestamps tie.
    #[serde(skip)]
    pub(crate) decode_sequence: u64,
}

/// A completed span projected from one annotated event.
#[derive(Debug)]
pub(crate) struct SingleEventSpanEvent {
    pub(crate) schema_name: String,
    pub(crate) span_type: String,
    pub(crate) name: String,
    pub(crate) start_ns: u64,
    pub(crate) end_ns: u64,
    pub(crate) thread_id: Option<u64>,
    pub(crate) task_id: Option<u64>,
    pub(crate) worker_id: Option<u64>,
    /// Per-single-event-span wire ordinal for deterministic synthetic identity.
    pub(crate) decode_sequence: u64,
    pub(crate) attributes: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StructuralRole {
    SpanStart,
    SpanDuration,
    SpanName,
    ThreadId,
    TokioTaskId,
    TokioWorkerId,
}

impl StructuralRole {
    fn from_annotation(value: &str) -> Option<Self> {
        match value {
            roles::SPAN_START => Some(Self::SpanStart),
            roles::SPAN_DURATION => Some(Self::SpanDuration),
            roles::SPAN_NAME => Some(Self::SpanName),
            roles::THREAD_ID => Some(Self::ThreadId),
            roles::TOKIO_TASK_ID => Some(Self::TokioTaskId),
            roles::TOKIO_WORKER_ID => Some(Self::TokioWorkerId),
            _ => None,
        }
    }
}

/// A timing field: its schema index and the multiplier that converts its
/// declared `unit` to nanoseconds.
#[derive(Debug, Clone, Copy)]
struct TimingField {
    index: usize,
    multiplier: u64,
}

/// The span's timing quantities. A span is `[start, end)`; the three
/// quantities relate by `end = start + duration`. Any two suffice — the third
/// is derived. The end always comes from the packed event timestamp (in ns), so
/// a schema declares one of start or duration. The compiler guarantees at least
/// two are resolvable; the decoder derives the rest.
#[derive(Debug, Clone, Copy)]
struct TimingLayout {
    start: Option<TimingField>,
    duration: Option<TimingField>,
}

#[derive(Debug, Clone)]
struct SingleEventSpanLayout {
    schema_name: String,
    span_type: String,
    timing: TimingLayout,
    name_index: Option<usize>,
    thread_id_index: Option<usize>,
    task_id_index: Option<usize>,
    worker_id_index: Option<usize>,
    attribute_indices: Vec<usize>,
}

#[derive(Debug, Clone)]
enum CompiledSingleEventSpan {
    NotSpan,
    Invalid(String),
    Layout(SingleEventSpanLayout),
}

fn compile_single_event_span(schema: &SchemaEntry) -> CompiledSingleEventSpan {
    let mut field_roles = vec![None; schema.fields().len()];
    let mut start_index = None;
    let mut duration_index = None;
    let mut name_index = None;
    let mut thread_id_index = None;
    let mut task_id_index = None;
    let mut worker_id_index = None;
    let mut validation_error = None;
    let mut saw_timing = false;

    for annotation in schema
        .annotations()
        .iter()
        .filter(|annotation| annotation.key() == schema_extensions::ROLE_KEY)
    {
        let Some(role) = StructuralRole::from_annotation(annotation.value()) else {
            continue;
        };
        saw_timing |= matches!(
            role,
            StructuralRole::SpanStart | StructuralRole::SpanDuration
        );
        let index = usize::from(annotation.field_index());
        if index >= schema.fields().len() {
            validation_error.get_or_insert_with(|| {
                format!(
                    "{} references missing field {}",
                    schema_extensions::ROLE_KEY,
                    annotation.field_index()
                )
            });
            continue;
        }
        match field_roles[index] {
            Some(existing) if existing == role => continue,
            Some(_) => {
                validation_error.get_or_insert_with(|| {
                    format!("field {} has conflicting structural roles", index)
                });
                continue;
            }
            None => field_roles[index] = Some(role),
        }

        let slot = match role {
            StructuralRole::SpanStart => &mut start_index,
            StructuralRole::SpanDuration => &mut duration_index,
            StructuralRole::SpanName => &mut name_index,
            StructuralRole::ThreadId => &mut thread_id_index,
            StructuralRole::TokioTaskId => &mut task_id_index,
            StructuralRole::TokioWorkerId => &mut worker_id_index,
        };
        if slot.replace(index).is_some() {
            validation_error
                .get_or_insert_with(|| format!("duplicate {} role", annotation.value()));
        }
    }

    // Not a span at all unless it carries at least one timing role.
    if !saw_timing {
        return CompiledSingleEventSpan::NotSpan;
    }
    if let Some(error) = validation_error {
        return CompiledSingleEventSpan::Invalid(error);
    }

    // A timing field's multiplier, or a validation error for a bad unit/type.
    let timing_field = |index: usize, role: &str| -> Result<TimingField, String> {
        if !is_integer_field(schema.fields()[index].field_type()) {
            return Err(format!("{role} field must have an integer wire type"));
        }
        // Per spec, an absent unit annotation defaults to nanoseconds.
        let multiplier = match unique_annotation(schema, index, "unit") {
            Ok(Some("ns")) | Ok(None) => 1,
            Ok(Some("us")) => 1_000,
            Ok(Some("ms")) => 1_000_000,
            Ok(Some("s")) => 1_000_000_000,
            Ok(Some(unit)) => return Err(format!("unsupported {role} unit {unit:?}")),
            Err(error) => return Err(error),
        };
        Ok(TimingField { index, multiplier })
    };

    let start = match start_index {
        Some(index) => match timing_field(index, roles::SPAN_START) {
            Ok(field) => Some(field),
            Err(error) => return CompiledSingleEventSpan::Invalid(error),
        },
        None => None,
    };
    let duration = match duration_index {
        Some(index) => match timing_field(index, roles::SPAN_DURATION) {
            Ok(field) => Some(field),
            Err(error) => return CompiledSingleEventSpan::Invalid(error),
        },
        None => None,
    };
    // A span is placed from any two of {start, duration, end}. The end is the
    // packed event timestamp (always present), so we need at least one of
    // start or duration.
    let quantities = usize::from(start.is_some()) + usize::from(duration.is_some()) + 1;
    if quantities < 2 {
        return CompiledSingleEventSpan::Invalid(
            "single-event span schema needs two of span.start, span.duration, and the packed \
             event timestamp (the span end)"
                .to_string(),
        );
    }

    let timing = TimingLayout { start, duration };

    if let Some(index) = name_index
        && !is_string_field(schema.fields()[index].field_type())
    {
        return CompiledSingleEventSpan::Invalid(
            "span.name field must have a string wire type".to_string(),
        );
    }
    for (role, index) in [
        (roles::THREAD_ID, thread_id_index),
        (roles::TOKIO_TASK_ID, task_id_index),
        (roles::TOKIO_WORKER_ID, worker_id_index),
    ] {
        if let Some(index) = index
            && !is_integer_field(schema.fields()[index].field_type())
        {
            return CompiledSingleEventSpan::Invalid(format!(
                "{role} field must have an integer wire type"
            ));
        }
    }

    // The span type annotation rides on whichever timing field is present
    // (producers put it on the start/duration field).
    let span_type_index = start
        .or(duration)
        .map(|f| f.index)
        .expect("at least one timing field exists");
    let span_type =
        match unique_annotation(schema, span_type_index, schema_extensions::SPAN_TYPE_KEY) {
            Ok(Some(value)) if !value.is_empty() => value.to_string(),
            Ok(_) => schema_extensions::DEFAULT_SPAN_TYPE.to_string(),
            Err(error) => return CompiledSingleEventSpan::Invalid(error),
        };
    let attribute_indices = field_roles
        .iter()
        .enumerate()
        .filter_map(|(index, role)| {
            (role.is_none() || *role == Some(StructuralRole::SpanName)).then_some(index)
        })
        .collect();

    CompiledSingleEventSpan::Layout(SingleEventSpanLayout {
        schema_name: schema.name().to_string(),
        span_type,
        timing,
        name_index,
        thread_id_index,
        task_id_index,
        worker_id_index,
        attribute_indices,
    })
}

fn unique_annotation<'a>(
    schema: &'a SchemaEntry,
    field_index: usize,
    key: &str,
) -> Result<Option<&'a str>, String> {
    let mut value = None;
    for annotation in schema.annotations().iter().filter(|annotation| {
        usize::from(annotation.field_index()) == field_index && annotation.key() == key
    }) {
        match value {
            Some(existing) if existing != annotation.value() => {
                return Err(format!(
                    "field {field_index} has conflicting {key} annotations"
                ));
            }
            Some(_) => {}
            None => value = Some(annotation.value()),
        }
    }
    Ok(value)
}

fn is_integer_field(field_type: FieldType) -> bool {
    matches!(
        field_type.inner(),
        FieldType::I64 | FieldType::Varint | FieldType::U8 | FieldType::U16 | FieldType::U32
    )
}

fn is_string_field(field_type: FieldType) -> bool {
    matches!(
        field_type.inner(),
        FieldType::String | FieldType::PooledString
    )
}

fn unsigned_value(value: &FieldValueRef<'_>) -> Result<Option<u64>, &'static str> {
    match value {
        FieldValueRef::Varint(value) => Ok(Some(*value)),
        FieldValueRef::I64(value) => u64::try_from(*value)
            .map(Some)
            .map_err(|_| "integer structural field is negative"),
        FieldValueRef::None => Ok(None),
        _ => Err("integer structural field has an incompatible value"),
    }
}

fn string_value<'a>(
    ev: &'a RawEvent<'_, '_>,
    index: usize,
) -> Result<Option<&'a str>, &'static str> {
    match &ev.fields[index] {
        FieldValueRef::String(value) => Ok(Some(value)),
        FieldValueRef::PooledString(id) => ev
            .string_pool
            .get(*id)
            .map(Some)
            .ok_or("span.name references an unresolved pooled string"),
        FieldValueRef::None => Ok(None),
        _ => Err("span.name field has an incompatible value"),
    }
}

impl TimingLayout {
    /// Read one timing field and scale it to nanoseconds.
    fn read(field: TimingField, ev: &RawEvent<'_, '_>) -> Result<Option<u64>, &'static str> {
        let Some(raw) = unsigned_value(&ev.fields[field.index])? else {
            return Ok(None);
        };
        raw.checked_mul(field.multiplier)
            .ok_or("single-event span timing field overflows nanoseconds")
            .map(Some)
    }

    /// Resolve `(start_ns, end_ns)` from any two of start, duration, and end,
    /// where the end is the packed event timestamp (always present). The
    /// compiler has already guaranteed at least one of start/duration is
    /// present in the schema; this handles a value being absent at runtime
    /// (optional field) and the arithmetic.
    fn resolve(&self, ev: &RawEvent<'_, '_>) -> Result<(u64, u64), &'static str> {
        let start = match self.start {
            Some(field) => Self::read(field, ev)?,
            None => None,
        };
        let duration = match self.duration {
            Some(field) => Self::read(field, ev)?,
            None => None,
        };
        let end = ev.timestamp_ns;

        match (start, duration) {
            // Start + end (the common metrique-style case uses end + duration
            // below; this covers a start field with the packed end).
            (Some(start), _) => {
                if start > end {
                    return Err("single-event span start follows its end");
                }
                Ok((start, end))
            }
            // End + duration: derive start. Duration is unsigned, so a start
            // after end is unrepresentable; saturate rather than underflow.
            (None, Some(duration)) => Ok((end.saturating_sub(duration), end)),
            // Neither start nor duration resolvable at runtime (both optional
            // timing fields were absent on this event).
            (None, None) => Err("single-event span is missing a required timing value"),
        }
    }
}

impl SingleEventSpanLayout {
    fn decode(
        &self,
        ev: &RawEvent<'_, '_>,
        decode_sequence: u64,
    ) -> Result<SingleEventSpanEvent, &'static str> {
        let (start_ns, end_ns) = self.timing.resolve(ev)?;

        let name = match self.name_index {
            Some(index) => string_value(ev, index)?
                .filter(|name| !name.trim().is_empty())
                .unwrap_or(&self.schema_name)
                .to_string(),
            None => self.schema_name.clone(),
        };
        let context_value = |index: Option<usize>| -> Result<Option<u64>, &'static str> {
            index
                .map(|index| unsigned_value(&ev.fields[index]))
                .transpose()
                .map(Option::flatten)
        };
        let thread_id = context_value(self.thread_id_index)?;
        let task_id = context_value(self.task_id_index)?.filter(|id| *id > 0);
        let worker_id = context_value(self.worker_id_index)?;
        let attributes = extract_attributes(ev, |index, _| {
            self.attribute_indices.binary_search(&index).is_ok()
        });

        Ok(SingleEventSpanEvent {
            schema_name: self.schema_name.clone(),
            span_type: self.span_type.clone(),
            name,
            start_ns,
            end_ns,
            thread_id,
            task_id,
            worker_id,
            decode_sequence,
            attributes,
        })
    }
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
    // Find the last `:digits` suffix for line number. The first single colon
    // before it separates target/name from the file; later colons belong to the
    // file path (for example the drive separator in `C:\src\lib.rs`).

    // Find the last `:` followed by only digits
    let line_colon_pos = rest.rfind(':')?;
    let line_str = &rest[line_colon_pos + 1..];
    let line: u32 = line_str.parse().ok()?;

    let before_line = &rest[..line_colon_pos];

    let file_colon_pos = find_first_single_colon(before_line)?;

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

/// Find the first `:` in `s` that is NOT part of a `::` sequence.
/// Returns the byte offset of the single colon, or None if not found.
pub(crate) fn find_first_single_colon(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':' {
            let preceded_by_colon = i > 0 && bytes[i - 1] == b':';
            let followed_by_colon = i + 1 < bytes.len() && bytes[i + 1] == b':';
            if !preceded_by_colon && !followed_by_colon {
                return Some(i);
            }
            if followed_by_colon {
                i += 2;
                continue;
            }
        }
        i += 1;
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
    TaskSpawn(TaskSpawn),
    TaskTerminate(TaskTerminate),
    Wake(WakeEvent),
}

impl TraceEvent {
    pub(crate) fn timestamp_ns(&self) -> u64 {
        match self {
            Self::CpuSample(e) => e.timestamp_ns,
            Self::WorkerPark(e) => e.timestamp_ns,
            Self::WorkerUnpark(e) => e.timestamp_ns,
            Self::PollStart(e) => e.timestamp_ns,
            Self::PollEnd(e) => e.timestamp_ns,
            Self::TaskSpawn(e) => e.timestamp_ns,
            Self::TaskTerminate(e) => e.timestamp_ns,
            Self::Wake(e) => e.timestamp_ns,
        }
    }
}

/// All information collected in one pass over the self-describing wire stream.
pub(crate) struct DecodedTrace {
    pub(crate) interner: Rodeo,
    pub(crate) addr_to_keys: FxHashMap<u64, Vec<(u64, Spur)>>,
    pub(crate) events: Vec<TraceEvent>,
    pub(crate) clock_offset: Option<ClockOffset>,
    pub(crate) first_clock_sync_mono: Option<MonoNs>,
    pub(crate) segment_metadata_boot_id: Option<String>,
    pub(crate) legacy_enters: Vec<(String, LegacySpanEnterEvent)>,
    pub(crate) legacy_exits: Vec<(String, LegacySpanExitEvent)>,
    pub(crate) legacy_closes: Vec<LegacySpanCloseEvent>,
    pub(crate) single_event_spans: Vec<SingleEventSpanEvent>,
}

/// Decode relevant wire events without imposing downstream ordering or attribution.
pub(crate) fn decode_trace(data: &[u8], source_key: &str) -> anyhow::Result<DecodedTrace> {
    let mut decoder = Decoder::new(data).ok_or_else(|| anyhow::anyhow!("invalid trace header"))?;

    let mut interner = Rodeo::default();
    let mut addr_to_keys: FxHashMap<u64, Vec<(u64, Spur)>> = FxHashMap::default();
    let mut events: Vec<TraceEvent> = Vec::new();
    let mut clock_offset: Option<ClockOffset> = None;
    let mut first_clock_sync_mono: Option<MonoNs> = None;
    // Authoritative boot_id decoded from SegmentMetadata entries. When the
    // namespace isolation layer is active, the writer stamps a
    // `boot_id` key in segment metadata. This is the stable cross-segment
    // process identity anchor. When absent, we fall back to the path-based
    // extraction which cannot claim cross-file stability.
    let mut segment_metadata_boot_id: Option<String> = None;
    // Monotonically increasing counter assigned to span enter/exit events in
    // wire decode order. Used to break timestamp ties deterministically.
    let mut span_decode_sequence: u64 = 0;

    // Span events: SpanEnter/Exit with span_id plus dial9.tokio.task_id
    // (current) or worker_id (legacy), and SpanCloseEvent with only span_id.
    // These are reconstructed into ResolvedSpan rows using local context
    // (`resolve_legacy_spans`).
    let mut legacy_enters: Vec<(String, LegacySpanEnterEvent)> = Vec::new(); // (schema_name, event)
    let mut legacy_exits: Vec<(String, LegacySpanExitEvent)> = Vec::new();
    let mut legacy_closes: Vec<LegacySpanCloseEvent> = Vec::new();
    let mut legacy_enter_decode_errors: u64 = 0;
    let mut legacy_exit_decode_errors: u64 = 0;
    let mut legacy_close_decode_errors: u64 = 0;
    let mut single_event_spans: Vec<SingleEventSpanEvent> = Vec::new();
    let mut single_event_decode_sequence: u64 = 0;
    let mut single_event_schema_errors: u64 = 0;
    let mut single_event_decode_errors: u64 = 0;
    let mut single_event_layouts: FxHashMap<WireTypeId, (SchemaEntry, CompiledSingleEventSpan)> =
        FxHashMap::default();
    // Single-pass span classification. This is correct because span-role
    // annotations are required to precede any event of their schema (see
    // docs/design/single-event-spans.md, "Annotation ordering"): `ev.schema` is
    // already complete when the first event of a type is compiled, so a later
    // frame can never turn an already-decoded event into a span. The JS decoder
    // relies on the same guarantee; neither side re-resolves spans after the
    // fact.
    decoder
        .for_each_event(|ev| {
            let needs_compile = single_event_layouts
                .get(&ev.type_id)
                .is_none_or(|(schema, _)| schema != ev.schema);
            if needs_compile {
                let compiled = compile_single_event_span(ev.schema);
                if let CompiledSingleEventSpan::Invalid(error) = &compiled {
                    single_event_schema_errors += 1;
                    tracing::debug!(
                        source_key,
                        schema = ev.name,
                        error,
                        "ignoring invalid single-event span schema"
                    );
                }
                single_event_layouts.insert(ev.type_id, (ev.schema.clone(), compiled));
            }
            let compiled = &single_event_layouts
                .get(&ev.type_id)
                .expect("schema was compiled above")
                .1;
            if let CompiledSingleEventSpan::Layout(layout) = compiled {
                let decode_sequence = single_event_decode_sequence;
                single_event_decode_sequence += 1;
                match layout.decode(&ev, decode_sequence) {
                    Ok(event) => single_event_spans.push(event),
                    Err(_) => single_event_decode_errors += 1,
                }
            }
            if !matches!(compiled, CompiledSingleEventSpan::NotSpan) {
                return;
            }
            match ev.name {
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
                        first_clock_sync_mono = Some(MonoNs(cs.timestamp_ns));
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
                "TaskSpawnEvent" => {
                    if let Ok(event) = ev.deserialize::<TaskSpawn>() {
                        events.push(TraceEvent::TaskSpawn(event));
                    }
                }
                "TaskTerminateEvent" => {
                    if let Ok(event) = ev.deserialize::<TaskTerminate>() {
                        events.push(TraceEvent::TaskTerminate(event));
                    }
                }
                "WakeEventEvent" => {
                    if let Ok(event) = ev.deserialize::<WakeEvent>() {
                        events.push(TraceEvent::Wake(event));
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
                        Ok(mut lc) if lc.span_id > 0 => {
                            lc.decode_sequence = span_decode_sequence;
                            span_decode_sequence += 1;
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
                            le.attributes = extract_span_attributes(&ev);
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
                            le.attributes = extract_span_attributes(&ev);
                            legacy_exits.push((name.to_string(), le));
                        }
                        _ => {
                            legacy_exit_decode_errors += 1;
                        }
                    }
                }
                _ => {}
            }
        })
        .map_err(|e| anyhow::anyhow!("decode error: {e}"))?;

    if legacy_close_decode_errors > 0
        || legacy_enter_decode_errors > 0
        || legacy_exit_decode_errors > 0
        || single_event_schema_errors > 0
        || single_event_decode_errors > 0
    {
        use dial9_core::rate_limited;
        rate_limited!(std::time::Duration::from_secs(60), {
            tracing::warn!(
                source_key,
                legacy_enter_errors = legacy_enter_decode_errors,
                legacy_exit_errors = legacy_exit_decode_errors,
                legacy_close_errors = legacy_close_decode_errors,
                single_event_schema_errors,
                single_event_decode_errors,
                "skipped malformed span event(s) during decode"
            );
        });
    }

    Ok(DecodedTrace {
        interner,
        addr_to_keys,
        events,
        clock_offset,
        first_clock_sync_mono,
        segment_metadata_boot_id,
        legacy_enters,
        legacy_exits,
        legacy_closes,
        single_event_spans,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dial9_trace_format::encoder::{Encoder, Schema};
    use dial9_trace_format::schema::{FieldAnnotation, FieldDef};
    use dial9_trace_format::types::FieldValue;

    fn schema(
        name: &str,
        fields: impl IntoIterator<Item = (&'static str, FieldType)>,
        annotations: impl IntoIterator<Item = FieldAnnotation>,
    ) -> SchemaEntry {
        SchemaEntry::with_annotations(
            name,
            fields
                .into_iter()
                .map(|(name, field_type)| FieldDef::new(name, field_type)),
            annotations,
        )
    }

    #[test]
    fn tracing_span_task_id_is_namespaced() {
        let current_schema = Schema::new(
            "SpanEnter:current::request:src/main.rs:10",
            vec![
                FieldDef::new(TOKIO_TASK_ID_FIELD, FieldType::OptionalVarint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("parent_span_id", FieldType::OptionalVarint),
                FieldDef::new("span_name", FieldType::PooledString),
                FieldDef::new("task_id", FieldType::OptionalPooledString),
            ],
        );
        let legacy_schema = Schema::new(
            "SpanEnter:legacy::request:src/main.rs:20",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("span_id", FieldType::Varint),
                FieldDef::new("parent_span_id", FieldType::OptionalVarint),
                FieldDef::new("span_name", FieldType::PooledString),
                FieldDef::new("task_id", FieldType::OptionalPooledString),
            ],
        );

        let mut encoder = Encoder::new();
        let current_name = encoder.intern_string("current").unwrap();
        let current_application_task_id = encoder.intern_string("current-application").unwrap();
        let legacy_name = encoder.intern_string("legacy").unwrap();
        let legacy_application_task_id = encoder.intern_string("legacy-application").unwrap();
        encoder
            .write_event(
                &current_schema,
                100,
                &[
                    FieldValue::Varint(42),
                    FieldValue::Varint(1),
                    FieldValue::None,
                    FieldValue::PooledString(current_name),
                    FieldValue::PooledString(current_application_task_id),
                ],
            )
            .unwrap();
        encoder
            .write_event(
                &legacy_schema,
                200,
                &[
                    FieldValue::Varint(7),
                    FieldValue::Varint(2),
                    FieldValue::None,
                    FieldValue::PooledString(legacy_name),
                    FieldValue::PooledString(legacy_application_task_id),
                ],
            )
            .unwrap();

        let decoded = decode_trace(&encoder.into_inner(), "source").unwrap();
        let current = decoded
            .legacy_enters
            .iter()
            .find(|(name, _)| name.contains(":current::"))
            .unwrap();
        assert_eq!(current.1.task_id, Some(42));
        assert_eq!(
            current.1.attributes,
            vec![("task_id".to_string(), "current-application".to_string())]
        );

        let legacy = decoded
            .legacy_enters
            .iter()
            .find(|(name, _)| name.contains(":legacy::"))
            .unwrap();
        assert_eq!(legacy.1.task_id, None);
        assert_eq!(legacy.1.worker_id, Some(7));
        assert_eq!(
            legacy.1.attributes,
            vec![("task_id".to_string(), "legacy-application".to_string())]
        );
    }

    #[test]
    fn schema_name_does_not_classify_single_event_spans() {
        let schema = schema(
            "metrique:Unannotated",
            [("duration", FieldType::Varint)],
            [],
        );
        assert!(matches!(
            compile_single_event_span(&schema),
            CompiledSingleEventSpan::NotSpan
        ));
    }

    #[test]
    fn compiles_roles_units_type_and_attribute_indices() {
        let schema = schema(
            "producer:Work",
            [
                ("began", FieldType::Varint),
                ("display", FieldType::PooledString),
                ("payload", FieldType::String),
                ("future_role", FieldType::Varint),
            ],
            [
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_START),
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_START),
                FieldAnnotation::new(0, "unit", "ms"),
                FieldAnnotation::new(0, schema_extensions::SPAN_TYPE_KEY, "test-producer"),
                FieldAnnotation::new(1, schema_extensions::ROLE_KEY, roles::SPAN_NAME),
                FieldAnnotation::new(3, schema_extensions::ROLE_KEY, "future.role"),
            ],
        );

        let CompiledSingleEventSpan::Layout(layout) = compile_single_event_span(&schema) else {
            panic!("expected a valid single-event span layout");
        };
        assert_eq!(layout.timing.start.map(|f| f.index), Some(0));
        assert_eq!(layout.timing.start.map(|f| f.multiplier), Some(1_000_000));
        assert!(layout.timing.duration.is_none());
        assert_eq!(layout.name_index, Some(1));
        assert_eq!(layout.span_type, "test-producer");
        assert_eq!(layout.attribute_indices, vec![1, 2, 3]);
    }

    #[test]
    fn compiles_span_duration_role() {
        // A `span.duration` field is the duration-encoded counterpart of
        // `span.start`; the packed timestamp remains the end.
        let schema = schema(
            "metrique:Work",
            [("dur", FieldType::Varint), ("display", FieldType::String)],
            [
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_DURATION),
                FieldAnnotation::new(0, "unit", "ns"),
                FieldAnnotation::new(1, schema_extensions::ROLE_KEY, roles::SPAN_NAME),
            ],
        );
        let CompiledSingleEventSpan::Layout(layout) = compile_single_event_span(&schema) else {
            panic!("expected a valid single-event span layout");
        };
        assert_eq!(layout.timing.duration.map(|f| f.index), Some(0));
        assert_eq!(layout.timing.duration.map(|f| f.multiplier), Some(1));
        assert!(layout.timing.start.is_none());
        assert_eq!(layout.name_index, Some(1));
    }

    #[test]
    fn start_plus_duration_compiles() {
        // Two explicit timing quantities plus the packed end: all three timing
        // sources are available.
        let schema = SchemaEntry::with_annotations(
            "producer:StartDur",
            [
                FieldDef::new("start", FieldType::Varint),
                FieldDef::new("dur", FieldType::Varint),
            ],
            [
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_START),
                FieldAnnotation::new(1, schema_extensions::ROLE_KEY, roles::SPAN_DURATION),
            ],
        );
        let CompiledSingleEventSpan::Layout(layout) = compile_single_event_span(&schema) else {
            panic!("start + duration must compile");
        };
        assert_eq!(layout.timing.start.map(|f| f.index), Some(0));
        assert_eq!(layout.timing.duration.map(|f| f.index), Some(1));
    }

    #[test]
    fn single_duration_with_packed_end_is_valid() {
        // With packed_end always true (all events carry a timestamp),
        // duration + packed end gives two quantities, which is enough to
        // place a span.
        let schema = schema(
            "producer:OnlyDuration",
            [("dur", FieldType::Varint)],
            [FieldAnnotation::new(
                0,
                schema_extensions::ROLE_KEY,
                roles::SPAN_DURATION,
            )],
        );
        let CompiledSingleEventSpan::Layout(layout) = compile_single_event_span(&schema) else {
            panic!("duration + packed end should be valid");
        };
        assert_eq!(layout.timing.duration.map(|f| f.index), Some(0));
        assert!(layout.timing.start.is_none());
    }

    #[test]
    fn missing_unit_annotation_defaults_to_nanoseconds() {
        let schema = schema(
            "producer:NoUnit",
            [("dur", FieldType::Varint)],
            [FieldAnnotation::new(
                0,
                schema_extensions::ROLE_KEY,
                roles::SPAN_DURATION,
            )],
        );
        let CompiledSingleEventSpan::Layout(layout) = compile_single_event_span(&schema) else {
            panic!("expected a valid single-event span layout");
        };
        assert_eq!(layout.timing.duration.map(|f| f.multiplier), Some(1));
    }

    #[test]
    fn duration_encoded_span_derives_start_from_end() {
        // Packed end = 500, duration = 120 -> start = 380.
        let schema = Schema::from_entry(schema(
            "metrique:Work",
            [("dur", FieldType::Varint)],
            [
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_DURATION),
                FieldAnnotation::new(0, "unit", "ns"),
            ],
        ));
        let mut encoder = Encoder::new();
        encoder
            .write_event(&schema, 500, &[FieldValue::Varint(120)])
            .unwrap();
        let decoded = decode_trace(&encoder.into_inner(), "source").unwrap();
        assert_eq!(decoded.single_event_spans.len(), 1);
        let span = &decoded.single_event_spans[0];
        assert_eq!(span.end_ns, 500);
        assert_eq!(span.start_ns, 380);
    }

    #[test]
    fn duration_longer_than_end_saturates_start_to_zero() {
        // A duration exceeding the packed end cannot happen from a sane clock,
        // but the decoder must not underflow: start saturates to 0.
        let schema = Schema::from_entry(schema(
            "metrique:Work",
            [("dur", FieldType::Varint)],
            [
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_DURATION),
                FieldAnnotation::new(0, "unit", "ns"),
            ],
        ));
        let mut encoder = Encoder::new();
        encoder
            .write_event(&schema, 100, &[FieldValue::Varint(999)])
            .unwrap();
        let decoded = decode_trace(&encoder.into_inner(), "source").unwrap();
        assert_eq!(decoded.single_event_spans.len(), 1);
        assert_eq!(decoded.single_event_spans[0].start_ns, 0);
    }

    #[test]
    fn rejects_duplicate_roles_and_conflicting_units() {
        let duplicate_start = schema(
            "producer:DuplicateStart",
            [("first", FieldType::Varint), ("second", FieldType::Varint)],
            [
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_START),
                FieldAnnotation::new(0, "unit", "ns"),
                FieldAnnotation::new(1, schema_extensions::ROLE_KEY, roles::SPAN_START),
            ],
        );
        assert!(matches!(
            compile_single_event_span(&duplicate_start),
            CompiledSingleEventSpan::Invalid(_)
        ));

        let conflicting_unit = schema(
            "producer:ConflictingUnit",
            [("start", FieldType::Varint)],
            [
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_START),
                FieldAnnotation::new(0, "unit", "ns"),
                FieldAnnotation::new(0, "unit", "ms"),
            ],
        );
        assert!(matches!(
            compile_single_event_span(&conflicting_unit),
            CompiledSingleEventSpan::Invalid(_)
        ));
    }

    #[test]
    fn out_of_range_span_start_annotation_is_invalid() {
        let schema = schema(
            "producer:MissingStartField",
            [("payload", FieldType::String)],
            [FieldAnnotation::new(
                7,
                schema_extensions::ROLE_KEY,
                roles::SPAN_START,
            )],
        );
        assert!(matches!(
            compile_single_event_span(&schema),
            CompiledSingleEventSpan::Invalid(_)
        ));
    }

    #[test]
    fn defaults_span_type_when_annotation_is_absent() {
        let schema = schema(
            "producer:DefaultType",
            [("start", FieldType::Varint)],
            [
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_START),
                FieldAnnotation::new(0, "unit", "ns"),
            ],
        );
        let CompiledSingleEventSpan::Layout(layout) = compile_single_event_span(&schema) else {
            panic!("expected a valid single-event span layout");
        };
        assert_eq!(layout.span_type, schema_extensions::DEFAULT_SPAN_TYPE);
    }

    #[test]
    fn annotated_schema_does_not_also_use_legacy_name_decoder() {
        let schema = Schema::from_entry(schema(
            "SpanClose__Collision",
            [("start", FieldType::Varint), ("span_id", FieldType::Varint)],
            [
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_START),
                FieldAnnotation::new(0, "unit", "ns"),
            ],
        ));
        let mut encoder = Encoder::new();
        encoder
            .write_event(
                &schema,
                200,
                &[FieldValue::Varint(100), FieldValue::Varint(42)],
            )
            .unwrap();

        let decoded = decode_trace(&encoder.into_inner(), "source").unwrap();
        assert_eq!(decoded.single_event_spans.len(), 1);
        assert!(decoded.legacy_closes.is_empty());
    }

    #[test]
    fn annotated_schema_does_not_also_use_runtime_name_decoder() {
        for annotations in [
            vec![
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_START),
                FieldAnnotation::new(0, "unit", "ns"),
            ],
            vec![FieldAnnotation::new(
                0,
                schema_extensions::ROLE_KEY,
                roles::SPAN_START,
            )],
        ] {
            let schema = Schema::from_entry(schema(
                "ClockSyncEvent",
                [
                    ("start", FieldType::Varint),
                    ("realtime_ns", FieldType::Varint),
                    ("timestamp_ns", FieldType::Varint),
                ],
                annotations,
            ));
            let mut encoder = Encoder::new();
            encoder
                .write_event(
                    &schema,
                    200,
                    &[
                        FieldValue::Varint(100),
                        FieldValue::Varint(1_000),
                        FieldValue::Varint(200),
                    ],
                )
                .unwrap();

            let decoded = decode_trace(&encoder.into_inner(), "source").unwrap();
            assert!(
                decoded.clock_offset.is_none(),
                "annotated schemas must not mutate runtime decode state"
            );
        }
    }

    #[test]
    fn parses_ad_hoc_span_schema_names() {
        let macro_span =
            parse_legacy_span_schema_name("SpanEnter:dial9_utils::adhoc_17:src/service.rs:42")
                .expect("macro span schema should parse");
        assert_eq!(macro_span.target, "dial9_utils");
        assert_eq!(macro_span.name, "adhoc_17");
        assert_eq!(macro_span.file.as_deref(), Some("src/service.rs"));
        assert_eq!(macro_span.line, Some(42));

        let runtime_span = parse_legacy_span_schema_name("SpanExit:dial9_utils::runtime:runtime:0")
            .expect("runtime span schema should parse");
        assert_eq!(runtime_span.target, "dial9_utils");
        assert_eq!(runtime_span.name, "runtime");
        assert_eq!(runtime_span.file.as_deref(), Some("runtime"));
        assert_eq!(runtime_span.line, Some(0));
    }
}

//! First-use descriptor walk: turns a metrique entry's descriptor segments
//! into a cached encode plan (wire schema + per-field routing actions).

use std::collections::HashSet;

use dial9_trace_format::encoder::Schema;
use dial9_trace_format::schema::{FieldAnnotation, FieldDef, SchemaEntry};
use dial9_trace_format::types::FieldType;
use metrique_writer::core::descriptor::{AvailableDescriptors, FieldShape, FieldView, KnownShape};

use super::context::field_names;
use super::{Context, Emit, Interned};

/// Annotation key carrying a field's unit. Matches the key the `TraceEvent`
/// derive emits, so viewers surface both through the same path.
pub(crate) const UNIT_ANNOTATION_KEY: &str = "unit";

/// Map a metrique unit to the vocabulary the viewer formats
/// (`us`/`ms`/`s`/`bytes`), plus `count` (rendered plain, kept by
/// trace-shape extraction). Units outside it keep their CloudWatch name and
/// render unformatted.
fn unit_annotation_value(unit: metrique_writer::Unit) -> &'static str {
    use metrique_writer::Unit;
    use metrique_writer::core::unit::{NegativeScale, PositiveScale};
    match unit {
        Unit::Second(NegativeScale::Micro) => "us",
        Unit::Second(NegativeScale::Milli) => "ms",
        Unit::Second(NegativeScale::One) => "s",
        Unit::Byte(PositiveScale::One) => "bytes",
        Unit::Count => "count",
        other => other.name(),
    }
}

/// How the sink handles one `EntryWriter::value` callback.
///
/// Actions are stored in descriptor order and consumed positionally: metrique
/// guarantees that walking `descriptors()` segments in sequence yields fields
/// in exactly the order `Entry::write` emits value callbacks
#[derive(Debug, Clone, Copy)]
pub(crate) enum FieldAction {
    /// Field is neither context nor payload (or its shape is unsupported):
    /// consume the callback and discard the value.
    Skip,
    /// One of [`Dial9Context`](super::Dial9Context)'s fields: route into the
    /// event header slots.
    Context(ContextRole),
    /// An `Emit`-tagged field: capture as `kind` and append to the event's
    /// payload values (walk order is the schema's payload order). `optional`
    /// mirrors the schema field's wire type: an optional payload may be
    /// absent, a required one missing drops the event.
    Payload { optional: bool, kind: ValueKind },
}

/// Which event-header slot a context field feeds.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ContextRole {
    WorkerId,
    TaskId,
    MonotonicStart,
    MonotonicEnd,
}

impl ContextRole {
    pub(crate) const COUNT: usize = 4;

    /// Index for per-role bookkeeping; exhaustive so a new role cannot
    /// silently exceed [`Self::COUNT`].
    fn index(self) -> usize {
        match self {
            ContextRole::WorkerId => 0,
            ContextRole::TaskId => 1,
            ContextRole::MonotonicStart => 2,
            ContextRole::MonotonicEnd => 3,
        }
    }
}

/// The wire value a payload field resolves to. Derived from the descriptor's
/// [`FieldShape`] plus the `Interned` flag; drives both the schema
/// [`FieldType`] and how the runtime `Value` callbacks are interpreted.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ValueKind {
    /// A single observation, arriving through the `string()` or `metric()`
    /// callback.
    Scalar(ScalarKind),
    /// A list-shaped field, arriving through the `values()` callback; each
    /// element captures as `elem`.
    List { elem: ScalarKind },
}

/// How one scalar observation maps to a
/// [`FieldValue`](dial9_trace_format::types::FieldValue): the wire
/// representation of a single `string()` or `metric()` callback.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ScalarKind {
    Bool,
    Uint,
    /// Signed values necessarily come from custom `Value` impls and arrive
    /// as integral floats: `Observation` has no signed variant.
    Int,
    Float,
    /// `String` on the wire, or a string-pool reference when `interned`.
    Str {
        interned: bool,
    },
}

/// Cached per-entry-type encode plan. Built once per distinct descriptor-id
/// sequence, used for every entry of that type.
#[derive(Debug)]
pub(crate) struct Plan {
    /// Wire schema: implicit timestamp, the four header fields, then one
    /// field per supported `Emit`-tagged descriptor field. Payload slot `i`
    /// is schema field `HEADER_FIELDS + i`.
    pub(crate) schema: Schema,
    /// Action per `value` callback, in descriptor order (= write order).
    pub(crate) actions: Vec<FieldAction>,
    /// No `Emit` fields and no context fields: entries of this type are
    /// skipped without walking `Entry::write`.
    pub(crate) inert: bool,
    /// Entry canonical name, for diagnostics.
    pub(crate) entry_name: String,
}

/// The event-header fields every metrique schema starts with, preceding the
/// payload fields. The event timestamp is implicit and not part of the
/// schema field list.
//
// `ALL` is the single source of truth for header order: the schema builder
// here and the value assembly in `writer.rs` both iterate it, so the two
// cannot disagree. A variant omitted from `ALL` is consistently absent from
// both sides (the header just doesn't ship), never misaligned.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Header {
    WorkerId,
    TaskId,
    /// Request duration (`monotonic_ns_end - monotonic_ns_start` from the
    /// context), encoded instead of an absolute end timestamp: durations
    /// varint-encode in a fraction of the bytes, and consumers want the
    /// duration anyway (`end = event timestamp + duration`).
    Duration,
    WallClock,
}

impl Header {
    pub(crate) const ALL: [Header; 4] = [
        Header::WorkerId,
        Header::TaskId,
        Header::Duration,
        Header::WallClock,
    ];

    fn name(self) -> &'static str {
        match self {
            Header::WorkerId => "worker_id",
            Header::TaskId => "task_id",
            Header::Duration => "duration_ns",
            Header::WallClock => "wall_clock_ns",
        }
    }

    fn field_type(self) -> FieldType {
        match self {
            Header::WorkerId => FieldType::Varint,
            Header::TaskId | Header::Duration | Header::WallClock => FieldType::OptionalVarint,
        }
    }
}

/// Number of header fields; payload value `i` is schema field
/// `HEADER_FIELDS + i`.
pub(crate) const HEADER_FIELDS: usize = Header::ALL.len();

fn header_field_defs() -> Vec<FieldDef> {
    Header::ALL
        .iter()
        .map(|h| FieldDef::new(h.name(), h.field_type()))
        .collect()
}

/// Walk the descriptor segments once and build the encode plan.
//
// Diagnostics for structurally unsupported fields fire here, once per
// distinct descriptor-id sequence (the plan is cached by the caller).
pub(crate) fn build_plan(
    descs: &AvailableDescriptors<'_>,
    used_names: &mut HashSet<String>,
) -> Plan {
    let entry_name = descs
        .iter()
        .next()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|| "<unnamed>".to_owned());

    let mut actions = Vec::new();
    let mut fields = header_field_defs();
    let mut annotations = Vec::new();
    let mut has_context = false;
    let mut has_emit = false;
    let mut roles_seen = [false; ContextRole::COUNT];

    for seg in descs.iter() {
        for field in seg.fields() {
            if field.flags().any(|f| f.is::<Context>()) {
                let action = context_action(&field, &entry_name, &mut roles_seen);
                has_context |= matches!(action, FieldAction::Context(_));
                actions.push(action);
                continue;
            }

            if !field.flags().any(|f| f.is::<Emit>()) {
                actions.push(FieldAction::Skip);
                continue;
            }
            has_emit = true;

            let full_name: String = field.name_parts().collect();

            // Positional routing does not care about names, but the schema
            // does: two fields with the same name would be indistinguishable
            // to trace consumers. First occurrence (headers included) wins.
            if fields.iter().any(|f| f.name() == full_name) {
                tracing::error!(
                    entry = %entry_name,
                    field = %full_name,
                    "metrique field name collides with a dial9 header field or an \
                     earlier payload field; skipping this occurrence"
                );
                actions.push(FieldAction::Skip);
                continue;
            }

            let interned = field.flags().any(|f| f.is::<Interned>());
            match resolve_shape(field.shape(), interned) {
                Ok((field_type, kind)) => {
                    // `to_option` filters `Unit::None`, which some custom
                    // `Value` impls surface as `Some(Unit::None)`.
                    if let Some(unit) = field.unit().and_then(|u| u.to_option()) {
                        match u16::try_from(fields.len()) {
                            Ok(index) => annotations.push(FieldAnnotation::new(
                                index,
                                UNIT_ANNOTATION_KEY,
                                unit_annotation_value(unit),
                            )),
                            Err(_) => tracing::warn!(
                                entry = %entry_name,
                                field = %full_name,
                                "field index exceeds annotation range; unit not recorded"
                            ),
                        }
                    }
                    fields.push(FieldDef::new(full_name, field_type));
                    actions.push(FieldAction::Payload {
                        optional: field_type.is_optional(),
                        kind,
                    });
                }
                Err(reason) => {
                    // Once per descriptor sequence (plans are cached), so no
                    // rate limiting needed.
                    tracing::error!(
                        entry = %entry_name,
                        field = %full_name,
                        reason,
                        "metrique field tagged for dial9 cannot be encoded; skipping field"
                    );
                    actions.push(FieldAction::Skip);
                }
            }
        }
    }

    if has_emit && !has_context {
        tracing::warn!(
            entry = %entry_name,
            "metrique entry has dial9 Emit fields but no Dial9Context; events will carry \
             an unknown worker and a flush-thread timestamp. Flatten a Dial9Context into \
             the entry to fix this."
        );
    }

    let inert = !has_emit && !has_context;

    let schema_name = schema_name(&entry_name, inert, &fields, &annotations, used_names);
    let schema = Schema::from_entry(SchemaEntry::with_annotations(
        schema_name,
        /* has_timestamp */ true,
        fields,
        annotations,
    ));

    Plan {
        schema,
        actions,
        inert,
        entry_name,
    }
}

/// Classify one `Context`-flagged field into a header role.
//
// Roles match on canonicalized base names (see `field_names` in
// `context.rs`). Each role routes at most once; repeats (a second
// `Dial9Context` in the same entry) and unrecognized names are skipped
// with a warning.
fn context_action(
    field: &FieldView<'_>,
    entry_name: &str,
    roles_seen: &mut [bool; ContextRole::COUNT],
) -> FieldAction {
    let role = match field_names::canonicalize(field.base_name()).as_str() {
        field_names::WORKER_ID => Some(ContextRole::WorkerId),
        field_names::TASK_ID => Some(ContextRole::TaskId),
        field_names::MONOTONIC_NS_START => Some(ContextRole::MonotonicStart),
        field_names::MONOTONIC_NS_END => Some(ContextRole::MonotonicEnd),
        _ => None,
    };
    match role {
        Some(role) if !roles_seen[role.index()] => {
            roles_seen[role.index()] = true;
            FieldAction::Context(role)
        }
        Some(_) => {
            tracing::warn!(
                entry = %entry_name,
                field = %field.base_name(),
                "duplicate dial9 context field; ignoring"
            );
            FieldAction::Skip
        }
        None => {
            // A foreign field carrying our internal flag; nothing sensible
            // to do with it.
            tracing::warn!(
                entry = %entry_name,
                field = %field.base_name(),
                "unrecognized dial9 context field; skipping"
            );
            FieldAction::Skip
        }
    }
}

/// Pick the wire schema name: `metrique:<EntryName>`, suffixed when taken.
//
// Distinct descriptor sequences can share a canonical name (the same child
// struct flattened under different prefixes, an Option-flattened child
// present vs absent, same-named types in different modules). The first
// arrival keeps the bare name (arrival-order-dependent); later ones get a
// suffix hashed from the schema layout, which is stable for a given layout.
// Inert plans never register a schema, so they do not claim the unsuffixed
// name.
fn schema_name(
    entry_name: &str,
    inert: bool,
    fields: &[FieldDef],
    annotations: &[FieldAnnotation],
    used_names: &mut HashSet<String>,
) -> String {
    let base = format!("metrique:{entry_name}");
    if inert || used_names.insert(base.clone()) {
        return base;
    }

    // FNV-1a rather than `DefaultHasher`: the suffix lands in trace files,
    // so it must not vary across Rust releases. Variable-length inputs are
    // length-prefixed so adjacent components cannot alias.
    fn mix(hash: &mut u64, bytes: &[u8]) {
        for &b in bytes {
            *hash = (*hash ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
    }
    fn mix_str(hash: &mut u64, s: &str) {
        mix(hash, &(s.len() as u32).to_le_bytes());
        mix(hash, s.as_bytes());
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for field in fields {
        mix_str(&mut hash, field.name());
        mix(&mut hash, &[field.field_type() as u8]);
    }
    for ann in annotations {
        mix(&mut hash, &ann.field_index().to_le_bytes());
        mix_str(&mut hash, ann.key());
        mix_str(&mut hash, ann.value());
    }
    format!("{base}#{hash:016x}")
}

/// Resolve a descriptor field shape to its wire type and capture kind.
///
/// `Err` reasons are user-facing diagnostic strings.
fn resolve_shape(
    shape: FieldShape<'_>,
    interned: bool,
) -> Result<(FieldType, ValueKind), &'static str> {
    let (optional, inner) = match shape {
        FieldShape::Optional(inner) => (true, *inner.get()),
        other => (false, other),
    };

    let (base_type, kind) = match inner {
        FieldShape::Known(known) => {
            let (field_type, scalar) = resolve_known(known, interned)?;
            (field_type, ValueKind::Scalar(scalar))
        }
        FieldShape::List(elem) => {
            let elem = resolve_element(*elem.get(), interned)?;
            (FieldType::DynamicList, ValueKind::List { elem })
        }
        FieldShape::Flex { .. } => return Err("dynamic-key (Flex) fields are not supported"),
        FieldShape::Optional(_) => return Err("nested optional shapes are not supported"),
        FieldShape::Opaque => {
            return Err(
                "field shape is opaque (not statically known); use a #[metrics(value)] \
                 newtype over a known scalar, or a custom Value impl that overrides SHAPE",
            );
        }
        _ => return Err("unrecognized field shape"),
    };

    if interned && !kind_has_string_data(kind) {
        return Err("Interned flag on a field with no string data");
    }

    let field_type = if optional {
        // Every supported field type has an optional variant; this cannot
        // fail today, but the flush thread must not panic if that changes.
        FieldType::from_tag(base_type as u8 | FieldType::OPTIONAL_BIT)
            .ok_or("field type has no optional variant")?
    } else {
        base_type
    };
    Ok((field_type, kind))
}

fn kind_has_string_data(kind: ValueKind) -> bool {
    matches!(
        kind,
        ValueKind::Scalar(ScalarKind::Str { .. })
            | ValueKind::List {
                elem: ScalarKind::Str { .. }
            }
    )
}

/// Resolve a list element shape. Elements are self-describing on the wire,
/// so an `Interned` flag on the list interns its string elements.
///
/// Optional elements are allowed: an absent element writes no value and is
/// omitted from the encoded list.
fn resolve_element(shape: FieldShape<'_>, interned: bool) -> Result<ScalarKind, &'static str> {
    match shape {
        FieldShape::Known(known) => resolve_known(known, interned).map(|(_, scalar)| scalar),
        FieldShape::Optional(inner) => resolve_element(*inner.get(), interned),
        FieldShape::List(_) => Err("nested list fields are not supported"),
        FieldShape::Flex { .. } => Err("dynamic-key (Flex) list elements are not supported"),
        FieldShape::Opaque => Err("opaque list element shape"),
        _ => Err("unrecognized list element shape"),
    }
}

fn resolve_known(
    known: KnownShape,
    interned: bool,
) -> Result<(FieldType, ScalarKind), &'static str> {
    Ok(match known {
        KnownShape::Bool => (FieldType::Bool, ScalarKind::Bool),
        // All unsigned widths declare `Varint` in the schema. Runtime values
        // travel through `FieldValue`, whose only unsigned variant is
        // `Varint(u64)`, and each value encodes bytes per its own variant;
        // declaring a fixed-width type (`U8`/`U16`/`U32`) would promise raw
        // bytes the varint-encoded value does not deliver, corrupting decode.
        KnownShape::U8 | KnownShape::U16 | KnownShape::U32 | KnownShape::U64 => {
            (FieldType::Varint, ScalarKind::Uint)
        }
        KnownShape::I8 | KnownShape::I16 | KnownShape::I32 | KnownShape::I64 => {
            (FieldType::I64, ScalarKind::Int)
        }
        KnownShape::F32 | KnownShape::F64 => (FieldType::F64, ScalarKind::Float),
        KnownShape::String => {
            if interned {
                (FieldType::PooledString, ScalarKind::Str { interned: true })
            } else {
                (FieldType::String, ScalarKind::Str { interned: false })
            }
        }
        KnownShape::Bytes => return Err("byte-slice fields are not supported"),
        _ => return Err("unrecognized scalar shape"),
    })
}

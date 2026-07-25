//! First-use descriptor walk: turns a metrique entry's descriptor segments
//! into a cached encode plan (wire schema + per-field routing actions).

use std::collections::HashMap;

use dial9_trace_format::encoder::Schema;
use dial9_trace_format::schema::{FieldAnnotation, FieldDef, SchemaEntry};
use dial9_trace_format::types::FieldType;
use metrique_writer::core::descriptor::{AvailableDescriptors, FieldShape, KnownShape};

use super::context::field_names;
use super::{Context, Emit, Interned};

/// Annotation key carrying a field's unit. Matches the key the `TraceEvent`
/// derive emits, so viewers surface both through the same path.
pub(crate) const UNIT_ANNOTATION_KEY: &str = "unit";

/// Map a metrique unit to the shared unit vocabulary the `TraceEvent` derive
/// validates and the viewer formats (`ns`/`us`/`ms`/`s`/`bytes`/`count`).
/// Units outside it keep their CloudWatch name; the viewer renders those
/// values unformatted.
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

/// How the sink handles one `EntryWriter::value` callback, looked up by the
/// callback's field name (post-rename, post-prefix, matching
/// `FieldView::name_parts`).
///
/// Routing is by name rather than position: metrique emits `value` callbacks
/// in field declaration order (flatten children inline at their declared
/// position), while `descriptors()` yields the parent's own fields as one
/// segment followed by the child segments, so positions cannot be aligned
/// between the two.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FieldAction {
    /// Field is neither context nor payload (or its shape is unsupported):
    /// consume the callback and discard the value.
    Skip,
    /// One of [`Dial9Context`](super::Dial9Context)'s fields: route into the
    /// event header slots.
    Context(ContextRole),
    /// An `Emit`-tagged field: capture into payload slot `slot` as `kind`.
    Payload { slot: usize, kind: ValueKind },
}

/// Which event-header slot a context field feeds.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ContextRole {
    WorkerId,
    TaskId,
    MonotonicStart,
    MonotonicEnd,
}

/// The wire value a payload field resolves to. Derived from the descriptor's
/// [`FieldShape`] plus the `Interned` flag; drives both the schema
/// [`FieldType`] and how the runtime `Value` callbacks are interpreted.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ValueKind {
    /// Single unsigned observation → `Bool`.
    Bool,
    /// Single unsigned observation → `Varint`.
    Uint,
    /// Single unsigned observation → `I64`.
    Int,
    /// Single floating (or unsigned) observation → `F64`.
    Float,
    /// `string()` callback → `String` or pooled string.
    ///
    /// Also used for list-shaped fields: `#[metrics(flags(..))]` wraps
    /// every flagged value in `ForceFlag`, whose write-path wrapper does not
    /// forward `ValueWriter::values()`, so list data always arrives through
    /// the default comma-joined `string()` fallback. Typed `DynamicList`
    /// encoding (the wire support already exists) is blocked on metrique
    /// forwarding `values()` through `ForceFlag`.
    Str { interned: bool },
}

/// Cached per-entry-type encode plan. Built once per distinct descriptor-id
/// sequence, used for every subsequent entry of that type.
#[derive(Debug)]
pub(crate) struct Plan {
    /// Wire schema: implicit timestamp, the four header fields, then one
    /// field per supported `Emit`-tagged descriptor field.
    pub(crate) schema: Schema,
    /// Action per field, keyed by the full field name emitted at write time.
    /// Used for the first (resolving) walk of each entry type.
    pub(crate) actions: HashMap<String, FieldAction>,
    /// Positional dispatch recorded by the first successful walk: action per
    /// `value` callback, in write order. See `Dispatch` in `writer.rs`.
    pub(crate) positional: Option<Vec<FieldAction>>,
    /// Total `value` callbacks the descriptor declares. A mismatch at walk
    /// time means dynamic fields; the event must be dropped.
    pub(crate) expected_values: usize,
    /// Whether each payload slot's wire type is optional (may carry
    /// `FieldValue::None`). Indexed by payload slot.
    pub(crate) payload_optional: Vec<bool>,
    /// No `Emit` fields and no context fields: entries of this type are
    /// skipped without walking `Entry::write`.
    pub(crate) inert: bool,
    /// Duplicate post-rename field name; entries dropped (reported once at
    /// plan build).
    pub(crate) unusable: bool,
    /// Entry canonical name, for diagnostics.
    pub(crate) entry_name: String,
}

/// Header field names, in schema order, preceding the payload fields. The
/// event timestamp is implicit and not part of the schema field list.
/// `fill_values` in `writer.rs` must push values in exactly this order.
const HEADER_NAMES: [&str; 4] = ["worker_id", "task_id", "monotonic_ns_end", "wall_clock_ns"];

/// Number of header fields; payload slot `i` is schema field
/// `HEADER_FIELDS + i`.
pub(crate) const HEADER_FIELDS: usize = HEADER_NAMES.len();

fn header_field_defs() -> Vec<FieldDef> {
    vec![
        FieldDef::new(HEADER_NAMES[0], FieldType::Varint),
        FieldDef::new(HEADER_NAMES[1], FieldType::OptionalVarint),
        FieldDef::new(HEADER_NAMES[2], FieldType::OptionalVarint),
        FieldDef::new(HEADER_NAMES[3], FieldType::OptionalVarint),
    ]
}

/// Walk the descriptor segments once and build the encode plan.
///
/// Diagnostics for structurally unsupported fields fire here, once per
/// distinct descriptor-id sequence (the plan is cached by the caller).
pub(crate) fn build_plan(
    descs: &AvailableDescriptors<'_>,
    used_names: &mut HashMap<String, u32>,
) -> Plan {
    let entry_name = descs
        .iter()
        .next()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|| "<unnamed>".to_owned());

    let mut actions: HashMap<String, FieldAction> = HashMap::new();
    let mut expected_values = 0usize;
    let mut duplicate_name: Option<String> = None;
    let mut fields = header_field_defs();
    let mut annotations = Vec::new();
    let mut payload_optional = Vec::new();
    let mut has_context = false;
    let mut has_emit = false;
    let mut roles_seen = [false; 4];

    let mut insert = |name: String, action: FieldAction, duplicate: &mut Option<String>| {
        use std::collections::hash_map::Entry;
        match actions.entry(name) {
            Entry::Occupied(entry) => {
                if duplicate.is_none() {
                    *duplicate = Some(entry.key().clone());
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(action);
            }
        }
    };

    for seg in descs.iter() {
        for field in seg.fields() {
            expected_values += 1;
            let full_name: String = field.name_parts().collect();

            if field.flags().any(|f| f.is::<Context>()) {
                let role = match field_names::canonicalize(field.base_name()).as_str() {
                    field_names::WORKER_ID => Some(ContextRole::WorkerId),
                    field_names::TASK_ID => Some(ContextRole::TaskId),
                    field_names::MONOTONIC_NS_START => Some(ContextRole::MonotonicStart),
                    field_names::MONOTONIC_NS_END => Some(ContextRole::MonotonicEnd),
                    _ => None,
                };
                match role {
                    Some(role) => {
                        let seen = &mut roles_seen[role as usize];
                        if *seen {
                            // Two Dial9Contexts flattened into one entry;
                            // only the first routes to the header.
                            tracing::warn!(
                                entry = %entry_name,
                                field = %full_name,
                                "duplicate dial9 context field; ignoring"
                            );
                            insert(full_name, FieldAction::Skip, &mut duplicate_name);
                        } else {
                            *seen = true;
                            has_context = true;
                            insert(full_name, FieldAction::Context(role), &mut duplicate_name);
                        }
                    }
                    None => {
                        // A foreign field carrying our internal flag; nothing
                        // sensible to do with it.
                        tracing::warn!(
                            entry = %entry_name,
                            field = %field.base_name(),
                            "unrecognized dial9 context field; skipping"
                        );
                        insert(full_name, FieldAction::Skip, &mut duplicate_name);
                    }
                }
                continue;
            }

            if !field.flags().any(|f| f.is::<Emit>()) {
                insert(full_name, FieldAction::Skip, &mut duplicate_name);
                continue;
            }
            has_emit = true;

            if HEADER_NAMES.contains(&full_name.as_str()) {
                tracing::error!(
                    entry = %entry_name,
                    field = %full_name,
                    "metrique field name collides with a dial9 header field; skipping field"
                );
                insert(full_name, FieldAction::Skip, &mut duplicate_name);
                continue;
            }

            let interned = field.flags().any(|f| f.is::<Interned>());
            match resolve_shape(&field.shape(), interned) {
                Ok((field_type, kind)) => {
                    if let Some(unit) = field.unit() {
                        annotations.push(FieldAnnotation::new(
                            fields.len() as u16,
                            UNIT_ANNOTATION_KEY,
                            unit_annotation_value(unit),
                        ));
                    }
                    let slot = payload_optional.len();
                    payload_optional.push(field_type.is_optional());
                    fields.push(FieldDef::new(full_name.clone(), field_type));
                    insert(
                        full_name,
                        FieldAction::Payload { slot, kind },
                        &mut duplicate_name,
                    );
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
                    insert(full_name, FieldAction::Skip, &mut duplicate_name);
                }
            }
        }
    }

    let unusable = if let Some(name) = duplicate_name {
        tracing::error!(
            entry = %entry_name,
            field = %name,
            "metrique entry declares the same field name twice; dial9 routes values by \
             name and cannot disambiguate, so entries of this type are dropped"
        );
        true
    } else {
        false
    };

    if has_emit && !has_context {
        tracing::warn!(
            entry = %entry_name,
            "metrique entry has dial9 Emit fields but no Dial9Context; events will carry \
             an unknown worker and a flush-thread timestamp. Flatten a Dial9Context into \
             the entry (a #[cfg]-gated or forgotten context field is the usual cause)"
        );
    }

    let inert = !has_emit && !has_context;

    // Distinct descriptor sequences occasionally share a canonical name
    // (e.g. the same child struct flattened under different prefixes).
    // Disambiguate so each plan registers its own wire schema. The `#N`
    // suffix follows first-use order, so it is not stable across runs.
    let base = format!("metrique:{entry_name}");
    let n = used_names.entry(base.clone()).or_insert(0);
    *n += 1;
    let schema_name = if *n == 1 { base } else { format!("{base}#{n}") };

    let schema = Schema::from_entry(SchemaEntry::with_annotations(
        schema_name,
        true,
        fields,
        annotations,
    ));

    Plan {
        schema,
        actions,
        positional: None,
        expected_values,
        payload_optional,
        inert,
        unusable,
        entry_name,
    }
}

/// Resolve a descriptor field shape to its wire type and capture kind.
///
/// `Err` reasons are user-facing diagnostic strings.
fn resolve_shape(
    shape: &FieldShape<'_>,
    interned: bool,
) -> Result<(FieldType, ValueKind), &'static str> {
    let (optional, inner) = match shape {
        FieldShape::Optional(inner) => (true, *inner.get()),
        other => (false, *other),
    };

    let (base_type, kind) = match inner {
        FieldShape::Known(known) => resolve_known(known, interned)?,
        FieldShape::List(elem) => {
            // Lists are carried comma-joined; see `ValueKind::Str`. Validate
            // the element shape so garbage text is rejected up front.
            validate_element(elem.get())?;
            if interned {
                (FieldType::PooledString, ValueKind::Str { interned: true })
            } else {
                (FieldType::String, ValueKind::Str { interned: false })
            }
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

    if interned && !matches!(kind, ValueKind::Str { .. }) {
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

/// Validate that a list element shape is encodable (as part of the list's
/// comma-joined string carrier). Rejects the shapes that would silently
/// produce garbage text.
fn validate_element(shape: &FieldShape<'_>) -> Result<(), &'static str> {
    match shape {
        FieldShape::Known(KnownShape::Bytes) => Err("byte-slice list elements are not supported"),
        FieldShape::Known(_) => Ok(()),
        FieldShape::Optional(inner) => validate_element(inner.get()),
        FieldShape::List(elem) => validate_element(elem.get()),
        FieldShape::Flex { .. } => Err("dynamic-key (Flex) list elements are not supported"),
        FieldShape::Opaque => Err("opaque list element shape"),
        _ => Err("unrecognized list element shape"),
    }
}

fn resolve_known(
    known: KnownShape,
    interned: bool,
) -> Result<(FieldType, ValueKind), &'static str> {
    Ok(match known {
        KnownShape::Bool => (FieldType::Bool, ValueKind::Bool),
        // All unsigned widths encode as varints: `FieldValue`'s scalar
        // integer carrier is `Varint`, and the fixed-width wire types
        // (`U8`/`U16`/`U32`) would not match its encoding.
        KnownShape::U8 | KnownShape::U16 | KnownShape::U32 | KnownShape::U64 => {
            (FieldType::Varint, ValueKind::Uint)
        }
        KnownShape::I8 | KnownShape::I16 | KnownShape::I32 | KnownShape::I64 => {
            (FieldType::I64, ValueKind::Int)
        }
        KnownShape::F32 | KnownShape::F64 => (FieldType::F64, ValueKind::Float),
        KnownShape::String => {
            if interned {
                (FieldType::PooledString, ValueKind::Str { interned: true })
            } else {
                (FieldType::String, ValueKind::Str { interned: false })
            }
        }
        KnownShape::Bytes => return Err("byte-slice fields are not supported"),
        _ => return Err("unrecognized scalar shape"),
    })
}

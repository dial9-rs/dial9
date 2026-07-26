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

/// Map a metrique unit to the vocabulary the viewer formats
/// (`ns`/`us`/`ms`/`s`/`bytes`), plus `count` (rendered plain, kept by
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

/// How the sink handles one `EntryWriter::value` callback, looked up by the
/// callback's field name (post-rename, post-prefix, matching
/// `FieldView::name_parts`).
///
/// Routing is by name rather than position because metrique's write order
/// differs from its descriptor segment order for flattened entries; see
/// "Value routing" in `docs/design/metrique-integration.md`.
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
    /// Also used for list-shaped fields, which arrive comma-joined through
    /// the default `string()` fallback (design doc, delta 2: `ForceFlag`
    /// drops the structured `values()` callback).
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
    /// Duplicate post-rename field name with conflicting routing; entries
    /// dropped (reported once at plan build).
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
    let mut roles_seen = [false; ContextRole::COUNT];

    // Name-keyed routing needs unique names: when two fields share a
    // post-rename name, every callback with that name hits the same action,
    // so unless both are Skip the values would be mis-attributed. Skip+Skip
    // collisions are harmless.
    fn insert(
        actions: &mut HashMap<String, FieldAction>,
        name: String,
        action: FieldAction,
        duplicate: &mut Option<String>,
    ) {
        use std::collections::hash_map::Entry;
        match actions.entry(name) {
            Entry::Occupied(entry) => {
                let harmless =
                    matches!(entry.get(), FieldAction::Skip) && matches!(action, FieldAction::Skip);
                if !harmless && duplicate.is_none() {
                    *duplicate = Some(entry.key().clone());
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(action);
            }
        }
    }

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
                        let seen = &mut roles_seen[role.index()];
                        if *seen {
                            // A second Dial9Context in the same entry. With
                            // distinct (prefixed) names it is skipped; with
                            // identical names the insert below records a
                            // routing conflict and the type is dropped.
                            if !actions.contains_key(full_name.as_str()) {
                                tracing::warn!(
                                    entry = %entry_name,
                                    field = %full_name,
                                    "duplicate dial9 context field; ignoring"
                                );
                            }
                            insert(
                                &mut actions,
                                full_name,
                                FieldAction::Skip,
                                &mut duplicate_name,
                            );
                        } else {
                            *seen = true;
                            has_context = true;
                            insert(
                                &mut actions,
                                full_name,
                                FieldAction::Context(role),
                                &mut duplicate_name,
                            );
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
                        insert(
                            &mut actions,
                            full_name,
                            FieldAction::Skip,
                            &mut duplicate_name,
                        );
                    }
                }
                continue;
            }

            if !field.flags().any(|f| f.is::<Emit>()) {
                insert(
                    &mut actions,
                    full_name,
                    FieldAction::Skip,
                    &mut duplicate_name,
                );
                continue;
            }
            has_emit = true;

            if HEADER_NAMES.contains(&full_name.as_str()) {
                tracing::error!(
                    entry = %entry_name,
                    field = %full_name,
                    "metrique field name collides with a dial9 header field; skipping field"
                );
                insert(
                    &mut actions,
                    full_name,
                    FieldAction::Skip,
                    &mut duplicate_name,
                );
                continue;
            }

            let interned = field.flags().any(|f| f.is::<Interned>());
            match resolve_shape(&field.shape(), interned) {
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
                    let slot = payload_optional.len();
                    payload_optional.push(field_type.is_optional());
                    fields.push(FieldDef::new(full_name.clone(), field_type));
                    insert(
                        &mut actions,
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
                    insert(
                        &mut actions,
                        full_name,
                        FieldAction::Skip,
                        &mut duplicate_name,
                    );
                }
            }
        }
    }

    let unusable = if let Some(name) = duplicate_name {
        tracing::error!(
            entry = %entry_name,
            field = %name,
            "metrique entry declares the same field name twice with conflicting dial9 \
             routing; entries of this type are dropped"
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

    // Distinct descriptor sequences can share a canonical name (the same
    // child struct flattened under different prefixes, an Option-flattened
    // child present vs absent, same-named types in different modules). The
    // first arrival keeps the bare name; later ones get a suffix hashed
    // from the schema layout, so a given layout maps to the same name in
    // every run of the same binary, and identical layouts safely share a
    // wire schema. Inert and unusable plans never register a schema, so
    // they do not claim the unsuffixed name.
    let base = format!("metrique:{entry_name}");
    let schema_name = if inert || unusable {
        base
    } else {
        let n = used_names.entry(base.clone()).or_insert(0);
        *n += 1;
        if *n == 1 {
            base
        } else {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for field in &fields {
                field.name().hash(&mut hasher);
                (field.field_type() as u8).hash(&mut hasher);
            }
            for ann in &annotations {
                ann.field_index().hash(&mut hasher);
                ann.key().hash(&mut hasher);
                ann.value().hash(&mut hasher);
            }
            format!("{base}#{:016x}", hasher.finish())
        }
    };

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

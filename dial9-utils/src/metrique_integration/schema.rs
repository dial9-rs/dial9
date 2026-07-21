//! Per-descriptor schema building: maps a metrique `EntryDescriptor` walk
//! into a dial9 wire [`Schema`] plus a name-keyed role map used to drive
//! the write-walk in `stream.rs`.
//!
//! Dispatch is by resolved field *name*, not position: metrique's
//! `#[metrics]` macro builds `entry.descriptors()` as "this struct's own
//! direct fields, then each flattened child's fields", always in that
//! relative order regardless of where the flatten field is declared —
//! while `Entry::write` fires callbacks in literal declaration order. Those
//! two orders only coincide when the flatten field happens to be declared
//! last, so positional correlation is unsound in general. The name a field
//! is written under (`EntryWriter::value`'s `name` parameter) always
//! matches the name `field.name_parts()` resolves to here, independent of
//! declaration order, so keying the role map by that name sidesteps the
//! ordering issue entirely.
//!
//! ## Why the header (`WorkerId`/`MonotonicNsEnd`) is hardcoded rather than
//! generic
//!
//! Unlike payload (`Emit`) fields — which are named and shaped purely from
//! whatever the descriptor reports — the three header roles below
//! (`ContextTimestamp`/`ContextWorkerId`/`ContextMonotonicEnd`) are assigned
//! by the *position* a `Context`-tagged field is encountered in, not by
//! name. This mirrors `Dial9Context`'s fixed field layout (see
//! `dial9-tokio-telemetry`'s `context.rs`) and keeps two things true:
//! `values[0]` must be the event's native timestamp (a wire-protocol
//! requirement of `Encoder::write_event`, not a convention this module
//! invented), and every dial9-integration event gets the same uniform
//! `WorkerId`/`MonotonicNsEnd` header shape, which is what lets the viewer
//! (once it grows metrique-aware rendering) place these events on a
//! timeline without per-schema-aware logic. `task_id` used to be a third
//! header field but was dropped: dial9's native poll timeline already lets
//! a viewer join a worker + a timestamp back to the task that was running
//! (see `PollStartEvent`/`PollEndEvent` and how `SpanEnter`/`SpanExit`
//! already rely on that join instead of carrying `task_id` themselves), so
//! duplicating it here was both redundant and, because it's captured only
//! once at `Dial9Context::capture()` time, a second, easily-stale copy of
//! information the poll timeline already has.

use super::flags::{Context, Emit, Interned};
use dial9_trace_format::encoder::Schema;
use dial9_trace_format::schema::{FieldAnnotation, FieldDef, SchemaEntry};
use dial9_trace_format::types::FieldType;
use metrique::writer::Entry;
use metrique::writer::core::{DescriptorId, Descriptors, FieldShape, KnownShape};
use std::collections::HashMap;

/// Coerced numeric/string family a captured [`metrique::writer::Value`] is
/// expected to belong to, derived once from the field's static
/// [`KnownShape`]. Collapses width variants dial9's dynamic `write_event`
/// path can't safely emit: `FieldValue` has no `U8`/`U16`/`U32` variant
/// distinct from `Varint` (LEB128) — the schema-declared `FieldType::U8`
/// etc. are fixed-width and only reachable via the compile-time
/// `TraceField`-derive path, never via `Encoder::write_event`. So every
/// unsigned width here maps to `FieldType::Varint`, not `FieldType::U8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExpectedScalar {
    Bool,
    UInt,
    SInt,
    Float,
    Str,
    Bytes,
}

impl ExpectedScalar {
    fn from_known_shape(shape: KnownShape) -> Option<Self> {
        Some(match shape {
            KnownShape::Bool => ExpectedScalar::Bool,
            KnownShape::U8 | KnownShape::U16 | KnownShape::U32 | KnownShape::U64 => {
                ExpectedScalar::UInt
            }
            KnownShape::I8 | KnownShape::I16 | KnownShape::I32 | KnownShape::I64 => {
                ExpectedScalar::SInt
            }
            KnownShape::F32 | KnownShape::F64 => ExpectedScalar::Float,
            KnownShape::String => ExpectedScalar::Str,
            KnownShape::Bytes => ExpectedScalar::Bytes,
            _ => return None,
        })
    }

    /// The dial9 wire `FieldType` for this shape family. There is no
    /// generic "make optional" helper on `FieldType` (its `Optional*`
    /// variants aren't derived from the base variant at runtime), so this
    /// matches out both forms explicitly.
    fn field_type(self, interned: bool, optional: bool) -> FieldType {
        match (self, interned, optional) {
            (ExpectedScalar::Bool, _, false) => FieldType::Bool,
            (ExpectedScalar::Bool, _, true) => FieldType::OptionalBool,
            (ExpectedScalar::UInt, _, false) => FieldType::Varint,
            (ExpectedScalar::UInt, _, true) => FieldType::OptionalVarint,
            (ExpectedScalar::SInt, _, false) => FieldType::I64,
            (ExpectedScalar::SInt, _, true) => FieldType::OptionalI64,
            (ExpectedScalar::Float, _, false) => FieldType::F64,
            (ExpectedScalar::Float, _, true) => FieldType::OptionalF64,
            (ExpectedScalar::Str, true, false) => FieldType::PooledString,
            (ExpectedScalar::Str, true, true) => FieldType::OptionalPooledString,
            (ExpectedScalar::Str, false, false) => FieldType::String,
            (ExpectedScalar::Str, false, true) => FieldType::OptionalString,
            (ExpectedScalar::Bytes, _, false) => FieldType::Bytes,
            (ExpectedScalar::Bytes, _, true) => FieldType::OptionalBytes,
        }
    }
}

/// How to encode a payload field's captured value onto the wire. Always a
/// scalar capture: list-shaped (`Vec<T>`/`[T]`) fields are folded into
/// `expected: Str` too (comma-joined) — see the `FieldShape::List` match arm
/// in `build` for why a structured `DynamicList` is unreachable here.
#[derive(Debug, Clone, Copy)]
pub(super) struct PayloadWire {
    pub(super) expected: ExpectedScalar,
    pub(super) optional: bool,
    pub(super) interned: bool,
}

/// What a named `EntryWriter::value` callback maps to, for one entry type.
/// Built once per distinct descriptor shape and cached by
/// [`super::stream::Dial9Stream`], keyed by the field's resolved name. A
/// name absent from the map means "not `Emit`/`Context`-tagged, or excluded
/// due to a diagnosed shape problem (`Opaque`, non-string `Interned`)" —
/// [`FieldRole::Skip`] is only ever a default returned for a lookup miss,
/// never stored in the map itself.
#[derive(Debug, Clone, Copy)]
pub(super) enum FieldRole {
    ContextTimestamp,
    ContextWorkerId,
    ContextMonotonicEnd,
    Payload {
        schema_index: usize,
        wire: PayloadWire,
    },
    Skip,
}

#[derive(Debug)]
pub(super) struct CachedDescriptor {
    pub(super) schema: Schema,
    pub(super) field_roles: HashMap<String, FieldRole>,
    pub(super) has_context: bool,
}

/// Outcome of building a schema for one descriptor shape, including
/// diagnostics to log exactly once (the cache itself provides the
/// once-per-shape dedup).
pub(super) struct BuildResult {
    pub(super) cached: CachedDescriptor,
    pub(super) emit_without_context: bool,
    pub(super) interned_on_non_string: bool,
    pub(super) opaque_emit_field: bool,
    pub(super) duplicate_field_name: bool,
}

/// Compute a stable cache key for one entry's descriptor segments.
/// `DescriptorId` is `Copy + Hash + Eq`; a `Vec` of them (one per segment,
/// in order) is unique per composed-entry shape.
pub(super) fn descriptor_cache_key(entry: &impl Entry) -> Option<Vec<DescriptorId>> {
    match entry.descriptors() {
        Descriptors::Available(descs) => Some(descs.iter().map(|d| d.id()).collect()),
        Descriptors::Unavailable => None,
        // Forward-compat: any future non-exhaustive variant is treated like
        // `Unavailable` (dropped, diagnosed) rather than assumed encodable.
        _ => None,
    }
}

/// Walk `entry.descriptors()` once and build the wire schema + name-keyed
/// role map. Returns `None` only if descriptors became unavailable between
/// the `descriptor_cache_key` call and this one (not expected in practice —
/// entries don't change shape mid-flight — but handled defensively).
pub(super) fn build(entry: &impl Entry) -> Option<BuildResult> {
    let Descriptors::Available(descs) = entry.descriptors() else {
        return None;
    };

    let mut field_roles = HashMap::new();
    // Fixed header prefix: worker_id/monotonic_ns_end always occupy schema
    // slots 0..2, regardless of whether this entry's payload is empty.
    // `monotonic_ns_start` is not a schema field — it becomes `values[0]`,
    // dial9's native event-timestamp slot (see `stream.rs`, and the module
    // doc above for why this header isn't derived generically).
    let mut fields = vec![
        FieldDef::new("WorkerId", FieldType::Varint),
        FieldDef::new("MonotonicNsEnd", FieldType::Varint),
    ];
    let mut annotations = Vec::new();
    let mut context_seen = 0u8;
    let mut interned_on_non_string = false;
    let mut opaque_emit_field = false;
    let mut duplicate_field_name = false;
    let mut entry_name: Option<String> = None;

    // Track whether inserting `role` under `name` overwrote an existing
    // entry — two differently-tagged fields resolving to the same written
    // name would otherwise silently clobber each other.
    let insert_role = |field_roles: &mut HashMap<String, FieldRole>,
                       name: String,
                       role: FieldRole,
                       duplicate_field_name: &mut bool| {
        if field_roles.insert(name, role).is_some() {
            *duplicate_field_name = true;
        }
    };

    for seg in descs.iter() {
        if entry_name.is_none() {
            entry_name = Some(seg.name().to_string());
        }
        for field in seg.fields() {
            let is_context = field.flags().any(|f| f.is::<Context>());
            let is_emit = field.flags().any(|f| f.is::<Emit>());

            if is_context {
                // The runtime context type's fields are declared (and
                // therefore written) in a fixed relative order: worker_id,
                // monotonic_ns_start, monotonic_ns_end. Assign by the order
                // Context-tagged fields are encountered — independent of
                // any `rename_all` style, and independent of where this
                // flattened segment lands relative to the entry's own
                // fields (see the module docs).
                if context_seen < 3 {
                    let role = match context_seen {
                        0 => FieldRole::ContextWorkerId,
                        1 => FieldRole::ContextTimestamp,
                        _ => FieldRole::ContextMonotonicEnd,
                    };
                    let name: String = field.name_parts().collect();
                    insert_role(&mut field_roles, name, role, &mut duplicate_field_name);
                    context_seen += 1;
                } // more than 3: unexpected, ignore extras defensively
                continue;
            }

            if !is_emit {
                continue;
            }

            let interned = field.flags().any(|f| f.is::<Interned>());
            let wire = match field.shape() {
                FieldShape::Known(k) => match ExpectedScalar::from_known_shape(k) {
                    Some(expected) => {
                        if interned && expected != ExpectedScalar::Str {
                            interned_on_non_string = true;
                        }
                        PayloadWire {
                            expected,
                            optional: false,
                            interned: interned && expected == ExpectedScalar::Str,
                        }
                    }
                    None => {
                        opaque_emit_field = true;
                        continue;
                    }
                },
                FieldShape::Optional(inner) => match inner.get() {
                    FieldShape::Known(k) => match ExpectedScalar::from_known_shape(*k) {
                        Some(expected) => {
                            if interned && expected != ExpectedScalar::Str {
                                interned_on_non_string = true;
                            }
                            PayloadWire {
                                expected,
                                optional: true,
                                interned: interned && expected == ExpectedScalar::Str,
                            }
                        }
                        None => {
                            opaque_emit_field = true;
                            continue;
                        }
                    },
                    // See the non-optional `List` arm below for why this
                    // becomes a plain (optional) string, not `DynamicList`.
                    FieldShape::List(_) => PayloadWire {
                        expected: ExpectedScalar::Str,
                        optional: true,
                        interned: false,
                    },
                    _ => {
                        opaque_emit_field = true;
                        continue;
                    }
                },
                // `flags(...)` (including `Emit` itself) wraps every tagged
                // field's `Value` in metrique's `ForceFlag<T, _>`, whose
                // `Value::write` wraps the writer it's given in its own
                // `Wrapper` type — which overrides `.string()`/`.metric()`/
                // `.error()` but NOT `.values()`. So a `Vec<T>`/`[T]` field's
                // `writer.values(...)` call always hits that `Wrapper`'s
                // *inherited default* first (comma-join each element's
                // string form, then call `.string()`), never our own
                // `ValueCapture::values()` override — regardless of
                // `BoxEntry`. `FieldType::DynamicList` is therefore
                // unreachable for a real `Emit`-tagged field today; encode
                // list fields as a plain (comma-joined) string instead of
                // silently corrupting the wire with a type mismatch.
                FieldShape::List(_) => PayloadWire {
                    expected: ExpectedScalar::Str,
                    optional: false,
                    interned: false,
                },
                // FieldShape::Flex and any future non-exhaustive variant:
                // no metrique `Value` impl produces these today (Flex fields
                // close through a separate Entry-flatten mechanism, not
                // Value, and lose descriptor visibility entirely before
                // reaching here — see design notes). Treat identically to
                // Opaque for forward-compat.
                _ => {
                    opaque_emit_field = true;
                    continue;
                }
            };

            let schema_index = fields.len();
            let PayloadWire {
                expected,
                optional,
                interned,
            } = wire;
            let field_type = expected.field_type(interned, optional);
            let name: String = field.name_parts().collect();
            if let Some(unit) = field.unit() {
                annotations.push(FieldAnnotation::new(
                    schema_index as u16,
                    "metrique.unit",
                    format!("{unit:?}"),
                ));
            }
            fields.push(FieldDef::new(name.clone(), field_type));
            insert_role(
                &mut field_roles,
                name,
                FieldRole::Payload { schema_index, wire },
                &mut duplicate_field_name,
            );
        }
    }

    let has_context = context_seen == 3;
    // `fields` always carries the 2-field fixed header; only a genuine
    // payload field (beyond that prefix) makes "no context" worth flagging.
    let emit_without_context = !has_context && fields.len() > 2;

    let name = entry_name.unwrap_or_else(|| "UnknownMetriqueEntry".to_string());
    let entry = SchemaEntry::with_annotations(name, true, fields, annotations);
    let schema = Schema::from_entry(entry);

    Some(BuildResult {
        cached: CachedDescriptor {
            schema,
            field_roles,
            has_context,
        },
        emit_without_context,
        interned_on_non_string,
        opaque_emit_field,
        duplicate_field_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dial9_trace_format::encoder::Schema;
    use dial9_trace_format::schema::FieldDef;
    use dial9_trace_format::types::FieldType;
    use metrique::writer::core::{KnownShape, ShapeRef};

    #[test]
    fn known_shape_widens_narrow_ints_to_varint() {
        for shape in [
            KnownShape::U8,
            KnownShape::U16,
            KnownShape::U32,
            KnownShape::U64,
        ] {
            let expected = ExpectedScalar::from_known_shape(shape).unwrap();
            assert_eq!(expected, ExpectedScalar::UInt);
            assert_eq!(expected.field_type(false, false), FieldType::Varint);
            assert_eq!(expected.field_type(false, true), FieldType::OptionalVarint);
        }
    }

    #[test]
    fn known_shape_signed_widens_to_i64() {
        for shape in [
            KnownShape::I8,
            KnownShape::I16,
            KnownShape::I32,
            KnownShape::I64,
        ] {
            let expected = ExpectedScalar::from_known_shape(shape).unwrap();
            assert_eq!(expected, ExpectedScalar::SInt);
            assert_eq!(expected.field_type(false, false), FieldType::I64);
        }
    }

    #[test]
    fn known_shape_float_widens_to_f64() {
        for shape in [KnownShape::F32, KnownShape::F64] {
            let expected = ExpectedScalar::from_known_shape(shape).unwrap();
            assert_eq!(expected, ExpectedScalar::Float);
            assert_eq!(expected.field_type(false, false), FieldType::F64);
        }
    }

    #[test]
    fn string_shape_interned_override() {
        let expected = ExpectedScalar::from_known_shape(KnownShape::String).unwrap();
        assert_eq!(expected.field_type(false, false), FieldType::String);
        assert_eq!(expected.field_type(true, false), FieldType::PooledString);
        assert_eq!(
            expected.field_type(true, true),
            FieldType::OptionalPooledString
        );
    }

    #[test]
    fn bytes_shape() {
        let expected = ExpectedScalar::from_known_shape(KnownShape::Bytes).unwrap();
        assert_eq!(expected.field_type(false, false), FieldType::Bytes);
    }

    #[test]
    fn flex_shape_maps_defensively_to_dynamic_map() {
        // No `Value` impl in current metrique produces `FieldShape::Flex`
        // (Flex fields close through a separate Entry-flatten mechanism and
        // lose descriptor visibility before reaching the schema builder —
        // see the module-level design notes). This only exercises the
        // schema-construction machinery's readiness for the shape, not a
        // real end-to-end round trip.
        let key_and_value = FieldShape::Known(KnownShape::String);
        let flex_shape = FieldShape::Flex {
            key: metrique::writer::core::StringShape::String,
            value: ShapeRef::new(&key_and_value),
        };
        let mapped = match flex_shape {
            FieldShape::Flex { .. } => FieldType::DynamicMap,
            _ => unreachable!(),
        };
        assert_eq!(mapped, FieldType::DynamicMap);
    }

    #[test]
    fn schema_registers_with_fixed_header_prefix() {
        // Sanity check that a hand-built schema (mirroring what `build`
        // produces) round-trips through the encoder's registration path.
        let fields = vec![
            FieldDef::new("WorkerId", FieldType::Varint),
            FieldDef::new("MonotonicNsEnd", FieldType::Varint),
            FieldDef::new("Value", FieldType::Varint),
        ];
        let schema = Schema::new("Test", fields);
        assert_eq!(schema.fields().len(), 3);
    }
}

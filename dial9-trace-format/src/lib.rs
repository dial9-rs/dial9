//! # dial9-trace-format
//!
//! A compact binary trace format for recording timestamped events with
//! schema-driven encoding. Events are described by schemas (registered at
//! write time) and encoded with delta-compressed timestamps, LEB128 varints,
//! and an interned string pool.
//!
//! ## Crate layout
//!
//! - [`encoder`] — high-level [`Encoder`](encoder::Encoder) for writing traces
//! - [`decoder`] — streaming [`Decoder`](decoder::Decoder) for reading traces
//! - [`codec`]   — wire-format types ([`WireTypeId`](codec::WireTypeId),
//!   [`PoolEntry`](codec::PoolEntry)) that appear in decoded frames
//! - [`schema`]  — [`SchemaEntry`] and
//!   [`FieldDef`] describing event layouts
//! - [`types`]   — field value types, the [`TraceField`]
//!   trait, and the [`EventEncoder`] used by derived code

pub mod codec;
#[cfg(feature = "serde-deserialize")]
pub mod de;
pub mod decoder;
pub mod encoder;
pub(crate) mod leb128;
pub mod schema;
pub mod types;

#[cfg(feature = "serde-deserialize")]
pub use de::DeserError;
pub use dial9_trace_format_derive::TraceEvent;
pub use types::DynamicListRef;
pub use types::DynamicMapRef;
pub use types::EventEncoder;
pub use types::FieldValue;
pub use types::InternedStackFrames;
pub use types::InternedString;
pub use types::StackFrames;
pub use types::TraceField;

use schema::{FieldDef, SchemaEntry};
use types::FieldValueRef;

/// Slots `1..STATIC_WIRE_ID_LIMIT` double as wire IDs and take the inline fast
/// path in the encoder. Only `#[traceevent(wire_slot)]` types claim a slot, so
/// this bounds how many event types share the fast range, dynamic registration
/// starts here.
pub const STATIC_WIRE_ID_LIMIT: u16 = 256;

/// Global counter for assigning dense type slots to opted-in `TraceEvent`
/// impls. Slot 0 is reserved as "unset".
#[doc(hidden)]
pub static __NEXT_TYPE_SLOT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(1);

/// Trait implemented by `#[derive(TraceEvent)]` for compile-time event types.
pub trait TraceEvent {
    /// Decoded form of this event, potentially borrowing from the input buffer.
    type Ref<'a>;

    /// Per-type wire-ID slot. Default 0 means no slot (dynamic path);
    /// `#[traceevent(wire_slot)]` overrides it to claim a fast-path slot.
    fn type_slot() -> u16 {
        0
    }

    /// The event type name (used in schema registration).
    fn event_name() -> &'static str;
    /// Field definitions for schema registration.
    /// When `has_timestamp()` is true, the timestamp is NOT included here —
    /// it is encoded in the event frame header.
    fn field_defs() -> Vec<FieldDef>;
    /// Whether this event type carries a packed timestamp in the event header.
    fn has_timestamp() -> bool {
        true
    }
    /// Return the event's timestamp in nanoseconds.
    fn timestamp(&self) -> u64;
    /// Encode this event's non-timestamp fields into the encoder.
    fn encode_fields<W: std::io::Write>(
        &self,
        enc: &mut types::EventEncoder<'_, W>,
    ) -> std::io::Result<()>;
    /// Decode from field values using field definitions for name resolution.
    /// `timestamp_ns` is the absolute timestamp from the event header (if present).
    fn decode<'a>(
        timestamp_ns: Option<u64>,
        fields: &[FieldValueRef<'a>],
        field_defs: &[FieldDef],
    ) -> Option<Self::Ref<'a>>;

    /// Build a SchemaEntry for this event type.
    fn schema_entry() -> SchemaEntry {
        SchemaEntry {
            name: Self::event_name().to_string(),
            has_timestamp: Self::has_timestamp(),
            fields: Self::field_defs(),
            annotations: Vec::new(),
        }
    }
}

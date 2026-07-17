//! Span decoding adapters and shared resolution machinery.
//!
//! Modern and legacy wire formats normalize into the same exact-key interval
//! pairer and [`span_builder::SpanCandidate`] finalizer. Format-specific modules
//! supply evidence; accounting and output construction live once.

pub(crate) mod interval_pairing;
pub(crate) mod legacy;
pub(crate) mod span_builder;

// Keep child adapters focused on span semantics while exposing only decode-
// internal dependencies through this namespace.
pub(crate) use super::clock;
pub(crate) use super::events::{
    LegacySpanCloseEvent, LegacySpanEnterEvent, LegacySpanExitEvent, LegacySpanSchemaInfo,
    parse_legacy_span_schema_name,
};
pub(crate) use super::polls;
pub(crate) use super::types::ResolvedSpan;

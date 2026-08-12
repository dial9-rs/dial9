//! Span decoding adapters and shared resolution machinery.
//!
//! Tracing and single-event wire formats normalize into the same
//! [`span_builder::SpanCandidate`] finalizer. Format-specific modules supply
//! interval evidence; accounting and output construction live once.

pub(crate) mod interval_pairing;
pub(crate) mod legacy;
pub(crate) mod single_event;
pub(crate) mod span_builder;

// Keep child adapters focused on span semantics while exposing only decode-
// internal dependencies through this namespace.
pub(crate) use super::clock;
pub(crate) use super::events::{
    LegacySpanCloseEvent, LegacySpanEnterEvent, LegacySpanExitEvent, LegacySpanSchemaInfo,
    SingleEventSpanEvent, parse_legacy_span_schema_name,
};
pub(crate) use super::polls;
pub(crate) use super::types::ResolvedSpan;

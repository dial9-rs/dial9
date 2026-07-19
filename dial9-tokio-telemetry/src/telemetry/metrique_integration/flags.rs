//! Field-tag types used to opt metrique fields into the dial9 trace.
//!
//! Applied via `#[metrics(flags(Emit))]` / `#[metrics(default_flags(Emit))]`
//! (and `flags(skip(Emit))` to override a struct-level default on one
//! field). See the `metrique_integration` module docs for usage.

use metrique::writer::value::{FlagConstructor, MetricFlags, MetricOptions};

/// Opts a field into the dial9 trace payload.
#[derive(Debug)]
pub(crate) struct EmitOpt;
impl MetricOptions for EmitOpt {}

/// Marker flag: apply with `#[metrics(flags(Emit))]` or
/// `#[metrics(default_flags(Emit))]` to include a field in the dial9 trace.
#[derive(Debug)]
pub struct Emit;
impl FlagConstructor for Emit {
    fn construct() -> MetricFlags<'static> {
        MetricFlags::upcast(&EmitOpt)
    }
}

/// Routes string data in this field through dial9's string pool.
#[derive(Debug)]
pub(crate) struct InternedOpt;
impl MetricOptions for InternedOpt {}

/// Marker flag: apply with `#[metrics(flags(Interned))]` to intern a
/// string-shaped field's value instead of encoding it inline. Orthogonal to
/// [`Emit`] — has no effect unless the field is also `Emit`-tagged.
#[derive(Debug)]
pub struct Interned;
impl FlagConstructor for Interned {
    fn construct() -> MetricFlags<'static> {
        MetricFlags::upcast(&InternedOpt)
    }
}

/// Internal marker carried by [`Dial9Context`](super::Dial9Context)'s own
/// fields, so [`Dial9Stream`](super::Dial9Stream) can find them by walking
/// the descriptor. Not a stable guarantee for external use — a future typed
/// source-extraction mechanism may replace this tag-based discovery.
#[derive(Debug)]
pub(crate) struct ContextOpt;
impl MetricOptions for ContextOpt {}

#[derive(Debug)]
pub(crate) struct Context;
impl FlagConstructor for Context {
    fn construct() -> MetricFlags<'static> {
        MetricFlags::upcast(&ContextOpt)
    }
}

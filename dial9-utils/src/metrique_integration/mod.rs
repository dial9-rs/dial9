//! Runtime-agnostic engine for recording [metrique](https://docs.rs/metrique)
//! entries into the dial9 trace.
//!
//! This module implements the descriptor-walking, schema-building, and
//! wire-encoding machinery: given any metrique entry whose macro-generated
//! descriptor tags fields with [`Emit`]/[`Interned`]/[`Context`], it encodes
//! the `Emit`-tagged fields into a dial9 trace event, with `Context`-tagged
//! fields routed into the event header instead of the payload. None of that
//! logic depends on a tokio runtime, or any runtime at all — it only needs
//! metrique's entry-descriptor system and a
//! [`Dial9Handle`](dial9_core::handle::Dial9Handle) to write through.
//!
//! What this module does *not* provide is a concrete context type: `Context`
//! is a bare marker flag, and it's up to the consuming crate to define a
//! metrique struct whose fields carry it (worker id, task id, timing — or
//! whatever a given runtime can capture) and flatten that into user entries.
//! `dial9-tokio-telemetry`'s `telemetry::metrique_integration::Dial9Context`
//! is the tokio-specific instance of that pattern; see its module docs for
//! the end-user-facing API (opting fields in, wiring up the sink, known
//! limitations). This module is the shared engine underneath it, kept here
//! (rather than in a tokio-coupled crate) so a future non-tokio dial9
//! integration could reuse it without pulling in tokio.

mod flags;
mod schema;
mod stream;

pub use flags::{Context, Emit, Interned};
pub use stream::Dial9Stream;

//! Metrique → dial9 sink.
//!
//! Records [metrique](https://docs.rs/metrique) unit-of-work entries into the
//! dial9 trace alongside the user's existing EMF/JSON pipeline, so a single
//! trace file carries both tokio runtime telemetry and per-request
//! application metrics.
//!
//! Requires the `metrique-sink` feature.
//!
//! # Usage
//!
//! Opt fields into the dial9 payload with the [`Emit`](crate::metrique_sink::Emit)
//! field flag, and flatten a [`Dial9Context`](crate::metrique_sink::Dial9Context)
//! into the entry so events carry caller-thread
//! runtime context:
//!
//! ```ignore
//! use dial9_tokio_telemetry::metrique_sink::{Dial9Context, Dial9Stream, Emit, Interned};
//! use metrique::unit_of_work::metrics;
//!
//! #[metrics(rename_all = "PascalCase", default_flags(Emit))]
//! struct RequestMetrics {
//!     // Captures worker id, task id, and start/end monotonic timestamps.
//!     #[metrics(flatten)]
//!     dial9: Dial9Context,
//!
//!     // Route repeated strings through dial9's string pool.
//!     #[metrics(flags(Interned))]
//!     route: String,
//!
//!     operation: &'static str,
//!
//!     // Opt noisy fields out of the dial9 payload.
//!     #[metrics(flags(skip(Emit)))]
//!     debug_blob: String,
//! }
//!
//! // Wire dial9 in as a peer of the existing EMF stream:
//! let stream = metrique::writer::stream::tee(emf_stream, Dial9Stream::new(&handle));
//! let _handle = ServiceMetrics::attach_to_stream(stream);
//!
//! // Use normally.
//! let mut m = RequestMetrics {
//!     dial9: Dial9Context::capture(),
//!     /* ... */
//! }
//! .append_on_drop(ServiceMetrics::sink());
//! ```
//!
//! All dial9 encoding happens on the thread that drives the metrique
//! pipeline (the `BackgroundQueue` flush thread for the standard setup).
//!
//! # Overhead
//!
//! Measured by `benches/metrique_sink_bench.rs`: `Dial9Context::capture()`
//! costs ~25 ns on the request path; encoding an entry (6 payload fields,
//! one interned string) costs ~650 ns on the flush thread; a disabled
//! handle costs ~2 ns per entry.
//!
//! # What lands in the trace
//!
//! One event per entry, carrying:
//!
//! - the entry's canonical name (schema name `metrique:<EntryName>`),
//! - start/end monotonic timestamps, worker id, and task id from
//!   [`Dial9Context`](crate::metrique_sink::Dial9Context) (worker
//!   `WorkerId::UNKNOWN` and absent task id when
//!   captured off-runtime),
//! - the wall-clock timestamp when the entry declares
//!   `#[metrics(timestamp)]`,
//! - every `Emit`-tagged field, with units carried as `unit` schema
//!   annotations (the same key the `TraceEvent` derive emits).
//!
//! # Limitations (initial release)
//!
//! - Hand-written `Entry` impls carry no descriptor and are skipped with a
//!   rate-limited warning; they still reach the other side of the `tee`.
//! - [`Flex`](metrique::flex::Flex) dynamic-key fields are not supported;
//!   entries containing them are dropped from the dial9 path (rate-limited
//!   warning) because their dynamic value callbacks cannot be matched
//!   against the static descriptor. EMF/JSON output is unaffected.
//! - Distribution-shaped fields (histograms) and other fields whose closed
//!   shape is `Opaque` are skipped with a diagnostic when tagged `Emit`.
//! - `Vec<T>` list fields are carried as comma-joined strings until an
//!   upstream metrique fix lands (design doc, delta 2).
//! - Fields are routed by their emitted names. An entry that emits the same
//!   post-rename name from two fields with different routing (for example a
//!   payload field named like a `Dial9Context` field, or two unprefixed
//!   `Dial9Context`s) is dropped with a once-per-type diagnostic and counted
//!   in the periodic sink counters.
//!
//! Roadmap and tracking for the above: [design doc, "Future evolution"](https://github.com/dial9-rs/dial9/blob/HEAD/docs/design/metrique-integration.md).

mod context;
mod plan;
mod stream;
mod writer;

pub use context::Dial9Context;
pub use stream::Dial9Stream;

use metrique_writer::value::{FlagConstructor, MetricFlags, MetricOptions};

/// Runtime payload for [`Emit`]. Formats other than dial9 ignore it.
#[derive(Debug)]
struct EmitOptions;
impl MetricOptions for EmitOptions {}

/// Field flag that opts a field into the dial9 trace payload.
///
/// Apply at struct scope via `#[metrics(default_flags(Emit))]` or at field
/// scope via `#[metrics(flags(Emit))]`; invert with
/// `#[metrics(flags(skip(Emit)))]`. Fields without this flag do not appear
/// in the dial9 event payload (they still reach EMF/JSON formats unchanged).
#[derive(Debug)]
pub struct Emit;

impl FlagConstructor for Emit {
    fn construct() -> MetricFlags<'static> {
        MetricFlags::upcast(&EmitOptions)
    }
}

/// Runtime payload for [`Interned`]. Formats other than dial9 ignore it.
#[derive(Debug)]
struct InternedOptions;
impl MetricOptions for InternedOptions {}

/// Field flag that routes string data in this field through dial9's string
/// pool.
///
/// Use for low-cardinality strings that repeat across events (route names,
/// operation names, status labels): each distinct value is written once per
/// flush cycle and events carry a compact pool reference. Orthogonal to
/// [`Emit`]; a field needs both flags to appear interned in the payload.
///
/// Applying `Interned` to a field whose shape is not string-capable is
/// reported as an error and the field is skipped on the wire.
#[derive(Debug)]
pub struct Interned;

impl FlagConstructor for Interned {
    fn construct() -> MetricFlags<'static> {
        MetricFlags::upcast(&InternedOptions)
    }
}

/// Runtime payload for [`Context`]. Formats other than dial9 ignore it.
#[derive(Debug)]
struct ContextOptions;
impl MetricOptions for ContextOptions {}

/// Dial9-internal field flag carried by [`Dial9Context`]'s own fields.
///
/// Users never name this type; they flatten [`Dial9Context`] into their
/// entry and the sink discovers the context fields by walking the descriptor
/// at first use. A future typed source-extraction mechanism in metrique
/// would replace this tag-based discovery.
#[derive(Debug)]
pub(crate) struct Context;

impl FlagConstructor for Context {
    fn construct() -> MetricFlags<'static> {
        MetricFlags::upcast(&ContextOptions)
    }
}

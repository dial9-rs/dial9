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
//! An entry opts into the dial9 trace by including a
//! [`Dial9Context`](crate::metrique_sink::Dial9Context). Every field of an
//! opted-in entry is recorded; exclude individual fields with the
//! [`Skip`](crate::metrique_sink::Skip) field flag:
//!
//! ```ignore
//! use dial9_tokio_telemetry::metrique_sink::{Dial9Context, Dial9Stream, Interned, Skip};
//! use metrique::unit_of_work::metrics;
//!
//! #[metrics(rename_all = "PascalCase")]
//! struct RequestMetrics {
//!     // Opts this entry in, and captures worker id, task id, and
//!     // start/end monotonic timestamps.
//!     #[metrics(flatten)]
//!     dial9: Dial9Context,
//!
//!     // Route repeated strings through dial9's string pool.
//!     #[metrics(flags(Interned))]
//!     route: String,
//!
//!     operation: &'static str,
//!
//!     // Keep bulky or high-cardinality fields out of the trace.
//!     #[metrics(flags(Skip))]
//!     debug_blob: String,
//! }
//!
//! // Wire dial9 in as a peer of the existing EMF stream. `tee` also keeps
//! // dial9's own `dial9.`-prefixed fields out of that stream.
//! let _handle = ServiceMetrics::attach_to_stream(
//!     Dial9Stream::tee(&handle, emf_stream),
//! );
//!
//! // Use normally.
//! let mut m = RequestMetrics {
//!     dial9: Dial9Context::capture(),
//!     /* ... */
//! }
//! .append_on_drop(ServiceMetrics::sink());
//! ```
//!
//! Entries without a `Dial9Context` are left alone: they flow through to the
//! rest of the pipeline and record nothing into the trace. Teeing the sink
//! into an existing pipeline therefore only records the entries you opt in.
//!
//! # Keeping dial9's fields out of your other sinks
//!
//! [`Dial9Context`](crate::metrique_sink::Dial9Context)'s fields are ordinary
//! metrique fields, so they would otherwise also appear in EMF/JSON output,
//! where the monotonic timestamps in particular are useless.
//! [`Dial9Stream::tee`](crate::metrique_sink::Dial9Stream::tee) wraps the
//! other side of the tee in
//! [`WithoutDial9Fields`](crate::metrique_sink::WithoutDial9Fields), which
//! drops every `dial9.`-prefixed field on the way in. Compose with metrique's
//! [`tee`](metrique_writer::stream::tee) and
//! [`Dial9Stream::new`](crate::metrique_sink::Dial9Stream::new) directly if
//! you would rather keep them.
//!
//! All dial9 encoding happens on the thread that drives the metrique
//! pipeline (the `BackgroundQueue` flush thread for the standard setup).
//!
//! # Overhead
//!
//! Measured by `benches/metrique_sink_bench.rs`: `Dial9Context::capture()`
//! costs ~26 ns on the request path, plus ~30 ns for the end-timestamp
//! clock read when the entry closes. Encoding costs ~490-630 ns per entry
//! on the flush thread, from an all-scalar payload up to one carrying an
//! allocating (non-interned) string; boxed entries from a global sink add
//! ~50 ns. A paused recorder or a disabled handle costs ~3 ns per entry.
//!
//! # What lands in the trace
//!
//! One event per entry, carrying:
//!
//! - the entry's canonical name (schema name `metrique:<EntryName>`,
//!   suffixed `#<layout hash>` when distinct entry types share a name),
//! - the start timestamp, plus `dial9.duration_ns`, `dial9.worker_id`, and
//!   `dial9.task_id` from
//!   [`Dial9Context`](crate::metrique_sink::Dial9Context) (worker
//!   `WorkerId::UNKNOWN` and absent task id when captured off-runtime),
//! - `dial9.wall_clock_ns` when the entry declares
//!   `#[metrics(timestamp)]`,
//! - every field not flagged [`Skip`](crate::metrique_sink::Skip), with units
//!   carried as `unit` schema annotations (the same key the `TraceEvent`
//!   derive emits). List-shaped fields (`Vec<T>`, slices) encode as typed
//!   lists.
//!
//! # Limitations
//!
//! - Hand-written `Entry` impls and entries containing
//!   [`Flex`](metrique::flex::Flex) dynamic-key fields carry no descriptors
//!   and cannot be recorded; they still reach the other side of the `tee`, so
//!   EMF/JSON output is unaffected.
//! - Distribution-shaped fields (histograms) and other fields whose closed
//!   shape is `Opaque` cannot be encoded and are left out of the payload.
//! - Only lists of strings work through a `GlobalEntrySink` (e.g.
//!   `ServiceMetrics::sink()`); entries with numeric list fields are
//!   dropped there (rate-limited warning), because boxing stringifies list
//!   elements. Typed sinks are unaffected.
//! - Two fields that emit the same post-rename name cannot share a schema:
//!   the first occurrence keeps the name and later ones are skipped with a
//!   diagnostic. Prefix flatten sites to disambiguate. Dial9's own fields
//!   are `dial9.`-prefixed, so they are never part of such a collision.
//! - A sink wrapped in
//!   [`WithoutDial9Fields`](crate::metrique_sink::WithoutDial9Fields) sees no
//!   descriptors at all for an entry that mixes `dial9.`-named fields with
//!   its own in one flatten site, because descriptor field lists cannot be
//!   subset. Formats that work off `Entry::write` (EMF, JSON, the local
//!   format) are unaffected.
//!
//! Roadmap and tracking for the above: [design doc, "Future evolution"](https://github.com/dial9-rs/dial9/blob/HEAD/docs/design/metrique-integration.md).

mod context;
mod filter;
mod plan;
mod stream;
mod writer;

pub use context::Dial9Context;
pub use filter::WithoutDial9Fields;
pub use stream::Dial9Stream;

use metrique_writer::value::{FlagConstructor, MetricFlags, MetricOptions};

// The three flag markers below follow metrique's FlagConstructor pattern:
// each has a zero-sized MetricOptions payload that only the dial9 sink
// inspects; other formats carry it through untouched.

/// Runtime payload for [`Skip`].
#[derive(Debug)]
struct SkipOptions;
impl MetricOptions for SkipOptions {}

/// Field flag that excludes a field from the dial9 trace payload.
///
/// An entry opts into the dial9 sink by including a
/// [`Dial9Context`](crate::metrique_sink::Dial9Context); every field of an
/// opted-in entry is then recorded by default. Apply this flag via
/// `#[metrics(flags(Skip))]` to keep a field out of the trace (it still
/// reaches EMF/JSON formats unchanged). Use it for high-cardinality or bulky
/// fields that would bloat the trace without helping analysis.
#[derive(Debug)]
pub struct Skip;

impl FlagConstructor for Skip {
    fn construct() -> MetricFlags<'static> {
        MetricFlags::upcast(&SkipOptions)
    }
}

/// Runtime payload for [`Interned`].
#[derive(Debug)]
struct InternedOptions;
impl MetricOptions for InternedOptions {}

/// Field flag that routes string data in this field through dial9's string
/// pool.
///
/// Use for low-cardinality strings that repeat across events (route names,
/// operation names, status labels): each distinct value is written once per
/// flush cycle and events carry a compact pool reference. On list-of-string
/// fields, each element is interned individually.
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

/// Runtime payload for [`Context`].
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

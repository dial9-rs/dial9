//! Metrique → dial9 sink.
//!
//! Records [metrique](https://docs.rs/metrique) unit-of-work entries into the
//! dial9 trace alongside the user's existing EMF/JSON pipeline, so a single
//! trace file carries both tokio runtime telemetry and per-request
//! application metrics.
//!
//! Available through the `dial9` facade as `dial9::metrique_sink`
//! (feature `metrique-sink`), or directly as a standalone crate. The `tokio`
//! feature adds task-id capture; nothing else in this crate needs a tokio
//! runtime.
//!
//! # Usage
//!
//! An entry opts into the dial9 trace by including a
//! [`Dial9Context`](crate::Dial9Context). Every field of an
//! opted-in entry is recorded; exclude individual fields with the
//! [`Skip`](crate::Skip) field flag:
//!
//! ```no_run
//! use dial9_metrique::{Dial9Context, Dial9Stream, Interned, Skip, SpanName};
//! use metrique::ServiceMetrics;
//! use metrique::unit_of_work::metrics;
//! use metrique::writer::{AttachGlobalEntrySinkExt, GlobalEntrySink};
//!
//! #[metrics(rename_all = "PascalCase")]
//! struct RequestMetrics {
//!     // Opts this entry in, and captures thread id, task id, and
//!     // start/end monotonic timestamps.
//!     #[metrics(flatten)]
//!     dial9: Dial9Context,
//!
//!     // Route repeated strings through dial9's string pool.
//!     #[metrics(flags(Interned))]
//!     route: String,
//!
//!     #[metrics(flags(Interned, SpanName))]
//!     operation: &'static str,
//!
//!     // Keep bulky or high-cardinality fields out of the trace.
//!     #[metrics(flags(Skip))]
//!     debug_blob: String,
//! }
//!
//! # let handle = dial9_core::handle::Dial9Handle::disabled();
//! # let emf_stream = Dial9Stream::new(&handle); // stand-in for your pipeline
//! // Wire dial9 in as a peer of the existing EMF stream. `tee` also keeps
//! // dial9's own `dial9.`-prefixed fields out of that stream.
//! let _join = ServiceMetrics::attach_to_stream(
//!     Dial9Stream::tee(&handle, emf_stream),
//! );
//!
//! // Use normally.
//! let mut m = RequestMetrics {
//!     dial9: Dial9Context::capture(),
//!     route: "/pets".to_owned(),
//!     operation: "GetPet",
//!     debug_blob: String::new(),
//! }
//! .append_on_drop(ServiceMetrics::sink());
//! ```
//!
//! Entries without a `Dial9Context` are left alone: they flow through to the
//! rest of the pipeline and record nothing into the trace. Teeing the sink
//! into an existing pipeline therefore only records the entries you opt in.
//!
//! ## Opting in without touching the entry
//!
//! Adding a field is intrusive when the metrics struct is shared, owned by
//! another team, or when dial9 should be switchable from one place.
//! [`Dial9EntryExt`](crate::Dial9EntryExt) attaches the same
//! context from the outside, so the entry definition stays as it was:
//!
//! ```no_run
//! use dial9_metrique::Dial9EntryExt;
//! use metrique::ServiceMetrics;
//! use metrique::unit_of_work::metrics;
//! use metrique::writer::GlobalEntrySink;
//!
//! #[metrics(rename_all = "PascalCase")]
//! struct RequestMetrics {
//!     operation: &'static str,
//!     latency_ms: u64,
//! }
//!
//! // `append_on_drop_dial9` in place of `append_on_drop`:
//! let mut m = RequestMetrics { operation: "GetPet", latency_ms: 0 }
//!     .append_on_drop_dial9(ServiceMetrics::sink());
//! m.latency_ms = 5; // field access reaches through the wrapper
//! ```
//!
//! Both paths produce the same event, named after your entry either way. The
//! wrapper is also the easier one to make conditional, since a `cfg` around
//! one call is simpler than one around a struct field:
//! [`with_dial9_context`](crate::Dial9EntryExt::with_dial9_context)
//! wraps an entry without attaching it to a sink.
//!
//! # Keeping dial9's fields out of your other sinks
//!
//! [`Dial9Context`](crate::Dial9Context)'s fields are ordinary
//! metrique fields, so they would otherwise also appear in EMF/JSON output,
//! where the monotonic timestamps in particular are useless.
//! [`Dial9Stream::tee`](crate::Dial9Stream::tee) wraps the
//! other side of the tee in
//! [`WithoutDial9Fields`](crate::WithoutDial9Fields), which
//! drops every `dial9.`-prefixed field on the way in. Compose with metrique's
//! [`tee`](metrique_writer::stream::tee) and
//! [`Dial9Stream::new`](crate::Dial9Stream::new) directly if
//! you would rather keep them.
//!
//! All dial9 encoding happens on the thread that drives the metrique
//! pipeline (the `BackgroundQueue` flush thread for the standard setup).
//!
//! # Overhead
//!
//! Measured by `benches/metrique_sink_bench.rs`: `Dial9Context::capture()`
//! costs ~32 ns on the request path, plus ~24 ns for the end-timestamp
//! clock read when the entry closes. Encoding costs ~445-540 ns per entry
//! on the flush thread, from an all-scalar payload up to one carrying an
//! allocating (non-interned) string; boxed entries from a global sink add
//! ~80 ns. A paused recorder or a disabled handle costs ~3.3 ns per entry.
//! [`Dial9Stream::tee`]'s field filtering adds well under a nanosecond per
//! entry to the other sink.
//!
//! # What lands in the trace
//!
//! One event per entry, carrying:
//!
//! - the entry's canonical name (schema name `metrique:<EntryName>`,
//!   suffixed `#<layout hash>` when distinct entry types share a name),
//! - the close timestamp (span end) in the packed event header, plus
//!   annotated `dial9.span.duration_ns`, `dial9.thread_id`, and
//!   `dial9.tokio.task_id` fields from [`Dial9Context`](crate::Dial9Context)
//!   (the task id needs the `tokio` feature and is absent when captured
//!   outside a task; the span start is `end - duration`),
//! - a field flagged [`SpanName`](crate::SpanName), when present, as the
//!   span's dynamic display name (otherwise viewers use the schema name),
//! - `dial9.wall_clock_ns` when the entry declares
//!   `#[metrics(timestamp)]`,
//! - every field not flagged [`Skip`](crate::Skip), with units
//!   carried as `unit` schema annotations (the same key the `TraceEvent`
//!   derive emits). List-shaped fields (`Vec<T>`, slices) encode as typed
//!   lists.
//!
//! # Limitations
//!
//! - An entry the sink cannot describe is not recorded: hand-written `Entry`
//!   impls that do not implement `descriptors()`, and entries containing
//!   [`Flex`](metrique::flex::Flex) dynamic-key fields, whose descriptors are
//!   unavailable by construction. They still reach the other side of the
//!   `tee`, so EMF/JSON output is unaffected.
//! - Distribution-shaped fields (histograms) and other fields whose closed
//!   shape is `Opaque` cannot be encoded and are left out of the payload.
//! - Only lists of strings work through a `GlobalEntrySink` (e.g.
//!   `ServiceMetrics::sink()`); entries with numeric list fields are
//!   dropped there (rate-limited warning), because boxing stringifies list
//!   elements. Typed sinks are unaffected.
//! - Two fields that emit the same post-rename name cannot share a schema:
//!   the first occurrence keeps the name and later ones are skipped with a
//!   diagnostic. This includes the generated `dial9.thread_id`,
//!   `dial9.tokio.task_id`, and `dial9.span.duration_ns` structural fields.
//! - A sink wrapped in
//!   [`WithoutDial9Fields`](crate::WithoutDial9Fields) sees no
//!   descriptors for an entry that declares its own field literally named
//!   `dial9.*` next to other fields, since that segment cannot be filtered.
//!   Formats that work off `Entry::write` are unaffected.
//!
//! Roadmap and tracking for the above: [design doc, "Future evolution"](https://github.com/dial9-rs/dial9/blob/HEAD/docs/design/metrique-integration.md).

#![cfg_attr(docsrs, feature(doc_cfg))]

mod context;
mod event;
mod filter;
mod plan;
mod stream;
mod writer;

pub use context::Dial9Context;
pub use event::{Dial9EntryExt, Dial9Event, Dial9EventClosed};
pub use filter::WithoutDial9Fields;
pub use stream::Dial9Stream;

use metrique_writer::value::{FlagConstructor, MetricFlags, MetricOptions};

/// Physical field names emitted by the metrique sink.
///
/// Consumers should use schema annotations for semantics; these constants
/// describe metrique's concrete names and are useful when inspecting its raw
/// events.
pub mod field_names {
    /// Span duration, in nanoseconds. The packed event timestamp is the span
    /// end (close), so the start is `end - duration`.
    pub const SPAN_DURATION_NS: &str = "dial9.span.duration_ns";

    /// OS thread ID captured when the metrique entry starts.
    pub const THREAD_ID: &str = "dial9.thread_id";

    /// Tokio task ID captured when the metrique entry starts.
    pub const TOKIO_TASK_ID: &str = "dial9.tokio.task_id";

    /// Optional wall-clock timestamp from metrique's timestamp field.
    pub const WALL_CLOCK_NS: &str = "dial9.wall_clock_ns";
}

/// Value emitted under the `dial9.span.type` annotation for metrique spans.
///
/// Re-exported from [`dial9_core::schema_extensions::span_types`], the shared
/// producer/consumer vocabulary, so decoders and this producer name the type
/// from one source.
pub const SPAN_TYPE: &str = dial9_core::schema_extensions::span_types::METRIQUE;

// The flag markers below follow metrique's FlagConstructor pattern:
// each has a zero-sized MetricOptions payload that only the dial9 sink
// inspects; other formats carry it through untouched.

/// Runtime payload for [`Skip`].
#[derive(Debug)]
struct SkipOptions;
impl MetricOptions for SkipOptions {}

/// Field flag that excludes a field from the dial9 trace payload.
///
/// An entry opts into the dial9 sink by including a
/// [`Dial9Context`](crate::Dial9Context); every field of an
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

/// Runtime payload for [`SpanName`].
#[derive(Debug)]
struct SpanNameOptions;
impl MetricOptions for SpanNameOptions {}

/// Field flag selecting the metrique field used as the span's display name.
///
/// Apply this to at most one scalar string field:
///
/// ```ignore
/// #[metrics(flags(dial9::Interned, dial9::SpanName))]
/// operation: &'static str,
/// ```
///
/// The field remains part of the event payload and receives the
/// `dial9.role=span.name` schema annotation. [`Interned`] can be combined with
/// this flag to pool repeated names. Without a `SpanName` field, viewers use
/// the event's schema name.
///
/// The flag is a best-effort display-name hint. If it cannot be honored — a
/// second `SpanName` field on the same entry, use on a non-string field,
/// combining it with [`Skip`], or a field whose name collides with a header
/// or an earlier field — the field is recorded as an ordinary payload field
/// (a warning is logged) and the span falls back to its schema name. The
/// event itself is still recorded; only the display name reverts.
#[derive(Debug)]
pub struct SpanName;

impl FlagConstructor for SpanName {
    fn construct() -> MetricFlags<'static> {
        MetricFlags::upcast(&SpanNameOptions)
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

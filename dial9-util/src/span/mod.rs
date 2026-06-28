//! Ad-hoc span instrumentation that does not require a `tracing` subscriber.
//!
//! The telemetry crate's `tracing` layer is the right tool when your code is
//! already instrumented with `tracing` and you have a subscriber wired up. This
//! module is for everything else: emitting span-like timing information into a
//! dial9 trace directly, with nothing but a dial9 runtime installed.
//!
//! Spans produced here use the exact same wire format as the tracing layer, so
//! the viewer renders them on the same span timeline.
//!
//! # Instrumenting a future
//!
//! The common case is to attach a span to a future so the viewer shows when it
//! was being polled (one segment per poll, gaps where it was suspended):
//!
//! ```no_run
//! use dial9_util::dial9_span;
//! use dial9_util::span::Instrument;
//!
//! # async fn handle(req: u32) {
//! async {
//!     // ... do work ...
//! }
//! .instrument(dial9_span!("handle_request", request_id = req))
//! .await
//! # }
//! ```
//!
//! # Fields
//!
//! The [`dial9_span!`](crate::dial9_span) macro captures arbitrary key/value
//! fields with `tracing`-style syntax — bare and `%` format with [`Display`],
//! `?` with [`Debug`]:
//!
//! ```no_run
//! # use dial9_util::dial9_span;
//! # let path = "/x"; let retries = 1u32;
//! # #[derive(Debug)] struct Cfg; let cfg = Cfg;
//! let span = dial9_span!("load", path = %path, retries = retries, config = ?cfg);
//! ```
//!
//! Fields added through the macro are encoded through a per-call-site
//! [`TraceEvent`](dial9_trace_format::TraceEvent) struct generated on your
//! behalf, so a span with fields costs no more to emit than a hand-written
//! event. Spans built with [`Dial9Span::new`] (e.g. by
//! [`Dial9SpanLayer`](crate::span::Dial9SpanLayer)) carry only their name; to
//! attach fields, use the macro.
//!
//! Values are **interned** (pooled) by default — the right choice for
//! low-cardinality fields, since a span re-emits its fields on every poll.
//! Prefix a macro value with `~` to store a high-cardinality value **inline**
//! instead, avoiding string-pool growth.
//!
//! # Instrumenting a synchronous scope
//!
//! For synchronous code, hold the guard returned by [`Dial9Span::enter`]; the
//! span is open until the guard drops:
//!
//! ```no_run
//! use dial9_util::dial9_span;
//!
//! let span = dial9_span!("expensive_computation");
//! let _entered = span.enter();
//! // ... work attributed to the span ...
//! ```
//!
//! Do **not** hold an [`Entered`] guard across an `.await` point — it records a
//! single contiguous segment and cannot represent suspension. Use
//! [`Instrument::instrument`] for futures instead.
//!
//! # Tower middleware
//!
//! With the `tower-layer` feature, [`Dial9SpanLayer`] wraps a
//! [`tower`](https://docs.rs/tower) service so each request future is
//! instrumented automatically.

mod future;
#[cfg(feature = "tower-layer")]
mod tower;
pub(crate) mod wire;

pub use future::{Instrument, Instrumented};
#[cfg(feature = "tower-layer")]
pub use tower::{Dial9SpanLayer, Dial9SpanService};

use dial9_tokio_telemetry::telemetry::{
    Dial9Handle, ThreadLocalEncoder, clock_monotonic_ns, current_worker_id,
};
use dial9_trace_format::{InternedString, TraceEvent};
use std::fmt;

/// Re-exports used by the [`dial9_span!`](crate::dial9_span) macro expansion.
/// Not a stable API.
#[doc(hidden)]
pub mod __rt {
    pub use super::{SpanEmit, SpanWire};
    pub use dial9_tokio_telemetry::telemetry::ThreadLocalEncoder;
    pub use dial9_trace_format::{InternedString, TraceEvent};
}

/// A span: a named, optionally-parented region of work that is recorded into
/// the trace with timing information.
///
/// A span carries an identity (a process-unique id), a name, optional parent,
/// and a set of fields. It does nothing on its own — timing is recorded when
/// you either:
///
/// - attach it to a future with [`Instrument::instrument`], or
/// - open it over a synchronous scope with [`enter`](Self::enter).
///
/// Construct one with the [`dial9_span!`](crate::dial9_span) macro (which
/// captures the call site and fields for you), or with [`Dial9Span::new`] for a
/// name-only span assembled at runtime.
///
/// When the span is dropped, a close event is recorded, telling the viewer the
/// span is complete. `Dial9Span` is intentionally **not** `Clone`: it owns a
/// single span identity with a single close.
pub struct Dial9Span {
    span_id: u64,
    name: String,
    parent_span_id: Option<u64>,
    /// Field values in field order, lining up with the generated [`SpanWire`]'s
    /// fields. Empty for spans built with [`Dial9Span::new`].
    field_values: Vec<String>,
    /// The per-call-site (or, for [`Dial9Span::new`], the shared name-only)
    /// generated enter/exit writers this span emits through.
    wire: &'static SpanWire,
}

/// Whether we are recording the start or the end of a span segment.
#[derive(Clone, Copy)]
enum Phase {
    Enter,
    Exit,
}

// ── Name-only runtime span (used by `Dial9Span::new`) ────────────────────────
//
// Spans built with `Dial9Span::new` carry no user fields, so they all share one
// generated schema rather than one per call site. The framework field set is
// fixed, so this is sound: every name-only span has the same wire shape.

#[derive(TraceEvent)]
#[traceevent(name = "SpanEnter:dial9-util::runtime")]
struct RuntimeSpanEnter {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u64,
    span_id: u64,
    parent_span_id: Option<u64>,
    span_name: InternedString,
}

#[derive(TraceEvent)]
#[traceevent(name = "SpanExit:dial9-util::runtime")]
struct RuntimeSpanExit {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u64,
    span_id: u64,
    span_name: InternedString,
}

fn runtime_enter(enc: &mut ThreadLocalEncoder<'_>, e: SpanEmit<'_>) {
    let span_name = enc.intern_string(e.name);
    enc.encode(&RuntimeSpanEnter {
        timestamp_ns: e.timestamp_ns,
        worker_id: e.worker_id,
        span_id: e.span_id,
        parent_span_id: e.parent_span_id,
        span_name,
    });
}

fn runtime_exit(enc: &mut ThreadLocalEncoder<'_>, e: SpanEmit<'_>) {
    let span_name = enc.intern_string(e.name);
    enc.encode(&RuntimeSpanExit {
        timestamp_ns: e.timestamp_ns,
        worker_id: e.worker_id,
        span_id: e.span_id,
        span_name,
    });
}

static RUNTIME_SPAN_WIRE: SpanWire = SpanWire {
    enter: runtime_enter,
    exit: runtime_exit,
};

// ── Generated-path plumbing (used by the macro) ──────────────────────────────

/// The per-call-site values an event-writer function needs. Borrowed from the
/// [`Dial9Span`] for the duration of one emit.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct SpanEmit<'a> {
    pub timestamp_ns: u64,
    pub worker_id: u64,
    pub span_id: u64,
    pub parent_span_id: Option<u64>,
    pub name: &'a str,
    pub field_values: &'a [String],
}

/// `&'static` function pointers to a call site's generated enter/exit writers.
/// Produced by the [`dial9_span!`](crate::dial9_span) macro.
#[doc(hidden)]
#[derive(Debug)]
pub struct SpanWire {
    pub enter: fn(&mut ThreadLocalEncoder<'_>, SpanEmit<'_>),
    pub exit: fn(&mut ThreadLocalEncoder<'_>, SpanEmit<'_>),
}

/// Bridges a generated writer fn into the [`Encodable`](dial9_tokio_telemetry::telemetry::Encodable)
/// path so it goes through `record_event` (enabled-check, flush accounting).
struct GeneratedEvent<'a> {
    write: fn(&mut ThreadLocalEncoder<'_>, SpanEmit<'_>),
    emit: SpanEmit<'a>,
}

impl dial9_tokio_telemetry::telemetry::Encodable for GeneratedEvent<'_> {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        (self.write)(enc, self.emit);
    }
}

impl fmt::Debug for Dial9Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dial9Span")
            .field("span_id", &self.span_id)
            .field("name", &self.name)
            .field("parent_span_id", &self.parent_span_id)
            .finish_non_exhaustive()
    }
}

impl Dial9Span {
    /// Create a name-only span.
    ///
    /// The span carries no user fields. When the fields are known at the call
    /// site, prefer the [`dial9_span!`](crate::dial9_span) macro, which captures
    /// them for you. This constructor is for spans whose name is chosen at
    /// runtime (for example by
    /// [`Dial9SpanLayer::named`](crate::span::Dial9SpanLayer::named)).
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            span_id: wire::next_span_id(),
            name: name.into(),
            parent_span_id: None,
            field_values: Vec::new(),
            wire: &RUNTIME_SPAN_WIRE,
        }
    }

    /// Set this span's parent. The viewer nests the child under the parent in
    /// the span tree.
    ///
    /// If no parent is set, the viewer infers nesting from timestamp
    /// containment instead.
    #[must_use]
    pub fn with_parent(mut self, parent: &Dial9Span) -> Self {
        self.parent_span_id = Some(parent.span_id);
        self
    }

    /// Set this span's parent by id (see [`Dial9Span::id`]).
    #[must_use]
    pub fn with_parent_id(mut self, parent_span_id: u64) -> Self {
        self.parent_span_id = Some(parent_span_id);
        self
    }

    /// This span's process-unique id, for use with
    /// [`with_parent_id`](Self::with_parent_id).
    pub fn id(&self) -> u64 {
        self.span_id
    }

    /// Open the span over the current scope, recording a single enter/exit
    /// segment that runs until the returned guard is dropped.
    ///
    /// For synchronous code only — see the [module docs](self) for why this must
    /// not be held across `.await`.
    pub fn enter(&self) -> Entered<'_> {
        self.emit(Phase::Enter);
        Entered { span: self }
    }

    fn emit(&self, phase: Phase) {
        let handle = Dial9Handle::current();
        if !handle.is_enabled() {
            return;
        }
        let timestamp_ns = clock_monotonic_ns();
        let worker_id = current_worker_id().as_u64();

        let write = match phase {
            Phase::Enter => self.wire.enter,
            Phase::Exit => self.wire.exit,
        };
        handle.record_event(GeneratedEvent {
            write,
            emit: SpanEmit {
                timestamp_ns,
                worker_id,
                span_id: self.span_id,
                parent_span_id: self.parent_span_id,
                name: &self.name,
                field_values: &self.field_values,
            },
        });
    }

    /// Implementation detail of the [`dial9_span!`](crate::dial9_span) macro.
    /// Not part of the stable API.
    #[doc(hidden)]
    pub fn __from_generated(
        wire: &'static SpanWire,
        name: impl Into<String>,
        field_values: Vec<String>,
    ) -> Self {
        Self {
            span_id: wire::next_span_id(),
            name: name.into(),
            parent_span_id: None,
            field_values,
            wire,
        }
    }
}

impl Drop for Dial9Span {
    fn drop(&mut self) {
        let Some(handle) = Dial9Handle::try_current() else {
            return;
        };
        handle.record_event(wire::SpanCloseEvent {
            timestamp_ns: clock_monotonic_ns(),
            span_id: self.span_id,
        });
    }
}

/// A guard representing an open span scope, returned by [`Dial9Span::enter`].
///
/// The span's exit segment is recorded when this guard is dropped.
#[must_use = "the span is exited as soon as the guard is dropped; bind it to a variable"]
#[derive(Debug)]
pub struct Entered<'a> {
    span: &'a Dial9Span,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        self.span.emit(Phase::Exit);
    }
}

/// Construct a [`Dial9Span`], capturing the call site and (optionally) fields.
///
/// This is the ergonomic, lowest-overhead way to create a span: it generates a
/// typed [`TraceEvent`](dial9_trace_format::TraceEvent) per call site so emitting
/// is as cheap as a hand-written event.
///
/// Field syntax mirrors `tracing` — bare and `%` format with [`Display`], `?`
/// with [`Debug`]:
///
/// ```
/// use dial9_util::dial9_span;
///
/// // Just a name:
/// let span = dial9_span!("load_config");
///
/// // With fields:
/// # let path = "/etc/app.toml";
/// # let retries = 3u32;
/// let span = dial9_span!("load_config", path = %path, retries = retries);
/// # #[derive(Debug)] struct Cfg;
/// # let cfg = Cfg;
/// let span = dial9_span!("validate", config = ?cfg);
/// ```
///
/// # Interning
///
/// By default field values are **interned** (pooled) on the wire, which dedups
/// repeated values — a span re-emits its fields on every poll, so this is the
/// right default for low-cardinality fields. Prefix a value with `~` to store
/// it **inline** instead, avoiding string-pool growth for high-cardinality
/// values such as request ids:
///
/// ```
/// # use dial9_util::dial9_span;
/// # let request_id = "req-abc123";
/// # let route = "/users";
/// let span = dial9_span!("request", route = route, id = ~request_id);
/// ```
///
/// The `~` marker combines with the format sigils: `~%expr` (inline Display),
/// `~?expr` (inline Debug).
///
/// Field values are formatted to strings eagerly at construction.
#[macro_export]
macro_rules! dial9_span {
    ($name:expr $(,)?) => {
        $crate::__dial9_span_build!($name; [])
    };
    ($name:expr, $($fields:tt)+) => {
        $crate::__dial9_span_munch!($name; [] ; $($fields)+)
    };
}

/// Token-muncher that formats each field value (handling the `%`/`?`/bare format
/// sigils and the optional `~` inline marker) and accumulates
/// `(key @ kind : value_expr)` triples for the build step. `kind` is `intern` or
/// `inline`.
#[doc(hidden)]
#[macro_export]
macro_rules! __dial9_span_munch {
    ($name:expr; [$($acc:tt)*] ; ) => {
        $crate::__dial9_span_build!($name; [$($acc)*])
    };
    // ── Inline (not interned): `~`, `~%`, `~?` ──
    ($name:expr; [$($acc:tt)*] ; $key:ident = ~ ?$val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($acc)* ($key @ inline : ::std::format!("{:?}", $val))] ; $($($rest)*)?
        )
    };
    ($name:expr; [$($acc:tt)*] ; $key:ident = ~ %$val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($acc)* ($key @ inline : ::std::format!("{}", $val))] ; $($($rest)*)?
        )
    };
    ($name:expr; [$($acc:tt)*] ; $key:ident = ~ $val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($acc)* ($key @ inline : ::std::format!("{}", $val))] ; $($($rest)*)?
        )
    };
    // ── Interned (default): `%`, `?`, bare ──
    ($name:expr; [$($acc:tt)*] ; $key:ident = %$val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($acc)* ($key @ intern : ::std::format!("{}", $val))] ; $($($rest)*)?
        )
    };
    ($name:expr; [$($acc:tt)*] ; $key:ident = ?$val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($acc)* ($key @ intern : ::std::format!("{:?}", $val))] ; $($($rest)*)?
        )
    };
    ($name:expr; [$($acc:tt)*] ; $key:ident = $val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($acc)* ($key @ intern : ::std::format!("{}", $val))] ; $($($rest)*)?
        )
    };
}

/// Maps a field `kind` token to its generated struct field type.
#[doc(hidden)]
#[macro_export]
macro_rules! __dial9_field_ty {
    (intern) => {
        $crate::span::__rt::InternedString
    };
    (inline) => {
        ::std::string::String
    };
}

/// Maps a field `kind` token to the expression that encodes one value: interned
/// values are pooled, inline values are written as owned strings.
#[doc(hidden)]
#[macro_export]
macro_rules! __dial9_field_enc {
    (intern, $enc:expr, $val:expr) => {
        $enc.intern_string($val)
    };
    (inline, $enc:expr, $val:expr) => {
        ::std::clone::Clone::clone($val)
    };
}

/// Build step: define the per-call-site enter/exit [`TraceEvent`] structs, the
/// writer functions, the `&'static` [`SpanWire`], and construct the span.
#[doc(hidden)]
#[macro_export]
macro_rules! __dial9_span_build {
    ($name:expr; [$( ($key:ident @ $kind:ident : $val:expr) )*]) => {{
        // NOTE: the framework field names (`worker_id`, `span_id`,
        // `parent_span_id`, `span_name`) are the on-wire field names the viewer
        // looks for — they must match exactly. A user field with one of these
        // names is a (compile-time) duplicate-field error.
        //
        // The wire schema name is made unique per call site
        // (`SpanEnter:<file>:<line>:<col>`) so that two call sites with
        // *different* field sets register *different* schemas. The viewer keys
        // on the `SpanEnter:` / `SpanExit:` prefix, so the suffix is free to
        // disambiguate. Without this, the encoder rejects a second call site
        // that reuses the name with a different field set.
        #[derive($crate::span::__rt::TraceEvent)]
        #[traceevent(name = ::core::concat!(
            "SpanEnter:", ::core::file!(), ":", ::core::line!(), ":", ::core::column!()
        ))]
        #[allow(non_snake_case)]
        struct __Dial9SpanEnter {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            worker_id: u64,
            span_id: u64,
            parent_span_id: ::std::option::Option<u64>,
            span_name: $crate::span::__rt::InternedString,
            $( $key: $crate::__dial9_field_ty!($kind), )*
        }

        #[derive($crate::span::__rt::TraceEvent)]
        #[traceevent(name = ::core::concat!(
            "SpanExit:", ::core::file!(), ":", ::core::line!(), ":", ::core::column!()
        ))]
        #[allow(non_snake_case)]
        struct __Dial9SpanExit {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            worker_id: u64,
            span_id: u64,
            span_name: $crate::span::__rt::InternedString,
            $( $key: $crate::__dial9_field_ty!($kind), )*
        }

        fn __dial9_enter(
            __enc: &mut $crate::span::__rt::ThreadLocalEncoder<'_>,
            __e: $crate::span::__rt::SpanEmit<'_>,
        ) {
            let mut __vals = __e.field_values.iter();
            let __ev = __Dial9SpanEnter {
                timestamp_ns: __e.timestamp_ns,
                worker_id: __e.worker_id,
                span_id: __e.span_id,
                parent_span_id: __e.parent_span_id,
                span_name: __enc.intern_string(__e.name),
                $( $key: $crate::__dial9_field_enc!(
                    $kind, __enc,
                    __vals.next().expect("field value count matches macro fields")
                ), )*
            };
            __enc.encode(&__ev);
        }

        fn __dial9_exit(
            __enc: &mut $crate::span::__rt::ThreadLocalEncoder<'_>,
            __e: $crate::span::__rt::SpanEmit<'_>,
        ) {
            let mut __vals = __e.field_values.iter();
            let __ev = __Dial9SpanExit {
                timestamp_ns: __e.timestamp_ns,
                worker_id: __e.worker_id,
                span_id: __e.span_id,
                span_name: __enc.intern_string(__e.name),
                $( $key: $crate::__dial9_field_enc!(
                    $kind, __enc,
                    __vals.next().expect("field value count matches macro fields")
                ), )*
            };
            __enc.encode(&__ev);
        }

        static __DIAL9_WIRE: $crate::span::__rt::SpanWire = $crate::span::__rt::SpanWire {
            enter: __dial9_enter,
            exit: __dial9_exit,
        };

        $crate::span::Dial9Span::__from_generated(
            &__DIAL9_WIRE,
            $name,
            ::std::vec![ $( $val ),* ],
        )
    }};
}

#[cfg(test)]
mod tests;

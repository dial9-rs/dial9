//! Ad-hoc span instrumentation that does not require a `tracing` subscriber.
//!
//! The `tracing` layer (`dial9_tokio_telemetry::tracing_layer`) is the right tool when your code
//! is already instrumented with `tracing` and you have a subscriber wired up.
//! This module is for everything else: emitting span-like timing information
//! into a dial9 trace directly, with nothing but a dial9 runtime installed.
//!
//! Spans produced here use the exact same wire format as the tracing layer, so
//! the viewer renders them on the same span timeline.
//!
//! # Zero-cost
//!
//! [`dial9_span!`] generates a dedicated [`TraceEvent`](dial9_trace_format::TraceEvent)
//! per call site with **typed** fields, and emits through the encoder's direct
//! typed path — no runtime schema map, no per-emit `format!`, no boxed
//! closures, and numeric fields ride the wire as `Varint`s. The result compiles
//! down to what you would write by hand with `#[derive(TraceEvent)]` +
//! `record_event`. `Copy` fields are free to re-emit; the only allocation is a
//! per-emit clone of owned `String` fields — the same cost a hand-written
//! re-emitting span pays (interned ids are not stable across flush cycles, so a
//! stored value must be re-materialized each emit).
//!
//! # Instrumenting a future
//!
//! ```no_run
//! use dial9_utils::dial9_span;
//! use dial9_utils::span::Instrument as _;
//!
//! # async fn handle(req: u32) {
//! async {
//!     // ... do work ...
//! }
//! .instrument(dial9_span!("handle_request", request_id: u32 = req))
//! .await
//! # }
//! ```
//!
//! # Fields
//!
//! The [`dial9_span!`] macro captures typed key/value fields: an eager field is
//! written `key: Type = value` and keeps its type (a `u64` is a `Varint` on the
//! wire); `%` formats via [`Display`](std::fmt::Display) and `?` via
//! [`Debug`](std::fmt::Debug) (both producing a `String`):
//!
//! ```no_run
//! # use dial9_utils::dial9_span;
//! # let retries = 1u32; let path = "/x";
//! # #[derive(Debug)] struct Cfg; let cfg = Cfg;
//! let span = dial9_span!("load", retries: u32 = retries, path = %path, config = ?cfg);
//! ```
//!
//! # Instrumenting a synchronous scope
//!
//! ```no_run
//! use dial9_utils::dial9_span;
//! use dial9_utils::span::Span as _;
//!
//! let span = dial9_span!("expensive_computation");
//! let _entered = span.enter();
//! // ... work attributed to the span ...
//! ```
//!
//! Do **not** hold an [`Entered`](crate::span::Entered) guard across an `.await`
//! point — it is `!Send` for exactly that reason. Use
//! [`Instrument::instrument`](crate::span::Instrument::instrument) for futures instead.
//!
//! # Tower middleware
//!
//! With the `tower` feature, [`Dial9SpanLayer`](crate::tower::Dial9SpanLayer) wraps a
//! [`tower`](https://docs.rs/tower) service so each request future is
//! instrumented automatically.

mod future;
pub(crate) mod wire;

pub use future::{Instrument, Instrumented};
pub use wire::SpanId;

use dial9_core::clock::clock_monotonic_ns;
use dial9_core::handle::Dial9Handle;
use dial9_trace_format::{InternedString, TraceEvent};
use std::fmt;
use std::marker::PhantomData;
use std::sync::{Arc, OnceLock};

/// Re-exports used by the [`dial9_span!`](crate::dial9_span) macro expansion.
/// Not a stable API.
#[doc(hidden)]
pub mod __rt {
    pub use super::wire::SpanId;
    pub use super::{MacroSpan, Slot, current_task_id};
    pub use dial9_core::clock::clock_monotonic_ns;
    pub use dial9_core::handle::Dial9Handle;
    pub use dial9_trace_format::{InternedString, TraceEvent, TraceField};
    pub use std::sync::{Arc, OnceLock};
}

mod private {
    use super::{Dial9Handle, SpanId};

    pub trait SpanImpl {
        fn span_id(&self) -> SpanId;
        fn set_parent(&mut self, parent_span_id: SpanId);
        fn emit_enter(&self, handle: &Dial9Handle);
        fn emit_exit(
            &self,
            handle: &Dial9Handle,
            active_ns: u64,
            idle_ns: u64,
            poll_count: u64,
            completed: bool,
        );
    }
}

/// A named, optionally-parented region of work recorded into the trace with
/// timing information.
///
/// Construct one with the [`dial9_span!`](crate::dial9_span) macro (which
/// captures the call site and typed fields for you), or with [`Dial9Span::new`]
/// for a name-only span whose name is chosen at runtime.
///
/// Timing is recorded when you attach the span to a future with
/// [`Instrument::instrument`], or open it over a synchronous scope with
/// [`enter`](Self::enter). A `SpanCloseEvent` is recorded when the span is
/// dropped, telling the viewer the span is complete — so span types are
/// intentionally **not** `Clone`: one identity, one close.
///
/// This trait is sealed: user code can use it as a bound and call its methods,
/// but cannot implement it. This keeps the event lifecycle under dial9's
/// control and allows its internal emission protocol to evolve without adding
/// required methods to a public extension point.
///
/// ```compile_fail
/// use dial9_utils::span::Span;
///
/// struct CustomSpan;
/// impl Span for CustomSpan {}
/// ```
#[allow(private_bounds)]
pub trait Span: private::SpanImpl + Sized {
    /// This span's process-unique id, for explicit parenting via
    /// [`with_parent_id`](Self::with_parent_id).
    fn id(&self) -> SpanId {
        private::SpanImpl::span_id(self)
    }

    /// Set this span's parent explicitly. The viewer nests the child under the
    /// parent; without a parent it infers nesting from timestamp containment.
    #[must_use]
    fn with_parent_id(mut self, parent_span_id: SpanId) -> Self {
        private::SpanImpl::set_parent(&mut self, parent_span_id);
        self
    }

    /// Set this span's parent from another span (see [`with_parent_id`](Self::with_parent_id)).
    #[must_use]
    fn with_parent(self, parent: &impl Span) -> Self {
        let id = parent.id();
        self.with_parent_id(id)
    }

    /// Open the span over the current scope, recording a single enter/exit
    /// segment that runs until the returned guard is dropped.
    ///
    /// For synchronous code only — the guard is `!Send` so it cannot be held
    /// across `.await`.
    fn enter(&self) -> Entered<'_, Self> {
        // Acquire the current handle once and reuse it for the matching exit:
        // the guard is `!Send`, so exit lands on this same thread.
        let handle = Dial9Handle::current();
        let enter_ns = clock_monotonic_ns();
        private::SpanImpl::emit_enter(self, &handle);
        Entered {
            span: self,
            handle,
            enter_ns,
            _not_send: PhantomData,
        }
    }
}

impl<T: private::SpanImpl> Span for T {}

/// A guard representing an open span scope, returned by [`Span::enter`].
///
/// The span's exit segment is recorded when this guard is dropped. `Entered` is
/// `!Send` — like [`tracing::span::Entered`] — so holding one across an `.await`
/// in a `Send` task is a compile error: its exit must land on the entering
/// thread, ordered after the enter. Use [`Instrument::instrument`] for futures.
///
/// [`tracing::span::Entered`]: https://docs.rs/tracing/latest/tracing/span/struct.Entered.html
#[must_use = "the span is exited as soon as the guard is dropped; bind it to a variable"]
pub struct Entered<'a, S: Span> {
    span: &'a S,
    /// The handle captured at enter, reused for the matching exit (safe because
    /// the guard is `!Send`, so exit runs on the entering thread).
    handle: Dial9Handle,
    /// Enter timestamp, so the exit can report the scope's active duration.
    enter_ns: u64,
    /// Makes the guard `!Send` (a raw pointer is neither `Send` nor `Sync`).
    _not_send: PhantomData<*const ()>,
}

impl<S: Span> fmt::Debug for Entered<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entered").finish_non_exhaustive()
    }
}

impl<S: Span> Drop for Entered<'_, S> {
    fn drop(&mut self) {
        // A synchronous scope is one contiguous on-CPU segment: active = its
        // wall duration, no idle, one "poll", completed.
        let active_ns = clock_monotonic_ns().saturating_sub(self.enter_ns);
        private::SpanImpl::emit_exit(self.span, &self.handle, active_ns, 0, 1, true);
    }
}

// ── Shared emit helpers (used by macro-generated spans and `Dial9Span`) ───────

/// The current Tokio task id, for the `dial9.tokio.task_id` wire field. Used by
/// the [`dial9_span!`](crate::dial9_span) expansion.
///
/// `tokio::task::Id` is opaque, but its `Hash` implementation writes the
/// underlying id as one `u64`, matching dial9's poll events. Outside a Tokio
/// task this is `None`, so the optional field is absent on the wire.
#[doc(hidden)]
pub fn current_task_id() -> Option<u64> {
    use std::hash::{Hash, Hasher};

    struct U64Extractor(u64);

    impl Hasher for U64Extractor {
        fn finish(&self) -> u64 {
            self.0
        }

        fn write(&mut self, _bytes: &[u8]) {
            debug_assert!(false, "tokio task id hashed as bytes")
        }

        fn write_u64(&mut self, value: u64) {
            self.0 = value;
        }
    }

    tokio::task::try_id().map(|id| {
        let mut extractor = U64Extractor(0);
        id.hash(&mut extractor);
        extractor.finish()
    })
}

/// Record a span's `SpanCloseEvent`. No-op off a dial9 runtime.
fn emit_close(span_id: SpanId) {
    let handle = Dial9Handle::current();
    if !handle.is_enabled() {
        return;
    }
    handle.record_event(wire::SpanCloseEvent {
        timestamp_ns: clock_monotonic_ns(),
        span_id,
    });
}

/// Library-owned carrier constructed by [`dial9_span!`](crate::dial9_span).
///
/// Its generic parameters are the call-site-specific name, field payload, and
/// two statically dispatched emitter closures. Keeping this type in the library
/// lets [`Span`] remain sealed without boxing the generated event path.
#[doc(hidden)]
pub struct MacroSpan<N, P, Enter, Exit> {
    span_id: SpanId,
    parent_span_id: Option<SpanId>,
    name: N,
    payload: P,
    emit_enter: Enter,
    emit_exit: Exit,
}

impl<N, P, Enter, Exit> MacroSpan<N, P, Enter, Exit> {
    /// Construct a macro-generated span. Not a stable API.
    #[doc(hidden)]
    pub fn new(name: N, payload: P, emit_enter: Enter, emit_exit: Exit) -> Self {
        Self {
            span_id: wire::next_span_id(),
            parent_span_id: None,
            name,
            payload,
            emit_enter,
            emit_exit,
        }
    }
}

impl<N, P, Enter, Exit> fmt::Debug for MacroSpan<N, P, Enter, Exit> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacroSpan")
            .field("span_id", &self.span_id)
            .field("parent_span_id", &self.parent_span_id)
            .finish_non_exhaustive()
    }
}

impl<N, P, Enter, Exit> private::SpanImpl for MacroSpan<N, P, Enter, Exit>
where
    N: AsRef<str>,
    Enter: Fn(&P, &str, SpanId, Option<SpanId>, &Dial9Handle),
    Exit: Fn(&P, &str, SpanId, &Dial9Handle, u64, u64, u64, bool),
{
    fn span_id(&self) -> SpanId {
        self.span_id
    }

    fn set_parent(&mut self, parent_span_id: SpanId) {
        self.parent_span_id = Some(parent_span_id);
    }

    fn emit_enter(&self, handle: &Dial9Handle) {
        (self.emit_enter)(
            &self.payload,
            self.name.as_ref(),
            self.span_id,
            self.parent_span_id,
            handle,
        );
    }

    fn emit_exit(
        &self,
        handle: &Dial9Handle,
        active_ns: u64,
        idle_ns: u64,
        poll_count: u64,
        completed: bool,
    ) {
        (self.emit_exit)(
            &self.payload,
            self.name.as_ref(),
            self.span_id,
            handle,
            active_ns,
            idle_ns,
            poll_count,
            completed,
        );
    }
}

impl<N, P, Enter, Exit> Drop for MacroSpan<N, P, Enter, Exit> {
    fn drop(&mut self) {
        emit_close(self.span_id);
    }
}

// ── Name-only runtime span ────────────────────────────────────────────────────

/// A span whose name is chosen at runtime and which carries no user fields.
///
/// When the fields are known at the call site, prefer the
/// [`dial9_span!`](crate::dial9_span) macro (typed fields, zero-cost). This type
/// is for names assembled at runtime, e.g. inside a
/// [`Dial9SpanLayer`](crate::tower::Dial9SpanLayer) `make_span` closure. All
/// name-only spans share one wire schema.
pub struct Dial9Span {
    span_id: SpanId,
    parent_span_id: Option<SpanId>,
    name: String,
    /// `file:line` of the `Dial9Span::new` call, formatted once so re-emitting
    /// the enter event on every poll stays allocation-free.
    location: String,
}

#[derive(TraceEvent)]
#[traceevent(name = "SpanEnter:dial9_utils::runtime:runtime:0")]
struct RuntimeEnter {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    #[traceevent(name = "dial9.tokio.task_id", role = "tokio.task_id")]
    task_id: Option<u64>,
    span_id: SpanId,
    parent_span_id: Option<u64>,
    #[traceevent(role = "span.name")]
    span_name: InternedString,
    /// Source location the span was created at, as `file:line`.
    location: InternedString,
}

#[derive(TraceEvent)]
#[traceevent(name = "SpanExit:dial9_utils::runtime:runtime:0")]
struct RuntimeExit {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    #[traceevent(name = "dial9.tokio.task_id", role = "tokio.task_id")]
    task_id: Option<u64>,
    span_id: SpanId,
    #[traceevent(role = "span.name")]
    span_name: InternedString,
    #[traceevent(unit = "ns")]
    active_ns: u64,
    #[traceevent(unit = "ns")]
    idle_ns: u64,
    poll_count: u64,
    /// Whether the span ended cleanly: `true` when the instrumented future ran
    /// to completion (or a sync scope's guard was dropped normally), `false`
    /// when the future was dropped before finishing, i.e. cancelled.
    completed: bool,
}

impl Dial9Span {
    /// Create a name-only span, recorded with the caller's source location.
    #[track_caller]
    pub fn new(name: impl Into<String>) -> Self {
        let caller = std::panic::Location::caller();
        Self {
            span_id: wire::next_span_id(),
            parent_span_id: None,
            name: name.into(),
            location: format!("{}:{}", caller.file(), caller.line()),
        }
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

impl private::SpanImpl for Dial9Span {
    fn span_id(&self) -> SpanId {
        self.span_id
    }

    fn set_parent(&mut self, parent_span_id: SpanId) {
        self.parent_span_id = Some(parent_span_id);
    }

    fn emit_enter(&self, handle: &Dial9Handle) {
        handle.with_encoder(|enc| {
            let span_name = enc.intern_string(&self.name);
            let location = enc.intern_string(&self.location);
            enc.encode(&RuntimeEnter {
                timestamp_ns: clock_monotonic_ns(),
                task_id: current_task_id(),
                span_id: self.span_id,
                // `Option<SpanId>` is not a `TraceField` (the blanket optional
                // impl is internal to dial9-trace-format), so the parent rides
                // the wire as the raw id it already was.
                parent_span_id: self.parent_span_id.map(SpanId::as_u64),
                span_name,
                location,
            });
        });
    }

    fn emit_exit(
        &self,
        handle: &Dial9Handle,
        active_ns: u64,
        idle_ns: u64,
        poll_count: u64,
        completed: bool,
    ) {
        handle.with_encoder(|enc| {
            let span_name = enc.intern_string(&self.name);
            enc.encode(&RuntimeExit {
                timestamp_ns: clock_monotonic_ns(),
                task_id: current_task_id(),
                span_id: self.span_id,
                span_name,
                active_ns,
                idle_ns,
                poll_count,
                completed,
            });
        });
    }
}

impl Drop for Dial9Span {
    fn drop(&mut self) {
        emit_close(self.span_id);
    }
}

// ── Late fields ───────────────────────────────────────────────────────────────

/// A write-once handle to a *late* span field, declared as `name: Type` in
/// [`dial9_span!`](crate::dial9_span). Set it any time before the span
/// completes; the value is recorded on the span's completion (exit) event as
/// `Some(value)`, or `None` if it was never set. First write wins (it wraps a
/// [`OnceLock`]).
///
/// The macro hands these back alongside the span — `let (span, slots) =
/// dial9_span!(..)` — but only when at least one late field is declared. A span
/// with only eager fields is returned bare, exactly as before.
pub struct Slot<T> {
    cell: Arc<OnceLock<T>>,
}

impl<T> Slot<T> {
    /// Record this field's value. Only the first call takes effect; later
    /// calls are ignored, like the [`OnceLock`] this wraps.
    pub fn set(&self, value: T) {
        // First-write-wins: a later `set` hands back the rejected value, which
        // we intentionally drop — the slot already holds its value.
        let _ = self.cell.set(value);
    }

    /// Wrap the shared cell the macro also stored in the span. Not a stable API.
    #[doc(hidden)]
    pub fn __from_arc(cell: Arc<OnceLock<T>>) -> Self {
        Self { cell }
    }
}

impl<T> fmt::Debug for Slot<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slot")
            .field("set", &self.cell.get().is_some())
            .finish()
    }
}

// ── The `dial9_span!` macro ───────────────────────────────────────────────────

/// Construct a span, capturing the call site and typed fields.
///
/// This is the ergonomic, zero-cost way to create a span: it generates a
/// dedicated [`TraceEvent`](dial9_trace_format::TraceEvent) type per call site,
/// so emitting is as cheap as a hand-written event and fields retain their
/// declared wire types.
///
/// ```
/// use dial9_utils::dial9_span;
///
/// // Just a name:
/// let span = dial9_span!("load_config");
///
/// // Typed fields preserve their declared type; `%` is Display and `?` is
/// // Debug:
/// # let retries = 3u32;
/// # let path = "/etc/app.toml";
/// let span = dial9_span!("load_config", retries: u32 = retries, path = %path);
/// # #[derive(Debug)] struct Cfg;
/// # let cfg = Cfg;
/// let span = dial9_span!("validate", config = ?cfg);
///
/// // The span name may also be owned and selected at runtime:
/// let operation = String::from("load_config");
/// let span = dial9_span!(operation, retries: u32 = retries);
/// ```
///
/// An eager field is written `name: Type = value` and keeps its Rust type on
/// the wire, so the type must implement
/// [`TraceField`](dial9_trace_format::TraceField) (`u64`, `i64`, `bool`, `f64`,
/// `String`, …). The type is required — it makes the generated event struct
/// concrete rather than generic. Use `%`/`?` to render any `Display`/`Debug`
/// value to an owned `String` without naming a type. The span name may be any
/// value implementing [`AsRef<str>`], including a string literal, borrowed
/// string, or owned `String`.
///
/// ## Late fields
///
/// A field declared as `name: Type` (a type instead of `= value`) is *late*:
/// its value is unknown at the call site and set later, before the span
/// completes. Declaring any late field changes the macro's return to
/// `(span, slots)`; set values through `slots`, and they land on the span's
/// completion (exit) event as `Some(value)` — or `None` if never set.
///
/// ```
/// use dial9_utils::dial9_span;
/// use dial9_utils::span::Instrument as _;
/// # async fn handle() -> u16 { 200 }
/// # async fn demo() {
/// let (span, slots) = dial9_span!("request", route: &'static str = "/checkout", status: u16);
/// async move {
///     let code = handle().await;
///     slots.status.set(code); // recorded on completion
/// }
/// .instrument(span)
/// .await;
/// # }
/// ```
///
/// The late field's type must satisfy `Option<Type>: TraceField` — the numeric
/// types, `bool`, and `String`. A late field costs one shared allocation; spans
/// with only eager fields are returned bare and stay allocation-free.
#[macro_export]
macro_rules! dial9_span {
    ($name:expr $(,)?) => {
        $crate::__dial9_span_build!($name; [])
    };
    ($name:expr, $($fields:tt)+) => {
        $crate::__dial9_span_munch!($name; [] ; [] ; $($fields)+)
    };
}

/// Token-muncher that sorts each field into one of two accumulators: eager
/// `(key : type = value)` triples (from `key: Type = value`, or the `%`/`?`
/// sigils which capture an owned `String`) and late `(key : type)` pairs
/// (declared as `key: Type`). The build step reads both. Each eager field
/// carries its declared type, so the generated event structs are concrete —
/// no generic type parameters.
#[doc(hidden)]
#[macro_export]
macro_rules! __dial9_span_munch {
    // Done, no late fields: the classic build, returning the span bare.
    ($name:expr; [$($eager:tt)*] ; [] ; ) => {
        $crate::__dial9_span_build!($name; [$($eager)*])
    };
    // Done, with late fields: build with slots, returning `(span, slots)`.
    ($name:expr; [$($eager:tt)*] ; [$($late:tt)+] ; ) => {
        $crate::__dial9_span_build_late!($name; [$($eager)*] ; [$($late)+])
    };
    // Eager typed field: `key: Type = value`. Keeps its declared type on the
    // wire (a `u64` stays a `Varint`). The type is required so the event struct
    // field is concrete rather than a generic parameter.
    ($name:expr; [$($eager:tt)*] ; [$($late:tt)*] ; $key:ident : $ty:ty = $val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($eager)* ($key : $ty = $val)] ; [$($late)*] ; $($($rest)*)?
        )
    };
    // Late field: `key: Type` (a type, no `= value`).
    ($name:expr; [$($eager:tt)*] ; [$($late:tt)*] ; $key:ident : $ty:ty $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($eager)*] ; [$($late)* ($key : $ty)] ; $($($rest)*)?
        )
    };
    // Eager `%` (Display) / `?` (Debug): captured as an owned `String`.
    ($name:expr; [$($eager:tt)*] ; [$($late:tt)*] ; $key:ident = %$val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($eager)* ($key : ::std::string::String = ::std::string::ToString::to_string(&$val))] ; [$($late)*] ; $($($rest)*)?
        )
    };
    ($name:expr; [$($eager:tt)*] ; [$($late:tt)*] ; $key:ident = ?$val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($eager)* ($key : ::std::string::String = ::std::format!("{:?}", $val))] ; [$($late)*] ; $($($rest)*)?
        )
    };
}

/// Build step: define the per-call-site concrete span + enter/exit
/// [`TraceEvent`](dial9_trace_format::TraceEvent) structs and construct the span.
#[doc(hidden)]
#[macro_export]
macro_rules! __dial9_span_build {
    ($name:expr; [$( ($key:ident : $ty:ty = $val:expr) )*]) => {{
        // The call site, so a span in the viewer points back at the code that
        // opened it (the schema name is per call site too, but is not shown).
        const __DIAL9_LOCATION: &str =
            ::core::concat!(::core::file!(), ":", ::core::line!());

        // Each eager field carries its declared type (`key: Type = value`), so
        // the event structs are concrete — no generic parameters — and each
        // field keeps its type on the wire (a `u64` stays a `Varint`, not a
        // string). The wire schema name follows the viewer's legacy span-name
        // grammar and remains unique per call site through file, line, and
        // column.
        #[derive($crate::span::__rt::TraceEvent)]
        #[traceevent(name = ::core::concat!(
            "SpanEnter:dial9_utils::adhoc_",
            ::core::column!(),
            ":",
            ::core::file!(),
            ":",
            ::core::line!()
        ))]
        #[allow(non_camel_case_types, non_snake_case)]
        struct __Dial9Enter {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            #[traceevent(name = "dial9.tokio.task_id", role = "tokio.task_id")]
            task_id: ::core::option::Option<u64>,
            span_id: $crate::span::__rt::SpanId,
            parent_span_id: ::core::option::Option<u64>,
            #[traceevent(role = "span.name")]
            span_name: $crate::span::__rt::InternedString,
            location: $crate::span::__rt::InternedString,
            $( $key: $ty, )*
        }

        #[derive($crate::span::__rt::TraceEvent)]
        #[traceevent(name = ::core::concat!(
            "SpanExit:dial9_utils::adhoc_",
            ::core::column!(),
            ":",
            ::core::file!(),
            ":",
            ::core::line!()
        ))]
        #[allow(non_camel_case_types, non_snake_case)]
        struct __Dial9Exit {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            #[traceevent(name = "dial9.tokio.task_id", role = "tokio.task_id")]
            task_id: ::core::option::Option<u64>,
            span_id: $crate::span::__rt::SpanId,
            #[traceevent(role = "span.name")]
            span_name: $crate::span::__rt::InternedString,
            #[traceevent(unit = "ns")]
            active_ns: u64,
            #[traceevent(unit = "ns")]
            idle_ns: u64,
            poll_count: u64,
            completed: bool,
            $( $key: $ty, )*
        }

        #[allow(non_camel_case_types, non_snake_case)]
        struct __Dial9Fields {
            $( $key: $ty, )*
        }

        $crate::span::__rt::MacroSpan::new(
            $name,
            __Dial9Fields {
                $( $key: $val, )*
            },
            |
                __fields: &__Dial9Fields,
                __span_name: &str,
                __span_id: $crate::span::__rt::SpanId,
                __parent_span_id: ::core::option::Option<$crate::span::__rt::SpanId>,
                __h: &$crate::span::__rt::Dial9Handle,
            | {
                __h.with_encoder(|__enc| {
                    let __name = __enc.intern_string(__span_name);
                    let __loc = __enc.intern_string(__DIAL9_LOCATION);
                    __enc.encode(&__Dial9Enter {
                        timestamp_ns: $crate::span::__rt::clock_monotonic_ns(),
                        task_id: $crate::span::__rt::current_task_id(),
                        span_id: __span_id,
                        parent_span_id: ::core::option::Option::map(
                            __parent_span_id,
                            $crate::span::__rt::SpanId::as_u64,
                        ),
                        span_name: __name,
                        location: __loc,
                        $( $key: ::core::clone::Clone::clone(&__fields.$key), )*
                    });
                });
            },
            |
                __fields: &__Dial9Fields,
                __span_name: &str,
                __span_id: $crate::span::__rt::SpanId,
                __h: &$crate::span::__rt::Dial9Handle,
                active_ns: u64,
                idle_ns: u64,
                poll_count: u64,
                completed: bool,
            | {
                __h.with_encoder(|__enc| {
                    let __name = __enc.intern_string(__span_name);
                    __enc.encode(&__Dial9Exit {
                        timestamp_ns: $crate::span::__rt::clock_monotonic_ns(),
                        task_id: $crate::span::__rt::current_task_id(),
                        span_id: __span_id,
                        span_name: __name,
                        active_ns,
                        idle_ns,
                        poll_count,
                        completed,
                        $( $key: ::core::clone::Clone::clone(&__fields.$key), )*
                    });
                });
            },
        )
    }};
}

/// Build step for spans with one or more *late* fields. Mirrors
/// [`__dial9_span_build!`](crate::__dial9_span_build), but the exit event gains
/// an `Option<Type>` column per late field, the span holds a shared
/// `Arc<OnceLock<Type>>` for each, and the macro returns `(span, slots)` where
/// `slots` exposes a [`Slot`](crate::span::Slot) per late field. Enter still
/// carries only eager fields — late values aren't set yet.
#[doc(hidden)]
#[macro_export]
macro_rules! __dial9_span_build_late {
    (
        $name:expr;
        [$( ($key:ident : $ty:ty = $val:expr) )*] ;
        [$( ($lkey:ident : $lty:ty) )+]
    ) => {{
        // The call site, so a span in the viewer points back at the code that
        // opened it (the schema name is per call site too, but is not shown).
        const __DIAL9_LOCATION: &str =
            ::core::concat!(::core::file!(), ":", ::core::line!());

        // Enter: eager fields only (late fields have no value yet).
        #[derive($crate::span::__rt::TraceEvent)]
        #[traceevent(name = ::core::concat!(
            "SpanEnter:dial9_utils::adhoc_",
            ::core::column!(),
            ":",
            ::core::file!(),
            ":",
            ::core::line!()
        ))]
        #[allow(non_camel_case_types, non_snake_case)]
        struct __Dial9Enter {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            #[traceevent(name = "dial9.tokio.task_id", role = "tokio.task_id")]
            task_id: ::core::option::Option<u64>,
            span_id: $crate::span::__rt::SpanId,
            parent_span_id: ::core::option::Option<u64>,
            #[traceevent(role = "span.name")]
            span_name: $crate::span::__rt::InternedString,
            location: $crate::span::__rt::InternedString,
            $( $key: $ty, )*
        }

        // Exit: eager fields, then each late field as `Option<Type>`.
        #[derive($crate::span::__rt::TraceEvent)]
        #[traceevent(name = ::core::concat!(
            "SpanExit:dial9_utils::adhoc_",
            ::core::column!(),
            ":",
            ::core::file!(),
            ":",
            ::core::line!()
        ))]
        #[allow(non_camel_case_types, non_snake_case)]
        struct __Dial9Exit {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            #[traceevent(name = "dial9.tokio.task_id", role = "tokio.task_id")]
            task_id: ::core::option::Option<u64>,
            span_id: $crate::span::__rt::SpanId,
            #[traceevent(role = "span.name")]
            span_name: $crate::span::__rt::InternedString,
            #[traceevent(unit = "ns")]
            active_ns: u64,
            #[traceevent(unit = "ns")]
            idle_ns: u64,
            poll_count: u64,
            completed: bool,
            $( $key: $ty, )*
            $( $lkey: ::core::option::Option<$lty>, )+
        }

        #[allow(non_camel_case_types, non_snake_case)]
        struct __Dial9Fields {
            $( $key: $ty, )*
            $( $lkey: $crate::span::__rt::Arc<$crate::span::__rt::OnceLock<$lty>>, )+
        }

        // One `Slot<Type>` per late field, each sharing the span's cell.
        #[allow(non_camel_case_types, non_snake_case)]
        struct __Dial9Slots {
            $( $lkey: $crate::span::__rt::Slot<$lty>, )+
        }

        // Allocate each shared cell once, then hand one clone to the span and
        // one to the slots.
        $(
            let $lkey: $crate::span::__rt::Arc<$crate::span::__rt::OnceLock<$lty>> =
                $crate::span::__rt::Arc::new($crate::span::__rt::OnceLock::new());
        )+

        let __dial9_span = $crate::span::__rt::MacroSpan::new(
            $name,
            __Dial9Fields {
                $( $key: $val, )*
                $( $lkey: ::core::clone::Clone::clone(&$lkey), )+
            },
            |
                __fields: &__Dial9Fields,
                __span_name: &str,
                __span_id: $crate::span::__rt::SpanId,
                __parent_span_id: ::core::option::Option<$crate::span::__rt::SpanId>,
                __h: &$crate::span::__rt::Dial9Handle,
            | {
                __h.with_encoder(|__enc| {
                    let __name = __enc.intern_string(__span_name);
                    let __loc = __enc.intern_string(__DIAL9_LOCATION);
                    __enc.encode(&__Dial9Enter {
                        timestamp_ns: $crate::span::__rt::clock_monotonic_ns(),
                        task_id: $crate::span::__rt::current_task_id(),
                        span_id: __span_id,
                        parent_span_id: ::core::option::Option::map(
                            __parent_span_id,
                            $crate::span::__rt::SpanId::as_u64,
                        ),
                        span_name: __name,
                        location: __loc,
                        $( $key: ::core::clone::Clone::clone(&__fields.$key), )*
                    });
                });
            },
            |
                __fields: &__Dial9Fields,
                __span_name: &str,
                __span_id: $crate::span::__rt::SpanId,
                __h: &$crate::span::__rt::Dial9Handle,
                active_ns: u64,
                idle_ns: u64,
                poll_count: u64,
                completed: bool,
            | {
                __h.with_encoder(|__enc| {
                    let __name = __enc.intern_string(__span_name);
                    __enc.encode(&__Dial9Exit {
                        timestamp_ns: $crate::span::__rt::clock_monotonic_ns(),
                        task_id: $crate::span::__rt::current_task_id(),
                        span_id: __span_id,
                        span_name: __name,
                        active_ns,
                        idle_ns,
                        poll_count,
                        completed,
                        $( $key: ::core::clone::Clone::clone(&__fields.$key), )*
                        $( $lkey: __fields.$lkey.get().cloned(), )+
                    });
                });
            },
        );
        let __dial9_slots = __Dial9Slots {
            $( $lkey: $crate::span::__rt::Slot::__from_arc($lkey), )+
        };
        (__dial9_span, __dial9_slots)
    }};
}

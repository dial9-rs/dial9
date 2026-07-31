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
//! [`dial9_span!`] generates a monomorphized [`TraceEvent`](dial9_trace_format::TraceEvent)
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
//! .instrument(dial9_span!("handle_request", request_id = req))
//! .await
//! # }
//! ```
//!
//! # Fields
//!
//! The [`dial9_span!`] macro captures typed key/value fields with
//! `tracing`-style syntax: a bare value keeps its type (a `u64` is a `Varint`
//! on the wire), `%` formats via [`Display`] and `?` via [`Debug`] (both
//! producing a `String`):
//!
//! ```no_run
//! # use dial9_utils::dial9_span;
//! # let retries = 1u32; let path = "/x";
//! # #[derive(Debug)] struct Cfg; let cfg = Cfg;
//! let span = dial9_span!("load", retries = retries, path = %path, config = ?cfg);
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
//! Do **not** hold an [`Entered`] guard across an `.await` point — it is `!Send`
//! for exactly that reason. Use [`Instrument::instrument`] for futures instead.
//!
//! # Tower middleware
//!
//! With the `tower` feature, [`Dial9SpanLayer`] wraps a
//! [`tower`](https://docs.rs/tower) service so each request future is
//! instrumented automatically.

mod future;
#[cfg(feature = "tower")]
mod tower;
pub(crate) mod wire;

pub use future::{Instrument, Instrumented};
#[cfg(feature = "tower")]
#[cfg_attr(docsrs, doc(cfg(feature = "tower")))]
pub use tower::{
    Dial9SpanLayer, Dial9SpanLayerWithResponse, Dial9SpanService, Dial9SpanServiceWithResponse,
    OnResponse,
};

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
    pub use super::wire::next_span_id;
    pub use super::{Slot, Span, current_worker_id_u64, emit_close};
    pub use dial9_core::clock::clock_monotonic_ns;
    pub use dial9_core::handle::Dial9Handle;
    pub use dial9_trace_format::{InternedString, TraceEvent, TraceField};
    pub use std::sync::{Arc, OnceLock};
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
pub trait Span: Sized {
    /// This span's process-unique id, for explicit parenting via
    /// [`with_parent_id`](Self::with_parent_id).
    fn id(&self) -> u64;

    /// Set the parent span id. Implementation detail of the parenting setters.
    #[doc(hidden)]
    fn __set_parent(&mut self, parent_span_id: u64);

    /// Emit the enter event on `handle`. Implementation detail of
    /// [`enter`](Self::enter) and [`Instrumented`].
    #[doc(hidden)]
    fn __emit_enter(&self, handle: &Dial9Handle);

    /// Emit the exit event on `handle`, carrying the aggregate timing the span
    /// observed: `active_ns` (time on-CPU, i.e. summed poll durations),
    /// `idle_ns` (time suspended between polls), `poll_count`, and whether the
    /// future ran to completion (`true`) or was cancelled (`false`). For the
    /// synchronous guard these are `(duration, 0, 1, true)`.
    #[doc(hidden)]
    fn __emit_exit(
        &self,
        handle: &Dial9Handle,
        active_ns: u64,
        idle_ns: u64,
        poll_count: u64,
        completed: bool,
    );

    /// Set this span's parent explicitly. The viewer nests the child under the
    /// parent; without a parent it infers nesting from timestamp containment.
    #[must_use]
    fn with_parent_id(mut self, parent_span_id: u64) -> Self {
        self.__set_parent(parent_span_id);
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
        self.__emit_enter(&handle);
        Entered {
            span: self,
            handle,
            enter_ns,
            _not_send: PhantomData,
        }
    }
}

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
        self.span.__emit_exit(&self.handle, active_ns, 0, 1, true);
    }
}

// ── Shared emit helpers (used by macro-generated spans and `Dial9Span`) ───────

/// Sentinel `worker_id` for a thread that isn't a known Tokio worker (or when
/// the `tokio` feature is off). Matches the tracing layer's "unknown" id.
const WORKER_ID_UNKNOWN: u64 = u64::MAX;

/// The current Tokio worker index as a raw `u64`, for the `worker_id` wire
/// field. Used by the [`dial9_span!`](crate::dial9_span) expansion.
///
/// Reads [`tokio::runtime::worker_index`]; off a runtime (e.g. a plain thread,
/// or code not running under Tokio) it is [`WORKER_ID_UNKNOWN`], so spans work
/// regardless of whether Tokio is in use.
#[doc(hidden)]
pub fn current_worker_id_u64() -> u64 {
    tokio::runtime::worker_index()
        .map(|i| i as u64)
        .unwrap_or(WORKER_ID_UNKNOWN)
}

/// Record a span's `SpanCloseEvent`. No-op off a dial9 runtime. Used by the
/// [`dial9_span!`](crate::dial9_span) expansion; not a stable API.
#[doc(hidden)]
pub fn emit_close(span_id: u64) {
    let handle = Dial9Handle::current();
    if !handle.is_enabled() {
        return;
    }
    handle.record_event(wire::SpanCloseEvent {
        timestamp_ns: clock_monotonic_ns(),
        span_id,
    });
}

// ── Name-only runtime span ────────────────────────────────────────────────────

/// A span whose name is chosen at runtime and which carries no user fields.
///
/// When the fields are known at the call site, prefer the
/// [`dial9_span!`](crate::dial9_span) macro (typed fields, zero-cost). This type
/// is for names assembled at runtime, e.g. by
/// [`Dial9SpanLayer::named`](crate::span::Dial9SpanLayer::named). All name-only
/// spans share one wire schema (`adhoc::runtime`).
pub struct Dial9Span {
    span_id: u64,
    parent_span_id: Option<u64>,
    name: String,
}

#[derive(TraceEvent)]
#[traceevent(name = "SpanEnter:adhoc::runtime")]
struct RuntimeEnter {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u64,
    span_id: u64,
    parent_span_id: Option<u64>,
    span_name: InternedString,
}

#[derive(TraceEvent)]
#[traceevent(name = "SpanExit:adhoc::runtime")]
struct RuntimeExit {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u64,
    span_id: u64,
    span_name: InternedString,
    #[traceevent(unit = "ns")]
    active_ns: u64,
    #[traceevent(unit = "ns")]
    idle_ns: u64,
    poll_count: u64,
    completed: bool,
}

impl Dial9Span {
    /// Create a name-only span.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            span_id: wire::next_span_id(),
            parent_span_id: None,
            name: name.into(),
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

impl Span for Dial9Span {
    fn id(&self) -> u64 {
        self.span_id
    }

    fn __set_parent(&mut self, parent_span_id: u64) {
        self.parent_span_id = Some(parent_span_id);
    }

    fn __emit_enter(&self, handle: &Dial9Handle) {
        handle.with_encoder(|enc| {
            let span_name = enc.intern_string(&self.name);
            enc.encode(&RuntimeEnter {
                timestamp_ns: clock_monotonic_ns(),
                worker_id: current_worker_id_u64(),
                span_id: self.span_id,
                parent_span_id: self.parent_span_id,
                span_name,
            });
        });
    }

    fn __emit_exit(
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
                worker_id: current_worker_id_u64(),
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
/// monomorphized [`TraceEvent`](dial9_trace_format::TraceEvent) per call site,
/// so emitting is as cheap as a hand-written event and numeric fields stay
/// numeric on the wire.
///
/// Field syntax mirrors `tracing`:
///
/// ```
/// use dial9_utils::dial9_span;
///
/// // Just a name:
/// let span = dial9_span!("load_config");
///
/// // Typed fields — bare keeps the type (Varint), `%` is Display, `?` is Debug:
/// # let retries = 3u32;
/// # let path = "/etc/app.toml";
/// let span = dial9_span!("load_config", retries = retries, path = %path);
/// # #[derive(Debug)] struct Cfg;
/// # let cfg = Cfg;
/// let span = dial9_span!("validate", config = ?cfg);
/// ```
///
/// A bare value keeps its Rust type, so it must implement
/// [`TraceField`](dial9_trace_format::TraceField) (`u64`, `i64`, `bool`, `f64`,
/// `String`, …) and be `'static`; use `%`/`?` to render any `Display`/`Debug`
/// value to an owned `String`. The span name must be a `&'static str`
/// expression (for a runtime name, use [`Dial9Span::new`](span::Dial9Span::new)).
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
/// let (span, slots) = dial9_span!("request", route = "/checkout", status: u16);
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
/// `(key : value_expr)` pairs (handling the `%`/`?`/bare sigils) and late
/// `(key : type)` pairs (declared as `key: Type`). The build step reads both.
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
    // Late field: `key: Type` (a type, no `= value`).
    ($name:expr; [$($eager:tt)*] ; [$($late:tt)*] ; $key:ident : $ty:ty $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($eager)*] ; [$($late)* ($key : $ty)] ; $($($rest)*)?
        )
    };
    // Eager `%` (Display), `?` (Debug), or bare (keeps its Rust type).
    ($name:expr; [$($eager:tt)*] ; [$($late:tt)*] ; $key:ident = %$val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($eager)* ($key : ::std::string::ToString::to_string(&$val))] ; [$($late)*] ; $($($rest)*)?
        )
    };
    ($name:expr; [$($eager:tt)*] ; [$($late:tt)*] ; $key:ident = ?$val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($eager)* ($key : ::std::format!("{:?}", $val))] ; [$($late)*] ; $($($rest)*)?
        )
    };
    ($name:expr; [$($eager:tt)*] ; [$($late:tt)*] ; $key:ident = $val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($eager)* ($key : $val)] ; [$($late)*] ; $($($rest)*)?
        )
    };
}

/// Build step: define the per-call-site monomorphized span + enter/exit
/// [`TraceEvent`](dial9_trace_format::TraceEvent) structs and construct the span.
#[doc(hidden)]
#[macro_export]
macro_rules! __dial9_span_build {
    ($name:expr; [$( ($key:ident : $val:expr) )*]) => {{
        // The span name must be a `&'static str` so it is baked into the
        // generated code rather than stored — a runtime name needs `Dial9Span`.
        const __DIAL9_NAME: &str = $name;

        // Each field's identifier doubles as its own type parameter (fields and
        // type params live in separate namespaces), so the field keeps its
        // concrete inferred type — a `u64` stays a `u64`/`Varint`, not a string.
        // The wire schema name is per call site (`SpanEnter:<file>:<line>:<col>`),
        // which the viewer groups on by its `SpanEnter:` prefix.
        #[derive($crate::span::__rt::TraceEvent)]
        #[traceevent(name = ::core::concat!(
            "SpanEnter:", ::core::file!(), ":", ::core::line!(), ":", ::core::column!()
        ))]
        #[allow(non_camel_case_types, non_snake_case)]
        struct __Dial9Enter<$($key: $crate::span::__rt::TraceField + ::core::clone::Clone + 'static),*> {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            worker_id: u64,
            span_id: u64,
            parent_span_id: ::core::option::Option<u64>,
            span_name: $crate::span::__rt::InternedString,
            $( $key: $key, )*
        }

        #[derive($crate::span::__rt::TraceEvent)]
        #[traceevent(name = ::core::concat!(
            "SpanExit:", ::core::file!(), ":", ::core::line!(), ":", ::core::column!()
        ))]
        #[allow(non_camel_case_types, non_snake_case)]
        struct __Dial9Exit<$($key: $crate::span::__rt::TraceField + ::core::clone::Clone + 'static),*> {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            worker_id: u64,
            span_id: u64,
            span_name: $crate::span::__rt::InternedString,
            #[traceevent(unit = "ns")]
            active_ns: u64,
            #[traceevent(unit = "ns")]
            idle_ns: u64,
            poll_count: u64,
            completed: bool,
            $( $key: $key, )*
        }

        #[allow(non_camel_case_types, non_snake_case)]
        struct __Dial9Span<$($key: $crate::span::__rt::TraceField + ::core::clone::Clone + 'static),*> {
            span_id: u64,
            parent_span_id: ::core::option::Option<u64>,
            $( $key: $key, )*
        }

        #[allow(non_camel_case_types, non_snake_case)]
        impl<$($key: $crate::span::__rt::TraceField + ::core::clone::Clone + 'static),*>
            $crate::span::__rt::Span for __Dial9Span<$($key),*>
        {
            fn id(&self) -> u64 {
                self.span_id
            }

            fn __set_parent(&mut self, parent_span_id: u64) {
                self.parent_span_id = ::core::option::Option::Some(parent_span_id);
            }

            fn __emit_enter(&self, __h: &$crate::span::__rt::Dial9Handle) {
                __h.with_encoder(|__enc| {
                    let __name = __enc.intern_string(__DIAL9_NAME);
                    __enc.encode(&__Dial9Enter {
                        timestamp_ns: $crate::span::__rt::clock_monotonic_ns(),
                        worker_id: $crate::span::__rt::current_worker_id_u64(),
                        span_id: self.span_id,
                        parent_span_id: self.parent_span_id,
                        span_name: __name,
                        $( $key: ::core::clone::Clone::clone(&self.$key), )*
                    });
                });
            }

            fn __emit_exit(
                &self,
                __h: &$crate::span::__rt::Dial9Handle,
                active_ns: u64,
                idle_ns: u64,
                poll_count: u64,
                completed: bool,
            ) {
                __h.with_encoder(|__enc| {
                    let __name = __enc.intern_string(__DIAL9_NAME);
                    __enc.encode(&__Dial9Exit {
                        timestamp_ns: $crate::span::__rt::clock_monotonic_ns(),
                        worker_id: $crate::span::__rt::current_worker_id_u64(),
                        span_id: self.span_id,
                        span_name: __name,
                        active_ns,
                        idle_ns,
                        poll_count,
                        completed,
                        $( $key: ::core::clone::Clone::clone(&self.$key), )*
                    });
                });
            }
        }

        #[allow(non_camel_case_types, non_snake_case)]
        impl<$($key: $crate::span::__rt::TraceField + ::core::clone::Clone + 'static),*>
            ::core::ops::Drop for __Dial9Span<$($key),*>
        {
            fn drop(&mut self) {
                $crate::span::__rt::emit_close(self.span_id);
            }
        }

        __Dial9Span {
            span_id: $crate::span::__rt::next_span_id(),
            parent_span_id: ::core::option::Option::None,
            $( $key: $val, )*
        }
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
        [$( ($key:ident : $val:expr) )*] ;
        [$( ($lkey:ident : $lty:ty) )+]
    ) => {{
        const __DIAL9_NAME: &str = $name;

        // Enter: eager fields only (late fields have no value yet).
        #[derive($crate::span::__rt::TraceEvent)]
        #[traceevent(name = ::core::concat!(
            "SpanEnter:", ::core::file!(), ":", ::core::line!(), ":", ::core::column!()
        ))]
        #[allow(non_camel_case_types, non_snake_case)]
        struct __Dial9Enter<$($key: $crate::span::__rt::TraceField + ::core::clone::Clone + 'static),*> {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            worker_id: u64,
            span_id: u64,
            parent_span_id: ::core::option::Option<u64>,
            span_name: $crate::span::__rt::InternedString,
            $( $key: $key, )*
        }

        // Exit: eager fields, then each late field as `Option<Type>`.
        #[derive($crate::span::__rt::TraceEvent)]
        #[traceevent(name = ::core::concat!(
            "SpanExit:", ::core::file!(), ":", ::core::line!(), ":", ::core::column!()
        ))]
        #[allow(non_camel_case_types, non_snake_case)]
        struct __Dial9Exit<$($key: $crate::span::__rt::TraceField + ::core::clone::Clone + 'static),*> {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            worker_id: u64,
            span_id: u64,
            span_name: $crate::span::__rt::InternedString,
            #[traceevent(unit = "ns")]
            active_ns: u64,
            #[traceevent(unit = "ns")]
            idle_ns: u64,
            poll_count: u64,
            completed: bool,
            $( $key: $key, )*
            $( $lkey: ::core::option::Option<$lty>, )+
        }

        #[allow(non_camel_case_types, non_snake_case)]
        struct __Dial9Span<$($key: $crate::span::__rt::TraceField + ::core::clone::Clone + 'static),*> {
            span_id: u64,
            parent_span_id: ::core::option::Option<u64>,
            $( $key: $key, )*
            $( $lkey: $crate::span::__rt::Arc<$crate::span::__rt::OnceLock<$lty>>, )+
        }

        #[allow(non_camel_case_types, non_snake_case)]
        impl<$($key: $crate::span::__rt::TraceField + ::core::clone::Clone + 'static),*>
            $crate::span::__rt::Span for __Dial9Span<$($key),*>
        {
            fn id(&self) -> u64 {
                self.span_id
            }

            fn __set_parent(&mut self, parent_span_id: u64) {
                self.parent_span_id = ::core::option::Option::Some(parent_span_id);
            }

            fn __emit_enter(&self, __h: &$crate::span::__rt::Dial9Handle) {
                __h.with_encoder(|__enc| {
                    let __name = __enc.intern_string(__DIAL9_NAME);
                    __enc.encode(&__Dial9Enter {
                        timestamp_ns: $crate::span::__rt::clock_monotonic_ns(),
                        worker_id: $crate::span::__rt::current_worker_id_u64(),
                        span_id: self.span_id,
                        parent_span_id: self.parent_span_id,
                        span_name: __name,
                        $( $key: ::core::clone::Clone::clone(&self.$key), )*
                    });
                });
            }

            fn __emit_exit(
                &self,
                __h: &$crate::span::__rt::Dial9Handle,
                active_ns: u64,
                idle_ns: u64,
                poll_count: u64,
                completed: bool,
            ) {
                __h.with_encoder(|__enc| {
                    let __name = __enc.intern_string(__DIAL9_NAME);
                    __enc.encode(&__Dial9Exit {
                        timestamp_ns: $crate::span::__rt::clock_monotonic_ns(),
                        worker_id: $crate::span::__rt::current_worker_id_u64(),
                        span_id: self.span_id,
                        span_name: __name,
                        active_ns,
                        idle_ns,
                        poll_count,
                        completed,
                        $( $key: ::core::clone::Clone::clone(&self.$key), )*
                        $( $lkey: self.$lkey.get().cloned(), )+
                    });
                });
            }
        }

        #[allow(non_camel_case_types, non_snake_case)]
        impl<$($key: $crate::span::__rt::TraceField + ::core::clone::Clone + 'static),*>
            ::core::ops::Drop for __Dial9Span<$($key),*>
        {
            fn drop(&mut self) {
                $crate::span::__rt::emit_close(self.span_id);
            }
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

        let __dial9_span = __Dial9Span {
            span_id: $crate::span::__rt::next_span_id(),
            parent_span_id: ::core::option::Option::None,
            $( $key: $val, )*
            $( $lkey: ::core::clone::Clone::clone(&$lkey), )+
        };
        let __dial9_slots = __Dial9Slots {
            $( $lkey: $crate::span::__rt::Slot::__from_arc($lkey), )+
        };
        (__dial9_span, __dial9_slots)
    }};
}

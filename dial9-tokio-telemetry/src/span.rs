//! Ad-hoc span instrumentation that does not require a `tracing` subscriber.
//!
//! The [`tracing` layer](crate::tracing_layer) is the right tool when your code
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
//! use dial9_tokio_telemetry::dial9_span;
//! use dial9_tokio_telemetry::span::Instrument as _;
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
//! # use dial9_tokio_telemetry::dial9_span;
//! # let retries = 1u32; let path = "/x";
//! # #[derive(Debug)] struct Cfg; let cfg = Cfg;
//! let span = dial9_span!("load", retries = retries, path = %path, config = ?cfg);
//! ```
//!
//! # Instrumenting a synchronous scope
//!
//! ```no_run
//! use dial9_tokio_telemetry::dial9_span;
//! use dial9_tokio_telemetry::span::Span as _;
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
pub use tower::{Dial9SpanLayer, Dial9SpanService};

use crate::telemetry::{Dial9Handle, clock_monotonic_ns, current_worker_id};
use dial9_trace_format::{InternedString, TraceEvent};
use std::fmt;
use std::marker::PhantomData;

/// Re-exports used by the [`dial9_span!`](crate::dial9_span) macro expansion.
/// Not a stable API.
#[doc(hidden)]
pub mod __rt {
    pub use super::wire::next_span_id;
    pub use super::{Span, current_worker_id_u64, emit_close};
    pub use crate::telemetry::{Dial9Handle, clock_monotonic_ns};
    pub use dial9_trace_format::{InternedString, TraceEvent, TraceField};
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

    /// Emit the enter segment on `handle`. Implementation detail of
    /// [`enter`](Self::enter) and [`Instrumented`].
    #[doc(hidden)]
    fn __emit_enter(&self, handle: &Dial9Handle);

    /// Emit the exit segment on `handle`.
    #[doc(hidden)]
    fn __emit_exit(&self, handle: &Dial9Handle);

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
        self.__emit_enter(&handle);
        Entered {
            span: self,
            handle,
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
        self.span.__emit_exit(&self.handle);
    }
}

// ── Shared emit helpers (used by macro-generated spans and `Dial9Span`) ───────

/// The current thread's global worker id as a raw `u64`, for the `worker_id`
/// wire field. Used by the [`dial9_span!`](crate::dial9_span) expansion.
#[doc(hidden)]
pub fn current_worker_id_u64() -> u64 {
    current_worker_id().as_u64()
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

    fn __emit_exit(&self, handle: &Dial9Handle) {
        handle.with_encoder(|enc| {
            let span_name = enc.intern_string(&self.name);
            enc.encode(&RuntimeExit {
                timestamp_ns: clock_monotonic_ns(),
                worker_id: current_worker_id_u64(),
                span_id: self.span_id,
                span_name,
            });
        });
    }
}

impl Drop for Dial9Span {
    fn drop(&mut self) {
        emit_close(self.span_id);
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
/// use dial9_tokio_telemetry::dial9_span;
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
#[macro_export]
macro_rules! dial9_span {
    ($name:expr $(,)?) => {
        $crate::__dial9_span_build!($name; [])
    };
    ($name:expr, $($fields:tt)+) => {
        $crate::__dial9_span_munch!($name; [] ; $($fields)+)
    };
}

/// Token-muncher that formats each field value (handling the `%`/`?`/bare
/// sigils) and accumulates `(key : value_expr)` pairs for the build step.
#[doc(hidden)]
#[macro_export]
macro_rules! __dial9_span_munch {
    ($name:expr; [$($acc:tt)*] ; ) => {
        $crate::__dial9_span_build!($name; [$($acc)*])
    };
    ($name:expr; [$($acc:tt)*] ; $key:ident = %$val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($acc)* ($key : ::std::string::ToString::to_string(&$val))] ; $($($rest)*)?
        )
    };
    ($name:expr; [$($acc:tt)*] ; $key:ident = ?$val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($acc)* ($key : ::std::format!("{:?}", $val))] ; $($($rest)*)?
        )
    };
    ($name:expr; [$($acc:tt)*] ; $key:ident = $val:expr $(, $($rest:tt)*)?) => {
        $crate::__dial9_span_munch!(
            $name; [$($acc)* ($key : $val)] ; $($($rest)*)?
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

            fn __emit_exit(&self, __h: &$crate::span::__rt::Dial9Handle) {
                __h.with_encoder(|__enc| {
                    let __name = __enc.intern_string(__DIAL9_NAME);
                    __enc.encode(&__Dial9Exit {
                        timestamp_ns: $crate::span::__rt::clock_monotonic_ns(),
                        worker_id: $crate::span::__rt::current_worker_id_u64(),
                        span_id: self.span_id,
                        span_name: __name,
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

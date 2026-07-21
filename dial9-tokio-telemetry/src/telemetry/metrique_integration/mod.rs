//! Integrate [metrique](https://docs.rs/metrique) entries into the dial9
//! trace.
//!
//! Dial9 acts as a peer metrique sink: entries flowing through your
//! existing EMF/JSON metrique pipeline are also recorded into the dial9
//! trace, correlated by worker id and a monotonic time span. A single trace
//! file then carries both tokio runtime telemetry and your application's
//! own request-scoped metrics.
//!
//! The descriptor-walking and wire-encoding engine ([`Dial9Stream`],
//! [`Emit`], [`Interned`]) lives in `dial9-utils` — it has no tokio
//! dependency of its own. [`Dial9Context`] is the tokio-specific piece
//! defined here: it captures worker id and monotonic timing from the
//! current tokio runtime and tags its fields with `dial9-utils`'s internal
//! `Context` flag so the engine can find them.
//!
//! # Choosing what to emit
//!
//! Flatten [`Dial9Context`] into your entry — anywhere; field order doesn't
//! matter, see "Field order" below. The common pattern is to opt in by
//! *default* and explicitly exclude the fields you don't want, via
//! `#[metrics(default_flags(Emit))]` at the struct level plus
//! `flags(skip(Emit))` on individual fields — struct-level defaults don't
//! propagate across the flatten boundary, so `Dial9Context`'s own fields are
//! unaffected either way. Tagging individual fields with [`Emit`] directly,
//! without a struct-level default, works the same way if you'd rather opt
//! in field-by-field instead:
//!
//! ```
//! use dial9_tokio_telemetry::telemetry::metrique_integration::{Dial9Context, Emit, Interned};
//! use metrique::unit_of_work::metrics;
//!
//! #[metrics(rename_all = "PascalCase", default_flags(Emit))]
//! struct RequestMetrics {
//!     #[metrics(flatten)]
//!     dial9: Dial9Context,
//!
//!     #[metrics(flags(Interned))]
//!     route: String,
//!
//!     operation: &'static str,
//!
//!     // Opted out of the struct-level `default_flags(Emit)` — won't reach
//!     // the dial9 trace, even though every other field does.
//!     #[metrics(flags(skip(Emit)))]
//!     debug_blob: String,
//! }
//!
//! let _m = RequestMetrics {
//!     dial9: Dial9Context::capture(),
//!     route: "/api/users".into(),
//!     operation: "GetUser",
//!     debug_blob: String::new(),
//! };
//! ```
//!
//! [`Interned`] routes a string-shaped field through dial9's string pool.
//!
//! ## Field order
//!
//! `Dial9Context` can be declared anywhere in your struct. metrique's macro
//! builds `entry.descriptors()` as "this struct's own direct fields, then
//! each flattened child's fields" — always in that order, regardless of
//! where the flatten field is declared — while `Entry::write` fires
//! callbacks in literal declaration order. Those two orders only coincide
//! when the flatten field happens to be declared last, so this sink doesn't
//! rely on them lining up: it dispatches each `value()` callback by the
//! field's resolved *name* (matched against the name computed from the
//! descriptor at schema-build time), not by position.
//!
//! # Wiring up the sink
//!
//! Compose [`Dial9Stream`] alongside your existing sink with
//! [`tee`](metrique::writer::stream::tee):
//!
//! ```ignore
//! use dial9_tokio_telemetry::telemetry::metrique_integration::Dial9Stream;
//! use metrique::writer::stream::tee;
//!
//! let stream = tee(emf_stream, Dial9Stream::new(&handle));
//! ```
//!
//! # Known limitations
//!
//! - **`Vec<T>`/`[T]` `Emit` fields always land as a comma-joined string,
//!   never a structured list.** metrique wraps every `#[metrics(flags(...))]`-
//!   tagged field's `Value` in `ForceFlag<T, _>` (that's how the flag itself
//!   is carried) — including plain `Emit`. `ForceFlag`'s writer wrapper
//!   overrides `.string()`/`.metric()`/`.error()` but not `.values()`, so a
//!   list field's `writer.values(...)` call is always intercepted by that
//!   wrapper's own inherited default (comma-join each element, then
//!   `.string()`) before it reaches this sink — whether or not the entry is
//!   boxed. There is no dial9-side workaround; encoding as a plain string
//!   is the only representation this sink can actually observe.
//! - **`Flex` fields are not supported.** No metrique `Value` impl
//!   currently produces `FieldShape::Flex`, and `Flex<T>`'s own descriptor
//!   support is `Unavailable` — flattening a `Flex` field anywhere in an
//!   entry currently makes the *whole entry* look hand-written to this
//!   sink (dropped, not just that field). Avoid flattening `Flex` on any
//!   entry that also wants dial9 integration.
//!
//!   Both limitations are tracked for a possible follow-up: routing
//!   un-modeled fields (lists, `Flex`, and any other shape this sink can't
//!   capture structurally) through a schema trailer using
//!   `FieldType::DynamicMap`/`OptionalDynamicMap` instead of falling back to
//!   a comma-joined string or dropping the field.

mod context;

pub use context::{Dial9Context, MonotonicAtClose};
pub use dial9_utils::metrique_integration::{Dial9Stream, Emit, Interned};

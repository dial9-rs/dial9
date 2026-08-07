//! Dial9 conventions layered on top of free-form trace schema annotations.
//!
//! The wire format treats annotation keys and values as opaque strings. This
//! module is the shared vocabulary used by Dial9 producers and consumers.

/// Annotation key assigning a structural role to a field.
pub const ROLE_KEY: &str = "dial9.role";

/// Annotation key carrying the producer or instrumentation family for a span.
pub const SPAN_TYPE_KEY: &str = "dial9.span.type";

/// Span type used when a single-event span schema does not declare a producer
/// or instrumentation family.
pub const DEFAULT_SPAN_TYPE: &str = "single-event";

/// Values emitted under [`SPAN_TYPE_KEY`].
pub mod span_types {
    /// The metrique adapter's single-event spans.
    pub const METRIQUE: &str = "metrique";
}

/// Values recognized under [`ROLE_KEY`].
pub mod roles {
    /// Start timestamp of a completed single-event span.
    pub const SPAN_START: &str = "span.start";

    /// Duration of a completed single-event span. A decoder needs any two of
    /// start, duration, and end (the packed event timestamp) to place the
    /// span; the third is derived.
    pub const SPAN_DURATION: &str = "span.duration";

    /// Dynamic display name of a completed single-event span.
    pub const SPAN_NAME: &str = "span.name";

    /// OS thread ID captured at span start.
    pub const THREAD_ID: &str = "thread_id";

    /// Tokio task ID captured at span start.
    pub const TOKIO_TASK_ID: &str = "tokio.task_id";

    /// Tokio worker ID captured at span start.
    pub const TOKIO_WORKER_ID: &str = "tokio.worker_id";
}

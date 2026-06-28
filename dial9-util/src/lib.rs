//! Utilities built on top of [`dial9-tokio-telemetry`](dial9_tokio_telemetry).
//!
//! Currently this crate provides ad-hoc [`span`] instrumentation: future
//! combinators, a synchronous span guard, the [`dial9_span!`] macro, and (with
//! the `tower-layer` feature) tower middleware. Spans produced here use the
//! exact same wire format as the telemetry crate's `tracing` layer, so the
//! viewer renders them on the same span timeline.
//!
//! Everything here is built on the *public* API of `dial9-tokio-telemetry`, so
//! this crate stays decoupled from the runtime internals.

/// Ad-hoc span instrumentation: future combinators, a span guard, the
/// [`dial9_span!`] macro, and (with the `tower-layer` feature) tower middleware.
pub mod span;

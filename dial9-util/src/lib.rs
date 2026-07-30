//! Ad-hoc span instrumentation for dial9 traces.
//!
//! A lightweight, `tracing`-subscriber-free way to get span-like timing into a
//! dial9 trace: the [`dial9_span!`] macro plus a sync guard, a future
//! combinator, and (with the `tower` feature) a tower layer. Built on
//! `dial9-core` only, so it works with or without Tokio — enable the `tokio`
//! feature to stamp the current worker index on the `worker_id` field.
//!
//! See the [`span`] module for details.

/// Ad-hoc span instrumentation (sync guard, future combinator, tower layer)
/// that emits span events directly into a dial9 trace with no `tracing`
/// subscriber. Construct spans with the [`dial9_span!`] macro.
pub mod span;

//! Opt-in integrations for dial9.
//!
//! Provides [`dial9_axum`] (traced replacements for `axum::serve`), the
//! [`tracing_layer`] subscriber layer, (behind the `span` feature) ad-hoc
//! `span` instrumentation, and (behind `metrics-rs`) [`metrics_rs`] sampling
//! of the application's metrics.rs registry.

#![warn(unreachable_pub)]

/// Axum servers that spawn connection and HTTP/2 tasks through a dial9 executor.
#[cfg(any(feature = "axum-07", feature = "axum-08"))]
pub mod dial9_axum;

/// Periodic sampling of the application's `metrics.rs` registry into dial9.
#[cfg(feature = "metrics-rs")]
pub mod metrics_rs;

/// Ad-hoc span instrumentation (sync guard, future combinator, tower layer)
/// that emits span events directly into a dial9 trace with no `tracing`
/// subscriber. Construct spans with the [`dial9_span!`] macro.
///
/// Behind the `span` cargo feature so consumers that don't need spans don't
/// compile `dial9-trace-format`/`pin-project-lite`.
#[cfg(feature = "span")]
pub mod span;

/// [`tower`](https://docs.rs/tower) middleware that records a span per request,
/// built on the [`span`] module.
///
/// Behind the `tower` cargo feature.
#[cfg(feature = "tower")]
pub mod tower;

/// Tracing subscriber layer for emitting span events into dial9 traces.
#[cfg(feature = "tracing-layer")]
pub mod tracing_layer;

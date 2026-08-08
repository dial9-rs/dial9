//! Opt-in integrations for dial9.
//!
//! Provides opt-in tracing and framework integrations.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(unreachable_pub)]

/// Axum servers that spawn connection and HTTP/2 tasks through a dial9 executor.
#[cfg(any(feature = "axum-07", feature = "axum-08"))]
pub mod dial9_axum;

/// Tracing subscriber layer for emitting span events into dial9 traces.
#[cfg(feature = "tracing-layer")]
#[cfg_attr(docsrs, doc(cfg(feature = "tracing-layer")))]
pub mod tracing_layer;

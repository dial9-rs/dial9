//! Utilities built on top of [`dial9-tokio-telemetry`](dial9_tokio_telemetry).
//!
//! This crate provides traced Axum servers built entirely on the public API of
//! `dial9-tokio-telemetry`.

/// Axum servers that spawn connection and HTTP/2 tasks through dial9.
#[cfg(any(feature = "axum-0-7", feature = "axum-0-8"))]
pub mod dial9_axum;

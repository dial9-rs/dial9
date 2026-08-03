//! Opt-in integrations for dial9.
//!
//! Provides [`dial9_axum`], traced replacements for `axum::serve`.

#![warn(unreachable_pub)]

/// Axum servers that spawn connection and HTTP/2 tasks through a dial9 executor.
#[cfg(any(feature = "axum-07", feature = "axum-08"))]
pub mod dial9_axum;

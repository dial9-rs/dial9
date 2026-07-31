//! Utilities for dial9 recording and runtime integrations.
//!
//! Provides [`S3PipelineUploader`](s3::S3PipelineUploader), a pipeline stage
//! that uploads sealed trace segments to S3, and (behind the `span` feature)
//! ad-hoc `span` instrumentation.

#![warn(unreachable_pub)]

mod connection;
/// Axum servers that spawn connection and HTTP/2 tasks through a dial9 executor.
#[cfg(any(feature = "axum-07", feature = "axum-08"))]
pub mod dial9_axum;
mod instance_metadata;
pub mod s3;

/// Ad-hoc span instrumentation (sync guard, future combinator, tower layer)
/// that emits span events directly into a dial9 trace with no `tracing`
/// subscriber. Construct spans with the [`dial9_span!`] macro.
///
/// Behind the `span` cargo feature so a crate that only uses the S3 uploader
/// does not compile `dial9-trace-format`/`pin-project-lite`.
#[cfg(feature = "span")]
pub mod span;

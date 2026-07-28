//! Utilities for dial9 recording and runtime integrations.
//!
//! Provides [`S3PipelineUploader`](s3::S3PipelineUploader), a pipeline stage
//! that uploads sealed trace segments to S3.

#![warn(unreachable_pub)]

mod connection;
/// Axum servers that spawn connection and HTTP/2 tasks through a dial9 executor.
#[cfg(any(feature = "axum-07", feature = "axum-08"))]
pub mod dial9_axum;
mod instance_metadata;
pub mod s3;

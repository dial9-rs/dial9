//! Supporting utilities for dial9.
//!
//! - [`s3`]: an S3 upload `SegmentProcessor` for dial9-core's pipeline.
//!   Provides [`S3PipelineUploader`](s3::S3PipelineUploader), a pipeline
//!   stage that uploads sealed trace segments to S3.
//! - [`metrique_integration`]: the runtime-agnostic engine for recording
//!   metrique entries into the dial9 trace. Gated behind the
//!   `metrique-integration` feature. See its module docs for details.

#![warn(unreachable_pub)]

mod connection;
mod instance_metadata;
#[cfg(feature = "metrique-integration")]
pub mod metrique_integration;
pub mod s3;

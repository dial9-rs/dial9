//! S3 upload `SegmentProcessor` for dial9-core's pipeline.
//!
//! Provides [`S3PipelineUploader`](s3::S3PipelineUploader), a pipeline stage
//! that uploads sealed trace segments to S3.

#![warn(unreachable_pub)]

mod connection;
mod instance_metadata;
pub mod s3;

//! S3 upload destination for dial9 trace segments.
//!
//! Provides [`S3PipelineUploader`], a pipeline stage that uploads sealed trace
//! segments to S3.

#![warn(unreachable_pub)]

mod connection;
mod instance_metadata;
mod s3;

pub use s3::{
    InstanceIdentity, KeyContext, S3Config, S3ConfigBuilder, S3KeyFn, S3PipelineUploader,
};

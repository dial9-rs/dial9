#[cfg(test)]
pub(crate) mod testutil;

pub use dial9_core::pipeline::{MemorySegment, Payload, SealedSegment, SegmentRef};

use crate::telemetry::buffer::{BufferMode, Disk};
use std::marker::PhantomData;
use std::path::PathBuf;

#[cfg(feature = "cpu-profiling")]
pub(crate) use dial9_perf_self_profile::SymbolizeProcessor;

pub use dial9_core::pipeline::{ProcessError, ProcessErrorKind, SegmentData, SegmentProcessor};
pub use dial9_core::worker::BackgroundTaskConfig;
pub(crate) use dial9_core::worker::processors::{GzipCompressor, WriteBackProcessor};
#[cfg(feature = "worker-s3")]
pub use dial9_utils::s3;
#[cfg(feature = "worker-s3")]
pub(crate) use dial9_utils::s3::S3PipelineUploader;

/// Closure-scoped builder for assembling a custom processor pipeline.
///
/// Obtained via `with_custom_pipeline(|p| ...)` on the runtime builder. The
/// `Mode` type parameter binds the pipeline to the writer's storage mode:
/// disk-only processors like [`write_back`](Self::write_back) are not in
/// scope on `PipelineBuilder<Memory>`, so wiring write-back into an
/// in-memory pipeline is a compile error.
///
/// # Example
///
/// ```ignore
/// struct Logger;
/// impl SegmentProcessor for Logger {
///     fn name(&self) -> &'static str { "logger" }
///     fn process(&mut self, data: SegmentData)
///         -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
///     {
///         Box::pin(async move {
///             println!("segment {} ({} bytes)", data.segment().index(), data.payload().len());
///             Ok(data)
///         })
///     }
/// }
///
/// builder.with_custom_pipeline(|p| p.pipe(Logger).gzip().write_back())
/// ```
#[must_use]
pub struct PipelineBuilder<Mode: BufferMode = Disk> {
    processors: Vec<Box<dyn SegmentProcessor>>,
    _marker: PhantomData<Mode>,
}

impl<Mode: BufferMode> PipelineBuilder<Mode> {
    pub(crate) fn new() -> Self {
        Self {
            processors: Vec::new(),
            _marker: PhantomData,
        }
    }

    pub(crate) fn into_processors(self) -> Vec<Box<dyn SegmentProcessor>> {
        self.processors
    }

    /// Append a user-supplied [`SegmentProcessor`] to the pipeline.
    pub fn pipe<S>(mut self, processor: S) -> Self
    where
        S: SegmentProcessor + 'static,
    {
        self.processors.push(Box::new(processor));
        self
    }

    /// Gzip the segment payload in-memory.
    pub fn gzip(mut self) -> Self {
        self.processors.push(Box::new(GzipCompressor));
        self
    }

    /// Resolve stack-frame addresses in the segment to symbol names.
    /// Only valid when the runtime is built with the `cpu-profiling` feature.
    ///
    /// The built-in S3 / default presets prepend this automatically when
    /// CPU profiling is on; on the custom path the pipeline is passed
    /// through verbatim, so chain `.symbolize()` first if you want
    /// symbolized stack frames in your trace files.
    #[cfg(feature = "cpu-profiling")]
    pub fn symbolize(mut self) -> Self {
        self.processors.push(Box::new(SymbolizeProcessor::new()));
        self
    }

    /// Upload the current payload to S3 with the given configuration. The
    /// AWS SDK default credential chain is used; call [`s3_with_client`]
    /// to supply a pre-built client.
    ///
    /// Does not auto-add gzip — chain `.gzip()` first if you want
    /// compressed uploads.
    ///
    /// [`s3_with_client`]: Self::s3_with_client
    #[cfg(feature = "worker-s3")]
    pub fn s3(mut self, config: s3::S3Config) -> Self {
        self.processors
            .push(Box::new(S3PipelineUploader::new(config, None)));
        self
    }

    /// Variant of [`s3`](Self::s3) that uses the supplied pre-built S3 client.
    #[cfg(feature = "worker-s3")]
    pub fn s3_with_client(mut self, config: s3::S3Config, client: aws_sdk_s3::Client) -> Self {
        self.processors
            .push(Box::new(S3PipelineUploader::new(config, Some(client))));
        self
    }
}

/// Disk-only methods on the pipeline builder.
impl PipelineBuilder<Disk> {
    /// Write the current payload bytes back to disk. When the payload has
    /// been gzipped earlier in the pipeline, the file is written with a
    /// `.gz` suffix and the original sealed segment is removed.
    pub fn write_back(mut self) -> Self {
        self.processors
            .push(Box::new(WriteBackProcessor::default()));
        self
    }

    /// Write the current payload bytes to a specific directory instead of
    /// back alongside the original segment. The file name is preserved;
    /// when the payload has been gzipped, a `.gz` suffix is appended.
    /// The original sealed segment is removed after a successful write.
    pub fn write_back_to(mut self, dir: impl Into<PathBuf>) -> Self {
        self.processors
            .push(Box::new(WriteBackProcessor::to_dir(dir.into())));
        self
    }
}

impl<Mode: BufferMode> std::fmt::Debug for PipelineBuilder<Mode> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineBuilder")
            .field("len", &self.processors.len())
            .finish()
    }
}

//! Segment-processing pipeline contract.
//!
//! [`SegmentProcessor`] is the extension point for post-seal segment handling:
//! compression, symbolization, upload, and so on. A driver reads each sealed
//! segment into a [`SegmentData`] and runs it through a sequence of processors.

use crate::fs::SegmentAccounting;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;

/// The segment payload threaded through the pipeline.
pub use crate::payload::Payload;
/// References to the sealed segment a [`SegmentData`] was loaded from.
pub use crate::sealed::{MemorySegment, SealedSegment, SegmentRef};

/// Data flowing through the processor pipeline.
///
/// The driver reads the sealed segment into `payload`, populates initial
/// `metadata`, then passes this through each [`SegmentProcessor`] in order.
///
/// `SegmentData` is intentionally `!Clone`: it is moved through the pipeline
/// and dropped exactly once, which is crucial for in-flight byte accounting
/// (a single `Drop` decrements the counters).
pub struct SegmentData {
    segment: SegmentRef,
    payload: Payload,
    metadata: HashMap<String, String>,
    /// Post-compression size reported by a processor via
    /// [`set_compressed_size`](Self::set_compressed_size). `None` until set.
    compressed_size: Option<u64>,
    /// Memory-mode in-flight accounting. `None` for disk-backed segments.
    /// Held only for its `Drop` (releases in-flight counters).
    accounting: Option<SegmentAccounting>,
}

impl SegmentData {
    /// Build segment data for the pipeline. Called by the driver after loading
    /// a sealed segment.
    pub(crate) fn new(
        segment: SegmentRef,
        payload: Payload,
        metadata: HashMap<String, String>,
        accounting: Option<SegmentAccounting>,
    ) -> Self {
        Self {
            segment,
            payload,
            metadata,
            compressed_size: None,
            accounting,
        }
    }

    /// Information about the sealed segment being processed.
    pub fn segment(&self) -> &SegmentRef {
        &self.segment
    }

    /// Current payload (raw, symbolized, compressed, etc.).
    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    /// Take ownership of the payload, leaving an empty [`Payload`] in its place.
    pub fn take_payload(&mut self) -> Payload {
        std::mem::take(&mut self.payload)
    }

    /// Replace the payload.
    pub fn set_payload(&mut self, payload: impl Into<Payload>) {
        self.payload = payload.into();
    }

    /// Record the segment's post-compression size. Surfaces as the
    /// `CompressedSize` metric for this segment.
    pub fn set_compressed_size(&mut self, bytes: u64) {
        self.compressed_size = Some(bytes);
    }

    /// The post-compression size reported by a processor, if any.
    pub fn compressed_size(&self) -> Option<u64> {
        self.compressed_size
    }

    /// Metadata accumulated by upstream processors.
    pub fn metadata(&self) -> &HashMap<String, String> {
        &self.metadata
    }

    /// Mutable reference to the metadata map. Processors can insert keys
    /// (e.g. `"content_encoding"`, `"write_back_extension"`) to signal
    /// downstream stages.
    pub fn metadata_mut(&mut self) -> &mut HashMap<String, String> {
        &mut self.metadata
    }

    /// Update memory-mode in-flight accounting to the current payload size.
    /// No-op for disk-backed segments.
    pub fn adjust_accounting(&mut self) {
        if let Some(acct) = self.accounting.as_mut() {
            acct.adjust(self.payload.len() as u64);
        }
    }

    /// In-flight accounting, for tests that assert the worker re-balances
    /// counters between pipeline stages.
    #[cfg(test)]
    pub(crate) fn accounting(&self) -> Option<&SegmentAccounting> {
        self.accounting.as_ref()
    }
}

impl std::fmt::Debug for SegmentData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentData")
            .field("segment", &self.segment)
            .field("payload", &self.payload)
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

/// A single step in the segment processing pipeline.
///
/// Implementations handle one concern: compress, symbolize, upload, etc.
/// The driver calls processors in sequence for each segment.
///
/// # Panic safety
///
/// The driver catches panics from [`process()`](Self::process) and skips the
/// panicking segment. The same processor instance is reused for subsequent
/// segments, so implementations **must** remain in a valid state after a panic
/// (i.e., no partially-updated invariants that would cause incorrect behavior
/// on the next call).
pub trait SegmentProcessor: Send {
    /// Human-readable name for this processor (used in metrics).
    fn name(&self) -> &'static str;

    /// Process a segment, transforming or consuming its data.
    /// Returns the (possibly modified) data for the next processor,
    /// or an error to skip this segment.
    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>;

    /// Called once per finished dump in triggered mode (see
    /// [`crate::dump`]), in pipeline order, so stages can flush any
    /// per-dump state they accumulated. Return the S3 key of a manifest
    /// written for this dump, or `None`; the last `Some` across the
    /// pipeline lands on [`DumpReceipt::manifest_key`](crate::dump::DumpReceipt::manifest_key).
    ///
    /// Default: no-op returning `None`. Never called in continuous mode.
    /// The same panic-safety contract as [`process()`](Self::process)
    /// applies.
    fn finalize_dump(
        &mut self,
        completion: &crate::dump::DumpCompletion,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        let _ = completion;
        Box::pin(std::future::ready(None))
    }
}

/// Error returned by a [`SegmentProcessor`].
///
/// Carries the [`SegmentData`] back so the caller can recover it for retry,
/// release, or error handling.
#[derive(Debug)]
pub struct ProcessError {
    data: SegmentData,
    kind: ProcessErrorKind,
}

impl ProcessError {
    /// Wrap `data` and `kind` into a new [`ProcessError`].
    pub fn new(data: SegmentData, kind: ProcessErrorKind) -> Self {
        Self { data, kind }
    }

    /// Shorthand for [`ProcessError::new`] with an I/O error.
    pub fn io(data: SegmentData, err: std::io::Error) -> Self {
        Self::new(data, ProcessErrorKind::Io(err))
    }

    /// The kind of failure.
    pub fn kind(&self) -> &ProcessErrorKind {
        &self.kind
    }

    /// Recover the carried [`SegmentData`].
    pub fn into_data(self) -> SegmentData {
        self.data
    }

    /// Recover both the carried [`SegmentData`] and the failure kind.
    pub fn into_parts(self) -> (SegmentData, ProcessErrorKind) {
        (self.data, self.kind)
    }
}

/// Kind of failure reported by a [`SegmentProcessor`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ProcessErrorKind {
    /// The processor hit an `std::io::Error`.
    #[non_exhaustive]
    Io(std::io::Error),

    /// An error transferring data off the host.
    #[non_exhaustive]
    Transfer {
        /// Underlying error source.
        source: Box<dyn std::error::Error + Send + Sync>,
        /// Whether this error is transient and the segment should be kept on
        /// disk for retry.
        retryable: bool,
    },
}

impl ProcessErrorKind {
    /// Build a transfer error from an arbitrary source. Used by upload
    /// processors that classify their own retryability.
    pub fn transfer(source: Box<dyn std::error::Error + Send + Sync>, retryable: bool) -> Self {
        Self::Transfer { source, retryable }
    }

    /// True when the underlying error is "not found", meaning the segment was
    /// already deleted.
    pub fn already_deleted(&self) -> bool {
        matches!(self, ProcessErrorKind::Io(err) if err.kind() == io::ErrorKind::NotFound)
    }

    /// Whether this error is transient and the segment should be kept on disk
    /// for retry.
    pub fn retryable(&self) -> bool {
        match self {
            ProcessErrorKind::Transfer { retryable, .. } => *retryable,
            _ => false,
        }
    }
}

impl std::fmt::Display for ProcessErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Transfer { source, .. } => write!(f, "S3 transfer error: {source}"),
        }
    }
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind.fmt(f)
    }
}

impl std::error::Error for ProcessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ProcessErrorKind::Io(e) => Some(e),
            ProcessErrorKind::Transfer { source, .. } => Some(source.as_ref()),
        }
    }
}

impl From<std::io::Error> for ProcessErrorKind {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::sync::Arc;
    use crate::primitives::sync::atomic::{AtomicU64, Ordering};

    /// `adjust_accounting` re-balances in-flight bytes when a processor grows
    /// or shrinks the payload, and `Drop` releases the last reported size.
    #[test]
    fn adjust_accounting_tracks_payload_size() {
        let in_flight = Arc::new(AtomicU64::new(50));
        let accounting = SegmentAccounting {
            in_flight_bytes: Arc::clone(&in_flight),
            in_flight_segments: Arc::new(AtomicU64::new(1)),
            in_flight_bytes_peak: Arc::new(AtomicU64::new(50)),
            size: 50,
        };
        let mut data = SegmentData::new(
            SegmentRef::Disk(SealedSegment {
                path: "x".into(),
                index: 0,
            }),
            Payload::from_vec(vec![0u8; 50]),
            HashMap::new(),
            Some(accounting),
        );

        // Growth (e.g. symbolize appends symbols).
        data.set_payload(Payload::from_vec(vec![0u8; 150]));
        data.adjust_accounting();
        assert_eq!(in_flight.load(Ordering::Acquire), 150);

        // Shrinkage (e.g. gzip).
        data.set_payload(Payload::from_vec(vec![0u8; 5]));
        data.adjust_accounting();
        assert_eq!(in_flight.load(Ordering::Acquire), 5);

        // Drop returns the last reported size to the atomic.
        drop(data);
        assert_eq!(in_flight.load(Ordering::Acquire), 0);
    }
}

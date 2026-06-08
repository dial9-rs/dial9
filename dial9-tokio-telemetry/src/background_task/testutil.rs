use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use super::{ProcessError, SegmentData, SegmentProcessor};

/// A [`SegmentProcessor`] that stores each sealed segment's payload bytes,
/// one `Vec<u8>` per segment.
///
/// Per-segment (not concatenated): each entry is a self-contained trace blob
/// with its own header, so [`decode_captured`] can decode them independently.
pub(crate) struct CapturingProcessor {
    segments: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl CapturingProcessor {
    pub(crate) fn new() -> (Self, Arc<Mutex<Vec<Vec<u8>>>>) {
        let segments = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                segments: segments.clone(),
            },
            segments,
        )
    }
}

impl SegmentProcessor for CapturingProcessor {
    fn name(&self) -> &'static str {
        "Capture"
    }

    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        self.segments
            .lock()
            .unwrap()
            .push(data.payload().clone().into_vec());
        Box::pin(async move { Ok(data) })
    }
}

/// Decode every event across the captured per-segment payloads.
pub(crate) fn decode_captured(
    segments: &[Vec<u8>],
) -> Vec<crate::telemetry::analysis_events::Dial9Event> {
    segments
        .iter()
        .flat_map(|seg| crate::telemetry::format::decode_events(seg).expect("decode segment"))
        .collect()
}

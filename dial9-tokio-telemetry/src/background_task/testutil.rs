use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use super::{ProcessError, SegmentData, SegmentProcessor};

/// Appends each segment's payload bytes to a shared buffer.
pub(crate) struct CaptureProcessor(pub Arc<Mutex<Vec<u8>>>);

impl CaptureProcessor {
    pub(crate) fn new() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        (Self(Arc::clone(&buf)), buf)
    }
}

impl SegmentProcessor for CaptureProcessor {
    fn name(&self) -> &'static str {
        "Capture"
    }

    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        self.0
            .lock()
            .unwrap()
            .extend_from_slice(&data.payload().clone().into_vec());
        Box::pin(async { Ok(data) })
    }
}

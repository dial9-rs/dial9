//! Shared helpers for the memory-profiling integration tests. Built entirely on
//! the public `dial9` facade so the tests exercise what downstream users see.
#![allow(dead_code)]

use dial9::MemoryBuffer;
use dial9::core::pipeline::{ProcessError, SegmentData, SegmentProcessor};
use dial9_trace_format::decoder::Decoder;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// Total in-memory byte budget for capture tests. Large enough that a test's
/// events fit without the ring dropping the oldest segment.
pub const CAPTURE_BUFFER_SIZE: u64 = 16 * 1024 * 1024;

/// Fixed-size in-memory writer for tests that run a runtime but don't read the
/// trace back.
pub fn small_mem_writer() -> MemoryBuffer {
    MemoryBuffer::builder()
        .max_total_size(16 * 1024 * 1024)
        .max_segment_size(4 * 1024 * 1024)
        .build()
        .expect("fixed sizes are valid")
}

/// A [`SegmentProcessor`] that stores each sealed segment's payload bytes, one
/// `Vec<u8>` per segment. Each entry is a self-contained trace blob with its own
/// header, so [`decode_all`] can decode them independently.
///
/// Pair with a [`MemoryBuffer`] via `.with_custom_pipeline(|p| p.pipe(capture))`,
/// then `graceful_shutdown(..)` to drain the worker before reading the captured
/// segments.
pub struct CapturingProcessor {
    segments: Arc<Mutex<Vec<Vec<u8>>>>,
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

/// Returns the processor (move into `.pipe(..)`) and the shared handle to read
/// the captured per-segment bytes after shutdown.
pub fn capture_processor() -> (CapturingProcessor, Arc<Mutex<Vec<Vec<u8>>>>) {
    let segments = Arc::new(Mutex::new(Vec::new()));
    (
        CapturingProcessor {
            segments: segments.clone(),
        },
        segments,
    )
}

/// Decode every segment in `segments`, deserializing each event as `T`.
pub fn decode_all<T: DeserializeOwned>(segments: &[Vec<u8>]) -> Vec<T> {
    let mut events = Vec::new();
    for bytes in segments {
        let mut dec = Decoder::new(bytes).expect("valid trace header");
        dec.for_each_event(|raw| {
            let ev: T = raw.deserialize().expect("deserialize event");
            events.push(ev);
        })
        .expect("decode segment");
    }
    events
}

/// Read a trace file from disk and decode all events as `T`.
pub fn decode_file<T: DeserializeOwned>(path: &Path) -> Vec<T> {
    let data = std::fs::read(path).expect("read trace file");
    let mut dec = Decoder::new(&data).expect("valid trace header");
    let mut events = Vec::new();
    dec.for_each_event(|raw| {
        let ev: T = raw.deserialize().expect("deserialize event");
        events.push(ev);
    })
    .expect("decode file");
    events
}

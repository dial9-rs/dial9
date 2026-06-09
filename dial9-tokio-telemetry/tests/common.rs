#![allow(dead_code)]
use dial9_tokio_telemetry::background_task::{ProcessError, SegmentData, SegmentProcessor};
use dial9_tokio_telemetry::telemetry::InMemoryWriter;
use dial9_trace_format::decoder::Decoder;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// Total in-memory byte budget for capture tests. Large enough that a test's
/// events fit without the ring dropping the oldest segment.
pub const CAPTURE_BUFFER_SIZE: u64 = 16 * 1024 * 1024;

/// Fixed-size in-memory writer for tests that run a telemetry runtime but don't
/// read the trace back.
pub fn small_mem_writer() -> InMemoryWriter {
    InMemoryWriter::builder()
        .max_total_size(16 * 1024 * 1024)
        .max_segment_size(4 * 1024 * 1024)
        .build()
        .expect("fixed sizes are valid")
}

/// A [`SegmentProcessor`] that stores each sealed segment's payload bytes,
/// one `Vec<u8>` per segment.
///
/// Per-segment (not concatenated): each entry is a self-contained trace blob
/// with its own header, so [`decode_all`] can decode them independently.
///
/// Pair with an [`InMemoryWriter`](dial9_tokio_telemetry::telemetry::InMemoryWriter)
/// via `.with_custom_pipeline(|p| p.pipe(capture))`, then
/// `guard.graceful_shutdown(..)` to drain the worker before reading the captured segments.
pub struct CapturingProcessor {
    segments: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl CapturingProcessor {
    pub fn new() -> (Self, Arc<Mutex<Vec<Vec<u8>>>>) {
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

/// Convenience constructor: returns the processor (move into `.pipe(..)`) and
/// the shared handle to read the captured per-segment bytes after shutdown.
pub fn capture_processor() -> (CapturingProcessor, Arc<Mutex<Vec<Vec<u8>>>>) {
    CapturingProcessor::new()
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

/// Read a thread's total context-switch count (`voluntary_ctxt_switches` +
/// `nonvoluntary_ctxt_switches`) from `/proc/self/task/<tid>/status`.
///
/// This is the same quantity perf's `SwContextSwitches` event counts, so it
/// serves as an independent kernel ground truth for sampling-ratio assertions.
/// Returns `None` if the thread has exited or the fields are absent.
#[cfg(target_os = "linux")]
pub fn read_switch_count(tid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/self/task/{tid}/status")).ok()?;
    let mut total = 0u64;
    let mut found = false;
    for line in status.lines() {
        if let Some(rest) = line
            .strip_prefix("voluntary_ctxt_switches:")
            .or_else(|| line.strip_prefix("nonvoluntary_ctxt_switches:"))
        {
            total += rest.trim().parse::<u64>().ok()?;
            found = true;
        }
    }
    found.then_some(total)
}

/// Snapshot the context-switch count of every thread in the current process,
/// keyed by tid. Threads that disappear mid-enumeration are simply skipped.
#[cfg(target_os = "linux")]
pub fn snapshot_task_switches() -> std::collections::HashMap<u32, u64> {
    let mut map = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return map;
    };
    for entry in entries.flatten() {
        if let Some(tid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
            && let Some(count) = read_switch_count(tid)
        {
            map.insert(tid, count);
        }
    }
    map
}

#![allow(dead_code)]
use dial9_tokio_telemetry::background_task::{ProcessError, SegmentData, SegmentProcessor};
use dial9_tokio_telemetry::telemetry::{DiskWriter, InMemoryWriter};
use dial9_trace_format::decoder::Decoder;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Total in-memory byte budget for capture tests. Large enough that a test's
/// events fit without the ring dropping the oldest segment.
pub const CAPTURE_BUFFER_SIZE: u64 = 16 * 1024 * 1024;

/// Wall-clock cap for the seal/upload polling loops in dump tests.
pub const SEAL_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Sleep between polls while waiting for a segment to seal.
pub const SEAL_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Trivial tasks spawned per workload burst; enough to seal a 64-byte segment.
pub const WORKLOAD_BURST: usize = 200;

/// A [`DiskWriter`] tuned to seal a segment within a few hundred ms under a tiny
/// workload: the small size threshold seals by bytes almost immediately, and the
/// short rotation period is a time-based backstop. The default 60s rotation +
/// larger size threshold could leave a tiny workload's bytes in an unsealed
/// active segment, capturing zero segments on slow CI.
pub fn fast_sealing_writer(trace_path: &Path) -> DiskWriter {
    DiskWriter::builder()
        .base_path(trace_path)
        .max_file_size(64)
        .max_total_size(50 * 1024)
        .rotation_period(Duration::from_millis(200))
        .build()
        .expect("fixed sizes are valid")
}

/// Spawn a burst of trivial tasks to generate trace events. Run repeatedly from
/// a polling loop: one burst can fail to seal on a starved runner.
pub fn drive_workload(runtime: &tokio::runtime::Runtime) {
    runtime.block_on(async {
        let mut handles = Vec::with_capacity(WORKLOAD_BURST);
        for _ in 0..WORKLOAD_BURST {
            handles.push(tokio::spawn(async { tokio::task::yield_now().await }));
        }
        for h in handles {
            let _ = h.await;
        }
    });
}

/// Drive trace workload until the flush thread has sealed at least one segment
/// (`trace.<n>.bin` on disk), so a look-back dump has something to capture.
///
/// Re-driving each iteration (rather than relying on a single prior burst) is
/// what makes capture deterministic: a triggered worker parks until a dump is
/// requested, so a confirmed-sealed segment persists in the ring and the
/// subsequent `dump_current_data` is guaranteed to match it. Panics on timeout
/// so a starved runner fails loudly instead of triggering against an empty ring.
pub fn wait_for_sealed_segment(runtime: &tokio::runtime::Runtime, trace_dir: &Path) {
    let deadline = Instant::now() + SEAL_WAIT_TIMEOUT;
    loop {
        // Keep producing trace data so segments keep sealing into the ring.
        drive_workload(runtime);
        let sealed = std::fs::read_dir(trace_dir)
            .expect("read trace dir")
            .filter_map(|e| e.ok())
            .any(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                // Sealed segments are `trace.<n>.bin`; the active file ends in
                // `.bin.active` and the base `trace.bin` is never a segment.
                name.ends_with(".bin") && !name.ends_with("trace.bin")
            });
        if sealed {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no segment sealed within {SEAL_WAIT_TIMEOUT:?}; cannot exercise look-back dump"
        );
        std::thread::sleep(SEAL_POLL_INTERVAL);
    }
}

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

/// Reconstruct `tid -> worker` from park/unpark events. CPU samples carry only
/// a `tid`, so tests that assert on a sample's worker resolve it through this
/// map, the same way analysis does.
pub fn tid_to_worker(
    events: &[dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event],
) -> std::collections::HashMap<u32, dial9_tokio_telemetry::telemetry::analysis_events::WorkerId> {
    use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
    let mut m = std::collections::HashMap::new();
    for e in events {
        match e {
            Dial9Event::WorkerParkEvent(p) => {
                m.insert(p.tid, p.worker_id);
            }
            Dial9Event::WorkerUnparkEvent(p) => {
                m.insert(p.tid, p.worker_id);
            }
            _ => {}
        }
    }
    m
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

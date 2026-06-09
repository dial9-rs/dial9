#![allow(dead_code)]
use dial9_tokio_telemetry::telemetry::{Batch, TraceWriter};
use dial9_trace_format::decoder::Decoder;
use serde::de::DeserializeOwned;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A [`TraceWriter`] that accumulates the raw encoded bytes of every batch it
/// receives.
///
/// Tests decode via the serde path using [`decode_all`] or [`decode_file`].
pub struct BytesCapturingWriter {
    batches: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl BytesCapturingWriter {
    pub fn new() -> (Self, Arc<Mutex<Vec<Vec<u8>>>>) {
        let batches = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                batches: batches.clone(),
            },
            batches,
        )
    }
}

impl TraceWriter for BytesCapturingWriter {
    fn write_encoded_batch(&mut self, batch: &Batch) -> std::io::Result<()> {
        self.batches
            .lock()
            .unwrap()
            .push(batch.encoded_bytes().to_vec());
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Decode every batch in `batches`, deserializing each event as `T`.
pub fn decode_all<T: DeserializeOwned>(batches: &[Vec<u8>]) -> Vec<T> {
    let mut events = Vec::new();
    for bytes in batches {
        let mut dec = Decoder::new(bytes).expect("valid trace header");
        dec.for_each_event(|raw| {
            let ev: T = raw.deserialize().expect("deserialize event");
            events.push(ev);
        })
        .expect("decode batch");
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

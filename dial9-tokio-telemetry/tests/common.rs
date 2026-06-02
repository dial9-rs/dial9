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

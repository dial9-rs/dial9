#![allow(dead_code)]
use dial9_tokio_telemetry::analysis_unstable::decode_events;
use dial9_tokio_telemetry::telemetry::{Batch, TelemetryEvent, TraceWriter};
use dial9_trace_format::decoder::Decoder;
use serde::de::DeserializeOwned;
use std::sync::{Arc, Mutex};

/// A [`TraceWriter`] that accumulates all events into a shared `Vec`.
///
/// Encoded batches are decoded back into `TelemetryEvent` variants so that
/// tests can inspect them uniformly regardless of the encoding path.
pub struct CapturingWriter {
    events: Arc<Mutex<Vec<TelemetryEvent>>>,
}

impl CapturingWriter {
    pub fn new() -> (Self, Arc<Mutex<Vec<TelemetryEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: events.clone(),
            },
            events,
        )
    }
}

impl TraceWriter for CapturingWriter {
    fn write_encoded_batch(&mut self, batch: &Batch) -> std::io::Result<()> {
        let events = decode_events(batch.encoded_bytes()).expect("invalid batch");
        self.events.lock().unwrap().extend_from_slice(&events);
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A [`TraceWriter`] that accumulates the raw encoded bytes of every batch it
/// receives.
///
/// Unlike [`CapturingWriter`], this writer does NOT pre-decode into
/// `TelemetryEvent` — the test layer is responsible for decoding via the serde
/// path under test.
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

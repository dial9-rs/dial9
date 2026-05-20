use std::collections::HashMap;

use dial9_tokio_telemetry::telemetry::{
    Encodable, TelemetryHandle, ThreadLocalEncoder, clock_monotonic_ns, record_event,
};
use dial9_trace_format::{InternedString, TraceEvent};
use tokio::sync::Mutex;

use crate::ddb::DdbClient;

/// Custom event emitted after each individual DDB put during a flush.
struct DdbFlush {
    timestamp_ns: u64,
    metric_name: String,
    latency_us: u64,
    success: bool,
}

#[derive(TraceEvent)]
struct DdbFlushWire {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    metric_name: InternedString,
    latency_us: u64,
    success: bool,
}

impl Encodable for DdbFlush {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let metric_name = enc.intern_string(&self.metric_name);
        enc.encode(&DdbFlushWire {
            timestamp_ns: self.timestamp_ns,
            metric_name,
            latency_us: self.latency_us,
            success: self.success,
        });
    }
}

#[derive(Default)]
struct Aggregate {
    sum: f64,
    count: u64,
    min: f64,
    max: f64,
}

impl Aggregate {
    fn record(&mut self, value: f64) {
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.sum += value;
        self.count += 1;
    }
}

pub struct MetricsBuffer {
    inner: Mutex<HashMap<String, Aggregate>>,
}

impl Default for MetricsBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsBuffer {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub async fn record(&self, name: String, value: f64) {
        self.inner
            .lock()
            .await
            .entry(name)
            .or_default()
            .record(value);
    }

    #[tracing::instrument(skip(self, ddb))]
    pub async fn flush_to_ddb(&self, ddb: &DdbClient) {
        use tracing::Instrument;

        let snapshot: HashMap<String, (f64, u64, f64, f64)> = {
            let mut guard = self.inner.lock().await;
            guard
                .drain()
                .map(|(k, v)| (k, (v.sum, v.count, v.min, v.max)))
                .collect()
        };

        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let parent = tracing::Span::current();
        let handle = TelemetryHandle::current();
        for (name, (sum, count, min, max)) in snapshot {
            let span = tracing::info_span!(parent: &parent, "put_aggregate", metric = %name);
            let start = std::time::Instant::now();
            let result = ddb
                .put_aggregate(&name, ts, sum, count, min, max)
                .instrument(span)
                .await;
            let latency_us = start.elapsed().as_micros() as u64;
            let success = result.is_ok();
            record_event(
                DdbFlush {
                    timestamp_ns: clock_monotonic_ns(),
                    metric_name: name.clone(),
                    latency_us,
                    success,
                },
                &handle,
            );
            if let Err(e) = result {
                eprintln!("flush error for {name}: {e}");
            }
        }
    }
}

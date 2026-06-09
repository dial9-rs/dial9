//! User-provided custom event callbacks.

use crate::primitives::sync::Arc;
use crate::primitives::sync::atomic::AtomicU64;
use crate::telemetry::buffer::{Encodable, record_encodable_event};
use crate::telemetry::collector::CentralCollector;
use crate::telemetry::events::clock_monotonic_ns;
use crate::telemetry::recorder::source::{FlushContext, Source};
use std::time::{Duration, Instant};

/// Configuration for custom event callbacks.
///
/// Built via `CustomEventsConfig::builder()...build()` and enabled with
/// [`TracedRuntimeBuilder::with_custom_events`](crate::telemetry::TracedRuntimeBuilder::with_custom_events).
#[derive(Debug, Clone, bon::Builder)]
pub struct CustomEventsConfig {
    /// Minimum time between callback invocations.
    ///
    /// Defaults to [`Duration::ZERO`], which runs the callback during every
    /// flush cycle while telemetry is enabled.
    #[builder(default)]
    minimum_interval: Duration,
}

impl Default for CustomEventsConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl CustomEventsConfig {
    /// Minimum time between callback invocations.
    ///
    /// [`Duration::ZERO`] means the callback runs during every flush cycle
    /// while telemetry is enabled.
    pub fn minimum_interval(&self) -> Duration {
        self.minimum_interval
    }
}

/// Context passed to a custom event callback.
///
/// Use [`record_event`](Self::record_event) to emit user-defined
/// [`Encodable`] events into the trace.
pub struct CustomEventsContext<'a> {
    collector: &'a Arc<CentralCollector>,
    drain_epoch: &'a AtomicU64,
    timestamp_ns: u64,
}

impl std::fmt::Debug for CustomEventsContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomEventsContext")
            .field("timestamp_ns", &self.timestamp_ns)
            .finish_non_exhaustive()
    }
}

impl CustomEventsContext<'_> {
    /// Monotonic timestamp captured for this callback invocation.
    pub fn timestamp_ns(&self) -> u64 {
        self.timestamp_ns
    }

    /// Record a user-defined event into the trace.
    pub fn record_event(&mut self, event: impl Encodable) {
        record_encodable_event(&event, self.collector, self.drain_epoch);
    }
}

type CustomEventsCallback = Box<dyn for<'a> FnMut(&mut CustomEventsContext<'a>) + Send + 'static>;

pub(crate) struct CustomEventsSource {
    config: CustomEventsConfig,
    callback: CustomEventsCallback,
    last_run: Option<Instant>,
}

impl std::fmt::Debug for CustomEventsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomEventsSource")
            .field("config", &self.config)
            .field("last_run", &self.last_run)
            .finish_non_exhaustive()
    }
}

impl CustomEventsSource {
    pub(crate) fn new<F>(config: CustomEventsConfig, callback: F) -> Self
    where
        F: for<'a> FnMut(&mut CustomEventsContext<'a>) + Send + 'static,
    {
        Self {
            config,
            callback: Box::new(callback),
            last_run: None,
        }
    }
}

impl Source for CustomEventsSource {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        let now = Instant::now();
        if let Some(last_run) = self.last_run
            && now.duration_since(last_run) < self.config.minimum_interval
        {
            return;
        }
        self.last_run = Some(now);

        let mut custom_ctx = CustomEventsContext {
            collector: ctx.collector,
            drain_epoch: ctx.drain_epoch,
            timestamp_ns: clock_monotonic_ns(),
        };
        (self.callback)(&mut custom_ctx);
    }

    fn name(&self) -> &'static str {
        "custom_events"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::buffer;
    use crate::telemetry::recorder::SharedState;
    use dial9_trace_format::TraceEvent;
    use dial9_trace_format::decoder::Decoder;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, serde::Deserialize, TraceEvent)]
    struct TestEvent {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        value: u64,
    }

    fn decode_test_events(bytes: &[u8]) -> Vec<TestEvent> {
        let mut decoder = Decoder::new(bytes).expect("batch should contain a valid trace");
        let mut events = Vec::new();
        decoder
            .for_each_event(|raw| {
                if raw.name == "TestEvent" {
                    events.push(raw.deserialize().expect("test event should decode"));
                }
            })
            .expect("decode batch");
        events
    }

    #[test]
    fn default_minimum_interval_runs_every_flush_cycle() {
        assert_eq!(
            CustomEventsConfig::default().minimum_interval(),
            Duration::ZERO
        );
    }

    #[test]
    fn source_records_callback_events() {
        let shared = SharedState::new(0, None);
        let thread_roles = std::collections::HashMap::new();
        let ctx = FlushContext {
            collector: &shared.collector,
            drain_epoch: &shared.drain_epoch,
            thread_roles: &thread_roles,
        };
        let mut source = CustomEventsSource::new(CustomEventsConfig::default(), |ctx| {
            ctx.record_event(TestEvent {
                timestamp_ns: ctx.timestamp_ns(),
                value: 42,
            });
        });

        source.flush(&ctx);
        buffer::drain_to_collector(&shared.collector);

        let batch = shared.collector.next().expect("source should emit a batch");
        let events = decode_test_events(batch.encoded_bytes());

        assert_eq!(events.len(), 1);
        assert!(events[0].timestamp_ns > 0);
        assert_eq!(events[0].value, 42);
    }

    #[test]
    fn source_respects_minimum_interval() {
        let shared = SharedState::new(0, None);
        let thread_roles = std::collections::HashMap::new();
        let ctx = FlushContext {
            collector: &shared.collector,
            drain_epoch: &shared.drain_epoch,
            thread_roles: &thread_roles,
        };
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let callback_calls = calls.clone();
        let config = CustomEventsConfig::builder()
            .minimum_interval(Duration::from_secs(60))
            .build();
        let mut source = CustomEventsSource::new(config, move |_ctx| {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        });

        source.flush(&ctx);
        source.flush(&ctx);

        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn zero_minimum_interval_does_not_throttle() {
        let shared = SharedState::new(0, None);
        let thread_roles = std::collections::HashMap::new();
        let ctx = FlushContext {
            collector: &shared.collector,
            drain_epoch: &shared.drain_epoch,
            thread_roles: &thread_roles,
        };
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let callback_calls = calls.clone();
        let mut source = CustomEventsSource::new(CustomEventsConfig::default(), move |_ctx| {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        });

        source.flush(&ctx);
        source.flush(&ctx);

        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}

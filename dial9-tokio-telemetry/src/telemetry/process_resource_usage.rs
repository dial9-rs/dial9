//! Process resource usage sampled from the operating system.

use std::time::Duration;

const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

/// Configuration for process resource usage sampling.
///
/// Built via `ProcessResourceUsageConfig::builder()...build()` and enabled with
/// [`TracedRuntimeBuilder::with_process_resource_usage`](crate::telemetry::TracedRuntimeBuilder::with_process_resource_usage).
#[derive(Debug, Clone, bon::Builder)]
pub struct ProcessResourceUsageConfig {
    /// Minimum time between samples.
    #[builder(default = DEFAULT_SAMPLE_INTERVAL)]
    sample_interval: Duration,
}

impl Default for ProcessResourceUsageConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl ProcessResourceUsageConfig {
    /// Minimum time between samples.
    pub fn sample_interval(&self) -> Duration {
        self.sample_interval
    }
}

#[cfg(unix)]
mod unix {
    use super::ProcessResourceUsageConfig;
    use crate::rate_limit::rate_limited;
    use crate::telemetry::buffer::record_encodable_event;
    use crate::telemetry::events::clock_monotonic_ns;
    use crate::telemetry::format::ProcessResourceUsageEvent;
    use crate::telemetry::recorder::source::{FlushContext, Source};
    use std::io;
    use std::mem::MaybeUninit;
    use std::time::{Duration, Instant};

    #[cfg(target_vendor = "apple")]
    const RU_MAXRSS_MULTIPLIER: u64 = 1;
    #[cfg(not(target_vendor = "apple"))]
    const RU_MAXRSS_MULTIPLIER: u64 = 1024;

    /// Flush-thread source that samples process `getrusage(RUSAGE_SELF)`.
    #[derive(Debug)]
    pub(crate) struct ProcessResourceUsageSource {
        config: ProcessResourceUsageConfig,
        last_sample: Option<Instant>,
    }

    #[derive(Debug, Clone, Copy)]
    struct ProcessResourceUsageSnapshot {
        user_cpu_ns: u64,
        system_cpu_ns: u64,
        max_rss_bytes: u64,
        minor_faults: u64,
        major_faults: u64,
        block_input_ops: u64,
        block_output_ops: u64,
        voluntary_context_switches: u64,
        involuntary_context_switches: u64,
    }

    impl ProcessResourceUsageSource {
        pub(crate) fn new(config: ProcessResourceUsageConfig) -> Self {
            Self {
                config,
                last_sample: None,
            }
        }
    }

    impl Source for ProcessResourceUsageSource {
        fn flush(&mut self, ctx: &FlushContext<'_>) {
            let now = Instant::now();
            if let Some(last_sample) = self.last_sample
                && now.duration_since(last_sample) < self.config.sample_interval
            {
                return;
            }
            self.last_sample = Some(now);

            match read_process_resource_usage() {
                Ok(snapshot) => {
                    let event = snapshot.into_event(clock_monotonic_ns());
                    record_encodable_event(&event, ctx.collector, ctx.drain_epoch);
                }
                Err(e) => rate_limited!(Duration::from_secs(60), {
                    tracing::warn!("failed to read process resource usage via getrusage: {e}");
                }),
            }
        }

        fn name(&self) -> &'static str {
            "process_resource_usage"
        }
    }

    impl ProcessResourceUsageSnapshot {
        fn into_event(self, timestamp_ns: u64) -> ProcessResourceUsageEvent {
            ProcessResourceUsageEvent {
                timestamp_ns,
                user_cpu_ns: self.user_cpu_ns,
                system_cpu_ns: self.system_cpu_ns,
                max_rss_bytes: self.max_rss_bytes,
                minor_faults: self.minor_faults,
                major_faults: self.major_faults,
                block_input_ops: self.block_input_ops,
                block_output_ops: self.block_output_ops,
                voluntary_context_switches: self.voluntary_context_switches,
                involuntary_context_switches: self.involuntary_context_switches,
            }
        }
    }

    fn read_process_resource_usage() -> io::Result<ProcessResourceUsageSnapshot> {
        snapshot_from_rusage(read_rusage()?)
    }

    fn read_rusage() -> io::Result<libc::rusage> {
        let mut usage = MaybeUninit::<libc::rusage>::uninit();

        // SAFETY: `usage.as_mut_ptr()` points to valid stack memory for libc
        // to initialize, and `RUSAGE_SELF` asks for this process only.
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: getrusage returned success, so it initialized `usage`.
        Ok(unsafe { usage.assume_init() })
    }

    fn snapshot_from_rusage(usage: libc::rusage) -> io::Result<ProcessResourceUsageSnapshot> {
        Ok(ProcessResourceUsageSnapshot {
            user_cpu_ns: timeval_to_ns(usage.ru_utime, "ru_utime")?,
            system_cpu_ns: timeval_to_ns(usage.ru_stime, "ru_stime")?,
            max_rss_bytes: max_rss_to_bytes(usage.ru_maxrss)?,
            minor_faults: nonnegative(usage.ru_minflt, "ru_minflt")?,
            major_faults: nonnegative(usage.ru_majflt, "ru_majflt")?,
            block_input_ops: nonnegative(usage.ru_inblock, "ru_inblock")?,
            block_output_ops: nonnegative(usage.ru_oublock, "ru_oublock")?,
            voluntary_context_switches: nonnegative(usage.ru_nvcsw, "ru_nvcsw")?,
            involuntary_context_switches: nonnegative(usage.ru_nivcsw, "ru_nivcsw")?,
        })
    }

    fn timeval_to_ns(tv: libc::timeval, field: &'static str) -> io::Result<u64> {
        let seconds = nonnegative(tv.tv_sec, field)?;
        let micros = nonnegative(tv.tv_usec, field)?;
        let second_ns = checked_mul(seconds, 1_000_000_000, field)?;
        let micro_ns = checked_mul(micros, 1_000, field)?;
        second_ns.checked_add(micro_ns).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{field} overflowed u64"),
            )
        })
    }

    fn max_rss_to_bytes(max_rss: libc::c_long) -> io::Result<u64> {
        let value = nonnegative(max_rss, "ru_maxrss")?;
        checked_mul(value, RU_MAXRSS_MULTIPLIER, "ru_maxrss")
    }

    fn checked_mul(value: u64, multiplier: u64, field: &'static str) -> io::Result<u64> {
        value.checked_mul(multiplier).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{field} overflowed u64"),
            )
        })
    }

    fn nonnegative<T>(value: T, field: &'static str) -> io::Result<u64>
    where
        T: TryInto<u64>,
    {
        value.try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("getrusage returned negative {field}"),
            )
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::telemetry::buffer;
        use crate::telemetry::recorder::SharedState;
        use serde::Deserialize;
        use std::collections::HashMap;

        #[derive(Debug, Deserialize)]
        #[serde(tag = "event")]
        enum DecodedEvent {
            ProcessResourceUsageEvent(DecodedProcessResourceUsageEvent),
            #[serde(other)]
            Other,
        }

        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct DecodedProcessResourceUsageEvent {
            timestamp_ns: u64,
            user_cpu_ns: u64,
            system_cpu_ns: u64,
            max_rss_bytes: u64,
            minor_faults: u64,
            major_faults: u64,
            block_input_ops: u64,
            block_output_ops: u64,
            voluntary_context_switches: u64,
            involuntary_context_switches: u64,
        }

        fn decode_process_resource_usage_events(
            bytes: &[u8],
        ) -> Vec<DecodedProcessResourceUsageEvent> {
            let mut decoder = dial9_trace_format::decoder::Decoder::new(bytes)
                .expect("encoded process resource usage batch should have a valid trace header");
            let mut events = Vec::new();
            decoder
                .for_each_event(|raw| {
                    match raw
                        .deserialize()
                        .expect("encoded process resource usage event should deserialize")
                    {
                        DecodedEvent::ProcessResourceUsageEvent(event) => events.push(event),
                        DecodedEvent::Other => {}
                    }
                })
                .expect("encoded process resource usage batch should decode");
            events
        }

        #[test]
        fn read_process_resource_usage_returns_metrics() {
            let snapshot = read_process_resource_usage()
                .expect("getrusage should succeed for the current process");
            assert!(snapshot.max_rss_bytes > 0);
        }

        #[test]
        fn source_emits_process_resource_usage_event() {
            let shared = SharedState::new(0, None);
            let thread_roles = HashMap::new();
            let ctx = FlushContext {
                collector: &shared.collector,
                drain_epoch: &shared.drain_epoch,
                thread_roles: &thread_roles,
            };
            let mut source = ProcessResourceUsageSource::new(ProcessResourceUsageConfig::default());

            source.flush(&ctx);
            buffer::drain_to_collector(&shared.collector);

            let batch = shared.collector.next().expect("source should emit a batch");
            let events = decode_process_resource_usage_events(batch.encoded_bytes());

            assert_eq!(events.len(), 1);
            let event = &events[0];
            assert!(event.timestamp_ns > 0);
            assert!(event.max_rss_bytes > 0);
        }

        #[test]
        fn source_respects_sample_interval() {
            let shared = SharedState::new(0, None);
            let thread_roles = HashMap::new();
            let ctx = FlushContext {
                collector: &shared.collector,
                drain_epoch: &shared.drain_epoch,
                thread_roles: &thread_roles,
            };
            let config = ProcessResourceUsageConfig::builder()
                .sample_interval(Duration::from_secs(60))
                .build();
            let mut source = ProcessResourceUsageSource::new(config);

            source.flush(&ctx);
            source.flush(&ctx);
            buffer::drain_to_collector(&shared.collector);

            let batch = shared.collector.next().expect("source should emit a batch");
            let events = decode_process_resource_usage_events(batch.encoded_bytes());

            assert_eq!(events.len(), 1);
        }
    }
}

#[cfg(unix)]
pub(crate) use unix::ProcessResourceUsageSource;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sample_interval_is_100ms() {
        assert_eq!(
            ProcessResourceUsageConfig::default().sample_interval(),
            DEFAULT_SAMPLE_INTERVAL
        );
    }
}

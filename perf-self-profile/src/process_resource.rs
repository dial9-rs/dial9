//! Process resource usage sampled from `getrusage(RUSAGE_SELF)`, exposed as a
//! dial9 [`Source`](dial9_core::source::Source).

use std::time::Duration;

const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

/// Wire-format event for process resource usage sampled from `getrusage(RUSAGE_SELF)`.
#[derive(Debug, dial9_trace_format::TraceEvent)]
#[traceevent(wire_slot)]
#[cfg_attr(not(feature = "unstable-events"), non_exhaustive)]
pub struct ProcessResourceUsageEvent {
    /// Monotonic timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Cumulative user CPU time used by this process.
    #[traceevent(unit = "ns", kind = "counter")]
    pub user_cpu_ns: u64,
    /// Cumulative system CPU time used by this process.
    #[traceevent(unit = "ns", kind = "counter")]
    pub system_cpu_ns: u64,
    /// Maximum resident set size in bytes.
    #[traceevent(unit = "bytes", kind = "gauge")]
    pub max_rss_bytes: u64,
    /// Page faults serviced without disk I/O.
    #[traceevent(kind = "counter")]
    pub minor_faults: u64,
    /// Page faults serviced with disk I/O.
    #[traceevent(kind = "counter")]
    pub major_faults: u64,
    /// Block input operations performed by the process.
    #[traceevent(kind = "counter")]
    pub block_input_ops: u64,
    /// Block output operations performed by the process.
    #[traceevent(kind = "counter")]
    pub block_output_ops: u64,
    /// Voluntary context switches performed by the process.
    #[traceevent(kind = "counter")]
    pub voluntary_context_switches: u64,
    /// Involuntary context switches performed by the process.
    #[traceevent(kind = "counter")]
    pub involuntary_context_switches: u64,
}

/// Configuration for process resource usage sampling.
///
/// Build via `ProcessResourceUsageConfig::builder()...build()`, then plug the
/// [`ProcessResourceUsageSource`] into a dial9 recorder.
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
    use super::{ProcessResourceUsageConfig, ProcessResourceUsageEvent};
    use dial9_core::clock::clock_monotonic_ns;
    use dial9_core::rate_limited;
    use dial9_core::source::{FlushContext, Source};
    use std::io;
    use std::mem::MaybeUninit;
    use std::time::{Duration, Instant};

    #[cfg(target_vendor = "apple")]
    const RU_MAXRSS_MULTIPLIER: u64 = 1;
    #[cfg(not(target_vendor = "apple"))]
    const RU_MAXRSS_MULTIPLIER: u64 = 1024;

    /// Flush-thread source that samples process `getrusage(RUSAGE_SELF)`.
    #[derive(Debug)]
    pub struct ProcessResourceUsageSource {
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
        /// Build a source that samples at the config's interval.
        pub fn new(config: ProcessResourceUsageConfig) -> Self {
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
                    ctx.record_event(&event);
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

    #[cfg(all(test, feature = "test-util"))]
    mod tests {
        use super::*;
        use dial9_core::shared_state::SharedState;
        use dial9_core::test_util;
        use serde::Deserialize;

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

        fn read_after_rss_accounting_tick() -> ProcessResourceUsageSnapshot {
            let deadline = Instant::now() + Duration::from_secs(1);
            let mut working_set = vec![0_u8; 1024 * 1024];
            loop {
                // FreeBSD updates ru_maxrss from its statclock path while the
                // process is running. Sleeping alone may never trigger that
                // accounting, so touch resident pages through at least one
                // likely clock tick before retrying getrusage.
                let work_until = Instant::now() + Duration::from_millis(10);
                while Instant::now() < work_until {
                    for page in working_set.chunks_mut(4096) {
                        page[0] = page[0].wrapping_add(1);
                    }
                    std::hint::black_box(&working_set);
                }

                let snapshot = read_process_resource_usage()
                    .expect("getrusage should succeed for the current process");
                if snapshot.max_rss_bytes > 0 {
                    return snapshot;
                }
                assert!(
                    Instant::now() < deadline,
                    "ru_maxrss remained zero after waiting for a resource-accounting tick"
                );
            }
        }

        #[test]
        fn read_process_resource_usage_returns_metrics() {
            let snapshot = read_after_rss_accounting_tick();
            assert!(snapshot.max_rss_bytes > 0);
        }

        #[test]
        fn max_rss_scales_to_bytes_and_rejects_negative_values() {
            assert_eq!(
                max_rss_to_bytes(4).expect("positive ru_maxrss should convert"),
                4 * RU_MAXRSS_MULTIPLIER
            );
            assert!(max_rss_to_bytes(-1).is_err());
        }

        #[test]
        fn source_emits_process_resource_usage_event() {
            // FreeBSD can report ru_maxrss == 0 before its first accounting
            // tick. Once it becomes non-zero it is a process maximum, so the
            // source sample below must retain the real RSS assertion.
            read_after_rss_accounting_tick();

            let shared = SharedState::new(0);
            let ctx = shared.flush_context();
            let mut source = ProcessResourceUsageSource::new(ProcessResourceUsageConfig::default());

            source.flush(&ctx);
            let events: Vec<_> = test_util::drain_encoded_batches(&shared)
                .iter()
                .flat_map(|b| decode_process_resource_usage_events(b))
                .collect();

            assert_eq!(events.len(), 1);
            let event = &events[0];
            assert!(event.timestamp_ns > 0);
            assert!(event.max_rss_bytes > 0);
        }

        #[test]
        fn source_respects_sample_interval() {
            let shared = SharedState::new(0);
            let ctx = shared.flush_context();
            let config = ProcessResourceUsageConfig::builder()
                .sample_interval(Duration::from_secs(60))
                .build();
            let mut source = ProcessResourceUsageSource::new(config);

            source.flush(&ctx);
            source.flush(&ctx);
            let events: Vec<_> = test_util::drain_encoded_batches(&shared)
                .iter()
                .flat_map(|b| decode_process_resource_usage_events(b))
                .collect();

            assert_eq!(events.len(), 1);
        }
    }
}

#[cfg(unix)]
pub use unix::ProcessResourceUsageSource;

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

    #[test]
    fn process_resource_usage_annotations() {
        use dial9_trace_format::TraceEvent;
        let entry = ProcessResourceUsageEvent::schema_entry();
        let annotations = |key| {
            entry
                .annotations()
                .iter()
                .filter(|annotation| annotation.key() == key)
                .map(|annotation| {
                    (
                        entry.fields()[annotation.field_index() as usize].name(),
                        annotation.value(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            annotations("unit"),
            vec![
                ("user_cpu_ns", "ns"),
                ("system_cpu_ns", "ns"),
                ("max_rss_bytes", "bytes"),
            ]
        );
        assert_eq!(
            annotations("kind"),
            vec![
                ("user_cpu_ns", "counter"),
                ("system_cpu_ns", "counter"),
                ("max_rss_bytes", "gauge"),
                ("minor_faults", "counter"),
                ("major_faults", "counter"),
                ("block_input_ops", "counter"),
                ("block_output_ops", "counter"),
                ("voluntary_context_switches", "counter"),
                ("involuntary_context_switches", "counter"),
            ]
        );
    }
}

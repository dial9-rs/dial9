use crate::telemetry::task_metadata::TaskId;
use dial9_trace_format::types::{EventEncoder, FieldType};
use dial9_trace_format::{InternedStackFrames, InternedString, TraceEvent, TraceField};
use serde::Serialize;
use std::fmt;
use std::io::{self, Write};

// ── WorkerId newtype ────────────────────────────────────────────────────────

/// Identifies a Tokio worker thread. Wraps a `u64` encoded as a varint on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Default)]
pub struct WorkerId(pub(crate) u64);

impl WorkerId {
    /// Sentinel for events from non-worker threads.
    pub const UNKNOWN: WorkerId = WorkerId(255);
    /// Sentinel for events from tokio's blocking thread pool.
    pub const BLOCKING: WorkerId = WorkerId(254);

    /// Returns the raw `u64` value.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<usize> for WorkerId {
    fn from(v: usize) -> Self {
        WorkerId(v as u64)
    }
}

impl From<u8> for WorkerId {
    fn from(v: u8) -> Self {
        WorkerId(v as u64)
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ── dial9-trace-format: TraceField impls ────────────────────────────────────

impl TraceField for TaskId {
    fn field_type() -> FieldType {
        FieldType::Varint
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_u64(self.0)
    }
}

impl TraceField for WorkerId {
    fn field_type() -> FieldType {
        FieldType::Varint
    }

    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_u64(self.0)
    }
}

// ── dial9-trace-format: derive structs ──────────────────────────────────────

/// Wire-format event for a task poll start.
#[derive(Debug, TraceEvent)]
#[traceevent(wire_slot)]
pub struct PollStartEvent {
    /// Timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Worker thread index.
    pub worker_id: WorkerId,
    /// Local queue depth (capped to u8).
    pub local_queue: u8,
    /// Task being polled.
    pub task_id: TaskId,
    /// Interned spawn location.
    pub spawn_loc: InternedString,
}

/// Wire-format event for a task poll end.
#[derive(Debug, TraceEvent)]
#[traceevent(wire_slot)]
pub struct PollEndEvent {
    /// Timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Worker thread index.
    pub worker_id: WorkerId,
}

/// Wire-format event for a worker park.
#[derive(Debug, TraceEvent)]
#[traceevent(wire_slot)]
pub struct WorkerParkEvent {
    /// Timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Worker thread index.
    pub worker_id: WorkerId,
    /// Local queue depth (capped to u8).
    pub local_queue: u8,
    /// Thread CPU time in nanoseconds.
    pub cpu_time_ns: u64,
    /// OS thread ID of the parking thread. On Linux/Android, the result of gettid();
    /// on other platforms, a synthetic per-process counter — see `events::current_tid`.
    pub tid: u32,
}

/// Wire-format event for a worker unpark.
#[derive(Debug, TraceEvent)]
#[traceevent(wire_slot)]
pub struct WorkerUnparkEvent {
    /// Timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Worker thread index.
    pub worker_id: WorkerId,
    /// Local queue depth (capped to u8).
    pub local_queue: u8,
    /// Thread CPU time in nanoseconds.
    pub cpu_time_ns: u64,
    /// Scheduling wait delta in nanoseconds.
    pub sched_wait_ns: u64,
    /// OS thread ID of the unparking thread. On Linux/Android, the result of gettid();
    /// on other platforms, a synthetic per-process counter — see `events::current_tid`.
    pub tid: u32,
}

#[derive(TraceEvent)]
#[traceevent(wire_slot)]
pub(crate) struct QueueSampleEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub global_queue: u8,
}

/// Wire-format event for process resource usage sampled from `getrusage(RUSAGE_SELF)`.
#[derive(Debug, TraceEvent)]
#[traceevent(wire_slot)]
#[cfg_attr(not(feature = "unstable-events"), non_exhaustive)]
pub struct ProcessResourceUsageEvent {
    /// Monotonic timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Cumulative user CPU time used by this process.
    #[traceevent(unit = "ns")]
    pub user_cpu_ns: u64,
    /// Cumulative system CPU time used by this process.
    #[traceevent(unit = "ns")]
    pub system_cpu_ns: u64,
    /// Maximum resident set size in bytes.
    #[traceevent(unit = "bytes")]
    pub max_rss_bytes: u64,
    /// Page faults serviced without disk I/O.
    pub minor_faults: u64,
    /// Page faults serviced with disk I/O.
    pub major_faults: u64,
    /// Block input operations performed by the process.
    pub block_input_ops: u64,
    /// Block output operations performed by the process.
    pub block_output_ops: u64,
    /// Voluntary context switches performed by the process.
    pub voluntary_context_switches: u64,
    /// Involuntary context switches performed by the process.
    pub involuntary_context_switches: u64,
}

/// Wire-format event for a TCP listener accept queue snapshot.
#[derive(Debug, TraceEvent)]
#[traceevent(wire_slot)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct TcpAcceptQueueEvent {
    /// Monotonic timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub(crate) timestamp_ns: u64,
    /// Linux socket cookie reported by sock_diag.
    pub(crate) socket_cookie: u64,
    /// Linux socket inode reported by sock_diag.
    pub(crate) socket_inode: u64,
    /// IP version for `local_addr`: 4 or 6.
    pub(crate) ip_version: u8,
    /// Local listener address.
    pub(crate) local_addr: String,
    /// Local listener port.
    pub(crate) local_port: u16,
    /// Completed connections waiting to be accepted.
    pub(crate) pending_connections: u32,
    /// Effective accept backlog limit.
    pub(crate) backlog_limit: u32,
}

/// Wire-format event for a task spawn.
#[derive(Debug, TraceEvent)]
#[traceevent(wire_slot)]
pub struct TaskSpawnEvent {
    /// Timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Spawned task identifier.
    pub task_id: TaskId,
    /// Interned spawn location.
    pub spawn_loc: InternedString,
    /// Whether this spawn was instrumented (via `Dial9TokioHandle::spawn`).
    pub instrumented: bool,
}

#[derive(TraceEvent)]
#[traceevent(wire_slot)]
pub(crate) struct TaskTerminateEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub task_id: TaskId,
}

/// Wire-format event for a task dump: async backtrace captured at a yield point
/// after the task stayed idle past the configured threshold.
#[derive(TraceEvent)]
#[traceevent(wire_slot)]
pub(crate) struct TaskDumpEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub task_id: TaskId,
    pub callchain: InternedStackFrames,
}

/// Wire-format event for a wake notification.
#[derive(Debug, TraceEvent)]
#[traceevent(wire_slot)]
pub struct WakeEventEvent {
    /// Timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Task that issued the wake.
    pub waker_task_id: TaskId,
    /// Task that was woken.
    pub woken_task_id: TaskId,
    /// Worker index that issued the wake (255 = unknown).
    pub target_worker: u8,
}

#[cfg(test)]
pub(crate) use dial9_core::format::{ClockSyncEvent, SegmentMetadataEvent};

// ── dial9-trace-format: decode ──────────────────────────────────────────────
// Decode via `Dial9Event` in `analysis_events.rs` using `Decoder::for_each_event`.

/// Decode all events from a `dial9-trace-format` byte slice into `Dial9Event`s.
/// Test-only helper used by internal tests across multiple modules.
#[cfg(test)]
pub(crate) fn decode_events(
    data: &[u8],
) -> std::io::Result<Vec<crate::telemetry::analysis_events::Dial9Event>> {
    use crate::telemetry::analysis_events::Dial9Event;
    use dial9_trace_format::decoder::Decoder;

    let mut dec = Decoder::new(data).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid trace header")
    })?;
    let mut events = Vec::new();

    dec.for_each_event(|raw| {
        let ev: Dial9Event = match raw.deserialize() {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(event_name = raw.name, error = %e, "skipping unrecognized event in decode");
                return;
            }
        };
        if !matches!(ev, Dial9Event::Other) {
            events.push(ev);
        }
    })
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_resource_usage_unit_annotations() {
        use dial9_trace_format::TraceEvent;
        let entry = ProcessResourceUsageEvent::schema_entry();
        let units: Vec<(&str, &str)> = entry
            .annotations()
            .iter()
            .filter(|a| a.key() == "unit")
            .map(|a| (entry.fields()[a.field_index() as usize].name(), a.value()))
            .collect();
        assert_eq!(
            units,
            vec![
                ("user_cpu_ns", "ns"),
                ("system_cpu_ns", "ns"),
                ("max_rss_bytes", "bytes"),
            ]
        );
    }
}

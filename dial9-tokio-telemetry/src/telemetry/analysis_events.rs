//! Decode-side companion structs for built-in trace events.
//!
//! These structs mirror the wire-format event types but use owned, resolved
//! types (`String` instead of `InternedString`, `Vec<u64>` instead of
//! `InternedStackFrames`). They derive [`serde::Deserialize`] so they can be
//! used as decode targets with the serde-based trace iterator.
//!
//! # Backwards Compatibility
//!
//! All structs and the [`Dial9Event`] enum are `#[non_exhaustive]`, so new
//! fields and variants can be added without breaking changes. When new fields
//! are added to the wire format, old traces simply won't have those fields in
//! their schema — serde will skip them during deserialization.
//!
//! # Usage
//!
//! ```no_run
//! use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
//! use dial9_trace_format::decoder::Decoder;
//!
//! # let bytes: &[u8] = &[];
//! let mut dec = Decoder::new(bytes).unwrap();
//! dec.for_each_event(|raw| {
//!     let ev: Dial9Event = raw.deserialize().unwrap();
//!     // ...
//! }).unwrap();
//! ```

use serde::Deserialize;

/// Worker thread identifier.
///
/// Wraps a `u64` matching the wire encoding. Use the [`UNKNOWN`](Self::UNKNOWN)
/// and [`BLOCKING`](Self::BLOCKING) constants to test for sentinel values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[repr(transparent)]
pub struct WorkerId(pub u64);

impl WorkerId {
    /// Sentinel for events from non-worker threads (value `255`).
    pub const UNKNOWN: WorkerId = WorkerId(255);
    /// Sentinel for events from Tokio's blocking thread pool (value `254`).
    pub const BLOCKING: WorkerId = WorkerId(254);

    /// Returns the raw `u64` value.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for WorkerId {
    fn from(v: u64) -> Self {
        WorkerId(v)
    }
}

impl std::fmt::Display for WorkerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Task identifier (decoded as a plain `u64`).
pub type TaskId = u64;

/// What triggered a CPU sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CpuSampleSource {
    /// Periodic CPU profiling sample (frequency-based).
    CpuProfile,
    /// Context switch captured by per-thread sched event tracking.
    SchedEvent,
    /// Unknown variant from a newer trace format.
    Unknown(u64),
}

impl<'de> serde::Deserialize<'de> for CpuSampleSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let v = u64::deserialize(deserializer)?;
        match v {
            0 => Ok(Self::CpuProfile),
            1 => Ok(Self::SchedEvent),
            other => Ok(Self::Unknown(other)),
        }
    }
}

/// A task poll began on a worker thread.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct PollStartEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Worker thread index.
    pub worker_id: WorkerId,
    /// Local queue depth.
    pub local_queue: u8,
    /// Task being polled.
    pub task_id: TaskId,
    /// Spawn location string.
    pub spawn_loc: String,
}

/// A task poll completed on a worker thread.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct PollEndEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Worker thread index.
    pub worker_id: WorkerId,
}

/// A worker thread parked (went idle).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct WorkerParkEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Worker thread index.
    pub worker_id: WorkerId,
    /// Local queue depth.
    pub local_queue: u8,
    /// Thread CPU time in nanoseconds.
    pub cpu_time_ns: u64,
    /// OS thread ID.
    pub tid: u32,
}

/// A worker thread unparked (resumed).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct WorkerUnparkEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Worker thread index.
    pub worker_id: WorkerId,
    /// Local queue depth.
    pub local_queue: u8,
    /// Thread CPU time in nanoseconds.
    pub cpu_time_ns: u64,
    /// Scheduling wait delta in nanoseconds.
    pub sched_wait_ns: u64,
    /// OS thread ID.
    pub tid: u32,
}

/// Periodic sample of the global task queue depth.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct QueueSampleEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Global queue depth.
    pub global_queue: u8,
}

/// A new task was spawned.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct TaskSpawnEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Spawned task identifier.
    pub task_id: TaskId,
    /// Spawn location string.
    pub spawn_loc: String,
    /// Whether this spawn was instrumented.
    #[serde(default)]
    pub instrumented: bool,
}

/// A task terminated.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct TaskTerminateEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Task that terminated.
    pub task_id: TaskId,
}

/// A CPU stack trace sample.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct CpuSampleEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Worker thread index.
    pub worker_id: WorkerId,
    /// OS thread ID.
    pub tid: u32,
    /// What triggered this sample.
    pub source: CpuSampleSource,
    /// Thread name, if known.
    pub thread_name: Option<String>,
    /// Raw instruction pointer addresses (leaf first).
    pub callchain: Vec<u64>,
    /// CPU the sample was taken on.
    pub cpu: Option<u64>,
}

/// Async backtrace captured at a yield point.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct TaskDumpEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Task that was idle.
    pub task_id: TaskId,
    /// Raw instruction pointer addresses (leaf first).
    pub callchain: Vec<u64>,
}

/// One task woke another task.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct WakeEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Task that issued the wake.
    pub waker_task_id: TaskId,
    /// Task that was woken.
    pub woken_task_id: TaskId,
    /// Worker thread index that issued the wake (255 = unknown).
    pub target_worker: u8,
}

/// Key-value metadata written at the start of each segment.
///
/// The wire format encodes entries as a list of key-value pairs, but we
/// deserialize into a `HashMap` for convenient lookup. Keys are unique
/// within a segment.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct SegmentMetadataEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Key-value metadata pairs.
    pub entries: std::collections::HashMap<String, String>,
}

/// Clock-correlation anchor.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct ClockSyncEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Nanoseconds since the Unix epoch.
    pub realtime_ns: u64,
}

/// A sampled memory allocation event.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct AllocEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// OS thread ID.
    pub tid: u32,
    /// Allocation size in bytes.
    pub size: u64,
    /// Returned pointer address.
    pub addr: u64,
    /// Raw instruction pointer addresses (leaf first).
    pub callchain: Vec<u64>,
}

/// A deallocation paired with a previously-sampled allocation.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct FreeEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// OS thread ID.
    pub tid: u32,
    /// Pointer that was freed.
    pub addr: u64,
    /// Size of the allocation being freed.
    pub size: u64,
    /// Monotonic-ns timestamp of the original allocation.
    pub alloc_timestamp_ns: u64,
}

/// Process resource usage sampled from `getrusage(RUSAGE_SELF)`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct ProcessResourceUsageEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Cumulative user CPU time used by this process.
    pub user_cpu_ns: u64,
    /// Cumulative system CPU time used by this process.
    pub system_cpu_ns: u64,
    /// Maximum resident set size in bytes.
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

/// TCP listener accept queue snapshot.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct TcpAcceptQueueEvent {
    /// Timestamp in nanoseconds (monotonic).
    pub timestamp_ns: u64,
    /// Linux socket cookie reported by sock_diag.
    pub socket_cookie: u64,
    /// Linux socket inode reported by sock_diag.
    pub socket_inode: u64,
    /// IP version for `local_addr`: 4 or 6.
    pub ip_version: u8,
    /// Local listener address.
    pub local_addr: String,
    /// Local listener port.
    pub local_port: u16,
    /// Completed connections waiting to be accepted.
    pub pending_connections: u32,
    /// Effective accept backlog limit.
    pub backlog_limit: u32,
}

/// An application-defined custom event not recognized as a built-in type.
///
/// Fields are resolved from the string/stack pools at parse time so they
/// remain valid after the segment's pools are discarded.
///
/// Can be deserialized directly from a `RawEvent` via
/// `raw_event.deserialize::<CustomEvent>()`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct CustomEvent {
    /// Event type name from the wire schema (e.g. `"RequestCompleted"`).
    pub name: String,
    /// Monotonic timestamp in nanoseconds, if the event schema has one.
    pub timestamp_ns: Option<u64>,
    /// Named field values, resolved from pools. Excludes `event` and `timestamp_ns`.
    pub fields: std::collections::HashMap<String, dial9_trace_format::FieldValue>,
}

impl<'de> Deserialize<'de> for CustomEvent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Deserialize the entire event as a flat HashMap. The deserializer
        // presents "event", "timestamp_ns", and all schema fields as map entries.
        let mut map: std::collections::HashMap<String, dial9_trace_format::FieldValue> =
            serde::Deserialize::deserialize(deserializer)?;

        let name = match map.remove("event") {
            Some(dial9_trace_format::FieldValue::String(s)) => s,
            _ => return Err(serde::de::Error::missing_field("event")),
        };

        let timestamp_ns = match map.remove("timestamp_ns") {
            Some(dial9_trace_format::FieldValue::Varint(t)) => Some(t),
            _ => None,
        };

        Ok(CustomEvent {
            name,
            timestamp_ns,
            fields: map,
        })
    }
}

/// Tagged enum of all built-in event types for use as a serde decode target.
///
/// The discriminant matches the wire schema name (e.g. `"PollStartEvent"`).
///
/// Unknown event types (custom user events, future additions) land in the
/// [`Custom`](Self::Custom) variant when decoded through
/// [`TraceReader`](crate::telemetry::analysis::TraceReader), or in
/// [`Other`](Self::Other) when using `raw_event.deserialize::<Dial9Event>()`
/// directly. In the latter case, call `raw_event.deserialize::<CustomEvent>()`
/// to get the full custom event with fields.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "event")]
#[non_exhaustive]
pub enum Dial9Event {
    /// A task poll began.
    PollStartEvent(PollStartEvent),
    /// A task poll completed.
    PollEndEvent(PollEndEvent),
    /// A worker thread parked.
    WorkerParkEvent(WorkerParkEvent),
    /// A worker thread unparked.
    WorkerUnparkEvent(WorkerUnparkEvent),
    /// Global queue depth sample.
    QueueSampleEvent(QueueSampleEvent),
    /// A task was spawned.
    TaskSpawnEvent(TaskSpawnEvent),
    /// A task terminated.
    TaskTerminateEvent(TaskTerminateEvent),
    /// A CPU stack trace sample.
    CpuSampleEvent(CpuSampleEvent),
    /// An async backtrace at a yield point.
    TaskDumpEvent(TaskDumpEvent),
    /// One task woke another.
    ///
    /// Wire schema name is `"WakeEventEvent"` for historical reasons.
    #[serde(rename = "WakeEventEvent")]
    WakeEvent(WakeEvent),
    /// Segment metadata.
    SegmentMetadataEvent(SegmentMetadataEvent),
    /// Clock sync anchor.
    ClockSyncEvent(ClockSyncEvent),
    /// A sampled allocation.
    AllocEvent(AllocEvent),
    /// A deallocation.
    FreeEvent(FreeEvent),
    /// Process resource usage.
    ProcessResourceUsageEvent(ProcessResourceUsageEvent),
    /// TCP listener accept queue snapshot.
    TcpAcceptQueueEvent(TcpAcceptQueueEvent),
    /// An application-defined custom event. Produced by
    /// [`TraceReader`](crate::telemetry::analysis::TraceReader) for unknown
    /// event types. Not populated by direct serde deserialization (use
    /// [`Other`](Self::Other) + `raw_event.deserialize::<CustomEvent>()`).
    #[serde(skip)]
    Custom(CustomEvent),
    /// Unknown event type encountered during direct serde deserialization.
    /// Use `raw_event.deserialize::<CustomEvent>()` to get the full event.
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::format;
    use crate::telemetry::task_metadata::TaskId;
    use dial9_trace_format::decoder::Decoder;
    use dial9_trace_format::encoder::Encoder;

    #[test]
    fn synthetic_trace_round_trip_all_events() {
        let mut enc = Encoder::new();

        // 1. PollStartEvent
        let spawn_loc = enc.intern_string("src/main.rs:42").unwrap();
        enc.write(&format::PollStartEvent {
            timestamp_ns: 1_000_000,
            worker_id: format::WorkerId::from(2u8),
            local_queue: 5,
            task_id: TaskId::from_u32(100),
            spawn_loc,
        })
        .unwrap();

        // 2. PollEndEvent
        enc.write(&format::PollEndEvent {
            timestamp_ns: 1_000_500,
            worker_id: format::WorkerId::from(2u8),
        })
        .unwrap();

        // 3. WorkerParkEvent
        enc.write(&format::WorkerParkEvent {
            timestamp_ns: 2_000_000,
            worker_id: format::WorkerId::from(1u8),
            local_queue: 0,
            cpu_time_ns: 500_000,
            tid: 12345,
        })
        .unwrap();

        // 4. WorkerUnparkEvent
        enc.write(&format::WorkerUnparkEvent {
            timestamp_ns: 3_000_000,
            worker_id: format::WorkerId::from(1u8),
            local_queue: 3,
            cpu_time_ns: 600_000,
            sched_wait_ns: 100_000,
            tid: 12345,
        })
        .unwrap();

        // 5. QueueSampleEvent
        enc.write(&format::QueueSampleEvent {
            timestamp_ns: 4_000_000,
            global_queue: 7,
        })
        .unwrap();

        // 6. TaskSpawnEvent
        let spawn_loc2 = enc.intern_string("src/lib.rs:10").unwrap();
        enc.write(&format::TaskSpawnEvent {
            timestamp_ns: 5_000_000,
            task_id: TaskId::from_u32(200),
            spawn_loc: spawn_loc2,
            instrumented: true,
        })
        .unwrap();

        // 7. TaskTerminateEvent
        enc.write(&format::TaskTerminateEvent {
            timestamp_ns: 6_000_000,
            task_id: TaskId::from_u32(200),
        })
        .unwrap();

        // 8. CpuSampleEvent
        let thread_name = enc.intern_string("tokio-runtime-worker").unwrap();
        let callchain = enc
            .intern_stack_frames(&[0xdead_beef, 0xcafe_babe])
            .unwrap();
        enc.write(&format::CpuSampleEvent {
            timestamp_ns: 7_000_000,
            worker_id: format::WorkerId::from(0u8),
            tid: 9999,
            source: crate::telemetry::events::CpuSampleSource::CpuProfile,
            thread_name: Some(thread_name),
            callchain,
            cpu: Some(3),
        })
        .unwrap();

        // 9. TaskDumpEvent
        let dump_chain = enc.intern_stack_frames(&[0x1111, 0x2222, 0x3333]).unwrap();
        enc.write(&format::TaskDumpEvent {
            timestamp_ns: 8_000_000,
            task_id: TaskId::from_u32(100),
            callchain: dump_chain,
        })
        .unwrap();

        // 10. WakeEventEvent
        enc.write(&format::WakeEventEvent {
            timestamp_ns: 9_000_000,
            waker_task_id: TaskId::from_u32(100),
            woken_task_id: TaskId::from_u32(200),
            target_worker: 1,
        })
        .unwrap();

        // 11. SegmentMetadataEvent
        enc.write(&format::SegmentMetadataEvent {
            timestamp_ns: 10_000_000,
            entries: vec![
                ("runtime".to_string(), "main".to_string()),
                ("version".to_string(), "0.3.11".to_string()),
            ],
        })
        .unwrap();

        // 12. ClockSyncEvent
        enc.write(&format::ClockSyncEvent {
            timestamp_ns: 11_000_000,
            realtime_ns: 1_700_000_000_000_000_000,
        })
        .unwrap();

        // 13. AllocEvent
        let alloc_chain = enc.intern_stack_frames(&[0xaaaa, 0xbbbb]).unwrap();
        enc.write(&format::AllocEvent {
            timestamp_ns: 12_000_000,
            tid: 5555,
            size: 1024,
            addr: 0x7fff_0000_1000,
            callchain: alloc_chain,
        })
        .unwrap();

        // 14. FreeEvent
        enc.write(&format::FreeEvent {
            timestamp_ns: 13_000_000,
            tid: 5555,
            addr: 0x7fff_0000_1000,
            size: 1024,
            alloc_timestamp_ns: 12_000_000,
        })
        .unwrap();

        // 15. ProcessResourceUsageEvent
        enc.write(&format::ProcessResourceUsageEvent {
            timestamp_ns: 14_000_000,
            user_cpu_ns: 1_000_000,
            system_cpu_ns: 2_000_000,
            max_rss_bytes: 64 * 1024 * 1024,
            minor_faults: 10,
            major_faults: 1,
            block_input_ops: 2,
            block_output_ops: 3,
            voluntary_context_switches: 4,
            involuntary_context_switches: 5,
        })
        .unwrap();

        // 16. TcpAcceptQueueEvent
        enc.write(&format::TcpAcceptQueueEvent {
            timestamp_ns: 15_000_000,
            socket_cookie: 67890,
            socket_inode: 12345,
            ip_version: 4,
            local_addr: "127.0.0.1".to_string(),
            local_port: 8080,
            pending_connections: 3,
            backlog_limit: 128,
        })
        .unwrap();

        // ── Decode ──────────────────────────────────────────────────────────
        let bytes = enc.finish();
        let mut dec = Decoder::new(&bytes).expect("synthetic trace should have a valid header");
        let mut events: Vec<Dial9Event> = Vec::new();
        dec.for_each_event(|raw| {
            events.push(
                raw.deserialize()
                    .expect("synthetic event should deserialize"),
            );
        })
        .expect("decode");

        assert_eq!(events.len(), 16);

        // 1. PollStartEvent
        let Dial9Event::PollStartEvent(ref e) = events[0] else {
            panic!("expected PollStartEvent, got {:?}", events[0]);
        };
        assert_eq!(e.timestamp_ns, 1_000_000);
        assert_eq!(e.worker_id, WorkerId(2));
        assert_eq!(e.local_queue, 5);
        assert_eq!(e.task_id, 100);
        assert_eq!(e.spawn_loc, "src/main.rs:42");

        // 2. PollEndEvent
        let Dial9Event::PollEndEvent(ref e) = events[1] else {
            panic!("expected PollEndEvent, got {:?}", events[1]);
        };
        assert_eq!(e.timestamp_ns, 1_000_500);
        assert_eq!(e.worker_id, WorkerId(2));

        // 3. WorkerParkEvent
        let Dial9Event::WorkerParkEvent(ref e) = events[2] else {
            panic!("expected WorkerParkEvent, got {:?}", events[2]);
        };
        assert_eq!(e.timestamp_ns, 2_000_000);
        assert_eq!(e.worker_id, WorkerId(1));
        assert_eq!(e.local_queue, 0);
        assert_eq!(e.cpu_time_ns, 500_000);
        assert_eq!(e.tid, 12345);

        // 4. WorkerUnparkEvent
        let Dial9Event::WorkerUnparkEvent(ref e) = events[3] else {
            panic!("expected WorkerUnparkEvent, got {:?}", events[3]);
        };
        assert_eq!(e.timestamp_ns, 3_000_000);
        assert_eq!(e.worker_id, WorkerId(1));
        assert_eq!(e.local_queue, 3);
        assert_eq!(e.cpu_time_ns, 600_000);
        assert_eq!(e.sched_wait_ns, 100_000);
        assert_eq!(e.tid, 12345);

        // 5. QueueSampleEvent
        let Dial9Event::QueueSampleEvent(ref e) = events[4] else {
            panic!("expected QueueSampleEvent, got {:?}", events[4]);
        };
        assert_eq!(e.timestamp_ns, 4_000_000);
        assert_eq!(e.global_queue, 7);

        // 6. TaskSpawnEvent
        let Dial9Event::TaskSpawnEvent(ref e) = events[5] else {
            panic!("expected TaskSpawnEvent, got {:?}", events[5]);
        };
        assert_eq!(e.timestamp_ns, 5_000_000);
        assert_eq!(e.task_id, 200);
        assert_eq!(e.spawn_loc, "src/lib.rs:10");
        assert!(e.instrumented);

        // 7. TaskTerminateEvent
        let Dial9Event::TaskTerminateEvent(ref e) = events[6] else {
            panic!("expected TaskTerminateEvent, got {:?}", events[6]);
        };
        assert_eq!(e.timestamp_ns, 6_000_000);
        assert_eq!(e.task_id, 200);

        // 8. CpuSampleEvent
        let Dial9Event::CpuSampleEvent(ref e) = events[7] else {
            panic!("expected CpuSampleEvent, got {:?}", events[7]);
        };
        assert_eq!(e.timestamp_ns, 7_000_000);
        assert_eq!(e.worker_id, WorkerId(0));
        assert_eq!(e.tid, 9999);
        assert_eq!(e.source, CpuSampleSource::CpuProfile);
        assert_eq!(e.thread_name.as_deref(), Some("tokio-runtime-worker"));
        assert_eq!(e.callchain, vec![0xdead_beef, 0xcafe_babe]);
        assert_eq!(e.cpu, Some(3));

        // 9. TaskDumpEvent
        let Dial9Event::TaskDumpEvent(ref e) = events[8] else {
            panic!("expected TaskDumpEvent, got {:?}", events[8]);
        };
        assert_eq!(e.timestamp_ns, 8_000_000);
        assert_eq!(e.task_id, 100);
        assert_eq!(e.callchain, vec![0x1111, 0x2222, 0x3333]);

        // 10. WakeEvent
        let Dial9Event::WakeEvent(ref e) = events[9] else {
            panic!("expected WakeEvent, got {:?}", events[9]);
        };
        assert_eq!(e.timestamp_ns, 9_000_000);
        assert_eq!(e.waker_task_id, 100);
        assert_eq!(e.woken_task_id, 200);
        assert_eq!(e.target_worker, 1);

        // 11. SegmentMetadataEvent
        let Dial9Event::SegmentMetadataEvent(ref e) = events[10] else {
            panic!("expected SegmentMetadataEvent, got {:?}", events[10]);
        };
        assert_eq!(e.timestamp_ns, 10_000_000);
        assert_eq!(e.entries.get("runtime").unwrap(), "main");
        assert_eq!(e.entries.get("version").unwrap(), "0.3.11");
        assert_eq!(e.entries.len(), 2);

        // 12. ClockSyncEvent
        let Dial9Event::ClockSyncEvent(ref e) = events[11] else {
            panic!("expected ClockSyncEvent, got {:?}", events[11]);
        };
        assert_eq!(e.timestamp_ns, 11_000_000);
        assert_eq!(e.realtime_ns, 1_700_000_000_000_000_000);

        // 13. AllocEvent
        let Dial9Event::AllocEvent(ref e) = events[12] else {
            panic!("expected AllocEvent, got {:?}", events[12]);
        };
        assert_eq!(e.timestamp_ns, 12_000_000);
        assert_eq!(e.tid, 5555);
        assert_eq!(e.size, 1024);
        assert_eq!(e.addr, 0x7fff_0000_1000);
        assert_eq!(e.callchain, vec![0xaaaa, 0xbbbb]);

        // 14. FreeEvent
        let Dial9Event::FreeEvent(ref e) = events[13] else {
            panic!("expected FreeEvent, got {:?}", events[13]);
        };
        assert_eq!(e.timestamp_ns, 13_000_000);
        assert_eq!(e.tid, 5555);
        assert_eq!(e.addr, 0x7fff_0000_1000);
        assert_eq!(e.size, 1024);
        assert_eq!(e.alloc_timestamp_ns, 12_000_000);

        // 15. ProcessResourceUsageEvent
        let Dial9Event::ProcessResourceUsageEvent(ref e) = events[14] else {
            panic!("expected ProcessResourceUsageEvent, got {:?}", events[14]);
        };
        assert_eq!(e.timestamp_ns, 14_000_000);
        assert_eq!(e.user_cpu_ns, 1_000_000);
        assert_eq!(e.system_cpu_ns, 2_000_000);
        assert_eq!(e.max_rss_bytes, 64 * 1024 * 1024);
        assert_eq!(e.minor_faults, 10);
        assert_eq!(e.major_faults, 1);
        assert_eq!(e.block_input_ops, 2);
        assert_eq!(e.block_output_ops, 3);
        assert_eq!(e.voluntary_context_switches, 4);
        assert_eq!(e.involuntary_context_switches, 5);

        // 16. TcpAcceptQueueEvent
        let Dial9Event::TcpAcceptQueueEvent(ref e) = events[15] else {
            panic!("expected TcpAcceptQueueEvent, got {:?}", events[15]);
        };
        assert_eq!(e.timestamp_ns, 15_000_000);
        assert_eq!(e.socket_cookie, 67890);
        assert_eq!(e.socket_inode, 12345);
        assert_eq!(e.ip_version, 4);
        assert_eq!(e.local_addr, "127.0.0.1");
        assert_eq!(e.local_port, 8080);
        assert_eq!(e.pending_connections, 3);
        assert_eq!(e.backlog_limit, 128);
    }

    #[test]
    fn unknown_event_deserializes_as_other() {
        use dial9_trace_format::TraceEvent;

        #[derive(TraceEvent)]
        struct MyCustomEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            value: u64,
        }

        let mut enc = Encoder::new();
        enc.write(&MyCustomEvent {
            timestamp_ns: 1_000,
            value: 42,
        })
        .unwrap();

        let bytes = enc.finish();
        let mut dec = Decoder::new(&bytes).expect("valid header");
        let mut events: Vec<Dial9Event> = Vec::new();
        dec.for_each_event(|raw| {
            events.push(raw.deserialize().expect("deserialize"));
        })
        .expect("decode");

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Dial9Event::Other));
    }

    #[test]
    fn optional_fields_decode_as_none() {
        let mut enc = Encoder::new();

        let callchain = enc.intern_stack_frames(&[0x1234]).unwrap();
        enc.write(&format::CpuSampleEvent {
            timestamp_ns: 1_000_000,
            worker_id: format::WorkerId::from(0u8),
            tid: 1111,
            source: crate::telemetry::events::CpuSampleSource::CpuProfile,
            thread_name: None,
            callchain,
            cpu: None,
        })
        .unwrap();

        let bytes = enc.finish();
        let mut dec = Decoder::new(&bytes).expect("valid header");
        let mut events: Vec<Dial9Event> = Vec::new();
        dec.for_each_event(|raw| {
            events.push(raw.deserialize().expect("deserialize"));
        })
        .expect("decode");

        assert_eq!(events.len(), 1);
        let Dial9Event::CpuSampleEvent(ref e) = events[0] else {
            panic!("expected CpuSampleEvent, got {:?}", events[0]);
        };
        assert_eq!(e.thread_name, None);
        assert_eq!(e.cpu, None);
    }
}

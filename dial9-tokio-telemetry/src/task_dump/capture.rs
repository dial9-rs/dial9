use crate::telemetry::format::{TaskDumpEvent, TaskSampleEvent};
use crate::telemetry::task_metadata::TaskId;
use crate::telemetry::{Encodable, ThreadLocalEncoder};
use dial9_core::handle::Dial9Handle;
use smallvec::SmallVec;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Initial heap reservation for the instruction-pointer buffer on first capture.
const FRAME_BUF_INITIAL_CAPACITY: usize = 256;

/// Metadata for one captured callchain stored in [`FrameBuf`].
struct ChainMeta {
    /// Index into `FrameBuf::ips` where this chain's frames start.
    ip_start: usize,
    /// Address of the root function (upper trim boundary). `None` means trim
    /// to the end of the buffer.
    root_addr: Option<*const core::ffi::c_void>,
    /// Address of the leaf function (lower trim boundary). `None` means no
    /// leaf boundary was available; the chain will be skipped at emit time.
    leaf_addr: Option<*const core::ffi::c_void>,
}

// SAFETY: raw pointers are only used for address comparison, never dereferenced
// across threads.
unsafe impl Send for ChainMeta {}

/// Reusable storage for one or more callchains captured during a single
/// `trace_with` sub-poll. Frames are appended flat to `ips`; each new chain's
/// metadata is pushed onto `chains`.
pub(super) struct FrameBuf {
    ips: Vec<u64>,
    chains: SmallVec<[ChainMeta; 4]>,
}

impl FrameBuf {
    pub(super) fn new() -> Self {
        Self {
            ips: Vec::new(),
            chains: SmallVec::new(),
        }
    }

    pub(super) fn clear(&mut self) {
        self.ips.clear();
        self.chains.clear();
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.ips.capacity()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.ips.is_empty() && self.chains.is_empty()
    }

    pub(super) fn has_data(&self) -> bool {
        !self.chains.is_empty()
    }

    /// Emit one `TaskDumpEvent` per recorded callchain, then clear.
    /// Trimming via `_Unwind_FindEnclosingFunction` happens here (emit path)
    /// rather than during capture, keeping the hot path lock-free.
    pub(super) fn emit(&mut self, handle: &Dial9Handle, task_id: TaskId, capture_ts: u64) {
        self.emit_with(|chain| {
            handle.record_event_with(|| TaskDumpData {
                timestamp_ns: capture_ts,
                task_id,
                callchain: chain,
            });
        });
    }

    pub(super) fn emit_sample(
        &mut self,
        handle: &Dial9Handle,
        task_id: TaskId,
        capture_ts: u64,
        inclusion_probability: f64,
    ) {
        self.emit_with(|chain| {
            handle.record_event_with(|| TaskSampleData {
                timestamp_ns: capture_ts,
                task_id,
                callchain: chain,
                inclusion_probability,
            });
        });
    }

    fn emit_with(&mut self, mut record: impl FnMut(&[u64])) {
        for (i, meta) in self.chains.iter().enumerate() {
            let ip_end = self
                .chains
                .get(i + 1)
                .map(|next| next.ip_start)
                .unwrap_or(self.ips.len());
            let raw = &self.ips[meta.ip_start..ip_end];
            let chain = match meta.leaf_addr {
                Some(leaf) => crate::unwind::trim_frames(raw, meta.root_addr, leaf),
                None => &[],
            };
            if !chain.is_empty() {
                record(chain);
            }
        }
        self.clear();
    }

    /// Capture backtraces at yield points by re-polling `inner` under the
    /// real waker inside `trace_with`, returning that re-poll's result.
    ///
    /// The re-poll can complete `inner`; that `Ready` is returned so the caller
    /// can adopt it.
    pub(super) fn capture<F: Future>(
        &mut self,
        inner: Pin<&mut F>,
        cx: &mut Context<'_>,
    ) -> Poll<F::Output> {
        if self.ips.capacity() == 0 {
            self.ips.reserve(FRAME_BUF_INITIAL_CAPACITY);
        }
        self.clear();

        let ips = &mut self.ips;
        let chains = &mut self.chains;

        // `trace_with`'s outer closure is `FnOnce`; `Option::take` moves the
        // pinned reference in without requiring a `Copy` bound or unsafe.
        let mut result = Poll::Pending;
        tokio::runtime::dump::trace_with(
            || {
                result = inner.poll(cx);
            },
            |meta| {
                let ip_start = ips.len();
                // Hot path: collect raw IPs only — no _Unwind_FindEnclosingFunction,
                // no dl_iterate_phdr, no global locks. Trimming to root/leaf
                // boundaries happens later in emit().
                crate::unwind::collect_frames_raw(ips);
                // Stash the root/leaf addresses so we can trim at emit time.
                chains.push(ChainMeta {
                    ip_start,
                    root_addr: meta.root_addr,
                    leaf_addr: Some(meta.trace_leaf_addr),
                });
            },
        );
        result
    }
}

/// Borrowed-callchain view of a task-dump event that implements [`Encodable`]
/// by interning its ips into the batch's stack pool.
pub(crate) struct TaskDumpData<'a> {
    pub(crate) timestamp_ns: u64,
    pub(crate) task_id: TaskId,
    pub(crate) callchain: &'a [u64],
}

impl Encodable for TaskDumpData<'_> {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let interned_callchain = enc.intern_stack_frames(self.callchain);
        enc.encode(&TaskDumpEvent {
            timestamp_ns: self.timestamp_ns,
            task_id: self.task_id,
            callchain: interned_callchain,
        });
    }
}

/// Borrowed-callchain view of a task-sample event that implements [`Encodable`]
/// by interning its ips into the batch's stack pool.
pub(crate) struct TaskSampleData<'a> {
    pub(crate) timestamp_ns: u64,
    pub(crate) task_id: TaskId,
    pub(crate) callchain: &'a [u64],
    pub(crate) inclusion_probability: f64,
}

impl Encodable for TaskSampleData<'_> {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let interned_callchain = enc.intern_stack_frames(self.callchain);
        enc.encode(&TaskSampleEvent {
            timestamp_ns: self.timestamp_ns,
            task_id: self.task_id,
            callchain: interned_callchain,
            inclusion_probability: self.inclusion_probability,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::TaskDumpData;
    use crate::telemetry::analysis_events::Dial9Event;
    use crate::telemetry::encoder::encode_single;
    use crate::telemetry::format::decode_events;
    use crate::telemetry::task_metadata::TaskId;

    #[test]
    fn task_dump_event_round_trips() {
        let dump = TaskDumpData {
            timestamp_ns: 42_000,
            task_id: TaskId::from_u32(17),
            callchain: &[0x1111_2222, 0x3333_4444, 0x5555_6666],
        };
        let encoded = encode_single(&dump);
        let events = decode_events(&encoded).expect("decode");
        assert_eq!(events.len(), 1);
        let Dial9Event::TaskDumpEvent(ref e) = events[0] else {
            panic!("expected TaskDumpEvent, got {:?}", events[0]);
        };
        assert_eq!(e.timestamp_ns, 42_000);
        assert_eq!(e.task_id, 17);
        assert_eq!(e.callchain, vec![0x1111_2222, 0x3333_4444, 0x5555_6666]);
    }
}

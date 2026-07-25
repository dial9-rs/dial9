//! Caller-thread context capture for metrique entries.

use metrique::CloseValue;
use metrique::unit_of_work::metrics;

use crate::telemetry::{TaskId, clock_monotonic_ns, current_worker_id};

use super::Context;

/// Field type that reads the monotonic clock when its `CloseValue` runs
/// (at `append_on_drop` / close time), so the event carries an end
/// timestamp without any user bookkeeping.
#[derive(Debug)]
pub(crate) struct MonotonicAtClose;

impl CloseValue for MonotonicAtClose {
    type Closed = u64;

    fn close(self) -> u64 {
        clock_monotonic_ns()
    }
}

// Subfield structs are closed by reference by the `#[metrics]` macro.
impl CloseValue for &MonotonicAtClose {
    type Closed = u64;

    fn close(self) -> u64 {
        clock_monotonic_ns()
    }
}

/// Metrique subfield that captures dial9 runtime context for an entry.
/// Flatten one into any entry that should land in the dial9 trace (see the
/// [module docs](super) for the full example).
///
/// [`capture`](Dial9Context::capture) reads the caller thread's tokio worker
/// id and task id (when on a dial9-traced runtime) plus the monotonic clock;
/// the end timestamp is captured automatically when the entry closes. The
/// sink routes these fields into the trace event header so the viewer can
/// place the event on the timeline and correlate it with other events on
/// the same worker and task.
///
/// The fields also flow to the other formats in the pipeline (EMF/JSON) as
/// ordinary fields named `worker_id`, `task_id`, `monotonic_ns_start`, and
/// `monotonic_ns_end` (restyled by a parent `rename_all`); use
/// `#[metrics(flatten, prefix = "...")]` at the flatten site if those names
/// collide with your own.
///
/// This type is neither `Default` nor `Clone`: a defaulted context would
/// silently look like "valid context captured at monotonic time 0".
/// Construct it with [`capture`](Dial9Context::capture) at the field
/// initializer site.
#[metrics(subfield)]
#[derive(Debug)]
pub struct Dial9Context {
    /// Global dial9 worker id of the capturing thread
    /// (`WorkerId::UNKNOWN` when off-runtime).
    #[metrics(flags(Context))]
    worker_id: u64,
    /// Tokio task id of the capturing task, absent when not inside a task.
    #[metrics(flags(Context))]
    task_id: Option<u64>,
    /// Monotonic nanoseconds at capture (request start).
    #[metrics(flags(Context))]
    monotonic_ns_start: u64,
    /// Monotonic nanoseconds at entry close (request end).
    #[metrics(flags(Context))]
    monotonic_ns_end: MonotonicAtClose,
}

impl Dial9Context {
    /// Capture the calling thread's runtime context. Infallible.
    ///
    /// Always records a monotonic start timestamp. Worker and task ids are
    /// captured when the calling thread is owned by a dial9-traced tokio
    /// runtime; otherwise the worker id is `WorkerId::UNKNOWN` and the task
    /// id is absent.
    pub fn capture() -> Self {
        Self {
            worker_id: current_worker_id().as_u64(),
            task_id: tokio::task::try_id().map(|id| TaskId::from(id).to_u64()),
            monotonic_ns_start: clock_monotonic_ns(),
            monotonic_ns_end: MonotonicAtClose,
        }
    }
}

/// Base names of [`Dial9Context`]'s fields, used by the sink to assign
/// context roles when walking descriptors. Kept next to the struct so the
/// two cannot drift independently.
///
/// A parent entry's `rename_all` style restyles flattened child names
/// (e.g. `worker_id` → `WorkerId`), so these are canonical forms compared
/// via [`canonicalize`]: lowercase with separators removed.
pub(crate) mod field_names {
    pub(crate) const WORKER_ID: &str = "workerid";
    pub(crate) const TASK_ID: &str = "taskid";
    pub(crate) const MONOTONIC_NS_START: &str = "monotonicnsstart";
    pub(crate) const MONOTONIC_NS_END: &str = "monotonicnsend";

    /// Canonicalize a possibly-restyled field name: strip `_`/`-` and
    /// lowercase, so `worker_id`, `WorkerId`, `workerId`, and `worker-id`
    /// all compare equal.
    pub(crate) fn canonicalize(name: &str) -> String {
        name.chars()
            .filter(|c| *c != '_' && *c != '-')
            .map(|c| c.to_ascii_lowercase())
            .collect()
    }
}

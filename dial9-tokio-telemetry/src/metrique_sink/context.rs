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
/// ordinary fields. Their names are literal and `dial9.`-prefixed
/// (`dial9.worker_id`, `dial9.task_id`, `dial9.monotonic_ns_start`,
/// `dial9.monotonic_ns_end`), which keeps them out of the way of your own
/// field names and makes them easy to filter out of a format that does not
/// want them.
//
// Deliberately neither `Default` nor `Clone`: a defaulted context would
// silently look like "valid context captured at monotonic time 0".
#[metrics(subfield)]
#[derive(Debug)]
pub struct Dial9Context {
    /// Global dial9 worker id of the capturing thread
    /// (`WorkerId::UNKNOWN` when off-runtime).
    #[metrics(name = "dial9.worker_id", flags(Context))]
    worker_id: u64,
    /// Tokio task id of the capturing task, absent when not inside a task.
    #[metrics(name = "dial9.task_id", flags(Context))]
    task_id: Option<u64>,
    /// Monotonic nanoseconds at capture (request start).
    #[metrics(name = "dial9.monotonic_ns_start", flags(Context))]
    monotonic_ns_start: u64,
    /// Monotonic nanoseconds at entry close (request end).
    #[metrics(name = "dial9.monotonic_ns_end", flags(Context))]
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

/// Names of [`Dial9Context`]'s fields, used by the sink to assign context
/// roles when walking descriptors. Kept next to the struct so the two cannot
/// drift independently.
///
/// These are literal `#[metrics(name = ...)]` names, so unlike ordinary
/// fields they are identical under every [`NameStyle`](metrique::NameStyle):
/// a parent's `rename_all` cannot restyle them, and matching is an exact
/// comparison against the descriptor's base name. A `prefix` at the flatten
/// site prepends to the emitted name but leaves the base name alone, so
/// routing survives that too.
pub(crate) mod field_names {
    pub(crate) const WORKER_ID: &str = "dial9.worker_id";
    pub(crate) const TASK_ID: &str = "dial9.task_id";
    pub(crate) const MONOTONIC_NS_START: &str = "dial9.monotonic_ns_start";
    pub(crate) const MONOTONIC_NS_END: &str = "dial9.monotonic_ns_end";
}

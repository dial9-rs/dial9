//! Caller-thread context capture for metrique entries.

use dial9_core::clock::clock_monotonic_ns;
use dial9_core::thread::cached_tid;
use metrique::CloseValue;
use metrique::unit_of_work::metrics;

use super::Context;

/// Field type that reads the monotonic clock when its `CloseValue` runs
/// (at `append_on_drop` / close time), so the event carries an end
/// timestamp without reading the clock later on the metrics queue.
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
/// Include one in any entry that should land in the dial9 trace (see the
/// [module docs](crate) for the full example).
///
/// [`capture`](Dial9Context::capture) and [`default`](Default::default) read
/// the calling thread's OS thread id and the monotonic clock; with the
/// `tokio` feature they also read the tokio task id when called inside a task.
/// The end timestamp is captured automatically when the entry closes. The
/// sink routes these fields into the trace event header so the viewer can
/// place the event on the timeline and correlate it with other events on the
/// same thread and task.
///
/// The thread id is a capture-time snapshot: a task may migrate to another
/// worker thread before the entry closes.
///
/// The fields also flow to the other formats in the pipeline (EMF/JSON) as
/// ordinary fields. Their names are literal and `dial9.`-prefixed
/// (`dial9.thread_id`, `dial9.tokio.task_id`, `dial9.monotonic_ns_start`,
/// `dial9.monotonic_ns_end`), which keeps them out of the way of your own
/// field names and makes them easy to filter out of a format that does not
/// want them (see [`WithoutDial9Fields`](crate::WithoutDial9Fields)).
//
// Deliberately not `Clone`: each entry should capture its own start context.
#[metrics(subfield)]
#[derive(Debug)]
pub struct Dial9Context {
    /// OS thread id of the capturing thread (`gettid` on Linux), the same
    /// id space dial9's worker and CPU-sample events record.
    #[metrics(name = "dial9.thread_id", flags(Context))]
    thread_id: u64,
    /// Tokio task id of the capturing task. Absent when not inside a task
    /// or when the `tokio` feature is disabled.
    #[metrics(name = "dial9.tokio.task_id", flags(Context))]
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
    /// Always records the OS thread id and a monotonic start timestamp. The
    /// task id is captured when the `tokio` feature is enabled and the
    /// calling thread is inside a tokio task; otherwise it is absent.
    pub fn capture() -> Self {
        Self {
            thread_id: u64::from(cached_tid()),
            task_id: current_task_id(),
            monotonic_ns_start: clock_monotonic_ns(),
            monotonic_ns_end: MonotonicAtClose,
        }
    }
}

impl Default for Dial9Context {
    fn default() -> Self {
        Self::capture()
    }
}

/// The current tokio task id, converted to the same `u64` dial9's runtime
/// hooks record for spawn and poll events.
#[cfg(feature = "tokio")]
fn current_task_id() -> Option<u64> {
    use std::hash::{Hash, Hasher};

    /// Extracts the raw `u64` from tokio's opaque `task::Id` by capturing
    /// the value it feeds its hasher. Mirrors `TaskId::from<tokio::task::Id>`
    /// in dial9-tokio-telemetry so the ids agree across event sources.
    struct U64Extractor(u64);

    impl Hasher for U64Extractor {
        fn finish(&self) -> u64 {
            self.0
        }

        fn write(&mut self, _bytes: &[u8]) {}

        fn write_u64(&mut self, i: u64) {
            self.0 = i;
        }
    }

    tokio::task::try_id().map(|id| {
        let mut extractor = U64Extractor(0);
        id.hash(&mut extractor);
        extractor.finish()
    })
}

#[cfg(not(feature = "tokio"))]
fn current_task_id() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_captures_current_context() {
        let before = clock_monotonic_ns();
        let context = Dial9Context::default();
        let after = clock_monotonic_ns();

        assert_eq!(context.thread_id, u64::from(cached_tid()));
        assert!(context.monotonic_ns_start >= before);
        assert!(context.monotonic_ns_start <= after);
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
pub(crate) mod context_field_names {
    pub(crate) const THREAD_ID: &str = crate::field_names::THREAD_ID;
    pub(crate) const TASK_ID: &str = crate::field_names::TOKIO_TASK_ID;
    pub(crate) const MONOTONIC_NS_START: &str = "dial9.monotonic_ns_start";
    pub(crate) const MONOTONIC_NS_END: &str = "dial9.monotonic_ns_end";
}

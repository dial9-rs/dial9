//! [`Dial9Context`]: the metrique field flattened into a user's entry to
//! carry dial9 correlation data (worker id, task id, monotonic span).

use super::flags::Context;
use crate::telemetry::{TaskId, WorkerId, clock_monotonic_ns, current_worker_id};
use metrique::CloseValue;
use metrique::unit_of_work::metrics;
use metrique::writer::core::{FieldShape, KnownShape};
use metrique::writer::value::{Observation, Value, ValueWriter};
use metrique::writer::{MetricFlags, Unit};

/// Dial9 correlation context, flattened into a metrique entry.
///
/// Flatten this into any `#[metrics]` struct with `#[metrics(flatten)]` and
/// construct it via [`Dial9Context::capture`] at the field initializer site.
/// Its four fields carry the internal, `#[doc(hidden)]`
/// [`Context`](super::flags::Context) flag so [`Dial9Stream`](super::Dial9Stream)
/// can find them by walking the entry's descriptor; they are never written
/// to the dial9 payload directly, only routed into the trace event's
/// worker/task/timing header.
///
/// Deliberately no `Default`/`Clone` impl: a zero-valued `Default` would
/// produce fields that look like validly-captured context (the `Context`
/// flag is still present) without ever tripping the "no context captured"
/// diagnostic. Construct with [`capture`](Self::capture) at each use site;
/// forgetting the field produces a compile error.
#[metrics(rename_all = "PascalCase")]
pub struct Dial9Context {
    #[metrics(flags(Context), no_close)]
    worker_id: WorkerId,
    #[metrics(flags(Context), no_close)]
    task_id: Option<TaskId>,
    #[metrics(flags(Context))]
    monotonic_ns_start: u64,
    #[metrics(flags(Context))]
    monotonic_ns_end: MonotonicAtClose,
}

/// Field type that reads the monotonic clock when its `CloseValue` runs
/// (at entry close, not at [`Dial9Context::capture`] time). Its closed form
/// is `u64` (monotonic nanoseconds at close).
#[derive(Debug)]
pub struct MonotonicAtClose;

impl CloseValue for MonotonicAtClose {
    type Closed = u64;
    fn close(self) -> u64 {
        clock_monotonic_ns()
    }
}

impl Dial9Context {
    /// Capture caller-thread dial9 context: current tokio worker id (if
    /// any), current task id (if any), and the monotonic clock. Infallible —
    /// off-runtime threads get [`WorkerId::UNKNOWN`] and `task_id: None`.
    pub fn capture() -> Self {
        Self {
            worker_id: current_worker_id(),
            task_id: tokio::task::try_id().map(TaskId::from),
            monotonic_ns_start: clock_monotonic_ns(),
            monotonic_ns_end: MonotonicAtClose,
        }
    }
}

// `Value` impls for metrique interop only — kept here (behind the
// `metrique-integration` feature) rather than on `WorkerId`/`TaskId`
// themselves, so the metrique dependency surface stays scoped to this
// module.

impl Value for WorkerId {
    const SHAPE: FieldShape<'static> = FieldShape::Known(KnownShape::U64);

    fn write(&self, writer: impl ValueWriter) {
        writer.metric(
            [Observation::Unsigned(self.as_u64())],
            Unit::None,
            [],
            MetricFlags::empty(),
        );
    }
}

impl Value for TaskId {
    const SHAPE: FieldShape<'static> = FieldShape::Known(KnownShape::U64);

    fn write(&self, writer: impl ValueWriter) {
        writer.metric(
            [Observation::Unsigned(self.to_u64())],
            Unit::None,
            [],
            MetricFlags::empty(),
        );
    }
}

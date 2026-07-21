//! [`Dial9Context`]: the metrique field flattened into a user's entry to
//! carry dial9 correlation data (worker id, monotonic span).

use crate::telemetry::{WorkerId, clock_monotonic_ns, current_worker_id};
use dial9_utils::metrique_integration::Context;
use metrique::CloseValue;
use metrique::unit_of_work::metrics;
use metrique::writer::core::{FieldShape, KnownShape};
use metrique::writer::value::{Observation, Value, ValueWriter};
use metrique::writer::{MetricFlags, Unit};

/// Dial9 correlation context, flattened into a metrique entry.
///
/// Flatten this into any `#[metrics]` struct with `#[metrics(flatten)]` and
/// construct it via [`Dial9Context::capture`] at the field initializer site.
/// Its three fields carry the internal, `#[doc(hidden)]`
/// [`Context`](dial9_utils::metrique_integration::Context) flag so
/// [`Dial9Stream`](super::Dial9Stream) can find them by walking the entry's
/// descriptor; they are never written to the dial9 payload directly, only
/// routed into the trace event's worker/timing header.
///
/// # `worker_id` is a snapshot, not a guarantee
///
/// `worker_id` is read once, at [`capture`](Self::capture) time — i.e. it
/// names the tokio worker that was running when the entry was constructed,
/// not necessarily the worker that ran for the entry's whole span. Tokio's
/// work-stealing scheduler can resume a task on a different worker after
/// any await point, so a long-running entry may finish having spent most of
/// its time on a worker other than the one recorded here. Treat `worker_id`
/// as "where this span started," not "where this span ran." For a complete
/// picture of which worker(s) were active for the full
/// `monotonic_ns_start..monotonic_ns_end` span, correlate against the
/// trace's own poll timeline (`PollStartEvent`/`PollEndEvent`) the same way
/// the dial9 tracing-span integration already does — it likewise only
/// records `worker_id` at each instantaneous enter/exit rather than
/// duplicating a `task_id` that the poll timeline can already reconstruct.
///
/// A `task_id` field was deliberately not included here for the same
/// reason: it would be redundant with (and, since it's captured only once
/// at `capture()` time, potentially staler than) what the poll timeline
/// already lets a viewer reconstruct by joining `worker_id` and a
/// timestamp.
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
    /// any) and the monotonic clock. Infallible — off-runtime threads get
    /// [`WorkerId::UNKNOWN`].
    pub fn capture() -> Self {
        Self {
            worker_id: current_worker_id(),
            monotonic_ns_start: clock_monotonic_ns(),
            monotonic_ns_end: MonotonicAtClose,
        }
    }
}

// `Value` impl for metrique interop only — kept here (behind the
// `metrique-integration` feature) rather than on `WorkerId` itself, so the
// metrique dependency surface stays scoped to this module.

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

//! Future combinator for attaching a [`Span`] to a future.

use super::Span;
use dial9_core::clock::clock_monotonic_ns;
use dial9_core::handle::Dial9Handle;
use pin_project_lite::pin_project;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pin_project! {
    /// A future that records span timing around the inner future, produced by
    /// [`Instrument::instrument`].
    ///
    /// Unlike a per-poll enter/exit, this emits a single `SpanEnter` on the
    /// first poll and a single completion `SpanExit` when the future resolves
    /// (or is dropped/cancelled). The exit carries the aggregate the span
    /// observed: `active_ns` (summed poll durations), `idle_ns` (time suspended
    /// between polls), `poll_count`, and whether it ran to completion. Per-poll
    /// detail is recoverable by correlating the span's window with the task's
    /// poll events, so the span itself stays cheap.
    ///
    /// When the future (and thus the span) is dropped, the span is closed.
    pub struct Instrumented<F, S: Span> {
        #[pin]
        inner: F,
        // Not pinned: dropped in place when `Instrumented` drops, which is what
        // emits the span's close event (the span type's `Drop`).
        span: S,
        entered: bool,
        exited: bool,
        first_enter_ns: u64,
        active_ns: u64,
        poll_count: u64,
    }

    impl<F, S: Span> PinnedDrop for Instrumented<F, S> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            // Cancelled (dropped before the inner future resolved): still emit
            // the completion exit with what we observed, marked not-completed.
            // `span`'s own `Drop` (a field, dropped after this) emits close.
            if *this.entered && !*this.exited {
                let handle = Dial9Handle::current();
                let idle_ns = clock_monotonic_ns()
                    .saturating_sub(*this.first_enter_ns)
                    .saturating_sub(*this.active_ns);
                this.span
                    .emit_exit(&handle, *this.active_ns, idle_ns, *this.poll_count, false);
            }
        }
    }
}

impl<F, S: Span + fmt::Debug> fmt::Debug for Instrumented<F, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Instrumented")
            .field("span", &self.span)
            .finish_non_exhaustive()
    }
}

impl<F: Future, S: Span> Future for Instrumented<F, S> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let handle = Dial9Handle::current();

        let t0 = clock_monotonic_ns();
        if !*this.entered {
            this.span.emit_enter(&handle);
            *this.first_enter_ns = t0;
            *this.entered = true;
        }

        let result = this.inner.poll(cx);

        let t1 = clock_monotonic_ns();
        *this.active_ns = this.active_ns.saturating_add(t1.saturating_sub(t0));
        *this.poll_count += 1;

        if result.is_ready() {
            let idle_ns = t1
                .saturating_sub(*this.first_enter_ns)
                .saturating_sub(*this.active_ns);
            this.span
                .emit_exit(&handle, *this.active_ns, idle_ns, *this.poll_count, true);
            *this.exited = true;
        }

        result
    }
}

/// Extension trait that attaches a [`Span`] to a future.
///
/// ```no_run
/// use dial9_utils::dial9_span;
/// use dial9_utils::span::Instrument as _;
///
/// # async fn demo() {
/// async {
///     // ... work ...
/// }
/// .instrument(dial9_span!("background_job"))
/// .await;
/// # }
/// ```
pub trait Instrument: Future + Sized {
    /// Attach `span` to this future. The span is entered on the first poll and
    /// its completion event (with aggregate timing) is emitted when the future
    /// resolves or is dropped; the span is closed on drop.
    fn instrument<S: Span>(self, span: S) -> Instrumented<Self, S> {
        Instrumented {
            inner: self,
            span,
            entered: false,
            exited: false,
            first_enter_ns: 0,
            active_ns: 0,
            poll_count: 0,
        }
    }
}

impl<F: Future> Instrument for F {}

//! Future combinator for attaching a [`Span`] to a future.

use super::Span;
use pin_project_lite::pin_project;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pin_project! {
    /// A future that records span enter/exit timing around each poll of the
    /// inner future, produced by [`Instrument::instrument`].
    ///
    /// Each poll records one span segment, so a future that is polled, suspends
    /// at an `.await`, and is later polled again shows up as multiple segments
    /// with a gap between them — matching how the `tracing` layer records
    /// instrumented futures. When the future (and thus the span) is dropped, the
    /// span is closed.
    pub struct Instrumented<F, S: Span> {
        #[pin]
        inner: F,
        // Not pinned: dropped in place when `Instrumented` drops, which is what
        // emits the span's close event (the span type's `Drop`).
        span: S,
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
        // Enter now; the guard emits the exit when it drops at the end of this
        // poll, capturing exactly the time spent in `inner.poll`.
        let _entered = this.span.enter();
        this.inner.poll(cx)
    }
}

/// Extension trait that attaches a [`Span`] to a future.
///
/// ```no_run
/// use dial9_tokio_telemetry::dial9_span;
/// use dial9_tokio_telemetry::span::Instrument as _;
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
    /// Attach `span` to this future. The span records timing around every poll
    /// and is closed when the future is dropped.
    fn instrument<S: Span>(self, span: S) -> Instrumented<Self, S> {
        Instrumented { inner: self, span }
    }
}

impl<F: Future> Instrument for F {}

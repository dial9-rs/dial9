//! Future combinator for attaching a [`Dial9Span`] to a future.

use super::Dial9Span;
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
    pub struct Instrumented<F> {
        #[pin]
        inner: F,
        span: Dial9Span,
    }
}

impl<F> fmt::Debug for Instrumented<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Instrumented")
            .field("span", &self.span)
            .finish_non_exhaustive()
    }
}

impl<F: Future> Future for Instrumented<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        // Records an enter now and an exit when `_entered` drops at the end of
        // this poll, capturing exactly the span of time spent in `inner.poll`.
        let _entered = this.span.enter();
        this.inner.poll(cx)
    }
}

/// Extension trait that attaches a [`Dial9Span`] to a future.
///
/// ```no_run
/// use dial9_tokio_telemetry::dial9_span;
/// use dial9_tokio_telemetry::span::Instrument;
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
    fn instrument(self, span: Dial9Span) -> Instrumented<Self> {
        Instrumented { inner: self, span }
    }
}

impl<F: Future> Instrument for F {}

//! Tower middleware that wraps one ad-hoc span per request.
//!
//! Behind the `tower` feature. See the module docs on
//! [`span`](crate::span) for the wire format and lifecycle; this layer is
//! generic over the request and response types and depends on nothing from
//! `http`.

use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;

use super::{Dial9Handle, Span, SpanHandle, SpanInner, emit_close};

type MakeSpan<Req> = Arc<dyn Fn(&Req) -> Span + Send + Sync>;
type OnResponse<Res> = Arc<dyn Fn(&SpanHandle, &Res) + Send + Sync>;

/// A [`tower::Layer`](tower_layer::Layer) that emits one span per request.
///
/// Build it with [`Dial9SpanLayer::builder`]. `make_span` (required) turns the
/// request into a [`Span`]; `on_response` (optional) records late fields once
/// the response is available. The response future is entered/exited per poll
/// and closed when it resolves or is dropped.
///
/// Streaming response bodies are not covered by the span in v1: the span
/// closes when the response future resolves (the response head for HTTP).
///
/// ```ignore
/// let layer = Dial9SpanLayer::builder()
///     .make_span(|req: &Request| span("http_request").field("path", req.uri().path()))
///     .on_response(|span, res: &Response| span.record("status", res.status().as_u16()))
///     .build();
/// ```
pub struct Dial9SpanLayer<Req, Res> {
    make_span: MakeSpan<Req>,
    on_response: Option<OnResponse<Res>>,
}

#[bon::bon]
impl<Req, Res> Dial9SpanLayer<Req, Res> {
    /// Start building a layer. `make_span` is required; `on_response` is
    /// optional.
    #[builder]
    pub fn new(
        #[builder(with = |f: impl Fn(&Req) -> Span + Send + Sync + 'static| {
            let boxed: MakeSpan<Req> = Arc::new(f);
            boxed
        })]
        make_span: MakeSpan<Req>,
        #[builder(with = |f: impl Fn(&SpanHandle, &Res) + Send + Sync + 'static| {
            let boxed: OnResponse<Res> = Arc::new(f);
            boxed
        })]
        on_response: Option<OnResponse<Res>>,
    ) -> Self {
        Self {
            make_span,
            on_response,
        }
    }
}

impl<Req, Res> Clone for Dial9SpanLayer<Req, Res> {
    fn clone(&self) -> Self {
        Self {
            make_span: Arc::clone(&self.make_span),
            on_response: self.on_response.clone(),
        }
    }
}

impl<Req, Res> fmt::Debug for Dial9SpanLayer<Req, Res> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dial9SpanLayer")
            .field("on_response", &self.on_response.is_some())
            .finish_non_exhaustive()
    }
}

impl<S, Req, Res> tower_layer::Layer<S> for Dial9SpanLayer<Req, Res> {
    type Service = Dial9SpanService<S, Req, Res>;

    fn layer(&self, inner: S) -> Self::Service {
        Dial9SpanService {
            inner,
            make_span: Arc::clone(&self.make_span),
            on_response: self.on_response.clone(),
            _pd: PhantomData,
        }
    }
}

/// The [`Service`](tower_service::Service) produced by [`Dial9SpanLayer`].
pub struct Dial9SpanService<S, Req, Res> {
    inner: S,
    make_span: MakeSpan<Req>,
    on_response: Option<OnResponse<Res>>,
    _pd: PhantomData<fn(Req) -> Res>,
}

impl<S: Clone, Req, Res> Clone for Dial9SpanService<S, Req, Res> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            make_span: Arc::clone(&self.make_span),
            on_response: self.on_response.clone(),
            _pd: PhantomData,
        }
    }
}

impl<S: fmt::Debug, Req, Res> fmt::Debug for Dial9SpanService<S, Req, Res> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dial9SpanService")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<S, Req, Res> tower_service::Service<Req> for Dial9SpanService<S, Req, Res>
where
    S: tower_service::Service<Req, Response = Res>,
{
    type Response = Res;
    type Error = S::Error;
    type Future = Dial9SpanFuture<S::Future, Res>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let shared = SpanInner::from_span((self.make_span)(&req));
        let handle = SpanHandle {
            inner: Arc::clone(&shared),
        };
        Dial9SpanFuture {
            inner: self.inner.call(req),
            shared,
            handle,
            on_response: self.on_response.clone(),
            closed: false,
        }
    }
}

pin_project! {
    /// The response future produced by [`Dial9SpanService`].
    ///
    /// Enters/exits the span per poll and, once the inner future yields
    /// `Ok(response)`, invokes `on_response` *before* that poll's exit so the
    /// recorded fields ride the exit event. Closes when the future resolves or
    /// is dropped (e.g. on cancellation).
    pub struct Dial9SpanFuture<F, Res> {
        #[pin]
        inner: F,
        shared: Arc<SpanInner>,
        handle: SpanHandle,
        on_response: Option<OnResponse<Res>>,
        closed: bool,
    }

    impl<F, Res> PinnedDrop for Dial9SpanFuture<F, Res> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if !*this.closed {
                *this.closed = true;
                emit_close(&Dial9Handle::current(), this.shared.id);
            }
        }
    }
}

impl<F, Res> fmt::Debug for Dial9SpanFuture<F, Res> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dial9SpanFuture").finish_non_exhaustive()
    }
}

impl<F, Res, E> Future for Dial9SpanFuture<F, Res>
where
    F: Future<Output = Result<Res, E>>,
{
    type Output = Result<Res, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let handle = Dial9Handle::current();

        this.shared.emit_enter(&handle);
        let result = this.inner.poll(cx);

        // Record response fields before the exit so they ride this poll's exit
        // event (the exit fires below, then close — there is no later exit).
        if let Poll::Ready(Ok(res)) = &result
            && let Some(cb) = this.on_response.as_ref()
        {
            cb(this.handle, res);
        }

        this.shared.emit_exit(&handle);

        if result.is_ready() && !*this.closed {
            *this.closed = true;
            emit_close(&handle, this.shared.id);
        }

        result
    }
}

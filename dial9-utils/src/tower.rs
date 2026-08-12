//! [`tower`](https://docs.rs/tower) middleware that instruments each request
//! future with a span.
//!
//! Requires the `tower` feature.

use crate::span::{Instrument as _, Instrumented, Span};
use pin_project_lite::pin_project;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use tower_layer::Layer;
use tower_service::Service;

/// A [`tower::Layer`](tower_layer::Layer) that wraps a service so each request's
/// response future is recorded as a span.
///
/// The span for each request is produced by the `make_span` closure passed to
/// [`new`](Dial9SpanLayer::new). It receives the request, so it can attach
/// request-derived fields — or ignore it for a fixed name.
///
/// ```no_run
/// # use dial9_utils::tower::Dial9SpanLayer;
/// # use dial9_utils::dial9_span;
/// // Per-request fields:
/// let layer = Dial9SpanLayer::new(|req: &u64| dial9_span!("rpc", id: u64 = *req));
/// // Or a fixed name, ignoring the request:
/// let fixed = Dial9SpanLayer::new(|_req: &u64| dial9_span!("http_request"));
/// ```
#[derive(Clone)]
pub struct Dial9SpanLayer<F> {
    make_span: F,
}

impl<F> fmt::Debug for Dial9SpanLayer<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dial9SpanLayer").finish_non_exhaustive()
    }
}

impl<F> Dial9SpanLayer<F> {
    /// Build a layer that produces a fresh span per request via `make_span`,
    /// which receives the request so it can attach request-derived fields:
    ///
    /// ```no_run
    /// # use dial9_utils::tower::Dial9SpanLayer;
    /// # use dial9_utils::dial9_span;
    /// # struct Request; impl Request { fn path(&self) -> &str { "/" } }
    /// let layer = Dial9SpanLayer::new(|req: &Request| {
    ///     dial9_span!("http_request", route = %req.path())
    /// });
    /// ```
    pub fn new(make_span: F) -> Self {
        Self { make_span }
    }
}

impl<Svc, F> Layer<Svc> for Dial9SpanLayer<F>
where
    F: Clone,
{
    type Service = Dial9SpanService<Svc, F>;

    fn layer(&self, inner: Svc) -> Self::Service {
        Dial9SpanService {
            inner,
            make_span: self.make_span.clone(),
        }
    }
}

/// The [`Service`] produced by [`Dial9SpanLayer`]. Instruments each
/// [`call`](Service::call)'s future with a freshly-created span.
#[derive(Clone)]
pub struct Dial9SpanService<Svc, F> {
    inner: Svc,
    make_span: F,
}

impl<Svc, F> fmt::Debug for Dial9SpanService<Svc, F>
where
    Svc: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dial9SpanService")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<Svc, F, S, Req> Service<Req> for Dial9SpanService<Svc, F>
where
    Svc: Service<Req>,
    F: Fn(&Req) -> S,
    S: Span,
{
    type Response = Svc::Response;
    type Error = Svc::Error;
    type Future = Instrumented<Svc::Future, S>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let span = (self.make_span)(&req);
        self.inner.call(req).instrument(span)
    }
}

// ── Recording response-derived (late) fields ──────────────────────────────────

/// A [`Layer`] like [`Dial9SpanLayer`], but whose `make_span` closure also
/// returns a *finish* callback for recording response-derived
/// [late fields](crate::span::Slot).
///
/// `make_span` returns `(span, finish)`: `span` instruments the request future
/// as usual, and `finish` is called with a reference to the **successful**
/// response just before the span closes — the place to set late `Slot`s
/// captured from this request's `dial9_span!`. Request-derived fields are still
/// best set eagerly inside `make_span`; `finish` is for values that only exist
/// once the response does (a status code, a body size). `finish` is not called
/// if the inner service returns an error.
///
/// Keeping the callback *inside* `make_span`'s return is deliberate: the slots
/// type is generated per call site and cannot be named, but a closure that
/// captures it never has to be — so no type annotations are required.
///
/// ```no_run
/// # use dial9_utils::tower::Dial9SpanLayerWithResponse;
/// # use dial9_utils::dial9_span;
/// # struct Req; struct Resp; impl Resp { fn status(&self) -> u16 { 200 } }
/// let layer = Dial9SpanLayerWithResponse::new(|_req: &Req| {
///     let (span, slots) = dial9_span!("http_request", status: u16);
///     (span, move |resp: &Resp| slots.status.set(resp.status()))
/// });
/// ```
#[derive(Clone)]
pub struct Dial9SpanLayerWithResponse<F> {
    make_span: F,
}

impl<F> fmt::Debug for Dial9SpanLayerWithResponse<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dial9SpanLayerWithResponse")
            .finish_non_exhaustive()
    }
}

impl<F> Dial9SpanLayerWithResponse<F> {
    /// Build a layer whose `make_span` returns `(span, finish)`; see the type
    /// docs.
    pub fn new(make_span: F) -> Self {
        Self { make_span }
    }
}

impl<Svc, F> Layer<Svc> for Dial9SpanLayerWithResponse<F>
where
    F: Clone,
{
    type Service = Dial9SpanServiceWithResponse<Svc, F>;

    fn layer(&self, inner: Svc) -> Self::Service {
        Dial9SpanServiceWithResponse {
            inner,
            make_span: self.make_span.clone(),
        }
    }
}

/// The [`Service`] produced by [`Dial9SpanLayerWithResponse`].
#[derive(Clone)]
pub struct Dial9SpanServiceWithResponse<Svc, F> {
    inner: Svc,
    make_span: F,
}

impl<Svc, F> fmt::Debug for Dial9SpanServiceWithResponse<Svc, F>
where
    Svc: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dial9SpanServiceWithResponse")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<Svc, F, S, G, Req> Service<Req> for Dial9SpanServiceWithResponse<Svc, F>
where
    Svc: Service<Req>,
    F: Fn(&Req) -> (S, G),
    S: Span,
    G: Fn(&Svc::Response),
{
    type Response = Svc::Response;
    type Error = Svc::Error;
    type Future = Instrumented<OnResponse<Svc::Future, G>, S>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let (span, on_response) = (self.make_span)(&req);
        OnResponse {
            inner: self.inner.call(req),
            on_response,
        }
        .instrument(span)
    }
}

pin_project! {
    /// Runs a callback with the successful response the moment the inner future
    /// resolves — before the enclosing [`Instrumented`] span emits its
    /// completion event, so late fields set by the callback ride that event.
    /// Constructed by [`Dial9SpanServiceWithResponse`]; not built directly.
    pub struct OnResponse<Fut, G> {
        #[pin]
        inner: Fut,
        on_response: G,
    }
}

impl<Fut, G, T, E> Future for OnResponse<Fut, G>
where
    Fut: Future<Output = Result<T, E>>,
    G: Fn(&T),
{
    type Output = Result<T, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let out = ready!(this.inner.poll(cx));
        if let Ok(ref resp) = out {
            (this.on_response)(resp);
        }
        Poll::Ready(out)
    }
}

impl<Fut, G> fmt::Debug for OnResponse<Fut, G>
where
    Fut: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnResponse")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

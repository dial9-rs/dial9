//! [`tower`](https://docs.rs/tower) middleware that instruments each request
//! future with a span.
//!
//! Requires the `tower` feature.

use super::{Instrument as _, Instrumented, Span};
use std::fmt;
use std::task::{Context, Poll};
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
/// # use dial9_utils::span::Dial9SpanLayer;
/// # use dial9_utils::dial9_span;
/// // Per-request fields:
/// let layer = Dial9SpanLayer::new(|req: &u64| dial9_span!("rpc", id = *req));
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
    /// # use dial9_utils::span::Dial9SpanLayer;
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

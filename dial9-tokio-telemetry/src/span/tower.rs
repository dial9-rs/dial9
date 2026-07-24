//! [`tower`](https://docs.rs/tower) middleware that instruments each request
//! future with a span.
//!
//! Requires the `tower` feature.

use super::{Dial9Span, Instrument as _, Instrumented, Span};
use std::fmt;
use std::task::{Context, Poll};
use tower_layer::Layer;
use tower_service::Service;

/// A [`tower::Layer`](tower_layer::Layer) that wraps a service so each request's
/// response future is recorded as a span.
///
/// The span for each request is produced by a `make_span` closure, letting you
/// give every request the same name or vary it per call. For a fixed name, use
/// [`Dial9SpanLayer::named`].
///
/// ```no_run
/// use dial9_tokio_telemetry::span::Dial9SpanLayer;
/// # use tower_layer::Layer;
/// # fn demo<S>(service: S) {
/// // Every request becomes a span named "http_request":
/// let layer = Dial9SpanLayer::named("http_request");
/// let instrumented = layer.layer(service);
/// # let _ = instrumented;
/// # }
/// ```
///
/// To attach fields, return a [`dial9_span!`](crate::dial9_span) span from
/// [`new`](Dial9SpanLayer::new):
///
/// ```no_run
/// # use dial9_tokio_telemetry::span::Dial9SpanLayer;
/// # use dial9_tokio_telemetry::dial9_span;
/// let layer = Dial9SpanLayer::new(|| dial9_span!("rpc", service = "auth"));
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

impl Dial9SpanLayer<()> {
    /// Build a layer that names every request span `name`.
    pub fn named(name: impl Into<String>) -> Dial9SpanLayer<impl Fn() -> Dial9Span + Clone> {
        let name = name.into();
        Dial9SpanLayer {
            make_span: move || Dial9Span::new(name.clone()),
        }
    }
}

impl<F, S> Dial9SpanLayer<F>
where
    F: Fn() -> S + Clone,
    S: Span,
{
    /// Build a layer that produces a fresh span per request via `make_span`.
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
    F: Fn() -> S,
    S: Span,
{
    type Response = Svc::Response;
    type Error = Svc::Error;
    type Future = Instrumented<Svc::Future, S>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let span = (self.make_span)();
        self.inner.call(req).instrument(span)
    }
}

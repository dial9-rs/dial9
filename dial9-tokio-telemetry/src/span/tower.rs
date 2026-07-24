//! [`tower`](https://docs.rs/tower) middleware that instruments each request
//! future with a [`Dial9Span`].
//!
//! Requires the `tower` feature.

use super::{Dial9Span, Instrument, Instrumented};
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

impl<F> Dial9SpanLayer<F>
where
    F: Fn() -> Dial9Span + Clone,
{
    /// Build a layer that produces a fresh span per request via `make_span`.
    ///
    /// Use this to attach fields, e.g.
    /// `Dial9SpanLayer::new(|| dial9_span!("rpc", service = "auth"))`.
    pub fn new(make_span: F) -> Self {
        Self { make_span }
    }
}

impl<S, F> Layer<S> for Dial9SpanLayer<F>
where
    F: Clone,
{
    type Service = Dial9SpanService<S, F>;

    fn layer(&self, inner: S) -> Self::Service {
        Dial9SpanService {
            inner,
            make_span: self.make_span.clone(),
        }
    }
}

/// The [`Service`] produced by [`Dial9SpanLayer`]. Instruments each
/// [`call`](Service::call)'s future with a freshly-created span.
#[derive(Clone)]
pub struct Dial9SpanService<S, F> {
    inner: S,
    make_span: F,
}

impl<S, F> fmt::Debug for Dial9SpanService<S, F>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dial9SpanService")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<S, F, Req> Service<Req> for Dial9SpanService<S, F>
where
    S: Service<Req>,
    F: Fn() -> Dial9Span,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Instrumented<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let span = (self.make_span)();
        self.inner.call(req).instrument(span)
    }
}

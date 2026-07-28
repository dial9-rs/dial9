//! Axum 0.8 support.

use std::{
    convert::Infallible, fmt::Debug, future::Future, io, marker::PhantomData, net::SocketAddr,
    pin::pin,
};

use axum_0_8::serve::Listener;
use axum_core_0_5::{body::Body, extract::Request, response::Response};
use dial9_core::{clock::clock_monotonic_ns, handle::Dial9Handle};
use futures_util::FutureExt as _;
use hyper::body::Incoming;
use hyper_util::{rt::TokioIo, server::conn::auto::Builder, service::TowerToHyperService};
use tokio::sync::watch;
use tower::ServiceExt as _;
use tower_service::Service;

use super::{
    ConnectionAccepted, ConnectionClosed, Executor, HyperExecutor, connection_metadata, elapsed_us,
};

/// The connection information passed to the make-service.
///
/// This mirrors Axum 0.8's `IncomingStream`, whose fields are private.
#[derive(Debug)]
pub struct IncomingStream<'a, L: Listener> {
    io: &'a TokioIo<L::Io>,
    remote_addr: L::Addr,
}

impl<L: Listener> IncomingStream<'_, L> {
    /// Return a reference to the listener's IO type.
    pub fn io(&self) -> &L::Io {
        self.io.inner()
    }

    /// Return the remote address for this connection.
    pub fn remote_addr(&self) -> &L::Addr {
        &self.remote_addr
    }
}

impl axum_0_8::extract::connect_info::Connected<IncomingStream<'_, tokio::net::TcpListener>>
    for SocketAddr
{
    fn connect_info(stream: IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        *stream.remote_addr()
    }
}

/// Serve an Axum 0.8 service with dial9-instrumented connection tasks.
pub fn serve<L, M, S>(listener: L, make_service: M) -> Serve<L, M, S>
where
    L: Listener,
    M: for<'a> Service<IncomingStream<'a, L>, Error = Infallible, Response = S>,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
{
    Serve {
        listener,
        make_service,
        executor: (),
        _marker: PhantomData,
    }
}

/// A traced Axum 0.8 server.
#[must_use = "servers must be given an executor and awaited"]
#[derive(Debug)]
pub struct Serve<L, M, S, E = ()> {
    listener: L,
    make_service: M,
    executor: E,
    _marker: PhantomData<fn() -> S>,
}

impl<L, M, S, E> Serve<L, M, S, E>
where
    L: Listener,
{
    /// Use `executor` to spawn connection and Hyper tasks.
    pub fn with_executor<E2>(self, executor: E2) -> Serve<L, M, S, E2>
    where
        E2: Executor,
    {
        Serve {
            listener: self.listener,
            make_service: self.make_service,
            executor,
            _marker: PhantomData,
        }
    }

    /// Shut down after `signal` completes, while allowing connections to drain.
    pub fn with_graceful_shutdown<F>(self, signal: F) -> WithGracefulShutdown<L, M, S, F, E>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        WithGracefulShutdown {
            listener: self.listener,
            make_service: self.make_service,
            signal,
            executor: self.executor,
            _marker: PhantomData,
        }
    }

    /// Return the local address this server is bound to.
    pub fn local_addr(&self) -> io::Result<L::Addr> {
        self.listener.local_addr()
    }
}

impl<L, M, S, E> IntoFuture for Serve<L, M, S, E>
where
    L: Listener,
    L::Addr: Debug,
    M: for<'a> Service<IncomingStream<'a, L>, Error = Infallible, Response = S> + Send + 'static,
    for<'a> <M as Service<IncomingStream<'a, L>>>::Future: Send,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
    E: Executor,
{
    type Output = io::Result<()>;
    type IntoFuture = futures_util::future::BoxFuture<'static, io::Result<()>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self {
            mut listener,
            mut make_service,
            executor,
            _marker,
        } = self;

        Box::pin(async move {
            let (signal_tx, _signal_rx) = watch::channel(());
            let (_close_tx, close_rx) = watch::channel(());
            let handle = Dial9Handle::current();

            loop {
                let (io, remote_addr) = listener.accept().await;
                handle_connection::<L, _, _, _>(
                    &mut make_service,
                    &signal_tx,
                    &close_rx,
                    io,
                    remote_addr,
                    &executor,
                    &handle,
                )
                .await;
            }
        })
    }
}

/// A traced Axum 0.8 server with graceful shutdown.
#[must_use = "servers must be given an executor and awaited"]
#[derive(Debug)]
pub struct WithGracefulShutdown<L, M, S, F, E = ()> {
    listener: L,
    make_service: M,
    signal: F,
    executor: E,
    _marker: PhantomData<fn() -> S>,
}

impl<L, M, S, F, E> WithGracefulShutdown<L, M, S, F, E>
where
    L: Listener,
{
    /// Use `executor` to spawn connection and Hyper tasks.
    pub fn with_executor<E2>(self, executor: E2) -> WithGracefulShutdown<L, M, S, F, E2>
    where
        E2: Executor,
    {
        WithGracefulShutdown {
            listener: self.listener,
            make_service: self.make_service,
            signal: self.signal,
            executor,
            _marker: PhantomData,
        }
    }

    /// Return the local address this server is bound to.
    pub fn local_addr(&self) -> io::Result<L::Addr> {
        self.listener.local_addr()
    }
}

impl<L, M, S, F, E> IntoFuture for WithGracefulShutdown<L, M, S, F, E>
where
    L: Listener,
    L::Addr: Debug,
    M: for<'a> Service<IncomingStream<'a, L>, Error = Infallible, Response = S> + Send + 'static,
    for<'a> <M as Service<IncomingStream<'a, L>>>::Future: Send,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
    F: Future<Output = ()> + Send + 'static,
    E: Executor,
{
    type Output = io::Result<()>;
    type IntoFuture = futures_util::future::BoxFuture<'static, io::Result<()>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self {
            mut listener,
            mut make_service,
            signal,
            executor,
            _marker,
        } = self;

        Box::pin(async move {
            let (signal_tx, signal_rx) = watch::channel(());
            executor.execute(Box::pin(async move {
                signal.await;
                drop(signal_rx);
            }));

            let (close_tx, close_rx) = watch::channel(());
            let handle = Dial9Handle::current();

            loop {
                let (io, remote_addr) = tokio::select! {
                    connection = listener.accept() => connection,
                    _ = signal_tx.closed() => break,
                };
                handle_connection::<L, _, _, _>(
                    &mut make_service,
                    &signal_tx,
                    &close_rx,
                    io,
                    remote_addr,
                    &executor,
                    &handle,
                )
                .await;
            }

            drop(close_rx);
            drop(listener);
            close_tx.closed().await;
            Ok(())
        })
    }
}

async fn handle_connection<L, M, S, E>(
    make_service: &mut M,
    signal_tx: &watch::Sender<()>,
    close_rx: &watch::Receiver<()>,
    io: L::Io,
    remote_addr: L::Addr,
    executor: &E,
    handle: &Dial9Handle,
) where
    L: Listener,
    L::Addr: Debug,
    M: for<'a> Service<IncomingStream<'a, L>, Error = Infallible, Response = S> + Send + 'static,
    for<'a> <M as Service<IncomingStream<'a, L>>>::Future: Send,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
    E: Executor,
{
    let remote = connection_metadata(&remote_addr);
    handle.record_event(ConnectionAccepted {
        timestamp_ns: clock_monotonic_ns(),
        remote: remote.clone(),
    });

    let io = TokioIo::new(io);
    make_service
        .ready()
        .await
        .unwrap_or_else(|error| match error {});
    let tower_service = make_service
        .call(IncomingStream {
            io: &io,
            remote_addr,
        })
        .await
        .unwrap_or_else(|error| match error {})
        .map_request(|request: Request<Incoming>| request.map(Body::new));

    let hyper_service = TowerToHyperService::new(tower_service);
    let signal_tx = signal_tx.clone();
    let close_rx = close_rx.clone();
    let connection_handle = handle.clone();
    let connection_start = std::time::Instant::now();

    let connection_executor = executor.clone();
    let hyper_executor = HyperExecutor(executor.clone());
    connection_executor.execute(Box::pin(async move {
        let builder = Builder::new(hyper_executor);
        let connection = builder.serve_connection_with_upgrades(io, hyper_service);
        let mut connection = pin!(connection);
        let mut signal_closed = pin!(signal_tx.closed().fuse());

        loop {
            tokio::select! {
                result = connection.as_mut() => {
                    if let Err(error) = result {
                        tracing::trace!("failed to serve connection: {error:#}");
                    }
                    break;
                }
                _ = &mut signal_closed => {
                    connection.as_mut().graceful_shutdown();
                }
            }
        }

        connection_handle.record_event(ConnectionClosed {
            timestamp_ns: clock_monotonic_ns(),
            remote,
            duration_us: elapsed_us(connection_start),
        });
        drop(close_rx);
    }));
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum_0_8::{Router, extract::ConnectInfo, routing::get};
    use tokio::{net::TcpListener, sync::oneshot};

    use super::super::TraceCapture;
    use super::serve;

    #[tokio::test(flavor = "current_thread")]
    async fn serves_connect_info_and_shuts_down() {
        let trace = TraceCapture::start();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/",
            get(|ConnectInfo(address): ConnectInfo<SocketAddr>| async move { address.to_string() }),
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let execution_count = Arc::new(AtomicUsize::new(0));
        let executor_count = execution_count.clone();
        let server = serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            shutdown_rx.await.expect("shutdown sender dropped");
        })
        .with_executor(move |future| {
            executor_count.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(future);
        });
        assert_eq!(server.local_addr().unwrap(), address);

        let server = tokio::spawn(async move { server.await });
        let response = super::super::send_request(address).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("127.0.0.1:"));

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server did not shut down")
            .expect("server task panicked")
            .expect("server failed");
        assert!(execution_count.load(Ordering::Relaxed) >= 2);

        let events = trace.finish();
        assert_eq!(events.accepted.len(), 1);
        assert_eq!(events.closed, events.accepted);
        assert_eq!(events.accepted[0].remote_addr, "127.0.0.1");
        assert!(events.accepted[0].remote_port.is_some());
    }

    #[tokio::test]
    async fn serve_without_graceful_shutdown_spawns_no_shutdown_task() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = Router::new().route("/", get(|| async { "ok" }));
        let execution_count = Arc::new(AtomicUsize::new(0));
        let executor_count = execution_count.clone();
        let server = serve(listener, app.into_make_service()).with_executor(move |future| {
            executor_count.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(future);
        });

        let result =
            tokio::time::timeout(Duration::from_millis(10), async move { server.await }).await;
        assert!(result.is_err(), "server unexpectedly stopped");
        assert_eq!(execution_count.load(Ordering::Relaxed), 0);
    }
}

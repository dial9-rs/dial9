//! Axum 0.7 support.

use std::{
    convert::Infallible, future::Future, io, marker::PhantomData, net::SocketAddr, pin::pin,
};

use axum_core_0_4::{body::Body, extract::Request, response::Response};
use dial9_tokio_telemetry::telemetry::{Dial9Handle, clock_monotonic_ns};
use futures_util::FutureExt as _;
use hyper::body::Incoming;
use hyper_util::{rt::TokioIo, server::conn::auto::Builder, service::TowerToHyperService};
use tokio::{net::TcpListener, sync::watch};
use tower::ServiceExt as _;
use tower_service::Service;

use super::{
    ConnectionAccepted, ConnectionClosed, CurrentDial9Executor, Executor, HyperExecutor, elapsed_us,
};

/// The connection information passed to the make-service.
///
/// This mirrors Axum 0.7's `IncomingStream`, whose fields are private.
#[derive(Debug)]
pub struct IncomingStream<'a> {
    io: &'a TokioIo<tokio::net::TcpStream>,
    remote_addr: SocketAddr,
}

impl IncomingStream<'_> {
    /// Return the local address for this connection.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.inner().local_addr()
    }

    /// Return the remote address for this connection.
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }
}

impl axum_0_7::extract::connect_info::Connected<IncomingStream<'_>> for SocketAddr {
    fn connect_info(stream: IncomingStream<'_>) -> Self {
        stream.remote_addr()
    }
}

/// Serve an Axum 0.7 service with dial9-instrumented connection tasks.
pub fn serve<M, S>(listener: TcpListener, make_service: M) -> Serve<M, S>
where
    M: for<'a> Service<IncomingStream<'a>, Error = Infallible, Response = S>,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
{
    Serve {
        listener,
        make_service,
        executor: CurrentDial9Executor,
        _marker: PhantomData,
    }
}

/// A traced Axum 0.7 server.
#[must_use = "servers must be awaited"]
pub struct Serve<M, S, E = CurrentDial9Executor> {
    listener: TcpListener,
    make_service: M,
    executor: E,
    _marker: PhantomData<fn() -> S>,
}

impl<M, S, E> Serve<M, S, E> {
    /// Use `executor` instead of resolving dial9 from the current runtime.
    pub fn with_executor<E2>(self, executor: E2) -> Serve<M, S, E2>
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
    pub fn with_graceful_shutdown<F>(self, signal: F) -> WithGracefulShutdown<M, S, F, E>
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
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

impl<M, S, E> IntoFuture for Serve<M, S, E>
where
    M: for<'a> Service<IncomingStream<'a>, Error = Infallible, Response = S> + Send + 'static,
    for<'a> <M as Service<IncomingStream<'a>>>::Future: Send,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
    E: Executor,
{
    type Output = io::Result<()>;
    type IntoFuture = futures_util::future::BoxFuture<'static, io::Result<()>>;

    fn into_future(self) -> Self::IntoFuture {
        self.with_graceful_shutdown(std::future::pending())
            .into_future()
    }
}

/// A traced Axum 0.7 server with graceful shutdown.
#[must_use = "servers must be awaited"]
pub struct WithGracefulShutdown<M, S, F, E = CurrentDial9Executor> {
    listener: TcpListener,
    make_service: M,
    signal: F,
    executor: E,
    _marker: PhantomData<fn() -> S>,
}

impl<M, S, F, E> WithGracefulShutdown<M, S, F, E> {
    /// Use `executor` instead of resolving dial9 from the current runtime.
    pub fn with_executor<E2>(self, executor: E2) -> WithGracefulShutdown<M, S, F, E2>
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
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

impl<M, S, F, E> IntoFuture for WithGracefulShutdown<M, S, F, E>
where
    M: for<'a> Service<IncomingStream<'a>, Error = Infallible, Response = S> + Send + 'static,
    for<'a> <M as Service<IncomingStream<'a>>>::Future: Send,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
    F: Future<Output = ()> + Send + 'static,
    E: Executor,
{
    type Output = io::Result<()>;
    type IntoFuture = futures_util::future::BoxFuture<'static, io::Result<()>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self {
            listener,
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
                    connection = listener.accept() => connection?,
                    _ = signal_tx.closed() => break,
                };

                let remote_addr_string = format!("{remote_addr:?}");
                handle.record_event(ConnectionAccepted {
                    timestamp_ns: clock_monotonic_ns(),
                    remote_addr: remote_addr_string.clone(),
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
                        remote_addr: remote_addr_string,
                        duration_us: elapsed_us(connection_start),
                    });
                    drop(close_rx);
                }));
            }

            drop(close_rx);
            drop(listener);
            close_tx.closed().await;
            Ok(())
        })
    }
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

    use axum_0_7::{Router, extract::ConnectInfo, routing::get};
    use tokio::{net::TcpListener, sync::oneshot};

    use super::serve;

    #[tokio::test]
    async fn serves_connect_info_and_shuts_down() {
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
        .with_executor(move |future| {
            executor_count.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(future);
        })
        .with_graceful_shutdown(async move {
            shutdown_rx.await.expect("shutdown sender dropped");
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
    }
}

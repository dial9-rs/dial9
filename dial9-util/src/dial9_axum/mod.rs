//! Traced replacements for `axum::serve`.
//!
//! Select the module matching your Axum dependency and replace
//! `axum::serve(listener, service)` with this module's `serve` function:
//!
//! ```ignore
//! use dial9_util::dial9_axum::axum_0_8;
//!
//! axum_0_8::serve(listener, app.into_make_service())
//!     .with_graceful_shutdown(shutdown_signal())
//!     .await?;
//! ```
//!
//! Connection tasks and Hyper's internal HTTP/2 tasks are spawned through
//! dial9, allowing wake events and scheduling delays to be captured.
//! To use a handle from a different dial9 version, supply its spawn operation
//! explicitly:
//!
//! ```ignore
//! let handle = other_dial9_runtime.handle();
//!
//! axum_0_8::serve(listener, app.into_make_service())
//!     .with_executor({
//!         let handle = handle.clone();
//!         move |future| {
//!             handle.spawn(future);
//!         }
//!     })
//!     .await?;
//! ```
//!
//! The custom executor controls task instrumentation. Connection lifecycle
//! events continue to use the dial9 version linked by `dial9-util`.

use std::{future::Future, pin::Pin};

use dial9_tokio_telemetry::telemetry::{Dial9TokioHandle, Encodable, ThreadLocalEncoder};
use dial9_trace_format::{InternedString, TraceEvent};

/// Axum 0.7 support.
#[cfg(feature = "axum-0-7")]
pub mod axum_0_7;

/// Axum 0.8 support.
#[cfg(feature = "axum-0-8")]
pub mod axum_0_8;

/// A boxed task accepted by an [`Executor`].
pub type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Spawns Axum connection tasks and Hyper's internal tasks.
///
/// Closures implement this trait automatically. This keeps the integration
/// independent of a particular dial9 version:
///
/// ```ignore
/// let handle = my_dial9_runtime.handle();
/// let server = axum_0_8::serve(listener, service).with_executor({
///     let handle = handle.clone();
///     move |future| {
///         handle.spawn(future);
///     }
/// });
/// ```
pub trait Executor: Clone + Send + Sync + 'static {
    /// Spawn `future` for asynchronous execution.
    fn execute(&self, future: BoxFuture);
}

impl<F> Executor for F
where
    F: Fn(BoxFuture) + Clone + Send + Sync + 'static,
{
    fn execute(&self, future: BoxFuture) {
        self(future);
    }
}

/// The default executor, which resolves dial9 from the current runtime.
#[derive(Clone, Copy, Debug, Default)]
pub struct CurrentDial9Executor;

impl Executor for CurrentDial9Executor {
    fn execute(&self, future: BoxFuture) {
        Dial9TokioHandle::current().spawn(future);
    }
}

struct ConnectionAccepted {
    timestamp_ns: u64,
    remote_addr: String,
}

#[derive(TraceEvent)]
struct ConnectionAcceptedWire {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    remote_addr: InternedString,
}

impl Encodable for ConnectionAccepted {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let remote_addr = enc.intern_string(&self.remote_addr);
        enc.encode(&ConnectionAcceptedWire {
            timestamp_ns: self.timestamp_ns,
            remote_addr,
        });
    }
}

struct ConnectionClosed {
    timestamp_ns: u64,
    remote_addr: String,
    duration_us: u64,
}

#[derive(TraceEvent)]
struct ConnectionClosedWire {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    remote_addr: InternedString,
    /// Rendered as a human-friendly duration in the viewer via the unit
    /// annotation.
    #[traceevent(unit = "us")]
    duration_us: u64,
}

impl Encodable for ConnectionClosed {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let remote_addr = enc.intern_string(&self.remote_addr);
        enc.encode(&ConnectionClosedWire {
            timestamp_ns: self.timestamp_ns,
            remote_addr,
            duration_us: self.duration_us,
        });
    }
}

/// A Hyper executor that routes internal HTTP/2 tasks through the selected executor.
#[derive(Clone)]
struct HyperExecutor<E>(E);

impl<E, Fut> hyper::rt::Executor<Fut> for HyperExecutor<E>
where
    E: Executor,
    Fut: Future + Send + 'static,
    Fut::Output: Send + 'static,
{
    fn execute(&self, future: Fut) {
        self.0.execute(Box::pin(async move {
            drop(future.await);
        }));
    }
}

fn elapsed_us(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
async fn send_request(address: std::net::SocketAddr) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to test server");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("write request");

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("read response");
    response
}

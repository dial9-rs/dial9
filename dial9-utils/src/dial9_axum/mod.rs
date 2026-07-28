//! Traced replacements for `axum::serve`.
//!
//! Select the module matching your Axum dependency and replace
//! `axum::serve(listener, service)` with this module's `serve` function:
//!
//! ```ignore
//! use dial9::Dial9TokioHandle;
//! use dial9_utils::dial9_axum::axum_0_8;
//!
//! let handle = Dial9TokioHandle::current();
//! axum_0_8::serve(listener, app.into_make_service())
//!     .with_executor(move |future| {
//!         handle.spawn(future);
//!     })
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
//! The executor controls task instrumentation and is required to avoid coupling
//! `dial9-utils` to a particular async runtime integration. Connection lifecycle
//! events use the `dial9-core` version linked by `dial9-utils`.

use std::{fmt::Debug, future::Future, net::SocketAddr, pin::Pin};

use dial9_core::encoder::{Encodable, ThreadLocalEncoder};
use dial9_trace_format::{InternedString, TraceEvent};

/// Axum 0.7 support.
#[cfg(feature = "axum-07")]
pub mod axum_0_7;

/// Axum 0.8 support.
#[cfg(feature = "axum-08")]
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConnectionMetadata {
    remote_addr: String,
    remote_port: Option<u16>,
}

fn connection_metadata<A>(remote_addr: &A) -> ConnectionMetadata
where
    A: Debug,
{
    // Axum 0.8 supports arbitrary listener address types. Keep those generic
    // while structuring standard TCP addresses for effective IP interning.
    let remote_addr = format!("{remote_addr:?}");
    if let Ok(remote_addr) = remote_addr.parse::<SocketAddr>() {
        ConnectionMetadata {
            remote_addr: remote_addr.ip().to_string(),
            remote_port: Some(remote_addr.port()),
        }
    } else {
        ConnectionMetadata {
            remote_addr,
            remote_port: None,
        }
    }
}

struct ConnectionAccepted {
    timestamp_ns: u64,
    remote: ConnectionMetadata,
}

#[derive(TraceEvent)]
struct ConnectionAcceptedWire {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    remote_addr: InternedString,
    remote_port: Option<u16>,
}

impl Encodable for ConnectionAccepted {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let remote_addr = enc.intern_string(&self.remote.remote_addr);
        enc.encode(&ConnectionAcceptedWire {
            timestamp_ns: self.timestamp_ns,
            remote_addr,
            remote_port: self.remote.remote_port,
        });
    }
}

struct ConnectionClosed {
    timestamp_ns: u64,
    remote: ConnectionMetadata,
    duration_us: u64,
}

#[derive(TraceEvent)]
struct ConnectionClosedWire {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    remote_addr: InternedString,
    remote_port: Option<u16>,
    /// Rendered as a human-friendly duration in the viewer via the unit
    /// annotation.
    #[traceevent(unit = "us")]
    duration_us: u64,
}

impl Encodable for ConnectionClosed {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let remote_addr = enc.intern_string(&self.remote.remote_addr);
        enc.encode(&ConnectionClosedWire {
            timestamp_ns: self.timestamp_ns,
            remote_addr,
            remote_port: self.remote.remote_port,
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

#[cfg(test)]
struct TraceCapture {
    directory: tempfile::TempDir,
    recorder: Option<dial9_core::recording::Recorder>,
}

#[cfg(test)]
#[derive(Default)]
struct RecordedConnectionEvents {
    accepted: Vec<ConnectionMetadata>,
    closed: Vec<ConnectionMetadata>,
}

#[cfg(test)]
impl TraceCapture {
    fn start() -> Self {
        let directory = tempfile::tempdir().expect("create trace directory");
        let writer =
            dial9_core::buffer::DiskBuffer::single_file(directory.path().join("trace.bin"))
                .expect("create trace writer");
        let recorder = dial9_core::recorder::recorder(writer).build();
        dial9_core::handle::set_tl_handle(recorder.handle().clone());
        Self {
            directory,
            recorder: Some(recorder),
        }
    }

    fn finish(mut self) -> RecordedConnectionEvents {
        dial9_core::handle::clear_tl_handle();
        self.recorder
            .take()
            .expect("trace recorder is present")
            .graceful_shutdown(std::time::Duration::ZERO);

        let bytes =
            std::fs::read(self.directory.path().join("trace.0.bin")).expect("read sealed trace");
        let mut decoder =
            dial9_trace_format::decoder::Decoder::new(&bytes).expect("decode trace header");
        let mut events = RecordedConnectionEvents::default();
        decoder
            .for_each_event(|event| {
                let destination = match event.name {
                    "ConnectionAcceptedWire" => &mut events.accepted,
                    "ConnectionClosedWire" => &mut events.closed,
                    _ => return,
                };

                let mut remote_addr = None;
                let mut remote_port = None;
                for (field, value) in event.schema.fields().iter().zip(event.fields) {
                    match (field.name(), value) {
                        (
                            "remote_addr",
                            dial9_trace_format::types::FieldValueRef::PooledString(id),
                        ) => {
                            remote_addr = Some(
                                event
                                    .string_pool
                                    .get(*id)
                                    .expect("remote address is in the string pool")
                                    .to_owned(),
                            );
                        }
                        ("remote_port", dial9_trace_format::types::FieldValueRef::Varint(port)) => {
                            remote_port =
                                Some(u16::try_from(*port).expect("remote port fits in u16"));
                        }
                        ("remote_port", dial9_trace_format::types::FieldValueRef::None) => {}
                        _ => {}
                    }
                }

                destination.push(ConnectionMetadata {
                    remote_addr: remote_addr.expect("connection event has a remote address"),
                    remote_port,
                });
            })
            .expect("decode trace events");
        events
    }
}

#[cfg(test)]
impl Drop for TraceCapture {
    fn drop(&mut self) {
        dial9_core::handle::clear_tl_handle();
    }
}

#[cfg(test)]
mod tests {
    use super::{ConnectionMetadata, connection_metadata};

    #[test]
    fn socket_connection_metadata_splits_ip_and_port() {
        let remote_addr: std::net::SocketAddr = "127.0.0.1:43210".parse().unwrap();

        assert_eq!(
            connection_metadata(&remote_addr),
            ConnectionMetadata {
                remote_addr: "127.0.0.1".to_owned(),
                remote_port: Some(43210),
            }
        );
    }
}

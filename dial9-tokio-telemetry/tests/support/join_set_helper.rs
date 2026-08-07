use dial9_tokio_telemetry::telemetry::JoinSetExt;
use std::future::Future;
use tokio::task::{AbortHandle, JoinSet};

// Kept in a separate file to make the `#[track_caller]` test exercise a
// realistic helper boundary.
#[track_caller]
pub fn spawn_traced<T, F>(set: &mut JoinSet<T>, future: F) -> AbortHandle
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    set.spawn_traced(future)
}

// Kept in a separate file to make the `#[track_caller]` test exercise a
// realistic helper boundary.
#[track_caller]
pub fn spawn_traced_on<T, F>(
    set: &mut JoinSet<T>,
    future: F,
    runtime: &tokio::runtime::Handle,
) -> AbortHandle
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    set.spawn_traced_on(future, runtime)
}

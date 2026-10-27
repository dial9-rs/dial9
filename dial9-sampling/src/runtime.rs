use std::future::{Future, poll_fn};
use tokio::runtime::{Handle, Runtime};

/// Even cancellation or unwinding inside another runtime must not perform a
/// blocking Tokio runtime drop.
#[derive(Debug)]
pub(crate) struct OwnedRuntime(Option<Runtime>);

impl OwnedRuntime {
    pub(crate) fn new() -> std::io::Result<Self> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("dial9-sampling")
            .enable_all()
            .build()
            .map(|runtime| Self(Some(runtime)))
    }

    pub(crate) fn handle(&self) -> &Handle {
        self.0.as_ref().expect("owned sampling runtime").handle()
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

/// Borrowed backend factories need not be Send or 'static. Enter only while
/// polling, so tasks and I/O belong to the sampler without leaking its context
/// across an await on the caller's executor.
pub(crate) async fn on_runtime<F: Future>(handle: &Handle, future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    poll_fn(|cx| {
        let _enter = handle.enter();
        future.as_mut().poll(cx)
    })
    .await
}

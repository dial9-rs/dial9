use super::handle::Dial9TokioHandle;
use std::future::Future;
use tokio::task::{AbortHandle, JoinSet};

/// Dial9 instrumentation for [`JoinSet`].
pub trait JoinSetExt<T>: private::Sealed {
    /// Spawn a task into this set with dial9 instrumentation.
    #[track_caller]
    fn spawn_traced<F>(&mut self, future: F) -> AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static;
}

impl<T> JoinSetExt<T> for JoinSet<T> {
    #[track_caller]
    fn spawn_traced<F>(&mut self, future: F) -> AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        Dial9TokioHandle::current().spawn_in_join_set(self, future)
    }
}

mod private {
    pub trait Sealed {}

    impl<T> Sealed for tokio::task::JoinSet<T> {}
}

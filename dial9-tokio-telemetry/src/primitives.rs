//! Cfg-gated concurrency primitives.
//!
//! Under normal compilation this re-exports from `std`. With `--cfg shuttle`
//! it re-exports from [`shuttle`], giving the shuttle scheduler control over all
//! synchronization points so that tests can explore thread interleavings
//! deterministically.

// ── std path (production) ───────────────────────────────────────────────────

#[cfg(not(shuttle))]
pub(crate) mod sync {
    pub(crate) use std::sync::atomic;
    pub(crate) use std::sync::mpsc;
    #[allow(unused_imports)]
    pub(crate) use std::sync::{Arc, Barrier, Mutex, Weak};
}

#[cfg(not(shuttle))]
pub(crate) mod thread {
    #[allow(unused_imports)]
    pub(crate) use std::thread::{JoinHandle, sleep, spawn};

    /// Spawn a named thread. Uses `std::thread::Builder` in production,
    /// falls back to plain `spawn` under shuttle (which has no Builder).
    pub(crate) fn spawn_named<F, T>(name: &str, f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        std::thread::Builder::new()
            .name(name.into())
            .spawn(f)
            .expect("failed to spawn thread")
    }
}

#[cfg(not(shuttle))]
macro_rules! define_thread_local {
    ($($tt:tt)*) => { std::thread_local! { $($tt)* } };
}
#[cfg(not(shuttle))]
pub(crate) use define_thread_local as thread_local;

// ── shuttle path (deterministic testing) ────────────────────────────────────

#[cfg(shuttle)]
pub(crate) mod sync {
    pub(crate) use shuttle::sync::atomic;
    #[allow(unused_imports)]
    pub(crate) use shuttle::sync::{Arc, Barrier, Mutex, Weak};

    /// Wrapper around shuttle's mpsc that adds random timeouts to
    /// `recv_timeout`. Shuttle's built-in `recv_timeout` ignores the
    /// timeout and blocks unconditionally, which means the flush loop
    /// never loops. This wrapper randomly returns `Timeout` so shuttle
    /// can explore interleavings where the flush loop actually runs
    /// multiple cycles.
    pub(crate) mod mpsc {
        pub(crate) use shuttle::sync::mpsc::{RecvTimeoutError, SyncSender};

        pub(crate) struct Receiver<T> {
            inner: shuttle::sync::mpsc::Receiver<T>,
        }

        // shuttle::sync::mpsc::Receiver is Send but the wrapper needs to be too
        // SAFETY: shuttle's Receiver<T> is Send when T: Send
        unsafe impl<T: Send> Send for Receiver<T> {}

        impl<T> Receiver<T> {
            pub(crate) fn recv_timeout(
                &self,
                _timeout: std::time::Duration,
            ) -> Result<T, RecvTimeoutError> {
                // Randomly decide whether to simulate a timeout, giving
                // the flush loop a chance to execute its body.
                if shuttle::rand::thread_rng().gen_bool(0.8) {
                    match self.inner.try_recv() {
                        Ok(val) => Ok(val),
                        Err(shuttle::sync::mpsc::TryRecvError::Empty) => {
                            Err(RecvTimeoutError::Timeout)
                        }
                        Err(shuttle::sync::mpsc::TryRecvError::Disconnected) => {
                            Err(RecvTimeoutError::Disconnected)
                        }
                    }
                } else {
                    // Delegate to shuttle's blocking recv to explore the
                    // "flush loop blocks waiting for command" path.
                    self.inner
                        .recv()
                        .map_err(|_| RecvTimeoutError::Disconnected)
                }
            }

            pub(crate) fn recv(&self) -> Result<T, shuttle::sync::mpsc::RecvError> {
                self.inner.recv()
            }
        }

        use shuttle::rand::Rng;

        /// Wraps shuttle's `sync_channel` to return our `Receiver` wrapper.
        pub(crate) fn sync_channel<T>(bound: usize) -> (SyncSender<T>, Receiver<T>) {
            let (tx, rx) = shuttle::sync::mpsc::sync_channel(bound);
            (tx, Receiver { inner: rx })
        }
    }
}

#[cfg(shuttle)]
pub(crate) mod thread {
    #[allow(unused_imports)]
    pub(crate) use shuttle::thread::{JoinHandle, sleep, spawn};

    pub(crate) fn spawn_named<F, T>(_name: &str, f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        spawn(f)
    }
}

#[cfg(shuttle)]
macro_rules! define_thread_local {
    ($($tt:tt)*) => { shuttle::thread_local! { $($tt)* } };
}
#[cfg(shuttle)]
pub(crate) use define_thread_local as thread_local;

#[cfg(not(shuttle))]
pub(crate) mod fs {
    use std::io::{self, Write};
    use std::path::Path;

    pub(crate) fn create_dir_all(path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }
    pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    pub(crate) fn remove_dir(path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }
    pub(crate) fn read_dir(path: &Path) -> io::Result<std::fs::ReadDir> {
        std::fs::read_dir(path)
    }
    pub(crate) fn metadata(path: &Path) -> io::Result<std::fs::Metadata> {
        std::fs::metadata(path)
    }
    pub(crate) fn read(path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    /// Active-segment file handle.
    #[derive(Debug)]
    pub(crate) struct File(std::fs::File);

    impl File {
        pub(crate) fn create(path: &Path) -> io::Result<File> {
            std::fs::File::create(path).map(File)
        }
    }

    impl Write for File {
        #[inline]
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }
        #[inline]
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }
}

#[cfg(shuttle)]
pub(crate) mod fs {
    use std::cell::Cell;
    use std::io::{self, ErrorKind, Write};
    use std::path::Path;

    use shuttle::rand::Rng;

    /// How to fail filesystem operations during a shuttle run.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum FaultPolicy {
        /// Delegate to real `std::fs`, nothing fails.
        None,
        /// Every fallible op returns `PermissionDenied`.
        FailAll,
        /// Each op independently fails with this probability, drawn from
        /// shuttle's RNG so the scheduler explores the fault schedule.
        FailProb(f64),
    }

    std::thread_local! {
        static FAULT: Cell<FaultPolicy> = const { Cell::new(FaultPolicy::None) };
    }

    /// Arm `policy`: the returned guard restores the previous one on drop so a
    /// fault can't leak into the next shuttle iteration.
    #[must_use]
    pub(crate) fn set_fault(policy: FaultPolicy) -> FaultGuard {
        let prev = FAULT.with(|f| f.replace(policy));
        FaultGuard { prev }
    }

    pub(crate) struct FaultGuard {
        prev: FaultPolicy,
    }

    impl Drop for FaultGuard {
        fn drop(&mut self) {
            FAULT.with(|f| f.set(self.prev));
        }
    }

    fn check() -> io::Result<()> {
        let fail = match FAULT.with(|f| f.get()) {
            FaultPolicy::None => false,
            FaultPolicy::FailAll => true,
            FaultPolicy::FailProb(p) => shuttle::rand::thread_rng().gen_bool(p),
        };
        if fail {
            Err(io::Error::from(ErrorKind::PermissionDenied))
        } else {
            Ok(())
        }
    }

    pub(crate) fn create_dir_all(path: &Path) -> io::Result<()> {
        check()?;
        std::fs::create_dir_all(path)
    }
    pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
        check()?;
        std::fs::rename(from, to)
    }
    pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
        check()?;
        std::fs::remove_file(path)
    }
    pub(crate) fn remove_dir(path: &Path) -> io::Result<()> {
        check()?;
        std::fs::remove_dir(path)
    }
    pub(crate) fn read_dir(path: &Path) -> io::Result<std::fs::ReadDir> {
        check()?;
        std::fs::read_dir(path)
    }
    pub(crate) fn metadata(path: &Path) -> io::Result<std::fs::Metadata> {
        check()?;
        std::fs::metadata(path)
    }
    pub(crate) fn read(path: &Path) -> io::Result<Vec<u8>> {
        check()?;
        std::fs::read(path)
    }

    /// Active-segment file handle whose `write`/`flush` honor the armed fault
    /// policy. `create` is never faulted, so a writer can always be built.
    #[derive(Debug)]
    pub(crate) struct File(std::fs::File);

    impl File {
        pub(crate) fn create(path: &Path) -> io::Result<File> {
            std::fs::File::create(path).map(File)
        }
    }

    impl Write for File {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            check()?;
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            check()?;
            self.0.flush()
        }
    }
}

// ── BoundedQueue ────────────────────────────────────────────────────────────

/// A bounded MPMC queue. Production uses `crossbeam_queue::ArrayQueue`;
/// under shuttle it uses a `Mutex<VecDeque>` so the scheduler can control
/// access.
#[cfg(not(shuttle))]
pub(crate) struct BoundedQueue<T> {
    inner: crossbeam_queue::ArrayQueue<T>,
}

#[cfg(not(shuttle))]
impl<T> BoundedQueue<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            inner: crossbeam_queue::ArrayQueue::new(capacity),
        }
    }

    /// Push a value, evicting the oldest if full. Returns the evicted value.
    pub(crate) fn force_push(&self, value: T) -> Option<T> {
        self.inner.force_push(value)
    }

    pub(crate) fn pop(&self) -> Option<T> {
        self.inner.pop()
    }
}

#[cfg(shuttle)]
pub(crate) struct BoundedQueue<T> {
    inner: shuttle::sync::Mutex<std::collections::VecDeque<T>>,
    capacity: usize,
}

#[cfg(shuttle)]
impl<T> BoundedQueue<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            inner: shuttle::sync::Mutex::new(std::collections::VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    pub(crate) fn force_push(&self, value: T) -> Option<T> {
        let mut q = self.inner.lock().unwrap();
        let evicted = if q.len() >= self.capacity {
            q.pop_front()
        } else {
            None
        };
        q.push_back(value);
        evicted
    }

    pub(crate) fn pop(&self) -> Option<T> {
        self.inner.lock().unwrap().pop_front()
    }
}

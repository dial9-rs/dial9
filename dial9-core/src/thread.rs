//! Thread identity helpers and per-thread source enrollment.

use std::{
    fmt::{Formatter, Result},
    marker::PhantomData,
};

use crate::handle::Dial9Handle;

/// Keeps the calling thread enrolled with the recorder's per-thread sources.
///
/// Dropping it stops tracking, so hold it for as long as the thread should be profiled.
#[must_use = "dropping the guard stops tracking this thread"]
pub struct ThreadTrackingGuard {
    handle: Dial9Handle,
    // Perf events are keyed by `gettid()` and the ctimer timer lives in a
    // thread-local, so the guard has to drop on the thread that created it.
    _not_send: PhantomData<*const ()>,
}

impl ThreadTrackingGuard {
    pub(crate) fn new(handle: Dial9Handle) -> Self {
        Self {
            handle,
            _not_send: PhantomData,
        }
    }
}

impl std::fmt::Debug for ThreadTrackingGuard {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        f.debug_struct("ThreadTrackingGuard")
            .finish_non_exhaustive()
    }
}

impl Drop for ThreadTrackingGuard {
    fn drop(&mut self) {
        let Some(shared) = self.handle.shared() else {
            return;
        };
        let stopped = shared.with_sources_mut(|sources| {
            for source in sources.iter_mut() {
                source.on_thread_stop();
            }
        });
        if stopped.is_none() {
            crate::rate_limited!(std::time::Duration::from_secs(60), {
                tracing::warn!("sources lock poisoned, thread left tracked");
            });
        }
    }
}

/// OS thread ID (tid) of the calling thread.
///
/// `gettid()` on Linux/Android (a vDSO/syscall); a stable per-thread counter
/// elsewhere. Allocation-free, so it is safe to call from the allocator hook.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn current_tid() -> u32 {
    // SAFETY: gettid takes no args and only returns the caller's tid.
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn current_tid() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    thread_local! { static TID: u32 = NEXT.fetch_add(1, Ordering::Relaxed); }
    TID.with(|t| *t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::MemoryBuffer;
    use crate::recorder::recorder;
    use crate::source::{FlushContext, Source};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Counts {
        started: AtomicUsize,
        stopped: AtomicUsize,
    }

    struct CountingSource {
        counts: Arc<Counts>,
        fails: bool,
    }

    impl Source for CountingSource {
        fn flush(&mut self, _ctx: &FlushContext<'_>) {}

        fn name(&self) -> &'static str {
            "counting"
        }

        fn on_thread_start(&mut self) -> std::io::Result<()> {
            if self.fails {
                return Err(std::io::Error::other("no room for this thread"));
            }
            self.counts.started.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn on_thread_stop(&mut self) {
            self.counts.stopped.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting_recorder(
        sources: impl IntoIterator<Item = CountingSource>,
    ) -> crate::recording::Recorder {
        let mut builder = recorder(MemoryBuffer::new(64 * 1024).expect("writer"));
        for source in sources {
            builder = builder.source(source);
        }
        builder.build()
    }

    #[test]
    fn guard_tracks_the_thread_until_it_drops() {
        let counts = Arc::new(Counts::default());
        let rec = counting_recorder([CountingSource {
            counts: Arc::clone(&counts),
            fails: false,
        }]);

        let guard = rec.handle().track_current_thread().expect("track");
        assert_eq!(counts.started.load(Ordering::SeqCst), 1);
        assert_eq!(counts.stopped.load(Ordering::SeqCst), 0);

        drop(guard);
        assert_eq!(counts.stopped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failing_source_rolls_back_the_started_ones() {
        let counts = Arc::new(Counts::default());
        let rec = counting_recorder([
            CountingSource {
                counts: Arc::clone(&counts),
                fails: false,
            },
            CountingSource {
                counts: Arc::clone(&counts),
                fails: true,
            },
        ]);

        let err = rec
            .handle()
            .track_current_thread()
            .expect_err("second source fails");
        assert_eq!(err.to_string(), "no room for this thread");
        // The one that did start was stopped again: no half-tracked thread.
        assert_eq!(counts.started.load(Ordering::SeqCst), 1);
        assert_eq!(counts.stopped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn disabled_handle_hands_back_an_inert_guard() {
        let guard = Dial9Handle::disabled()
            .track_current_thread()
            .expect("inert guard");
        drop(guard);
    }
}

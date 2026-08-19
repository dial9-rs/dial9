use crate::encoder::{Encodable, ThreadLocalEncoder};
use crate::primitives::sync::Arc;
use crate::shared_state::SharedState;
use crate::source::Source;
use crate::thread::ThreadTrackingGuard;
use std::any::Any;
use std::cell::RefCell;

/// First registered source of type `T`, if any.
fn find_source<T: Source>(sources: &mut [Box<dyn Source>]) -> Option<&mut T> {
    sources
        .iter_mut()
        .find_map(|source| (&mut **source as &mut dyn Any).downcast_mut::<T>())
}

crate::primitives::thread_local! {
    /// Per-thread [`Dial9Handle`], populated via [`set_tl_handle`] and cleared
    /// via [`clear_tl_handle`] (from a runtime's thread-start/stop hooks).
    /// Backs [`Dial9Handle::current`] and [`current_handle`].
    static CURRENT_HANDLE: RefCell<Option<Dial9Handle>> = const { RefCell::new(None) };
}

/// Commands sent to the flush thread by [`Recorder`](crate::recording::Recorder).
pub(crate) enum ControlCommand {
    /// Flush, finalize (seal segment), then exit the thread.
    FinalizeAndStop(crate::primitives::sync::mpsc::SyncSender<()>),
}

/// Cheap, cloneable handle for recording events and controlling telemetry.
///
/// A handle may be in one of two modes:
///
/// - **Enabled** — backed by a live recorder; methods record
///   events and control recording.
/// - **Disabled** — an inert sentinel returned by
///   [`Dial9Handle::disabled`] and by [`Dial9Handle::current`]
///   when called from a thread that is not owned by a dial9 runtime.
///   All methods are no-ops.
///
/// Use [`is_enabled`](Self::is_enabled) to distinguish the two modes.
#[derive(Clone)]
pub struct Dial9Handle {
    inner: Option<HandleInner>,
}

#[derive(Clone)]
struct HandleInner {
    shared: Arc<SharedState>,
    control_tx: crate::primitives::sync::mpsc::SyncSender<ControlCommand>,
}

impl std::fmt::Debug for Dial9Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dial9Handle")
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

impl Dial9Handle {
    /// Build an enabled handle wired to a flush thread's control sender.
    /// [`Recorder::start`](crate::recording::Recorder::start) mints the channel
    /// and owns the matching receiver.
    pub(crate) fn enabled(
        shared: Arc<SharedState>,
        control_tx: crate::primitives::sync::mpsc::SyncSender<ControlCommand>,
    ) -> Self {
        Self {
            inner: Some(HandleInner { shared, control_tx }),
        }
    }

    /// Return an inert handle that is not connected to any recorder.
    /// All methods are no-ops.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Whether recording through this handle currently does anything: the
    /// handle is connected to a live recorder AND recording is enabled (not
    /// paused via [`disable`](Self::disable)).
    ///
    /// Returns `false` for handles obtained via [`Dial9Handle::disabled`],
    /// for handles returned by [`Dial9Handle::current`] on a thread not
    /// owned by a dial9 runtime, and while a connected recorder is paused.
    ///
    /// Check this before doing per-event work that would be wasted while
    /// recording is off, such as work leading up to [`with_encoder`](Self::with_encoder).
    /// The check can race a concurrent enable/disable, which is benign since the event either
    /// lands or is skipped anyway.
    ///
    /// To ask only whether the handle is connected at all, regardless of
    /// pause state, use [`is_connected`](Self::is_connected).
    pub fn is_enabled(&self) -> bool {
        self.inner.as_ref().is_some_and(|i| i.shared.is_enabled())
    }

    crate::test_util_pub! {
        /// Access this handle's [`SharedState`].
        fn shared(&self) -> Option<&Arc<SharedState>> {
            self.inner.as_ref().map(|i| &i.shared)
        }
    }

    pub(crate) fn control_tx(
        &self,
    ) -> Option<&crate::primitives::sync::mpsc::SyncSender<ControlCommand>> {
        self.inner.as_ref().map(|i| &i.control_tx)
    }

    /// On-demand dump trigger for this runtime's recorder.
    ///
    /// Returns `None` on a disabled handle (see [`disabled`](Self::disabled))
    /// and when the runtime was built without a dump trigger
    /// (`with_dump_trigger`). The returned [`DumpTrigger`](crate::dump::DumpTrigger)
    /// is cheap to clone and every clone shares the configured debounce gate.
    #[cfg(feature = "pipeline")]
    pub fn dump_trigger(&self) -> Option<crate::dump::DumpTrigger> {
        self.inner
            .as_ref()
            .and_then(|i| i.shared.dump_trigger().cloned())
    }

    /// Return the [`Dial9Handle`] for the current thread.
    ///
    /// On threads claimed by a dial9 runtime (via [`set_tl_handle`], cleared
    /// by [`clear_tl_handle`]) this returns the live handle for that runtime.
    /// On any other thread it returns an inert handle whose methods are all
    /// no-ops — see [`Dial9Handle::disabled`].
    ///
    /// Use [`is_enabled`](Self::is_enabled) when you need to branch on
    /// whether telemetry is actually live on the current thread.
    pub fn current() -> Self {
        CURRENT_HANDLE
            .with(|cell| cell.borrow().clone())
            .unwrap_or_else(Self::disabled)
    }

    /// Return the [`Dial9Handle`] installed for the current thread,
    /// or `None` if no dial9 runtime has claimed this thread.
    ///
    /// Prefer [`current`](Self::current) instead.
    pub fn try_current() -> Option<Self> {
        CURRENT_HANDLE.with(|cell| cell.borrow().clone())
    }

    /// Enable telemetry recording. No-op on a disabled handle.
    pub fn enable(&self) {
        if let Some(inner) = &self.inner {
            inner.shared.enable();
        }
    }

    /// Disable telemetry recording. No-op on a disabled handle.
    pub fn disable(&self) {
        if let Some(inner) = &self.inner {
            inner.shared.disable();
        }
    }

    /// Profile the calling thread.
    ///
    /// Per-thread sources, such as the scheduler-event profiler, only sample
    /// threads that opt in. Tokio workers opt in on their own, call this from
    /// any other thread you want profiled. Profiling lasts until the returned
    /// guard drops.
    ///
    /// Returns an error if a source could not start on this thread. No-op on a
    /// disabled handle.
    ///
    /// ```no_run
    /// use dial9_core::buffer::MemoryBuffer;
    /// use dial9_core::recorder::recorder;
    ///
    /// let rec = recorder(MemoryBuffer::new(1 << 20)?).build();
    /// let handle = rec.handle().clone();
    ///
    /// std::thread::spawn(move || -> std::io::Result<()> {
    ///     let _tracking = handle.track_current_thread()?;
    ///     // work here is sampled by the recorder's per-thread sources
    ///     Ok(())
    /// });
    /// # Ok::<_, std::io::Error>(())
    /// ```
    pub fn track_current_thread(&self) -> std::io::Result<ThreadTrackingGuard> {
        let Some(inner) = &self.inner else {
            return Ok(ThreadTrackingGuard::new(self.clone()));
        };

        let started = inner.shared.with_sources_mut(|sources| {
            let mut done = 0;
            let mut failure = None;
            for source in sources.iter_mut() {
                match source.on_thread_start() {
                    Ok(()) => done += 1,
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }
            match failure {
                // Leave the thread untracked rather than half-tracked.
                Some(e) => {
                    for source in &mut sources[..done] {
                        source.on_thread_stop();
                    }
                    Err(e)
                }
                None => Ok(()),
            }
        });

        match started {
            Some(Ok(())) => Ok(ThreadTrackingGuard::new(self.clone())),
            Some(Err(e)) => Err(e),
            None => Err(std::io::Error::other("dial9: sources lock poisoned")),
        }
    }

    /// Whether this handle is wired to a recorder at all, regardless of whether
    /// recording is currently paused.
    ///
    /// [`is_enabled`](Self::is_enabled) answers the narrower question of whether
    /// a record right now would land.
    pub fn is_connected(&self) -> bool {
        self.inner.is_some()
    }

    /// Whether the recorder behind this handle has shut down.
    ///
    /// Terminal: a stopped recorder never records again. Returns `false` for a
    /// handle that is merely paused (see [`disable`](Self::disable)) and for a
    /// disabled handle, neither of which is stopped.
    pub fn is_stopped(&self) -> bool {
        self.inner.as_ref().is_some_and(|i| i.shared.is_stopped())
    }

    /// Run `f` against this recorder's source of type `T`.
    ///
    /// `None` when the handle is disabled, no `T` is registered, or the source
    /// lock is poisoned.
    pub fn with_source<T: Source, R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let inner = self.inner.as_ref()?;
        inner
            .shared
            .with_sources_mut(|sources| Some(f(find_source::<T>(sources)?)))
            .flatten()
    }

    /// Run `f` against this recorder's source of type `T`, registering the one
    /// `make` builds if there is not one yet.
    ///
    /// `None` when the handle is disabled, the recorder has shut down, or the
    /// source lock is poisoned.
    pub fn with_source_or_insert<T: Source, R>(
        &self,
        make: impl FnOnce() -> T,
        f: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        let inner = self.inner.as_ref()?;
        inner
            .shared
            .with_sources_vec(|sources| {
                if inner.shared.is_stopped() {
                    return None;
                }
                if find_source::<T>(sources).is_none() {
                    sources.push(Box::new(make()));
                }
                Some(f(find_source::<T>(sources).expect("just registered")))
            })
            .flatten()
    }

    /// Record a custom event into the trace.
    ///
    /// Any type implementing [`dial9_trace_format::TraceEvent`] (typically via
    /// `#[derive(TraceEvent)]`) works directly. No-op on a disabled handle or
    /// when recording is paused.
    pub fn record_event(&self, event: impl Encodable) {
        if let Some(inner) = &self.inner {
            inner
                .shared
                .if_enabled(|buf| buf.record_encodable_event(&event));
        }
    }

    /// Record an event that is only built when recording is on.
    ///
    /// Reach for this over [`record_event`](Self::record_event) when building
    /// the event costs something you would rather not pay while recording is
    /// paused, such as a clock read or a lookup. `make` runs only if the event
    /// will be recorded.
    pub fn record_event_with<E: Encodable>(&self, make: impl FnOnce() -> E) {
        if let Some(inner) = &self.inner {
            inner
                .shared
                .if_enabled(|buf| buf.record_encodable_event(&make()));
        }
    }

    /// Run a closure with direct access to the thread-local encoder.
    ///
    /// The closure is only invoked if telemetry is enabled.
    /// No-op on a disabled handle or when recording is paused.
    #[doc(hidden)]
    pub fn with_encoder(&self, f: impl FnOnce(&mut ThreadLocalEncoder<'_>)) {
        if let Some(inner) = &self.inner {
            inner.shared.if_enabled(|buf| buf.with_encoder(f));
        }
    }
}

/// Install `handle` as the current thread's [`Dial9Handle`].
///
/// Runtime integrations call this from their thread-start hook (e.g. tokio's
/// `on_thread_start`) so that [`current_handle`] / [`Dial9Handle::current`]
/// return the live handle on worker threads.
pub fn set_tl_handle(handle: Dial9Handle) {
    CURRENT_HANDLE.with(|cell| *cell.borrow_mut() = Some(handle));
}

/// Clear the current thread's [`Dial9Handle`], installed by [`set_tl_handle`].
///
/// Runtime integrations call this from their thread-stop hook.
pub fn clear_tl_handle() {
    CURRENT_HANDLE.with(|cell| *cell.borrow_mut() = None);
}

/// Return the [`Dial9Handle`] for the current thread, or an inert handle if
/// no dial9 runtime has claimed it. Equivalent to [`Dial9Handle::current`].
pub fn current_handle() -> Dial9Handle {
    Dial9Handle::current()
}

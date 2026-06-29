use crate::buffer::{Encodable, ThreadLocalEncoder};
use crate::primitives::sync::Arc;
use crate::shared_state::SharedState;
use std::cell::RefCell;

crate::primitives::thread_local! {
    /// Per-thread [`Dial9Handle`], populated via [`set_tl_handle`] and cleared
    /// via [`clear_tl_handle`] (from a runtime's thread-start/stop hooks).
    /// Backs [`Dial9Handle::current`] and [`current_handle`].
    static CURRENT_HANDLE: RefCell<Option<Dial9Handle>> = const { RefCell::new(None) };
}

/// Commands sent to the flush thread by [`CoreSession`](crate::session::CoreSession).
pub(crate) enum ControlCommand {
    /// Flush, finalize (seal segment), then exit the thread.
    FinalizeAndStop(crate::primitives::sync::mpsc::SyncSender<()>),
}

/// Cheap, cloneable handle for recording events and controlling telemetry.
///
/// A handle may be in one of two modes:
///
/// - **Enabled** — backed by a real telemetry session; methods record
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
    /// [`CoreSession::start`](crate::session::CoreSession::start) mints the channel
    /// and owns the matching receiver.
    pub(crate) fn enabled(
        shared: Arc<SharedState>,
        control_tx: crate::primitives::sync::mpsc::SyncSender<ControlCommand>,
    ) -> Self {
        Self {
            inner: Some(HandleInner { shared, control_tx }),
        }
    }

    /// Return an inert handle that is not connected to any telemetry
    /// session. All methods are no-ops.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Whether this handle is connected to a live telemetry session.
    ///
    /// Returns `false` for handles obtained via
    /// [`Dial9Handle::disabled`], and for handles returned by
    /// [`Dial9Handle::current`] when called from a thread that is
    /// not owned by a dial9 runtime.
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Access this handle's [`SharedState`].
    ///
    /// You can use it to subscribe new sources via [`push_source`](SharedState::push_source)
    pub fn shared(&self) -> Option<&Arc<SharedState>> {
        self.inner.as_ref().map(|i| &i.shared)
    }

    pub(crate) fn control_tx(
        &self,
    ) -> Option<&crate::primitives::sync::mpsc::SyncSender<ControlCommand>> {
        self.inner.as_ref().map(|i| &i.control_tx)
    }

    /// On-demand dump trigger for this runtime's telemetry session.
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

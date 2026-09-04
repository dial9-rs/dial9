//! On-trigger pipeline runs.
//!
//! By default the background worker processes sealed trace segments
//! continuously. Wiring a trigger flips the same pipeline into on-demand
//! operation: segments keep accumulating in the ring (memory or disk), and
//! the pipeline only runs when the application explicitly requests a dump.
//!
//! A runtime wires a trigger at build time and reaches it through the ambient
//! handle. See `with_dump_trigger` on the runtime builder for a worked example.
//!
//! [`DumpTrigger::dump_current_data`](crate::dump::DumpTrigger::dump_current_data) and
//! [`DumpTrigger::dump_time_range`](crate::dump::DumpTrigger::dump_time_range) build a
//! [`DumpRun`](crate::dump::DumpRun); the request is dispatched when that run is awaited
//! or dropped, whichever comes first (so `.with_metadata(...)` can mutate it before it
//! is sent). In the common temporary-statement form
//! (`trigger.dump_current_data();`) the run drops at the end of the statement and
//! dispatches right there; if you bind it to a variable, dispatch waits until that
//! binding is awaited or goes out of scope. Awaiting is optional and only retrieves the
//! [`DumpReceipt`](crate::dump::DumpReceipt). Window coverage is best-effort:
//! a window wider than what the ring retained captures whatever survived.
//! A current-data dump installed through the recorder also checkpoints the
//! active fragment and may return
//! [`DumpError::Checkpoint`](crate::dump::DumpError::Checkpoint) if it cannot
//! be drained and sealed.
//!
//! # Concurrent dumps
//!
//! Dumps accepted by the worker are independent: each has its own
//! [`DumpId`](crate::dump::DumpId), and a segment whose span overlaps two
//! windows is captured by both. Recorder-installed current-data dumps first
//! enter the flush thread's bounded, single-slot control queue to establish
//! their boundary. If that queue is busy, the new request resolves with
//! [`DumpError::Checkpoint`](crate::dump::DumpError::Checkpoint) wrapping
//! [`std::io::ErrorKind::WouldBlock`] instead of accumulating an unbounded
//! backlog. Only one eviction-protected current-data checkpoint may remain
//! active in the worker; a second resolves with the same busy error.
//!
//! When a single source fires repeatedly (a watcher that re-trips every
//! poll, a hot path that dumps on every slow request), configure
//! [`DumpTriggerConfig::debounce`](crate::dump::DumpTriggerConfig::debounce) to
//! coalesce a burst into one dump: triggers within the debounce window after a
//! dump dispatched resolve
//! [`DumpError::Coalesced`](crate::dump::DumpError::Coalesced), naming the dump
//! they folded into instead of starting a new one. The gate lives on the
//! trigger stored on the recorder, so every
//! [`dump_trigger`](crate::handle::Dial9Handle::dump_trigger) clone shares it.
//! (A *cooldown* that rejects extra triggers outright, rather than folding
//! them, is a possible future addition.)

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{mpsc, oneshot};

use crate::fs::CheckpointGuard;
use crate::pipeline::ProcessErrorKind;

/// Mint a dump trigger + receiver pair. The builder wires the receiver into
/// the worker and stashes the trigger in the recorder so it can be reached via
/// [`Dial9Handle::dump_trigger`](crate::handle::Dial9Handle::dump_trigger).
pub fn channel() -> (DumpTrigger, DumpRx) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        DumpTrigger {
            tx,
            debounce: None,
            checkpoint_tx: None,
        },
        DumpRx { rx },
    )
}

/// On-demand dump configuration, passed to
/// `with_dump_trigger`.
///
/// Flips the worker from continuous processing into on-demand operation:
/// segments keep accumulating in the ring and the pipeline only runs when the
/// application requests a dump. Configure coalescing with
/// [`debounce`](Self::debounce); the resulting [`DumpTrigger`] is then reached
/// through any [`Dial9Handle`](crate::handle::Dial9Handle) for the runtime
/// via [`dump_trigger`](crate::handle::Dial9Handle::dump_trigger).
#[derive(Debug, Default, Clone)]
pub struct DumpTriggerConfig {
    debounce: Option<Duration>,
}

impl DumpTriggerConfig {
    /// Default trigger: on-demand dumps with no debounce.
    pub fn new() -> Self {
        Self::default()
    }

    /// Coalesce duplicate triggers within `window` into a single dump.
    ///
    /// The first trigger in a quiet period dispatches normally; any trigger
    /// arriving within `window` of that dispatch resolves
    /// [`DumpError::Coalesced`] (naming the dump it folded into) without
    /// starting a new dump. The gate is shared by every
    /// [`dump_trigger`](crate::handle::Dial9Handle::dump_trigger) clone,
    /// so the effective rate is at most one dump per `window` across all
    /// callers.
    pub fn debounce(&mut self, window: Duration) {
        self.debounce = Some(window);
    }

    pub fn debounce_window(&self) -> Option<Duration> {
        self.debounce
    }
}

/// Identifier minted for each dump request.
///
/// A ULID: time-sortable, encoded as Crockford base32 in its `Display`
/// form. Surfaces as `dump-id` user metadata on each S3 object the dump
/// produces and names the dump's manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DumpId(ulid::Ulid);

impl DumpId {
    pub(crate) fn new() -> Self {
        Self(ulid::Ulid::new())
    }

    /// The instant the dump was triggered, embedded in the id.
    pub fn timestamp(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(self.0.timestamp_ms())
    }
}

impl std::fmt::Display for DumpId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for DumpId {
    type Err = ulid::DecodeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

/// How far back a dump looks from its trigger time.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Lookback {
    /// Everything the ring still holds (`dump_current_data`).
    Unbounded,
    /// Segments with creation epoch `>= trigger - window`.
    Window(Duration),
}

/// A dump request as it travels over the trigger channel to the worker.
#[derive(Debug)]
pub(crate) struct DumpRequest {
    pub(crate) id: DumpId,
    pub(crate) triggered_at: SystemTime,
    pub(crate) lookback: Lookback,
    pub(crate) lookforward: Duration,
    pub(crate) metadata: Vec<(String, String)>,
    /// Exact ring snapshot and its eviction-protection lease. `None` for
    /// ordinary time-range requests.
    pub(crate) checkpoint: Option<CheckpointGuard>,
    pub(crate) receipt_tx: oneshot::Sender<Result<DumpReceipt, DumpError>>,
}

/// Leading-edge debounce gate shared across [`DumpTrigger`] clones.
///
/// Records when the last request was accepted and the [`DumpId`] it was
/// given. Recorder checkpoints arm the gate only after reserving their exact
/// snapshot; other dumps arm it when the worker channel accepts them. A
/// request arriving within `window` of that instant coalesces into that id
/// rather than starting a new dump. The window is measured from the last
/// accepted request and is not extended by coalesced requests, so a burst all
/// folds into the first dump and the effective rate is at most one dump per
/// `window`.
#[derive(Debug)]
struct Debounce {
    window: Duration,
    last: Mutex<Option<(Instant, DumpId)>>,
}

impl Debounce {
    /// Undo an accepted request that failed before reaching the worker.
    ///
    /// A later accepted request may already have replaced this slot, so only
    /// clear it when it still names the request being rolled back.
    fn clear_if(&self, id: DumpId) {
        let mut last = self.last.lock().expect("debounce mutex poisoned");
        if last.is_some_and(|(_, accepted)| accepted == id) {
            *last = None;
        }
    }
}

/// Sending half of the trigger channel.
///
/// Cloneable; reach it from any thread owned by the runtime via
/// [`Dial9Handle::dump_trigger`](crate::handle::Dial9Handle::dump_trigger)
/// and hand it to whatever subsystem decides when to dump (an idle-ratio
/// watcher, a panic hook, a `/dump` HTTP handler, ...). Every clone shares the
/// debounce gate configured by [`DumpTriggerConfig::debounce`].
#[derive(Debug, Clone)]
pub struct DumpTrigger {
    tx: mpsc::UnboundedSender<DumpRequest>,
    /// `Some` once a debounce window is configured via
    /// [`DumpTriggerConfig::debounce`]; shared by reference so every clone honors one
    /// gate.
    debounce: Option<Arc<Debounce>>,
    /// Flush-thread control sender installed by `RecorderBuilder`. Manually
    /// constructed trigger/receiver pairs retain their historical behavior and
    /// dispatch directly to the worker.
    checkpoint_tx: Option<crate::primitives::sync::mpsc::SyncSender<crate::handle::ControlCommand>>,
}

impl DumpTrigger {
    pub(crate) fn bind_checkpoint(
        &mut self,
        checkpoint_tx: crate::primitives::sync::mpsc::SyncSender<crate::handle::ControlCommand>,
    ) {
        self.checkpoint_tx = Some(checkpoint_tx);
    }

    /// Coalesce duplicate triggers within `window` into a single dump.
    ///
    /// Applied once at build time from [`DumpTriggerConfig::debounce`]; every
    /// [`dump_trigger`](crate::handle::Dial9Handle::dump_trigger) clone
    /// then shares one gate. The first trigger in a quiet period dispatches
    /// normally; any trigger arriving within `window` of that dispatch resolves
    /// [`DumpError::Coalesced`] (naming the dump it folded into) without
    /// starting a new dump. Useful when a single source - a watcher that
    /// re-trips every poll, a hot path that dumps per slow request - would
    /// otherwise fire a burst of near-identical dumps.
    pub fn with_debounce(mut self, window: Duration) -> Self {
        self.debounce = Some(Arc::new(Debounce {
            window,
            last: Mutex::new(None),
        }));
        self
    }

    /// Capture everything the ring still holds, right now. No forward
    /// window. When this trigger was installed by
    /// [`RecorderBuilder::with_dump_trigger`](crate::recorder::RecorderBuilder::with_dump_trigger),
    /// the flush thread first drains pending events and seals the active
    /// fragment, so callers do not need to coordinate trace segments.
    ///
    /// If recording was already disabled, registered
    /// [`Source`](crate::source::Source)s are not flushed: a source flush may
    /// sample new state, which would move post-boundary data into the capture.
    /// Source-owned data buffered before [`Dial9Handle::disable`](crate::handle::Dial9Handle::disable)
    /// may therefore remain buffered until recording is enabled again. To end
    /// a bounded capture, prefer
    /// [`stop_recording_and_dump_current_data`](Self::stop_recording_and_dump_current_data),
    /// which takes the final source sample and closes the gate atomically on
    /// the flush thread.
    ///
    /// The flush-thread control queue and active checkpoint are bounded. If
    /// another control operation is queued or a protected checkpoint is still
    /// running, this request resolves with
    /// [`DumpError::Checkpoint`] wrapping [`std::io::ErrorKind::WouldBlock`].
    pub fn dump_current_data(&self) -> DumpRun<'_> {
        self.request(
            Lookback::Unbounded,
            Duration::ZERO,
            Checkpoint::CurrentData {
                stop_recording: false,
            },
            true,
        )
    }

    /// Stop recording and capture everything accumulated through that boundary.
    ///
    /// This is the preferred operation for ending a bounded capture.
    ///
    /// The recording gate is closed on the flush thread after one final source
    /// sample. In-flight event writers are synchronized with the subsequent
    /// thread-local drain before the active fragment is sealed and dispatched
    /// to the dump pipeline. The recorder remains alive and may be re-enabled
    /// after this future completes.
    ///
    /// Unlike ordinary dump requests, capture-stop requests are never
    /// debounced: closing the recording gate is a lifecycle operation, so it
    /// is always attempted. The flush-thread control queue remains bounded; if
    /// another control operation is already queued or a protected checkpoint
    /// is still running, this request resolves with [`DumpError::Checkpoint`]
    /// wrapping [`std::io::ErrorKind::WouldBlock`].
    ///
    /// This operation requires a trigger installed through
    /// `with_dump_trigger`; a standalone pair from [`channel`] has no recorder
    /// to stop and resolves with [`DumpError::Checkpoint`].
    pub fn stop_recording_and_dump_current_data(&self) -> DumpRun<'_> {
        self.request(
            Lookback::Unbounded,
            Duration::ZERO,
            Checkpoint::CurrentData {
                stop_recording: true,
            },
            false,
        )
    }

    /// Capture the window `[trigger - lookback, trigger + lookforward]`.
    /// Either side may be `Duration::ZERO`.
    ///
    /// `lookback` captures pre-trigger segments; you can look back only as
    /// far as the ring still retains, so a `lookback` wider than the
    /// retained history is best-effort and captures what survived.
    /// `lookforward` keeps the dump open until `trigger + lookforward`,
    /// attaching segments as they seal; it is bounded only by the process
    /// lifetime and is best-effort under upload pressure. The actual
    /// covered span is reported on [`DumpReceipt::time_range`]. This never
    /// errors and never resizes or pins the ring, so matching data may also be
    /// evicted after the request is made while the worker claims or processes
    /// it.
    pub fn dump_time_range(&self, lookback: Duration, lookforward: Duration) -> DumpRun<'_> {
        self.request(
            Lookback::Window(lookback),
            lookforward,
            Checkpoint::None,
            true,
        )
    }

    fn request(
        &self,
        lookback: Lookback,
        lookforward: Duration,
        checkpoint: Checkpoint,
        allow_debounce: bool,
    ) -> DumpRun<'_> {
        let id = DumpId::new();

        let (receipt_tx, receipt_rx) = oneshot::channel();
        DumpRun {
            request: Some(DumpRequest {
                id,
                triggered_at: SystemTime::now(),
                lookback,
                lookforward,
                metadata: Vec::new(),
                checkpoint: None,
                receipt_tx,
            }),
            tx: &self.tx,
            checkpoint_tx: self.checkpoint_tx.as_ref(),
            checkpoint,
            debounce: allow_debounce
                .then(|| self.debounce.as_ref().map(Arc::clone))
                .flatten(),
            receipt_rx: Some(receipt_rx),
        }
    }
}

/// Receiving half of the trigger channel; the builder wires it into the worker.
#[derive(Debug)]
pub struct DumpRx {
    pub(crate) rx: mpsc::UnboundedReceiver<DumpRequest>,
}

#[derive(Debug, Clone, Copy)]
enum Checkpoint {
    None,
    CurrentData { stop_recording: bool },
}

/// A current-data dump waiting for the flush thread to establish its boundary.
#[derive(Debug)]
pub(crate) struct CheckpointDump {
    request: DumpRequest,
    worker_tx: mpsc::UnboundedSender<DumpRequest>,
    pub(crate) stop_recording: bool,
    debounce: Option<Arc<Debounce>>,
}

impl CheckpointDump {
    fn new(
        request: DumpRequest,
        worker_tx: mpsc::UnboundedSender<DumpRequest>,
        stop_recording: bool,
        debounce: Option<Arc<Debounce>>,
    ) -> Self {
        Self {
            request,
            worker_tx,
            stop_recording,
            debounce,
        }
    }

    /// Return the accepted dump this request would fold into, if any.
    pub(crate) fn debounce_target(&self) -> Option<DumpId> {
        let debounce = self.debounce.as_ref()?;
        let now = Instant::now();
        let last = debounce.last.lock().expect("debounce mutex poisoned");
        match *last {
            Some((at, into)) if now.duration_since(at) < debounce.window => Some(into),
            _ => None,
        }
    }

    /// Recheck and arm debounce after the checkpoint reservation succeeds.
    pub(crate) fn accept_debounce(&self) -> Result<(), DumpId> {
        let Some(debounce) = &self.debounce else {
            return Ok(());
        };
        let now = Instant::now();
        let mut last = debounce.last.lock().expect("debounce mutex poisoned");
        match *last {
            Some((at, into)) if now.duration_since(at) < debounce.window => Err(into),
            _ => {
                *last = Some((now, self.request.id));
                Ok(())
            }
        }
    }

    pub(crate) fn dispatch(mut self, checkpoint: CheckpointGuard) {
        self.request.checkpoint = Some(checkpoint);
        if let Err(error) = self.worker_tx.send(self.request) {
            if let Some(debounce) = &self.debounce {
                debounce.clear_if(error.0.id);
            }
            let _ = error.0.receipt_tx.send(Err(DumpError::WorkerStopped));
        }
    }

    pub(crate) fn fail(self, error: std::io::Error) {
        if let Some(debounce) = &self.debounce {
            debounce.clear_if(self.request.id);
        }
        let _ = self
            .request
            .receipt_tx
            .send(Err(DumpError::Checkpoint(error)));
    }

    pub(crate) fn coalesce(self, into: DumpId) {
        let _ = self
            .request
            .receipt_tx
            .send(Err(DumpError::Coalesced { into }));
    }
}

/// In-flight dump request.
///
/// The dump is dispatched within the statement that created it: either
/// when this handle is awaited or when it is dropped, whichever comes
/// first. The handle is only needed to retrieve the [`DumpReceipt`];
/// dropping it does not cancel the dump. Chain
/// [`with_metadata`](Self::with_metadata) before awaiting to attach
/// correlation pairs.
#[derive(Debug)]
pub struct DumpRun<'a> {
    request: Option<DumpRequest>,
    tx: &'a mpsc::UnboundedSender<DumpRequest>,
    checkpoint_tx:
        Option<&'a crate::primitives::sync::mpsc::SyncSender<crate::handle::ControlCommand>>,
    checkpoint: Checkpoint,
    debounce: Option<Arc<Debounce>>,
    receipt_rx: Option<oneshot::Receiver<Result<DumpReceipt, DumpError>>>,
}

impl<'a> DumpRun<'a> {
    /// Attach a caller-supplied correlation pair. Chainable. Each pair is
    /// stamped onto every captured segment's metadata (namespaced as
    /// `dump.{key}`) before the pipeline runs; pipeline stages decide what
    /// to do with them (the S3 stage surfaces them as additional user
    /// metadata).
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        if let Some(req) = self.request.as_mut() {
            req.metadata.push((key.into(), value.into()));
        }
        self
    }

    /// Send the request over the trigger channel, once. Returns `false`
    /// when the worker is gone (channel closed).
    fn dispatch(&mut self) -> bool {
        match self.request.take() {
            Some(req) => {
                let Checkpoint::CurrentData { stop_recording } = self.checkpoint else {
                    return self.dispatch_direct(req);
                };
                let Some(checkpoint_tx) = self.checkpoint_tx else {
                    if stop_recording {
                        CheckpointDump::new(req, self.tx.clone(), true, None).fail(
                            std::io::Error::new(
                                std::io::ErrorKind::Unsupported,
                                "capture-stop requires a recorder-installed dump trigger",
                            ),
                        );
                        return true;
                    }
                    return self.dispatch_direct(req);
                };
                let command = crate::handle::ControlCommand::CheckpointDump(CheckpointDump::new(
                    req,
                    self.tx.clone(),
                    stop_recording,
                    self.debounce.take(),
                ));
                match checkpoint_tx.try_send(command) {
                    Ok(()) => true,
                    Err(crate::primitives::sync::mpsc::TrySendError::Full(command)) => {
                        let crate::handle::ControlCommand::CheckpointDump(checkpoint) = command
                        else {
                            unreachable!("submitted a checkpoint command");
                        };
                        checkpoint.fail(std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "dial9 flush thread is handling another control operation",
                        ));
                        true
                    }
                    Err(crate::primitives::sync::mpsc::TrySendError::Disconnected(command)) => {
                        let crate::handle::ControlCommand::CheckpointDump(checkpoint) = command
                        else {
                            unreachable!("submitted a checkpoint command");
                        };
                        checkpoint.fail(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "dial9 flush thread has stopped",
                        ));
                        true
                    }
                }
            }
            // Already dispatched.
            None => true,
        }
    }

    /// Dispatch a request that does not need a flush-thread reservation.
    /// The debounce mutex stays held across the non-blocking send so only a
    /// request actually accepted by the worker can arm the gate.
    fn dispatch_direct(&self, req: DumpRequest) -> bool {
        let Some(debounce) = &self.debounce else {
            return self.tx.send(req).is_ok();
        };
        let now = Instant::now();
        let mut last = debounce.last.lock().expect("debounce mutex poisoned");
        if let Some((at, into)) = *last
            && now.duration_since(at) < debounce.window
        {
            let _ = req.receipt_tx.send(Err(DumpError::Coalesced { into }));
            return true;
        }
        let id = req.id;
        if self.tx.send(req).is_err() {
            return false;
        }
        *last = Some((now, id));
        true
    }
}

impl Drop for DumpRun<'_> {
    fn drop(&mut self) {
        // Dispatch even when the caller never awaits. A closed channel
        // means the worker is gone; nothing to do.
        let _ = self.dispatch();
    }
}

impl<'a> IntoFuture for DumpRun<'a> {
    type Output = Result<DumpReceipt, DumpError>;
    type IntoFuture = DumpFuture;

    fn into_future(mut self) -> Self::IntoFuture {
        let sent = self.dispatch();
        let inner = match (sent, self.receipt_rx.take()) {
            (true, Some(rx)) => DumpFutureInner::Waiting(rx),
            _ => DumpFutureInner::Stopped,
        };
        DumpFuture { inner }
    }
}

/// Future resolving to the dump's [`DumpReceipt`].
///
/// For a look-back-only dump it resolves once the last captured segment
/// finishes the pipeline; for a dump with a look-forward it resolves after
/// the forward deadline elapses and the last in-window segment finishes.
#[derive(Debug)]
pub struct DumpFuture {
    inner: DumpFutureInner,
}

#[derive(Debug)]
enum DumpFutureInner {
    Waiting(oneshot::Receiver<Result<DumpReceipt, DumpError>>),
    Stopped,
}

impl Future for DumpFuture {
    type Output = Result<DumpReceipt, DumpError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.get_mut().inner;
        match inner {
            DumpFutureInner::Waiting(rx) => match Pin::new(rx).poll(cx) {
                Poll::Ready(Ok(result)) => Poll::Ready(result),
                // Worker exited without resolving the receipt.
                Poll::Ready(Err(_)) => Poll::Ready(Err(DumpError::WorkerStopped)),
                Poll::Pending => Poll::Pending,
            },
            DumpFutureInner::Stopped => Poll::Ready(Err(DumpError::WorkerStopped)),
        }
    }
}

/// Worker → pipeline-stage signal that a dump finished; passed to
/// [`SegmentProcessor::finalize_dump`](crate::pipeline::SegmentProcessor::finalize_dump)
/// so stages can flush per-dump state (the S3 stage writes the dump's
/// manifest from it).
#[derive(Debug)]
#[non_exhaustive]
pub struct DumpCompletion {
    /// Id of the dump that finished.
    pub dump_id: DumpId,
    /// When the dump was dispatched.
    pub triggered_at: SystemTime,
    /// Actual covered span (see [`DumpReceipt::time_range`]).
    pub time_range: (SystemTime, SystemTime),
    /// Count of segments that made it through the pipeline.
    pub segments_processed: usize,
    /// Caller correlation pairs from `with_metadata(...)`.
    pub metadata: Vec<(String, String)>,
    /// True when the dump resolves with [`DumpError::Pipeline`]: a captured
    /// segment failed terminally and nothing made it through. Stages still
    /// get to clear per-dump state, but should skip success artifacts (the
    /// S3 stage writes no manifest for a failed dump).
    pub failed: bool,
}

/// What a completed dump produced.
///
/// Best-effort: a dump where some matched segments make it through the
/// pipeline and others fail terminally still resolves `Ok`, with
/// [`segments_processed`](Self::segments_processed) counting only the
/// survivors (the failures are dropped silently, exactly like a segment the
/// ring evicted before the worker reached it). [`DumpError::Pipeline`] is
/// reserved for total failure: every captured segment failed and nothing
/// landed.
#[derive(Debug)]
#[non_exhaustive]
pub struct DumpReceipt {
    /// ULID minted when the dump was dispatched. Time-sortable; surfaces
    /// as `dump-id` user metadata on each S3 object.
    pub dump_id: DumpId,
    /// Count of segments that made it through the pipeline.
    pub segments_processed: usize,
    /// When the last segment finished the pipeline.
    pub finished_at: SystemTime,
    /// Actual covered span. May be shorter than the requested window on
    /// either side: look-back if the ring did not retain that much
    /// history, look-forward if the dump stopped before the deadline.
    pub time_range: (SystemTime, SystemTime),
    /// `Some({prefix}/dumps/{dump_id}.json)` when the pipeline ends at S3;
    /// `None` otherwise (no manifest is written off S3).
    pub manifest_key: Option<String>,
}

/// Why a dump failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum DumpError {
    /// The worker is shutting down or already stopped.
    WorkerStopped,
    /// The flush thread could not accept, drain, or seal the active trace
    /// fragment before dispatching the dump.
    Checkpoint(std::io::Error),
    /// Every captured segment failed in a pipeline stage.
    Pipeline(ProcessErrorKind),
    /// The trigger was coalesced into an in-flight dump by the debounce gate
    /// (see [`DumpTrigger::with_debounce`]). No new dump ran; `into` names the
    /// dump that covers this trigger.
    Coalesced {
        /// Id of the dump this trigger folded into.
        into: DumpId,
    },
}

impl std::fmt::Display for DumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkerStopped => write!(f, "worker is shutting down or already stopped"),
            Self::Checkpoint(error) => {
                write!(f, "could not checkpoint current trace data: {error}")
            }
            Self::Pipeline(kind) => write!(f, "pipeline stage failed: {kind}"),
            Self::Coalesced { into } => write!(f, "coalesced into dump {into}"),
        }
    }
}

impl std::error::Error for DumpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::WorkerStopped => None,
            Self::Checkpoint(error) => Some(error),
            Self::Pipeline(ProcessErrorKind::Io(e)) => Some(e),
            Self::Pipeline(ProcessErrorKind::Transfer { source, .. }) => Some(source.as_ref()),
            Self::Coalesced { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatches_on_drop_without_await() {
        let (trigger, mut rx) = channel();
        {
            let _run = trigger
                .dump_time_range(Duration::from_secs(300), Duration::from_secs(60))
                .with_metadata("reason", "test")
                .with_metadata("incident", "i-123");
            // Not awaited; dispatch happens on drop at end of scope.
        }
        let req = rx.rx.try_recv().expect("request dispatched on drop");
        assert!(matches!(req.lookback, Lookback::Window(d) if d == Duration::from_secs(300)));
        assert_eq!(req.lookforward, Duration::from_secs(60));
        assert_eq!(
            req.metadata,
            vec![
                ("reason".to_string(), "test".to_string()),
                ("incident".to_string(), "i-123".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn dump_current_data_is_unbounded_lookback() {
        let (trigger, mut rx) = channel();
        trigger.dump_current_data();
        let req = rx.rx.try_recv().expect("dispatched");
        assert!(matches!(req.lookback, Lookback::Unbounded));
        assert_eq!(req.lookforward, Duration::ZERO);
    }

    #[tokio::test]
    async fn awaiting_dispatches_exactly_once_and_resolves_receipt() {
        let (trigger, mut rx) = channel();
        let run = trigger.dump_current_data();

        let worker = tokio::spawn(async move {
            let req = rx.rx.recv().await.expect("one request");
            assert!(rx.rx.try_recv().is_err(), "no second dispatch");
            let receipt = DumpReceipt {
                dump_id: req.id,
                segments_processed: 3,
                finished_at: SystemTime::now(),
                time_range: (req.triggered_at, req.triggered_at),
                manifest_key: None,
            };
            let _ = req.receipt_tx.send(Ok(receipt));
        });

        let receipt = run.await.expect("receipt");
        assert_eq!(receipt.segments_processed, 3);
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn closed_channel_resolves_worker_stopped() {
        let (trigger, rx) = channel();
        drop(rx);
        let err = trigger.dump_current_data().await.unwrap_err();
        assert!(matches!(err, DumpError::WorkerStopped));
    }

    #[tokio::test]
    async fn standalone_trigger_cannot_stop_a_recorder() {
        let (trigger, _rx) = channel();
        let error = trigger
            .stop_recording_and_dump_current_data()
            .await
            .expect_err("standalone trigger has no recording gate");
        assert!(matches!(
            error,
            DumpError::Checkpoint(ref error)
                if error.kind() == std::io::ErrorKind::Unsupported
        ));
    }

    #[tokio::test]
    async fn dropped_receipt_sender_resolves_worker_stopped() {
        use std::future::IntoFuture;

        let (trigger, mut rx) = channel();
        let fut = trigger.dump_current_data().into_future();
        let req = rx.rx.try_recv().expect("dispatched at into_future");
        // Worker exiting without resolving the receipt drops `receipt_tx`.
        drop(req);
        let err = fut.await.unwrap_err();
        assert!(matches!(err, DumpError::WorkerStopped));
    }

    #[tokio::test]
    async fn debounce_coalesces_into_the_first_dump() {
        let (trigger, mut rx) = channel();
        let trigger = trigger.with_debounce(Duration::from_secs(60));

        // First trigger dispatches (drop dispatches the un-awaited run).
        let _ = trigger.dump_current_data();
        let first = rx.rx.try_recv().expect("first trigger dispatched");

        // Second trigger within the window folds into the first.
        let err = trigger.dump_current_data().await.unwrap_err();
        assert!(matches!(err, DumpError::Coalesced { into } if into == first.id));
        assert!(
            rx.rx.try_recv().is_err(),
            "coalesced trigger must not dispatch"
        );
    }

    #[tokio::test]
    async fn debounce_dispatches_again_after_window() {
        let (trigger, mut rx) = channel();
        let trigger = trigger.with_debounce(Duration::from_millis(30));

        let _ = trigger.dump_current_data();
        let first = rx.rx.try_recv().expect("first dispatched");

        tokio::time::sleep(Duration::from_millis(80)).await;

        let _ = trigger.dump_current_data();
        let second = rx.rx.try_recv().expect("dispatched again after window");
        assert_ne!(first.id, second.id, "post-window dump gets a fresh id");
    }

    #[tokio::test]
    async fn debounce_gate_is_shared_across_clones() {
        let (trigger, mut rx) = channel();
        let trigger = trigger.with_debounce(Duration::from_secs(60));
        let clone = trigger.clone();

        let _ = trigger.dump_current_data();
        let first = rx.rx.try_recv().expect("first dispatched");

        // A clone honors the same gate, so its trigger coalesces too.
        let err = clone.dump_current_data().await.unwrap_err();
        assert!(matches!(err, DumpError::Coalesced { into } if into == first.id));
    }

    #[tokio::test]
    async fn without_debounce_duplicate_triggers_both_dispatch() {
        let (trigger, mut rx) = channel();
        let _ = trigger.dump_current_data();
        let _ = trigger.dump_current_data();
        assert!(rx.rx.try_recv().is_ok(), "first dispatched");
        assert!(
            rx.rx.try_recv().is_ok(),
            "second dispatched (no coordination)"
        );
    }

    #[test]
    fn dump_id_is_time_sorted_and_timestamp_round_trips() {
        let before = SystemTime::now();
        let a = DumpId::new();
        std::thread::sleep(Duration::from_millis(2));
        let b = DumpId::new();
        let after = SystemTime::now();
        assert!(a < b);
        assert!(a.timestamp() >= before - Duration::from_millis(1));
        assert!(b.timestamp() <= after + Duration::from_millis(1));

        let parsed: DumpId = a.to_string().parse().expect("round-trip");
        assert_eq!(parsed, a);
    }
}

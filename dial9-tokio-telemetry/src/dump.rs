//! On-trigger pipeline runs.
//!
//! By default the background worker processes sealed trace segments
//! continuously. Wiring a trigger flips the same pipeline into on-demand
//! operation: segments keep accumulating in the ring (memory or disk), and
//! the pipeline only runs when the application explicitly requests a dump.
//!
//! ```ignore
//! use dial9_tokio_telemetry::dump;
//!
//! let (control, rx) = dump::trigger();
//!
//! let (runtime, _guard) = TracedRuntime::builder()
//!     .with_trace_path(path)
//!     .with_s3_uploader(s3_config)
//!     .with_trigger(rx)
//!     .build_and_start(builder, writer)?;
//!
//! // Hand `control` to whatever subsystem decides when to dump.
//! control.dump_current_data();
//! ```
//!
//! Both [`DumpControl::dump_current_data`](crate::dump::DumpControl::dump_current_data) and
//! [`DumpControl::dump_time_range`](crate::dump::DumpControl::dump_time_range) dispatch
//! immediately; awaiting the returned [`DumpRun`](crate::dump::DumpRun) is optional and
//! only retrieves the [`DumpReceipt`](crate::dump::DumpReceipt).
//! Dumps are strictly best-effort: a window wider than what the ring
//! retained captures whatever survived, with no error and no effect on the
//! live stream.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use tokio::sync::{mpsc, oneshot};

use crate::background_task::ProcessErrorKind;

/// Create a dump control + receiver pair; pass the receiver to
/// `with_trigger(rx)` on the runtime builder.
pub fn trigger() -> (DumpControl, DumpRx) {
    let (tx, rx) = mpsc::unbounded_channel();
    (DumpControl { tx }, DumpRx { rx })
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
    pub(crate) receipt_tx: oneshot::Sender<Result<DumpReceipt, DumpError>>,
}

/// Sending half of the trigger channel.
///
/// Cloneable; share it with whatever subsystem decides when to dump (an
/// idle-ratio watcher, a panic hook, a `/dump` HTTP handler, ...).
#[derive(Debug, Clone)]
pub struct DumpControl {
    tx: mpsc::UnboundedSender<DumpRequest>,
}

impl DumpControl {
    /// Capture everything the ring still holds, right now. No forward
    /// window.
    pub fn dump_current_data(&self) -> DumpRun<'_> {
        self.request(Lookback::Unbounded, Duration::ZERO)
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
    /// errors and never resizes or pins the ring.
    pub fn dump_time_range(&self, lookback: Duration, lookforward: Duration) -> DumpRun<'_> {
        self.request(Lookback::Window(lookback), lookforward)
    }

    fn request(&self, lookback: Lookback, lookforward: Duration) -> DumpRun<'_> {
        let (receipt_tx, receipt_rx) = oneshot::channel();
        DumpRun {
            request: Some(DumpRequest {
                id: DumpId::new(),
                triggered_at: SystemTime::now(),
                lookback,
                lookforward,
                metadata: Vec::new(),
                receipt_tx,
            }),
            tx: &self.tx,
            receipt_rx: Some(receipt_rx),
        }
    }
}

/// Receiving half of the trigger channel; pass to `with_trigger(rx)`.
#[derive(Debug)]
pub struct DumpRx {
    pub(crate) rx: mpsc::UnboundedReceiver<DumpRequest>,
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
    receipt_rx: Option<oneshot::Receiver<Result<DumpReceipt, DumpError>>>,
}

impl DumpRun<'_> {
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
            Some(req) => self.tx.send(req).is_ok(),
            // Already dispatched.
            None => true,
        }
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
        match &mut self.get_mut().inner {
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
/// [`SegmentProcessor::finalize_dump`](crate::background_task::SegmentProcessor::finalize_dump)
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
    /// Every captured segment failed in a pipeline stage.
    Pipeline(ProcessErrorKind),
}

impl std::fmt::Display for DumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkerStopped => write!(f, "worker is shutting down or already stopped"),
            Self::Pipeline(kind) => write!(f, "pipeline stage failed: {kind}"),
        }
    }
}

impl std::error::Error for DumpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::WorkerStopped => None,
            Self::Pipeline(ProcessErrorKind::Io(e)) => Some(e),
            Self::Pipeline(ProcessErrorKind::Transfer { source, .. }) => Some(source.as_ref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatches_on_drop_without_await() {
        let (control, mut rx) = trigger();
        {
            let _run = control
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
        let (control, mut rx) = trigger();
        control.dump_current_data();
        let req = rx.rx.try_recv().expect("dispatched");
        assert!(matches!(req.lookback, Lookback::Unbounded));
        assert_eq!(req.lookforward, Duration::ZERO);
    }

    #[tokio::test]
    async fn awaiting_dispatches_exactly_once_and_resolves_receipt() {
        let (control, mut rx) = trigger();
        let run = control.dump_current_data();

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
        let (control, rx) = trigger();
        drop(rx);
        let err = control.dump_current_data().await.unwrap_err();
        assert!(matches!(err, DumpError::WorkerStopped));
    }

    #[tokio::test]
    async fn dropped_receipt_sender_resolves_worker_stopped() {
        use std::future::IntoFuture;

        let (control, mut rx) = trigger();
        let fut = control.dump_current_data().into_future();
        let req = rx.rx.try_recv().expect("dispatched at into_future");
        // Worker exiting without resolving the receipt drops `receipt_tx`.
        drop(req);
        let err = fut.await.unwrap_err();
        assert!(matches!(err, DumpError::WorkerStopped));
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

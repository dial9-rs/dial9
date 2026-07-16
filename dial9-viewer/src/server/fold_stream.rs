//! Generic SSE fold-stream driver shared by `/api/flamegraph` and
//! `/api/span-stats`.
//!
//! Both endpoints hold one connection open, [resolve](refine::resolve) a scope,
//! emit the already-folded snapshot immediately, then fold the not-yet-folded
//! capped files one at a time, pushing a fresh full snapshot as each file lands
//! and closing when the work-list drains. That control flow — the `Start`/
//! `Folding` phase machine, the growing `folded` set, the [`FoldErrors`] tally,
//! and the crucial "insert into `folded` only after BOTH the GET and the merge
//! succeed" discipline (ADR-0003 folded-set semantics) — is identical between
//! the two endpoints. It lives here, once, so neither adapter can get the
//! invariant wrong.
//!
//! Each endpoint supplies a thin [`FoldSink`] adapter providing only the three
//! operations that actually differ: **seed** (prime the accumulator from the
//! already-folded parts), **fold_one** (fetch + merge one just-folded file), and
//! **snapshot_event** (shape the endpoint's response type into an SSE `Event`).
//! Both `seed` and `fold_one` return a [`PartOutcome`] rather than touching the
//! `folded` set themselves, so the driver — and only the driver — decides when a
//! leaf becomes folded.

use std::collections::HashSet;
use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;

use axum::response::sse::Event;
use futures::stream::{self, BoxStream, Stream, StreamExt};

use crate::ingest::aggregate::{AggContext, Coverage, FoldLimits};
use crate::ingest::refine::{self, FoldErrors, FoldOutcome, Folded, Resolved};

/// The outcome of attempting to merge one part-file into a sink's accumulator,
/// returned by [`FoldSink::seed`] (once per pre-folded leaf) and
/// [`FoldSink::fold_one`] (once per just-folded file). The driver turns this into
/// the `folded`-set / [`FoldErrors`] update, keeping the "folded only on success"
/// invariant in one place.
pub(crate) enum PartOutcome {
    /// Both the GET and the merge succeeded — the driver inserts `leaf` into the
    /// folded set (the ADR-0003 membership token).
    Folded { leaf: String },
    /// The GET or the merge failed — the driver records `(key, error)` into the
    /// running [`FoldErrors`] tally and the leaf stays unfolded. `key` is the
    /// endpoint-specific error key (a per-leaf hash, `"seed"`, or a raw object
    /// key) and `error` the endpoint-specific message; both are preserved
    /// verbatim because tests assert on them.
    Failed { key: String, error: String },
}

/// The three operations that vary between the flamegraph and span-stats streams.
/// Everything else (phase machine, folded-set discipline, error tally, coverage
/// construction, disconnect handling) is owned by [`drive`].
///
/// The async methods return `impl Future + Send` explicitly: the driver's
/// `unfold` future must be `Send` for axum's SSE response body, which in turn
/// requires the sink's futures to be `Send`.
pub(crate) trait FoldSink {
    /// Prime the accumulator from the already-folded parts of `resolved`. Fetches
    /// and merges each pre-folded leaf into the sink's own accumulator, returning
    /// one [`PartOutcome`] per leaf so the driver can build the initial folded set
    /// and error tally. Runs once, in the `Start` phase.
    fn seed(
        &mut self,
        agg: &AggContext,
        resolved: &Resolved,
    ) -> impl Future<Output = Vec<PartOutcome>> + Send;

    /// Fetch + merge ONE just-folded file into the accumulator, returning its
    /// [`PartOutcome`]. Runs once per [`FoldOutcome::Folded`] pulled off the fold
    /// stream.
    fn fold_one(
        &mut self,
        agg: &AggContext,
        folded: &Folded,
    ) -> impl Future<Output = PartOutcome> + Send;

    /// Shape the accumulator's current snapshot into one SSE `data:` event.
    ///
    /// The endpoint takes its own snapshot (from which its `samples_folded` scalar
    /// falls out — `total_samples` for flamegraph, `total_instances()` for
    /// span-stats), builds the shared [`Coverage`] via [`coverage_from`], and
    /// wraps its own response type. Only the `samples_folded` scalar and the
    /// response wrapping differ; everything feeding `coverage` is shared, so the
    /// sink is handed the driver-owned `folded` set and error tally directly.
    fn snapshot_event(
        &self,
        resolved: &Resolved,
        folded: &HashSet<String>,
        errors: &FoldErrors,
    ) -> Event;
}

/// Build the [`Coverage`] block from the resolved scope, the growing `folded`
/// set, the running error tally, and the endpoint's `samples_folded` scalar. The
/// only field that differs between endpoints is `samples_folded`; everything else
/// is derived identically, so this lives in the driver.
pub(crate) fn coverage_from(
    resolved: &Resolved,
    folded: &HashSet<String>,
    errors: &FoldErrors,
    samples_folded: usize,
) -> Coverage {
    Coverage {
        files_matched: resolved.files_matched,
        files_folded: resolved.files_folded_in(folded),
        samples_folded,
        total_bytes: resolved.total_bytes,
        hosts_matched: resolved.hosts_matched,
        hosts_folded: resolved.folded_hosts(folded),
        fold_errors: errors.count,
        fold_error_sample: errors.sample.clone(),
    }
}

/// Rate-limited warn for the per-file merge path (reachable once per folded file
/// on a large scope) and the per-event serialize path, so a systematic decode /
/// serialize failure can't spam the log. Shared by both sink adapters.
pub(crate) fn rate_limited_warn(msg: &str, err: &anyhow::Error) {
    use dial9_core::rate_limited;
    rate_limited!(std::time::Duration::from_secs(60), {
        tracing::warn!("{msg}: {err}");
    });
}

/// Phase of the SSE fold state machine. `Start` primes the accumulator over the
/// already-folded set and emits the first snapshot; `Folding` pulls one folded
/// file at a time, merges it, and emits a refined snapshot.
enum Phase {
    Start,
    Folding,
}

/// The mutable state threaded through the `unfold`: the immutable per-request
/// context (`agg` + `resolved`), the endpoint's `sink` (which owns the
/// accumulator), the growing folded-leaf set, the running fold-error tally, the
/// bounded fold stream, and the current phase.
struct Driver<S> {
    agg: Arc<AggContext>,
    resolved: Resolved,
    sink: S,
    /// The growing folded-leaf set. A leaf is inserted ONLY after both its GET
    /// and its merge succeed (ADR-0003 folded-set semantics) — enforced here, in
    /// the driver, so neither adapter can violate it.
    folded: HashSet<String>,
    /// Files whose fold failed this stream, and the most recent error message —
    /// surfaced in the coverage block so a systematic failure isn't silent.
    errors: FoldErrors,
    folds: BoxStream<'static, FoldOutcome>,
    phase: Phase,
}

impl<S: FoldSink> Driver<S> {
    /// Fold one [`PartOutcome`] into the folded set / error tally. This is the
    /// single place the "folded only on success" invariant is applied.
    fn apply(&mut self, outcome: PartOutcome) {
        match outcome {
            PartOutcome::Folded { leaf } => {
                self.folded.insert(leaf);
            }
            PartOutcome::Failed { key, error } => {
                self.errors.record(&key, &error);
            }
        }
    }

    /// Build one SSE event from the current accumulator snapshot + coverage.
    fn snapshot_event(&self) -> Event {
        self.sink
            .snapshot_event(&self.resolved, &self.folded, &self.errors)
    }
}

/// Drive one endpoint's SSE event stream, given its resolved scope and a
/// [`FoldSink`] adapter.
///
/// The first `unfold` step seeds the accumulator over the already-folded set and
/// emits an instant snapshot. Each later step pulls one file off
/// [`refine::fold_stream`], fetches + merges its part-files, and emits a refined
/// snapshot, closing when the work-list drains. Dropping the returned stream
/// (client disconnect) drops the fold stream, cancelling in-flight folds.
///
/// All arguments are owned, so the returned stream captures no borrows
/// (`use<S>`).
pub(crate) fn drive<S>(
    agg: AggContext,
    resolved: Resolved,
    limits: FoldLimits,
    sink: S,
) -> impl Stream<Item = Result<Event, Infallible>> + use<S>
where
    S: FoldSink + Send + 'static,
{
    let agg = Arc::new(agg);

    // `Box::pin` so the fold stream is `Unpin` and we can `.next()` it inside the
    // `unfold` step. Bounded concurrency comes from the shared `FoldLimits`.
    let folds: BoxStream<'static, FoldOutcome> =
        refine::fold_stream(Arc::clone(&agg), limits, resolved.unfolded_capped()).boxed();

    let driver = Driver {
        agg,
        resolved,
        sink,
        folded: HashSet::new(),
        errors: FoldErrors::default(),
        folds,
        phase: Phase::Start,
    };

    stream::unfold(driver, |mut d| async move {
        match d.phase {
            Phase::Start => {
                // Prime the accumulator over the already-folded set. Each returned
                // outcome becomes a folded-set insert (success) or an error record
                // (GET/merge failure) — the leaf is folded ONLY on success.
                let outcomes = d.sink.seed(&d.agg, &d.resolved).await;
                for outcome in outcomes {
                    d.apply(outcome);
                }
                let event = d.snapshot_event();
                d.phase = Phase::Folding;
                Some((Ok(event), d))
            }
            Phase::Folding => {
                // Pull the next fold outcome; `None` = work-list drained → close.
                match d.folds.next().await? {
                    FoldOutcome::Folded(f) => {
                        // Only mark the leaf folded after fetch AND merge succeed;
                        // failures increment errors. The sink does the fetch+merge
                        // and reports the outcome; the driver applies the rule.
                        let outcome = d.sink.fold_one(&d.agg, &f).await;
                        d.apply(outcome);
                    }
                    FoldOutcome::Failed { raw_key, error } => {
                        // Count it and carry a sample message so the client can see
                        // that folding is failing (e.g. unwritable output).
                        d.errors.record(&raw_key, &error);
                    }
                }
                let event = d.snapshot_event();
                Some((Ok(event), d))
            }
        }
    })
}

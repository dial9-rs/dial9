//! Generic SSE fold-stream driver shared by `/api/flamegraph` and
//! `/api/span-stats`.
//!
//! Both endpoints hold one connection open, [resolve](refine::resolve) a scope,
//! stream already-folded data in bounded cumulative snapshots, then fold the
//! not-yet-folded capped files one at a time, pushing a fresh full snapshot as
//! each file lands and closing when the work-list drains. That control flow —
//! the `Seeding`/`Folding` phase machine, the growing `folded` set, the
//! [`FoldErrors`] tally, and the crucial "insert into `folded` only after BOTH
//! the GET and the merge succeed" discipline (ADR-0003 folded-set semantics) —
//! is identical between the two endpoints. It lives here, once, so neither
//! adapter can get the invariant wrong.
//!
//! Each endpoint supplies a thin [`FoldSink`] adapter providing only the three
//! operations that actually differ: **seed_batch** (prime the accumulator from
//! a bounded slice of already-folded parts), **fold_one** (fetch + merge one
//! just-folded file), and **snapshot_event** (shape the endpoint's response type
//! into an SSE `Event`). Both merge operations return a [`PartOutcome`] rather
//! than touching the `folded` set themselves, so the driver — and only the
//! driver — decides when a leaf becomes folded.

use std::collections::HashSet;
use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;

use axum::response::sse::Event;
use futures::stream::{self, BoxStream, Stream, StreamExt};

use crate::ingest::aggregate::{AggContext, Coverage, FoldLimits};
use crate::ingest::refine::{self, FoldErrors, FoldOutcome, Folded, Resolved};
use crate::server::metrics::{SpanStatsPhaseDurations, SpanStatsStreamMetricsGuard};

/// The outcome of attempting to merge one part-file into a sink's accumulator,
/// returned by [`FoldSink::seed_batch`] (once per pre-folded leaf) and
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
    /// Number of cached part-files merged before the driver emits another seed
    /// snapshot. Endpoints may tune this for payload size and merge cost.
    const SEED_BATCH_SIZE: usize = 24;

    /// Runtime seed cadence. Most sinks use the associated default; sinks with
    /// lightweight response modes may emit a smaller first preview without
    /// penalizing throughput for the rest of the aggregation.
    fn seed_batch_size(&self, _first_batch: bool) -> usize {
        Self::SEED_BATCH_SIZE
    }

    /// Prime the accumulator from one bounded batch of already-folded parts.
    /// The driver repeatedly invokes this with stable sampling-order slices and
    /// emits an SSE snapshot after every batch, so deep cached refinements do
    /// not block time-to-first-data.
    fn seed_batch(
        &mut self,
        agg: &AggContext,
        full_keys: &[String],
    ) -> impl Future<Output = Vec<PartOutcome>> + Send;

    /// Fetch + merge ONE just-folded file into the accumulator, returning its
    /// [`PartOutcome`]. Runs once per [`FoldOutcome::Folded`] pulled off the fold
    /// stream.
    fn fold_one(
        &mut self,
        agg: &AggContext,
        folded: &Folded,
    ) -> impl Future<Output = PartOutcome> + Send;

    /// Drain endpoint-specific phase timings recorded by the preceding
    /// `seed_batch` or `fold_one` operation. The default keeps endpoints without
    /// a stream-lifetime breakdown (currently flamegraph) free of bookkeeping.
    fn take_phase_durations(&mut self) -> SpanStatsPhaseDurations {
        SpanStatsPhaseDurations::default()
    }

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
        files_folded: usize,
        folded_set_id: &str,
        target_folded_set_id: Option<&str>,
        hosts_folded: usize,
        errors: &FoldErrors,
    ) -> Event;
}

/// Build the [`Coverage`] block from the resolved scope, the growing `folded`
/// set, the running error tally, and the endpoint's `samples_folded` scalar. The
/// only field that differs between endpoints is `samples_folded`; everything else
/// is derived identically, so this lives in the driver.
pub(crate) fn coverage_from(
    resolved: &Resolved,
    files_folded: usize,
    folded_set_id: &str,
    target_folded_set_id: Option<&str>,
    hosts_folded: usize,
    errors: &FoldErrors,
    samples_folded: usize,
) -> Coverage {
    Coverage {
        files_matched: resolved.files_matched,
        files_folded,
        folded_set_id: Some(folded_set_id.to_string()),
        target_folded_set_id: target_folded_set_id.map(str::to_string),
        fold_work_cap: resolved.fold_work_cap(),
        samples_folded,
        total_bytes: resolved.total_bytes,
        hosts_matched: resolved.hosts_matched,
        hosts_folded,
        fold_errors: errors.count,
        fold_error_sample: errors.sample.clone(),
    }
}

fn folded_set_id(folded: &HashSet<String>) -> String {
    let mut leaves = folded.iter().collect::<Vec<_>>();
    leaves.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    for leaf in leaves {
        hasher.update(&(leaf.len() as u64).to_le_bytes());
        hasher.update(leaf.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
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

fn seed_batch_end(next: usize, len: usize, batch_size: usize) -> usize {
    next.saturating_add(batch_size.max(1)).min(len)
}

/// Phase of the SSE fold state machine. `Seeding` merges cached part-files in
/// bounded batches and emits after each batch; `Folding` pulls one newly folded
/// file at a time, merges it, and emits a refined snapshot.
enum Phase {
    Seeding,
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
    /// Successful matched merges and represented hosts, updated once per newly
    /// inserted leaf so snapshot coverage is O(1) in the matched-scope size.
    files_folded: usize,
    /// Split of `files_folded` by phase, for the stream-lifetime metric:
    /// `files_seeded` = leaves served from already-folded spans parts (seeding),
    /// `files_folded_cold` = leaves cold-folded from raw traces (folding). Their
    /// sum equals `files_folded`.
    files_seeded: usize,
    files_folded_cold: usize,
    folded_hosts: HashSet<String>,
    /// Optional stream-lifetime metric guard (span-stats only). Held for the life
    /// of the response body and updated as the stream seeds/folds, so the entry
    /// carries FINAL coverage and the full `stream_duration` when the stream is
    /// dropped (normal end or client disconnect).
    stream_metrics: Option<SpanStatsStreamMetricsGuard>,
    /// Files whose fold failed this stream, and the most recent error message —
    /// surfaced in the coverage block so a systematic failure isn't silent.
    errors: FoldErrors,
    /// Already-folded keys anywhere in the matched scope, consumed in bounded
    /// batches before new capped fold work. Keeping this separate from `folded`
    /// lets coverage report only cached parts actually fetched and merged.
    seed_keys: Vec<String>,
    seed_next: usize,
    target_folded_set_id: Option<String>,
    folds: BoxStream<'static, FoldOutcome>,
    phase: Phase,
}

impl<S: FoldSink> Driver<S> {
    /// Fold one [`PartOutcome`] into the folded set / error tally. This is the
    /// single place the "folded only on success" invariant is applied.
    fn apply(&mut self, outcome: PartOutcome) {
        match outcome {
            PartOutcome::Folded { leaf } => {
                if self.folded.insert(leaf.clone())
                    && let Some(host) = self.resolved.matched_host_for_leaf(&leaf)
                {
                    self.files_folded += 1;
                    // Split the count by the phase that produced it: seeding
                    // reads already-folded spans parts (GET + merge), folding
                    // cold-folds raw traces. Lets the stream metric show whether
                    // stream time went to re-reading Parquet or cold-folding.
                    match self.phase {
                        Phase::Seeding => self.files_seeded += 1,
                        Phase::Folding => self.files_folded_cold += 1,
                    }
                    self.folded_hosts.insert(host.to_string());
                }
            }
            PartOutcome::Failed { key, error } => {
                self.errors.record(&key, &error);
            }
        }
        self.update_stream_metrics();
    }

    /// Push the current coverage split onto the stream-lifetime metric guard, if
    /// one is armed. Called after every applied outcome so the guard carries the
    /// FINAL counts whenever the stream ends (drop). `files_matched` is fixed by
    /// the resolved scope; the folded counts grow as the stream progresses.
    fn update_stream_metrics(&mut self) {
        if let Some(guard) = self.stream_metrics.as_mut() {
            guard.files_matched = self.resolved.files_matched as u32;
            guard.files_folded = self.files_folded as u32;
            guard.files_seeded = self.files_seeded as u32;
            guard.files_folded_cold = self.files_folded_cold as u32;
            guard.failed = (self.errors.count > 0) as u32;
        }
    }

    /// Add the endpoint work recorded by the most recent sink operation to the
    /// stream-lifetime metric. Draining after each awaited operation preserves
    /// partial measurements if the client disconnects later in the stream.
    fn record_phase_durations(&mut self) {
        let phases = self.sink.take_phase_durations();
        if let Some(guard) = self.stream_metrics.as_mut() {
            guard.download_duration += phases.download;
            guard.parse_duration += phases.parse;
            guard.query_duration += phases.query;

            // Sub-phase breakdown (sum == parse_duration).
            guard.reader_setup_duration += phases.reader_setup;
            guard.batch_decode_duration += phases.batch_decode;
            guard.row_materialize_duration += phases.row_materialize;

            // Work counters.
            guard.parquet_bytes += phases.parquet_bytes;
            guard.record_batches_decoded += phases.record_batches_decoded;
            guard.rows_materialized += phases.rows_materialized;
            guard.attribute_entries += phases.attribute_entries;
        }
    }

    /// Build one SSE event from the current accumulator snapshot + coverage.
    fn snapshot_event(&self) -> Event {
        let folded_set_id = folded_set_id(&self.folded);
        self.sink.snapshot_event(
            &self.resolved,
            self.files_folded,
            &folded_set_id,
            self.target_folded_set_id.as_deref(),
            self.folded_hosts.len(),
            &self.errors,
        )
    }
}

/// Drive one endpoint's SSE event stream, given its resolved scope and a
/// [`FoldSink`] adapter.
///
/// Each `Seeding` step merges one bounded batch of already-folded parts and
/// emits a cumulative snapshot. Once cached state is exhausted, each `Folding`
/// step pulls one file off [`refine::fold_stream`], fetches + merges its
/// part-files, and emits a refined snapshot, closing when the work-list drains.
/// Dropping the returned stream (client disconnect) drops the fold stream,
/// cancelling in-flight folds.
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
    drive_with_options(agg, resolved, limits, sink, false, None)
}

/// Like [`drive`], but when `seed_only` is true the fold work-list is empty:
/// the stream seeds from already-folded part-files and closes, parsing NO
/// additional raw trace files. Used by the span-stats duration-band exemplar
/// refetch, which re-selects exemplars from spans already in Parquet and must
/// not trigger any new folding.
///
/// `stream_metrics` is an optional armed [`SpanStatsStreamMetricsGuard`] the
/// driver holds for the life of the returned stream and updates as it
/// seeds/folds. When the stream is dropped (normal end or client disconnect),
/// the guard emits the stream-lifetime entry with FINAL coverage and the full
/// wall-clock `stream_duration`.
pub(crate) fn drive_with_options<S>(
    agg: AggContext,
    resolved: Resolved,
    limits: FoldLimits,
    sink: S,
    seed_only: bool,
    stream_metrics: Option<SpanStatsStreamMetricsGuard>,
) -> impl Stream<Item = Result<Event, Infallible>> + use<S>
where
    S: FoldSink + Send + 'static,
{
    let agg = Arc::new(agg);

    // `Box::pin` so the fold stream is `Unpin` and we can `.next()` it inside the
    // `unfold` step. Bounded concurrency comes from the shared `FoldLimits`.
    // In seed-only mode the work-list is empty, so the `Folding` phase closes
    // immediately after seeding without folding any unfolded file.
    let fold_worklist = if seed_only {
        Vec::new()
    } else {
        resolved.unfolded_capped()
    };
    let folds: BoxStream<'static, FoldOutcome> =
        refine::fold_stream(Arc::clone(&agg), limits, fold_worklist).boxed();

    let seed_keys = resolved.folded_matching_full_keys();
    let target_folded_set_id = seed_only.then(|| {
        let target = seed_keys
            .iter()
            .map(|key| crate::ingest::aggregate::part_leaf_of(key))
            .collect::<HashSet<_>>();
        folded_set_id(&target)
    });
    let mut driver = Driver {
        agg,
        resolved,
        sink,
        folded: HashSet::new(),
        files_folded: 0,
        files_seeded: 0,
        files_folded_cold: 0,
        folded_hosts: HashSet::new(),
        errors: FoldErrors::default(),
        seed_keys,
        seed_next: 0,
        target_folded_set_id,
        folds,
        phase: Phase::Seeding,
        stream_metrics,
    };
    // Seed `files_matched` onto the guard up front so even a stream that folds
    // nothing (or disconnects before its first outcome) still reports the scope
    // size and a real `stream_duration` on drop.
    driver.update_stream_metrics();

    stream::unfold(driver, |mut d| async move {
        match d.phase {
            Phase::Seeding => {
                let first_batch = d.seed_next == 0;
                let batch_size = d.sink.seed_batch_size(first_batch);
                let end = seed_batch_end(d.seed_next, d.seed_keys.len(), batch_size);
                let keys = d.seed_keys[d.seed_next..end].to_vec();
                let outcomes = d.sink.seed_batch(&d.agg, &keys).await;
                d.record_phase_durations();
                for outcome in outcomes {
                    d.apply(outcome);
                }
                d.seed_next = end;
                let event = d.snapshot_event();
                if d.seed_next == d.seed_keys.len() {
                    d.phase = Phase::Folding;
                }
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
                        d.record_phase_durations();
                        d.apply(outcome);
                    }
                    FoldOutcome::Failed { raw_key, error } => {
                        // Count it and carry a sample message so the client can see
                        // that folding is failing (e.g. unwritable output).
                        d.errors.record(&raw_key, &error);
                        d.update_stream_metrics();
                    }
                }
                let event = d.snapshot_event();
                Some((Ok(event), d))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use axum::response::sse::Event;
    use futures::StreamExt;
    use metrique::ServiceMetrics;
    use metrique::test_util::{TestEntrySink, test_entry_sink};

    use super::{
        FoldSink, PartOutcome, coverage_from, drive_with_options, folded_set_id, seed_batch_end,
    };
    use crate::ingest::aggregate::{self, AggContext, FoldLimits};
    use crate::ingest::refine::{FoldErrors, Folded, Resolved};
    use crate::server::metrics::{SpanStatsPhaseDurations, SpanStatsStreamMetrics};
    use crate::storage::{LocalBackend, StorageBackend};

    #[test]
    fn folded_set_identity_tracks_membership_not_iteration_order() {
        let first = HashSet::from(["leaf-a".to_string(), "leaf-b".to_string()]);
        let reordered = HashSet::from(["leaf-b".to_string(), "leaf-a".to_string()]);
        let different = HashSet::from(["leaf-a".to_string(), "leaf-c".to_string()]);

        assert_eq!(folded_set_id(&first), folded_set_id(&reordered));
        assert_ne!(folded_set_id(&first), folded_set_id(&different));
    }

    #[test]
    fn cached_seed_batches_are_bounded_and_cover_all_keys() {
        let mut next = 0;
        let mut ranges = Vec::new();
        while next < 53 {
            let end = seed_batch_end(next, 53, 24);
            ranges.push(next..end);
            next = end;
        }

        assert_eq!(ranges, vec![0..24, 24..48, 48..53]);
    }

    #[test]
    fn cached_seed_batch_always_makes_progress() {
        assert_eq!(seed_batch_end(0, 2, 0), 1);
        assert_eq!(seed_batch_end(1, 2, 0), 2);
        assert_eq!(seed_batch_end(2, 2, 0), 2);
    }

    type RecordedSnapshot = (usize, usize, Option<String>);

    struct RecordingSink {
        batches: Arc<Mutex<Vec<Vec<String>>>>,
        snapshots: Arc<Mutex<Vec<RecordedSnapshot>>>,
        failed_key: String,
    }

    impl FoldSink for RecordingSink {
        const SEED_BATCH_SIZE: usize = 2;

        async fn seed_batch(
            &mut self,
            _agg: &AggContext,
            full_keys: &[String],
        ) -> Vec<PartOutcome> {
            self.batches.lock().unwrap().push(full_keys.to_vec());
            let mut outcomes: Vec<_> = full_keys
                .iter()
                .map(|full_key| {
                    if full_key == &self.failed_key {
                        PartOutcome::Failed {
                            key: full_key.clone(),
                            error: "injected merge failure".to_string(),
                        }
                    } else {
                        PartOutcome::Folded {
                            leaf: aggregate::part_leaf_of(full_key),
                        }
                    }
                })
                .collect();
            if let Some(PartOutcome::Folded { leaf }) = outcomes
                .iter()
                .find(|outcome| matches!(outcome, PartOutcome::Folded { .. }))
            {
                outcomes.push(PartOutcome::Folded { leaf: leaf.clone() });
            }
            outcomes.push(PartOutcome::Folded {
                leaf: "out-of-scope.parquet".to_string(),
            });
            outcomes
        }

        async fn fold_one(&mut self, _agg: &AggContext, _folded: &Folded) -> PartOutcome {
            panic!("all capped keys are already folded in this test")
        }

        fn snapshot_event(
            &self,
            resolved: &Resolved,
            files_folded: usize,
            folded_set_id: &str,
            target_folded_set_id: Option<&str>,
            hosts_folded: usize,
            errors: &FoldErrors,
        ) -> Event {
            let coverage = coverage_from(
                resolved,
                files_folded,
                folded_set_id,
                target_folded_set_id,
                hosts_folded,
                errors,
                files_folded,
            );
            self.snapshots.lock().unwrap().push((
                coverage.files_folded,
                coverage.fold_errors,
                coverage.target_folded_set_id,
            ));
            Event::default()
        }
    }

    #[tokio::test]
    async fn cached_seed_stream_emits_bounded_cumulative_snapshots() {
        let full_keys: Vec<String> = (0..5)
            .map(|index| format!("s3://bucket/2026-01-01/0000/svc/host-{index}/boot/{index}.bin"))
            .collect();
        let capped = full_keys
            .iter()
            .enumerate()
            .map(|(index, full)| (format!("raw-{index}"), full.clone()))
            .collect();
        let folded = full_keys
            .iter()
            .map(|full| aggregate::part_leaf_of(full))
            .collect::<HashSet<_>>();
        let expected_target = folded_set_id(&folded);
        let resolved = Resolved::for_test(capped, folded);

        let backend: Arc<dyn StorageBackend> = Arc::new(LocalBackend::new_temporary_aggregate());
        let agg = AggContext {
            source: Arc::clone(&backend),
            output: backend,
            source_bucket: String::new(),
            source_is_local: true,
            output_bucket: String::new(),
            output_prefix: "test".to_string(),
            source_prefixes: Vec::new(),
            segment_duration_secs: 60,
        };
        let batches = Arc::new(Mutex::new(Vec::new()));
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let sink = RecordingSink {
            batches: Arc::clone(&batches),
            snapshots: Arc::clone(&snapshots),
            failed_key: full_keys[1].clone(),
        };

        let events: Vec<_> =
            drive_with_options(agg, resolved, FoldLimits::new(1, 1, 1), sink, true, None)
                .collect()
                .await;

        assert_eq!(
            events.len(),
            3,
            "one event is emitted after each seed batch"
        );
        assert_eq!(
            batches
                .lock()
                .unwrap()
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            vec![2, 2, 1],
            "the cached scope is consumed in bounded stable batches"
        );
        assert_eq!(
            *snapshots.lock().unwrap(),
            vec![
                (1, 1, Some(expected_target.clone())),
                (3, 1, Some(expected_target.clone())),
                (4, 1, Some(expected_target)),
            ],
            "coverage grows cumulatively while retaining the exact final seed target"
        );
    }

    /// A sink that records whether `fold_one` (new fold work) is ever invoked.
    struct FoldCountingSink {
        fold_one_calls: Arc<Mutex<usize>>,
    }

    impl FoldSink for FoldCountingSink {
        async fn seed_batch(
            &mut self,
            _agg: &AggContext,
            full_keys: &[String],
        ) -> Vec<PartOutcome> {
            full_keys
                .iter()
                .map(|full_key| PartOutcome::Folded {
                    leaf: aggregate::part_leaf_of(full_key),
                })
                .collect()
        }

        async fn fold_one(&mut self, _agg: &AggContext, _folded: &Folded) -> PartOutcome {
            *self.fold_one_calls.lock().unwrap() += 1;
            PartOutcome::Failed {
                key: "unexpected".to_string(),
                error: "fold_one must not run in seed-only mode".to_string(),
            }
        }

        fn snapshot_event(
            &self,
            resolved: &Resolved,
            files_folded: usize,
            folded_set_id: &str,
            target_folded_set_id: Option<&str>,
            hosts_folded: usize,
            errors: &FoldErrors,
        ) -> Event {
            let _ = coverage_from(
                resolved,
                files_folded,
                folded_set_id,
                target_folded_set_id,
                hosts_folded,
                errors,
                files_folded,
            );
            Event::default()
        }
    }

    /// Seed-only mode must never fold an unfolded capped file — it reads only the
    /// already-folded parts. This backs the span-stats duration-band exemplar
    /// refetch, which must parse no additional raw traces.
    #[tokio::test]
    async fn seed_only_never_folds_unfolded_capped_files() {
        // One folded file (seeded) and one UNfolded capped file (would be folded
        // in normal mode). Seed-only must skip the latter entirely.
        let folded_full = "s3://bucket/2026-01-01/0000/svc/host-0/boot/0.bin".to_string();
        let unfolded_full = "s3://bucket/2026-01-01/0000/svc/host-1/boot/1.bin".to_string();
        let capped = vec![
            ("raw-0".to_string(), folded_full.clone()),
            ("raw-1".to_string(), unfolded_full.clone()),
        ];
        let folded: std::collections::HashSet<String> = [aggregate::part_leaf_of(&folded_full)]
            .into_iter()
            .collect();
        let resolved = Resolved::for_test(capped, folded);

        let backend: Arc<dyn StorageBackend> = Arc::new(LocalBackend::new_temporary_aggregate());
        let agg = AggContext {
            source: Arc::clone(&backend),
            output: backend,
            source_bucket: String::new(),
            source_is_local: true,
            output_bucket: String::new(),
            output_prefix: "test".to_string(),
            source_prefixes: Vec::new(),
            segment_duration_secs: 60,
        };
        let fold_one_calls = Arc::new(Mutex::new(0usize));
        let sink = FoldCountingSink {
            fold_one_calls: Arc::clone(&fold_one_calls),
        };

        let _events: Vec<_> =
            drive_with_options(agg, resolved, FoldLimits::new(1, 1, 1), sink, true, None)
                .collect()
                .await;

        assert_eq!(
            *fold_one_calls.lock().unwrap(),
            0,
            "seed-only mode must not fold the unfolded capped file"
        );
    }

    struct MetricsSink {
        pending: SpanStatsPhaseDurations,
    }

    impl FoldSink for MetricsSink {
        async fn seed_batch(
            &mut self,
            _agg: &AggContext,
            _full_keys: &[String],
        ) -> Vec<PartOutcome> {
            Vec::new()
        }

        async fn fold_one(&mut self, _agg: &AggContext, _folded: &Folded) -> PartOutcome {
            panic!("empty test scope has no fold work")
        }

        fn take_phase_durations(&mut self) -> SpanStatsPhaseDurations {
            std::mem::take(&mut self.pending)
        }

        fn snapshot_event(
            &self,
            _resolved: &Resolved,
            _files_folded: usize,
            _folded_set_id: &str,
            _target_folded_set_id: Option<&str>,
            _hosts_folded: usize,
            _errors: &FoldErrors,
        ) -> Event {
            Event::default()
        }
    }

    #[tokio::test]
    async fn sink_phases_reach_stream_metrics() {
        let TestEntrySink { inspector, sink } = test_entry_sink();
        let _guard = ServiceMetrics::set_test_sink_on_current_tokio_runtime(sink);

        let backend: Arc<dyn StorageBackend> = Arc::new(LocalBackend::new_temporary_aggregate());
        let agg = AggContext {
            source: Arc::clone(&backend),
            output: backend,
            source_bucket: String::new(),
            source_is_local: true,
            output_bucket: String::new(),
            output_prefix: "test".to_string(),
            source_prefixes: Vec::new(),
            segment_duration_secs: 60,
        };
        let phases = SpanStatsPhaseDurations {
            download: std::time::Duration::from_millis(1),
            parse: std::time::Duration::from_millis(9),
            query: std::time::Duration::from_millis(5),
            reader_setup: std::time::Duration::from_millis(2),
            batch_decode: std::time::Duration::from_millis(3),
            row_materialize: std::time::Duration::from_millis(4),
            parquet_bytes: 100,
            record_batches_decoded: 2,
            rows_materialized: 30,
            attribute_entries: 40,
        };
        let sink = MetricsSink { pending: phases };
        let resolved = Resolved::for_test(Vec::new(), std::collections::HashSet::new());
        let stream_metrics = SpanStatsStreamMetrics::arm("/api/span-stats");

        let _: Vec<_> = drive_with_options(
            agg,
            resolved,
            FoldLimits::new(1, 1, 1),
            sink,
            true,
            Some(stream_metrics),
        )
        .collect()
        .await;

        let entries = inspector.entries();
        assert_eq!(entries.len(), 1);
        let metrics = &entries[0].metrics;
        assert_eq!(metrics["download_duration"].as_f64(), 1.0);
        assert_eq!(metrics["parse_duration"].as_f64(), 9.0);
        assert_eq!(metrics["query_duration"].as_f64(), 5.0);
        assert_eq!(metrics["reader_setup_duration"].as_f64(), 2.0);
        assert_eq!(metrics["batch_decode_duration"].as_f64(), 3.0);
        assert_eq!(metrics["row_materialize_duration"].as_f64(), 4.0);
        assert_eq!(metrics["parquet_bytes"].as_u64(), 100);
        assert_eq!(metrics["record_batches_decoded"].as_u64(), 2);
        assert_eq!(metrics["rows_materialized"].as_u64(), 30);
        assert_eq!(metrics["attribute_entries"].as_u64(), 40);
    }
}

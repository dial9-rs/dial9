//! `/api/tokio-stats` endpoint: stream aggregated polls Parquet data as
//! Server-Sent Events — long polls classified as on-CPU vs off-CPU, grouped by
//! spawn location, refining as source files fold (same engine as flamegraph).
//!
//! One request holds the connection open: it [resolves](refine::resolve) the
//! scope, emits the already-folded snapshot, then folds up to the sampling cap
//! and pushes a fresh [`TokioStatsResponse`] SSE event as each file lands.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Extension;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum_extra::extract::Query as QueryExtra;
use futures::stream::{self, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::ingest::aggregate::{self, Scope};
use crate::ingest::refine::{self, FoldErrors, FoldOutcome, RefineOpts, Resolved};
use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::server::metrics::OperationMetrics;

use arrow::array::Array;

/// Floor: only send polls longer than this to the client (saves bandwidth).
const DURATION_FLOOR_NS: i64 = 100_000; // 100µs

/// Number of longest polls shipped to the client. Ported from IRIS's top-100.
const LONG_POLL_TOP: usize = 100;
/// Compact the long-poll buffer once it grows past this, keeping the top
/// [`LONG_POLL_TOP`]. The 4× headroom mirrors IRIS's `analyze.rs` (buffer 400,
/// truncate to 100) — it amortizes the sort while keeping memory bounded
/// regardless of scope size.
const LONG_POLL_SOFT_CAP: usize = LONG_POLL_TOP * 4;

#[derive(Deserialize)]
pub struct TokioStatsParams {
    pub bucket: Option<String>,
    pub prefix: Option<String>,
    pub service: Option<String>,
    #[serde(default)]
    pub host: Vec<String>,
    pub start_ns: Option<i64>,
    pub end_ns: Option<i64>,
    /// "Load more": raise the absolute sampling-cap ceiling for this scope.
    /// Clamped server-side to a hard ceiling (see `sampling_cap`), so a crafted
    /// request can't drive an unbounded fold.
    pub max_files: Option<usize>,
}

#[derive(Serialize)]
pub struct TokioStatsResponse {
    /// Time span covered by the data (ns), for computing per-minute rates.
    pub time_span_ns: i64,
    pub total_polls: u64,
    /// Source bucket (for constructing viewer deep links in the UI).
    pub bucket: String,
    pub by_spawn_loc: Vec<SpawnLocStats>,
    /// Longest individual polls across the whole scope, ranked by duration — the
    /// single futures that held a worker thread longest in one poll (these starve
    /// other tasks on that worker). Bounded server-side to the top
    /// [`LONG_POLL_TOP`]: `task_id` is high-cardinality, so this is a reduction,
    /// not a per-poll axis shipped to the client. Each row carries the
    /// coordinates to deep-link its trace segment. Ported from IRIS's
    /// `longPolls.top` (rust-ingest `analyze.rs`).
    pub top_long_polls: Vec<LongPoll>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<aggregate::Coverage>,
}

#[derive(Serialize)]
pub struct SpawnLocStats {
    pub spawn_loc: String,
    pub total_polls: u64,
    /// All poll durations above 100µs floor, sorted descending.
    /// Client filters by threshold locally for instant re-render.
    pub durations_ns: Vec<i64>,
    /// Classification per duration: 0=off-cpu, 1=on-cpu, 2=mixed, 3=unknown. Same index as durations_ns.
    pub classes: Vec<u8>,
    /// Worst exemplar per class for deep-linking. Index: 0=off_cpu, 1=on_cpu, 2=mixed, 3=unknown.
    pub exemplars: [Option<PollExemplar>; 4],
}

#[derive(Serialize, Clone)]
pub struct PollExemplar {
    pub start_ns: i64,
    pub end_ns: i64,
    pub duration_ns: i64,
    pub host: String,
    /// Source trace file key for constructing the viewer deep link.
    pub source_key: String,
}

/// One long-running poll, for the top-N "Longest polls" list. Ported from IRIS's
/// `longPolls.top` — its `(durMs, worker, taskId, spawnLoc, startMs)` tuple, plus
/// the `host` + `source_key` dial9 needs to deep-link the poll into the viewer.
#[derive(Serialize, Clone)]
pub struct LongPoll {
    pub duration_ns: i64,
    pub worker_id: u32,
    pub task_id: u64,
    /// Where the future was spawned; `None` when the trace didn't record it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_loc: Option<String>,
    pub start_ns: i64,
    pub end_ns: i64,
    pub host: String,
    /// Source trace file key for constructing the viewer deep link.
    pub source_key: String,
}

/// Handler for GET /api/tokio-stats — a Server-Sent Events stream.
///
/// [Resolves](refine::resolve) the scope, reads the already-folded `polls/`
/// part-files into an accumulator, and emits an initial snapshot. Then it folds
/// the not-yet-folded capped files (up to the sampling cap), reading + merging
/// each file's polls as it lands and emitting a fresh [`TokioStatsResponse`]
/// event, closing when the work-list drains. `max_files` ("Load more") raises
/// the sampling-cap ceiling, so a reopened stream folds deeper.
pub async fn get_tokio_stats(
    State(state): State<AppState>,
    creds: MaybeCreds,
    QueryExtra(params): QueryExtra<TokioStatsParams>,
) -> Result<
    (
        Extension<OperationMetrics>,
        Sse<impl Stream<Item = Result<Event, Infallible>>>,
    ),
    (StatusCode, String),
> {
    let Some(agg) = state
        .agg_context_for(params.bucket.as_deref(), params.prefix.as_deref(), creds)
        .await?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            "tokio-stats requires aggregation (start with --agg or supply a bucket)".to_string(),
        ));
    };

    let scope = Scope {
        start_ns: params.start_ns,
        end_ns: params.end_ns,
        service: params.service.clone(),
        hosts: params.host.clone(),
    };

    tracing::debug!(
        source_bucket = %agg.source_bucket,
        source_prefixes = ?agg.source_prefixes,
        service = scope.service.as_deref().unwrap_or("(all)"),
        hosts = ?scope.hosts,
        start_ns = ?scope.start_ns,
        end_ns = ?scope.end_ns,
        "tokio-stats: starting"
    );

    // Resolve up front so an empty scope maps to 404 rather than an empty stream.
    // "Load more" raises the sampling cap, so a reopened stream folds deeper
    // into the matched set (the already-folded prefix is served instantly).
    let opts = RefineOpts {
        max_files: params.max_files,
    };
    let Some(resolved) = refine::resolve(&agg, &scope, opts).await else {
        return Err((
            StatusCode::NOT_FOUND,
            "no source files match this scope".to_string(),
        ));
    };

    // Operation-specific metrics, attached at response-head time — all the
    // middleware can see for a streamed body (folding happens after the headers
    // go out). Coverage is therefore the RESOLVE-TIME snapshot; the notable-poll
    // count is not known until polls part-files are read inside the stream, so
    // it is reported as absent rather than a misleading zero.
    let op = OperationMetrics::tokio_stats(
        resolved.files_matched as u32,
        resolved.files_folded_in(resolved.folded()) as u32,
        None,
    );

    let stream = tokio_stats_stream(agg, resolved, scope, state.fold_limits.clone());
    Ok((
        Extension(op),
        Sse::new(stream).keep_alive(KeepAlive::default()),
    ))
}

/// Immutable per-request context threaded through the tokio-stats SSE stream.
struct StreamCtx {
    agg: Arc<crate::ingest::aggregate::AggContext>,
    resolved: Resolved,
    scope: Scope,
    source_bucket: String,
}

/// Phase of the tokio-stats SSE state machine (see [`crate::server::flamegraph`]
/// for the mirror-image flamegraph version). `Start` reads + accumulates the
/// already-folded polls; `Folding` pulls one folded file at a time.
enum Phase {
    Start,
    Folding {
        acc: TokioStatsAccum,
        folded: HashSet<String>,
        errors: FoldErrors,
    },
}

/// Build the SSE event stream for one tokio-stats request. Mirrors
/// [`crate::server::flamegraph`]'s `flamegraph_stream`, but reads `polls/`
/// part-files into a [`TokioStatsAccum`] instead of samples.
fn tokio_stats_stream(
    agg: crate::ingest::aggregate::AggContext,
    resolved: Resolved,
    scope: Scope,
    limits: aggregate::FoldLimits,
) -> impl Stream<Item = Result<Event, Infallible>> + use<> {
    let agg = Arc::new(agg);
    let ctx = Arc::new(StreamCtx {
        agg: Arc::clone(&agg),
        source_bucket: agg.source_bucket.clone(),
        scope,
        resolved,
    });

    let folds = Box::pin(refine::fold_stream(
        agg,
        limits,
        ctx.resolved.unfolded_capped(),
    ));

    stream::unfold(
        (ctx, folds, Phase::Start),
        |(ctx, mut folds, phase)| async move {
            match phase {
                Phase::Start => {
                    // Read the already-folded polls part-files concurrently.
                    let polls_data = aggregate::read_polls_parts(
                        &*ctx.agg.output,
                        &ctx.agg.output_bucket,
                        &ctx.agg.output_prefix,
                        &ctx.resolved.capped,
                        ctx.resolved.folded(),
                    )
                    .await;
                    let mut acc = TokioStatsAccum::default();
                    for (raw_key, data) in &polls_data {
                        read_polls_part_lossy(data, &ctx.scope, raw_key, &mut acc);
                    }
                    let folded = ctx.resolved.folded().clone();
                    let errors = FoldErrors::default();
                    let event = snapshot_event(&ctx, &acc, &folded, &errors);
                    Some((
                        Ok(event),
                        (
                            ctx,
                            folds,
                            Phase::Folding {
                                acc,
                                folded,
                                errors,
                            },
                        ),
                    ))
                }
                Phase::Folding {
                    mut acc,
                    mut folded,
                    mut errors,
                } => {
                    match folds.next().await? {
                        FoldOutcome::Folded(f) => {
                            if let Some(data) = aggregate::fetch_polls_part(
                                &*ctx.agg.output,
                                &ctx.agg.output_bucket,
                                &ctx.agg.output_prefix,
                                &f.full_key,
                            )
                            .await
                            {
                                read_polls_part_lossy(&data, &ctx.scope, &f.raw_key, &mut acc);
                            }
                            folded.insert(aggregate::part_leaf_of(&f.full_key));
                        }
                        FoldOutcome::Failed { raw_key, error } => {
                            errors.record(&raw_key, &error);
                        }
                    }
                    let event = snapshot_event(&ctx, &acc, &folded, &errors);
                    Some((
                        Ok(event),
                        (
                            ctx,
                            folds,
                            Phase::Folding {
                                acc,
                                folded,
                                errors,
                            },
                        ),
                    ))
                }
            }
        },
    )
}

/// Merge one polls part-file into `acc`, logging (rate-limited) and skipping on a
/// decode error rather than aborting the whole stream — one corrupt part-file
/// shouldn't kill an otherwise-good refinement.
fn read_polls_part_lossy(data: &[u8], scope: &Scope, source_key: &str, acc: &mut TokioStatsAccum) {
    use dial9_core::rate_limited;
    if let Err((_, e)) = read_polls_part(data, scope, source_key, acc) {
        rate_limited!(std::time::Duration::from_secs(60), {
            tracing::warn!(key = %source_key, error = %e, "tokio-stats: failed to read polls part");
        });
    }
}

/// Build one SSE event from the accumulator's current state and the coverage
/// implied by `folded` (a growing superset of the resolved folded set) plus the
/// running fold-error tally. Snapshots without consuming the accumulator so the
/// stream can keep folding.
fn snapshot_event(
    ctx: &StreamCtx,
    acc: &TokioStatsAccum,
    folded: &HashSet<String>,
    errors: &FoldErrors,
) -> Event {
    let time_span_ns = match (acc.min_ts, acc.max_ts) {
        (Some(min), Some(max)) => (max - min).max(1),
        _ => 1,
    };

    // Per-spawn-loc with durations sorted descending. Built from borrowed state
    // (clone the per-loc vecs) so repeated snapshots don't consume the accumulator.
    let mut by_spawn_loc: Vec<SpawnLocStats> = acc
        .by_loc
        .iter()
        .map(|(loc, la)| {
            let mut durs = la.durations.clone();
            durs.sort_unstable_by_key(|&(d, _)| std::cmp::Reverse(d));
            let durations_ns = durs.iter().map(|(d, _)| *d).collect();
            let classes = durs.iter().map(|(_, c)| *c).collect();
            SpawnLocStats {
                spawn_loc: loc.clone(),
                total_polls: la.total,
                durations_ns,
                classes,
                exemplars: la.worst_by_class.clone(),
            }
        })
        .collect();
    by_spawn_loc.sort_by_key(|l| std::cmp::Reverse(l.durations_ns.len()));

    let files_folded = ctx.resolved.files_folded_in(folded);
    let resp = TokioStatsResponse {
        time_span_ns,
        total_polls: acc.total_polls,
        bucket: ctx.source_bucket.clone(),
        by_spawn_loc,
        top_long_polls: acc.top_long_polls(),
        coverage: Some(aggregate::Coverage {
            files_matched: ctx.resolved.files_matched,
            files_folded,
            // tokio-stats counts folded files, not samples, as its "folded" unit.
            samples_folded: files_folded,
            total_bytes: ctx.resolved.total_bytes,
            hosts_matched: ctx.resolved.hosts_matched,
            hosts_folded: ctx.resolved.folded_hosts(folded),
            fold_errors: errors.count,
            fold_error_sample: errors.sample.clone(),
        }),
    };
    Event::default().json_data(&resp).unwrap_or_else(|e| {
        use dial9_core::rate_limited;
        rate_limited!(std::time::Duration::from_secs(60), {
            tracing::warn!(error = %e, "tokio-stats: event serialize failed");
        });
        Event::default().comment("serialize error")
    })
}

// ─── Internal types ──────────────────────────────────────────────────────────

#[derive(Default)]
struct TokioStatsAccum {
    total_polls: u64,
    min_ts: Option<i64>,
    max_ts: Option<i64>,
    by_loc: HashMap<String, LocAccum>,
    /// Longest polls seen so far, kept bounded by [`TokioStatsAccum::push_long_poll`].
    /// Unsorted between compactions; [`TokioStatsAccum::top_long_polls`] produces
    /// the final ranked list.
    long_polls: Vec<LongPoll>,
}

impl TokioStatsAccum {
    /// Record a candidate long poll, compacting to the top [`LONG_POLL_TOP`] by
    /// duration whenever the buffer exceeds the soft cap. Mirrors IRIS's
    /// `analyze.rs` grow-then-truncate strategy so the buffer stays bounded no
    /// matter how many files fold in.
    fn push_long_poll(&mut self, poll: LongPoll) {
        self.long_polls.push(poll);
        if self.long_polls.len() > LONG_POLL_SOFT_CAP {
            self.long_polls
                .sort_unstable_by_key(|p| std::cmp::Reverse(p.duration_ns));
            self.long_polls.truncate(LONG_POLL_TOP);
        }
    }

    /// The final ranked top-N longest polls (descending by duration). Clones so
    /// repeated SSE snapshots don't consume the still-growing accumulator.
    fn top_long_polls(&self) -> Vec<LongPoll> {
        let mut top = self.long_polls.clone();
        top.sort_unstable_by_key(|p| std::cmp::Reverse(p.duration_ns));
        top.truncate(LONG_POLL_TOP);
        top
    }
}

struct LocAccum {
    total: u64,
    durations: Vec<(i64, u8)>, // (duration_ns, class)
    /// Worst exemplar per class: index 0=off_cpu, 1=on_cpu, 2=mixed, 3=unknown
    worst_by_class: [Option<PollExemplar>; 4],
}

/// Minimum duration (ns) to confidently classify a poll as off-CPU.
/// Below this, 0 CPU samples is statistically expected (at 99Hz, a 10ms poll
/// has only ~63% chance of a sample). Above this threshold, 0 samples strongly
/// indicates the poll was blocked off-CPU.
const OFF_CPU_CONFIDENCE_NS: i64 = 10_000_000; // 10ms

enum PollClass {
    OnCpu,
    OffCpu,
    Mixed,
    /// Too short to classify from sample count alone.
    Unknown,
}

fn classify_poll(cpu_count: u32, sched_count: u32, duration_ns: i64) -> PollClass {
    if cpu_count > 0 && sched_count > 0 {
        return PollClass::Mixed;
    }
    if cpu_count > 0 {
        return PollClass::OnCpu;
    }
    // 0 cpu samples: only call it off-CPU if the poll was long enough
    // that we'd statistically expect at least one sample.
    if duration_ns >= OFF_CPU_CONFIDENCE_NS {
        PollClass::OffCpu
    } else {
        PollClass::Unknown
    }
}

fn read_polls_part(
    data: &[u8],
    scope: &Scope,
    source_key: &str,
    acc: &mut TokioStatsAccum,
) -> Result<(), (StatusCode, String)> {
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReader::try_new(
        bytes::Bytes::from(data.to_vec()),
        4096,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    for batch in reader {
        let batch = batch.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let duration_arr = batch
            .column_by_name("duration_ns")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::Int64Array>());
        let start_arr = batch
            .column_by_name("start_ns")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::Int64Array>());
        let end_arr = batch
            .column_by_name("end_ns")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::Int64Array>());
        let cpu_arr = batch
            .column_by_name("cpu_sample_count")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt32Array>());
        let sched_arr = batch
            .column_by_name("sched_sample_count")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt32Array>());
        let spawn_loc_arr = batch
            .column_by_name("spawn_loc")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::StringArray>());
        let host_arr = batch
            .column_by_name("host")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::StringArray>());
        // worker_id / task_id feed the top-long-polls list. Both are already in
        // the polls schema (parquet_writer.rs); older part-files without them
        // just yield None here and the columns are skipped.
        let worker_arr = batch
            .column_by_name("worker_id")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt32Array>());
        let task_arr = batch
            .column_by_name("task_id")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt64Array>());

        let Some(duration_arr) = duration_arr else {
            continue;
        };

        for i in 0..batch.num_rows() {
            if let Some(sa) = start_arr
                && let Some(start) = scope.start_ns
                && sa.value(i) < start
            {
                continue;
            }
            if let Some(sa) = start_arr
                && let Some(end) = scope.end_ns
                && sa.value(i) >= end
            {
                continue;
            }

            let dur = duration_arr.value(i);
            acc.total_polls += 1;

            if let Some(sa) = start_arr {
                let ts = sa.value(i);
                acc.min_ts = Some(acc.min_ts.map_or(ts, |m| m.min(ts)));
                acc.max_ts = Some(acc.max_ts.map_or(ts, |m| m.max(ts)));
            }

            let loc = spawn_loc_arr
                .and_then(|a| if a.is_null(i) { None } else { Some(a.value(i)) })
                .unwrap_or("(unknown)");
            let la = acc
                .by_loc
                .entry(loc.to_string())
                .or_insert_with(|| LocAccum {
                    total: 0,
                    durations: Vec::new(),
                    worst_by_class: [None, None, None, None],
                });
            la.total += 1;

            if dur < DURATION_FLOOR_NS {
                continue;
            }

            let cpu_count = cpu_arr.map_or(0, |a| a.value(i));
            let sched_count = sched_arr.map_or(0, |a| a.value(i));
            let class = match classify_poll(cpu_count, sched_count, dur) {
                PollClass::OffCpu => 0u8,
                PollClass::OnCpu => 1,
                PollClass::Mixed => 2,
                PollClass::Unknown => 3,
            };
            la.durations.push((dur, class));

            // Borrowed here; each consumer below owns its copy only when it
            // actually stores the row, so a poll that is neither a new worst
            // exemplar nor pushed (old part-file lacking worker/task) allocates
            // nothing.
            let host = host_arr
                .and_then(|a| if a.is_null(i) { None } else { Some(a.value(i)) })
                .unwrap_or("");

            let slot = &mut la.worst_by_class[class as usize];
            if slot.as_ref().is_none_or(|w| dur > w.duration_ns) {
                *slot = Some(PollExemplar {
                    start_ns: start_arr.map_or(0, |a| a.value(i)),
                    end_ns: end_arr.map_or(0, |a| a.value(i)),
                    duration_ns: dur,
                    host: host.to_string(),
                    source_key: source_key.to_string(),
                });
            }

            // Top longest polls (IRIS `longPolls.top`): a poll is only useful in
            // this list if we can attribute it to a worker + task, so skip rows
            // from older part-files that predate those columns rather than
            // fabricating zeros (AGENTS.md: no plausible-default masking).
            if let (Some(workers), Some(tasks)) = (worker_arr, task_arr) {
                let spawn_loc = spawn_loc_arr
                    .and_then(|a| if a.is_null(i) { None } else { Some(a.value(i)) })
                    .map(str::to_string);
                acc.push_long_poll(LongPoll {
                    duration_ns: dur,
                    worker_id: workers.value(i),
                    task_id: tasks.value(i),
                    spawn_loc,
                    start_ns: start_arr.map_or(0, |a| a.value(i)),
                    end_ns: end_arr.map_or(0, |a| a.value(i)),
                    host: host.to_string(),
                    source_key: source_key.to_string(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::decode::decode_samples;
    use crate::ingest::parquet_writer;

    #[test]
    fn test_read_polls_from_demo_trace() {
        let data =
            std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/demo-trace.bin")).unwrap();
        let decompressed = {
            use std::io::Read;
            let mut dec = flate2::read::GzDecoder::new(data.as_slice());
            let mut buf = Vec::new();
            dec.read_to_end(&mut buf).unwrap();
            buf
        };
        let (_, _, polls) = decode_samples(&decompressed, "demo-trace.bin").unwrap();
        assert!(!polls.is_empty());

        let mut buf = Vec::new();
        parquet_writer::write_polls(&mut buf, &polls).unwrap();

        let scope = Scope::default();
        let mut acc = TokioStatsAccum::default();
        read_polls_part(&buf, &scope, "test-key", &mut acc).unwrap();

        assert_eq!(acc.total_polls, polls.len() as u64);
        let notable: usize = acc.by_loc.values().map(|la| la.durations.len()).sum();
        assert!(notable > 0, "expected polls above 100µs floor");
        // Check exemplars exist for locations with notable polls.
        let with_exemplar = acc
            .by_loc
            .values()
            .filter(|la| la.worst_by_class.iter().any(|e| e.is_some()))
            .count();
        assert!(with_exemplar > 0);

        // Top longest polls: populated, bounded to LONG_POLL_TOP, ranked
        // descending by duration, and each carries the coordinates the viewer
        // needs to deep-link the poll (source_key + host).
        let top = acc.top_long_polls();
        assert!(!top.is_empty(), "expected some notable long polls");
        assert!(top.len() <= LONG_POLL_TOP);
        assert!(
            top.windows(2).all(|w| w[0].duration_ns >= w[1].duration_ns),
            "top long polls must be ranked descending by duration"
        );
        assert!(
            top.iter().all(|p| !p.source_key.is_empty()),
            "each long poll must carry a source key for deep-linking"
        );
        assert!(
            top[0].duration_ns >= DURATION_FLOOR_NS,
            "long polls must clear the notable floor"
        );
        eprintln!(
            "tokio-stats: {} total, {} above floor, {} locs with exemplars, {} long polls (worst {}ns)",
            acc.total_polls,
            notable,
            with_exemplar,
            top.len(),
            top[0].duration_ns,
        );
    }
}

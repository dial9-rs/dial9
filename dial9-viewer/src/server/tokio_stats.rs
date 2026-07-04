//! `/api/tokio-stats` endpoint: read aggregated polls Parquet data and return
//! Tokio runtime stats — long polls classified as on-CPU vs off-CPU,
//! grouped by spawn location. Supports progressive refinement (same as flamegraph).

use axum::Extension;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum_extra::extract::Query as QueryExtra;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::ingest::aggregate::{self, Scope};
use crate::ingest::refine::{self, RefineOpts};
use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::server::metrics::OperationMetrics;

use arrow::array::Array;

/// Floor: only send polls longer than this to the client (saves bandwidth).
const DURATION_FLOOR_NS: i64 = 100_000; // 100µs

#[derive(Deserialize)]
pub struct TokioStatsParams {
    pub bucket: Option<String>,
    pub prefix: Option<String>,
    pub service: Option<String>,
    #[serde(default)]
    pub host: Vec<String>,
    pub start_ns: Option<i64>,
    pub end_ns: Option<i64>,
    #[serde(default)]
    pub refine: bool,
}

#[derive(Serialize)]
pub struct TokioStatsResponse {
    /// Time span covered by the data (ns), for computing per-minute rates.
    pub time_span_ns: i64,
    pub total_polls: u64,
    /// Source bucket (for constructing viewer deep links in the UI).
    pub bucket: String,
    pub by_spawn_loc: Vec<SpawnLocStats>,
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

/// Handler for GET /api/tokio-stats.
pub async fn get_tokio_stats(
    State(state): State<AppState>,
    creds: MaybeCreds,
    QueryExtra(params): QueryExtra<TokioStatsParams>,
) -> Result<(Extension<OperationMetrics>, Json<TokioStatsResponse>), (StatusCode, String)> {
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
        refine = params.refine,
        "tokio-stats: starting"
    );

    // Run the shared refinement loop: list + scope-filter, cap, and fold a
    // bounded batch (identical policy to flamegraph). tokio-stats reads only the
    // `polls/` part-files, so it does not "fetch more"; the default cap applies.
    let opts = RefineOpts {
        refine: params.refine,
        max_files: None,
    };
    let Some(refined) = refine::refine(&agg, &scope, opts, &state.fold_limits).await else {
        return Err((
            StatusCode::NOT_FOUND,
            "no source files match this scope".to_string(),
        ));
    };

    // Read the polls part-files for the folded-in-cap files concurrently, then
    // accumulate the long polls per spawn location.
    let polls_data = aggregate::read_polls_parts(
        &*agg.output,
        &agg.output_bucket,
        &agg.output_prefix,
        &refined.capped,
        refined.folded(),
    )
    .await;

    let mut acc = TokioStatsAccum::default();
    let files_read = polls_data.len();
    for (raw_key, data) in &polls_data {
        read_polls_part(data, &scope, raw_key, &mut acc)?;
    }

    let files_matched = refined.files_matched;
    let files_folded = refined.files_folded();

    let notable_polls: usize = acc.by_loc.values().map(|la| la.durations.len()).sum();
    tracing::debug!(
        files_read,
        files_folded,
        files_matched,
        total_polls = acc.total_polls,
        notable_polls,
        spawn_locs = acc.by_loc.len(),
        "tokio-stats: returning ({files_folded}/{files_matched} files, {notable_polls} polls above floor)"
    );

    // Compute time span.
    let time_span_ns = match (acc.min_ts, acc.max_ts) {
        (Some(min), Some(max)) => (max - min).max(1),
        _ => 1,
    };

    // Build response: per-spawn-loc with durations sorted descending.
    let mut by_spawn_loc: Vec<SpawnLocStats> = acc
        .by_loc
        .into_iter()
        .map(|(loc, mut la)| {
            la.durations
                .sort_unstable_by_key(|&(d, _)| std::cmp::Reverse(d));
            let durations_ns = la.durations.iter().map(|(d, _)| *d).collect();
            let classes = la.durations.iter().map(|(_, c)| *c).collect();
            SpawnLocStats {
                spawn_loc: loc,
                total_polls: la.total,
                durations_ns,
                classes,
                exemplars: la.worst_by_class,
            }
        })
        .collect();
    // Sort by number of notable polls (above floor) descending.
    by_spawn_loc.sort_by_key(|l| std::cmp::Reverse(l.durations_ns.len()));

    // Operation-specific metrics: coverage plus the notable-poll count, the
    // signal this endpoint exists to surface.
    let op = OperationMetrics::tokio_stats(
        files_matched as u32,
        files_folded as u32,
        notable_polls as u64,
    );

    Ok((
        Extension(op),
        Json(TokioStatsResponse {
            time_span_ns,
            total_polls: acc.total_polls,
            bucket: agg.source_bucket.clone(),
            by_spawn_loc,
            coverage: Some(aggregate::Coverage {
                files_matched,
                files_folded,
                // tokio-stats counts files read, not samples, as its "folded" unit.
                samples_folded: files_read,
                total_bytes: refined.total_bytes,
                hosts_matched: refined.hosts_matched,
                hosts_folded: refined.hosts_folded(),
            }),
        }),
    ))
}

// ─── Internal types ──────────────────────────────────────────────────────────

#[derive(Default)]
struct TokioStatsAccum {
    total_polls: u64,
    min_ts: Option<i64>,
    max_ts: Option<i64>,
    by_loc: HashMap<String, LocAccum>,
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

            let slot = &mut la.worst_by_class[class as usize];
            if slot.as_ref().is_none_or(|w| dur > w.duration_ns) {
                *slot = Some(PollExemplar {
                    start_ns: start_arr.map_or(0, |a| a.value(i)),
                    end_ns: end_arr.map_or(0, |a| a.value(i)),
                    duration_ns: dur,
                    host: host_arr
                        .and_then(|a| if a.is_null(i) { None } else { Some(a.value(i)) })
                        .unwrap_or("")
                        .to_string(),
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
        eprintln!(
            "tokio-stats: {} total, {} above floor, {} locs with exemplars",
            acc.total_polls, notable, with_exemplar
        );
    }
}

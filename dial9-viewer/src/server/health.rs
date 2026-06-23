//! `/api/health` endpoint: read aggregated polls Parquet data and return
//! runtime health metrics — long polls classified as on-CPU vs off-CPU,
//! grouped by spawn location. Supports progressive refinement (same as flamegraph).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum_extra::extract::Query as QueryExtra;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::ingest::aggregate::{self, AggContext, Scope};
use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::storage::ObjectInfo;

use arrow::array::Array;
use futures::stream::StreamExt;

/// Floor: only send polls longer than this to the client (saves bandwidth).
const DURATION_FLOOR_NS: i64 = 100_000; // 100µs

#[derive(Deserialize)]
pub struct HealthParams {
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
pub struct HealthResponse {
    /// Time span covered by the data (ns), for computing per-minute rates.
    pub time_span_ns: i64,
    pub total_polls: u64,
    /// Source bucket (for constructing viewer deep links in the UI).
    pub bucket: String,
    pub by_spawn_loc: Vec<SpawnLocHealth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<aggregate::Coverage>,
}

#[derive(Serialize)]
pub struct SpawnLocHealth {
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

/// Handler for GET /api/health.
pub async fn get_health(
    State(state): State<AppState>,
    creds: MaybeCreds,
    QueryExtra(params): QueryExtra<HealthParams>,
) -> Result<Json<HealthResponse>, (StatusCode, String)> {
    let agg = if let Some(bucket) = &params.bucket {
        let backend = state.resolve(creds)?;
        let source_prefix = params
            .prefix
            .clone()
            .or_else(|| state.default_prefix.clone())
            .unwrap_or_default();
        let (output_bucket, output) = match (&state.agg_output_bucket, &state.agg_output_backend) {
            (Some(out_bucket), Some(out_backend)) => (out_bucket.clone(), Arc::clone(out_backend)),
            _ => (bucket.clone(), Arc::clone(&backend)),
        };
        AggContext {
            source: backend,
            output,
            source_bucket: bucket.clone(),
            source_is_local: false,
            output_bucket,
            output_prefix: state.agg_output_prefix.clone(),
            source_prefixes: vec![source_prefix],
            segment_duration_secs: state.agg_segment_secs,
        }
    } else if let Some(agg) = &state.agg {
        agg.clone()
    } else {
        return Err((
            StatusCode::NOT_FOUND,
            "health requires aggregation (start with --agg or supply a bucket)".to_string(),
        ));
    };

    let scope = Scope {
        start_ns: params.start_ns,
        end_ns: params.end_ns,
        service: params.service.clone(),
        hosts: params.host.clone(),
    };

    tracing::info!(
        source_bucket = %agg.source_bucket,
        source_prefixes = ?agg.source_prefixes,
        service = scope.service.as_deref().unwrap_or("(all)"),
        hosts = ?scope.hosts,
        start_ns = ?scope.start_ns,
        end_ns = ?scope.end_ns,
        refine = params.refine,
        "health: starting"
    );

    // List + scope-filter (same as flamegraph).
    let listing_prefixes =
        crate::server::flamegraph::time_scoped_prefixes(&agg.source_prefixes, &scope);
    tracing::info!(listing_prefixes = ?listing_prefixes, "health: listing prefixes");

    let per_prefix: Vec<Vec<ObjectInfo>> = futures::stream::iter(listing_prefixes)
        .map(|prefix| {
            let agg = &agg;
            async move {
                agg.source
                    .list_objects_all(&agg.source_bucket, &prefix)
                    .await
                    .unwrap_or_default()
            }
        })
        .buffer_unordered(24)
        .collect()
        .await;
    let raw_objects: Vec<ObjectInfo> = per_prefix.into_iter().flatten().collect();
    let total_listed = raw_objects.len();
    let ordered = aggregate::ordered_full_keys(
        raw_objects,
        &scope,
        agg.segment_duration_secs,
        agg.source_is_local,
        &agg.source_bucket,
    );
    let files_matched = ordered.len();
    tracing::info!(
        total_listed,
        files_matched,
        sample_matched = ?ordered.iter().take(3).map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        "health: scope filter result"
    );

    if ordered.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            format!(
                "no source files match this scope (listed {} objects)",
                total_listed
            ),
        ));
    }

    // Folded set + progressive refinement (identical to flamegraph).
    let mut folded = aggregate::list_folded_leaves(
        &*agg.output,
        &agg.output_bucket,
        &agg.output_prefix,
        &agg.source_bucket,
    )
    .await;

    const BASELINE_FILES: usize = 4;
    const REFINE_BATCH_FILES: usize = 12;
    let cap = ordered.len().min(100);
    let capped: Vec<&(String, String)> = ordered.iter().take(cap).collect();
    let already_folded_in_cap = capped
        .iter()
        .filter(|(_, full)| folded.contains(&aggregate::part_leaf_of(full)))
        .count();

    tracing::info!(
        files_matched,
        cap,
        already_folded = already_folded_in_cap,
        refine = params.refine,
        "health: refinement state"
    );

    if params.refine {
        let budget = if already_folded_in_cap == 0 {
            BASELINE_FILES
        } else {
            REFINE_BATCH_FILES
        };
        let room = cap.saturating_sub(already_folded_in_cap);
        let budget = budget.min(room);

        if budget > 0 {
            let to_fold: Vec<(String, String)> = capped
                .iter()
                .filter(|(_, full)| !folded.contains(&aggregate::part_leaf_of(full)))
                .take(budget)
                .map(|kv| (*kv).clone())
                .collect();

            let agg_arc = Arc::new(agg.clone());
            let limits = state.fold_limits.clone();
            let mut tasks: tokio::task::JoinSet<Option<String>> = tokio::task::JoinSet::new();
            for (raw_key, full_key) in to_fold {
                let a = Arc::clone(&agg_arc);
                let l = limits.clone();
                tasks.spawn(async move {
                    match aggregate::fold_one(&a, &raw_key, &l).await {
                        Ok(()) => Some(aggregate::part_leaf_of(&full_key)),
                        Err(e) => {
                            tracing::warn!(key = %raw_key, error = %e, "health: fold failed");
                            None
                        }
                    }
                });
            }
            while let Some(joined) = tasks.join_next().await {
                if let Ok(Some(leaf)) = joined {
                    folded.insert(leaf);
                }
            }
        }
    }

    // Read polls part-files concurrently.
    let polls_fetches: Vec<(String, String)> = capped
        .iter()
        .filter(|(_, full)| folded.contains(&aggregate::part_leaf_of(full)))
        .map(|(raw, full)| {
            (
                raw.clone(),
                aggregate::polls_part_key_pub(&agg.output_prefix, full),
            )
        })
        .collect();

    let polls_data: Vec<(String, Vec<u8>)> = futures::stream::iter(polls_fetches)
        .map(|(raw_key, polls_key)| {
            let output = &agg.output;
            let bucket = &agg.output_bucket;
            async move {
                output
                    .get_object(bucket, &polls_key)
                    .await
                    .ok()
                    .map(|data| (raw_key, data))
            }
        })
        .buffer_unordered(24)
        .filter_map(|x| async { x })
        .collect()
        .await;

    let mut acc = HealthAccum::default();
    let files_read = polls_data.len();
    for (raw_key, data) in &polls_data {
        read_polls_part(data, &scope, raw_key, &mut acc)?;
    }

    let files_folded = capped
        .iter()
        .filter(|(_, full)| folded.contains(&aggregate::part_leaf_of(full)))
        .count();

    let notable_polls: usize = acc.by_loc.values().map(|la| la.durations.len()).sum();
    tracing::info!(
        files_read,
        files_folded,
        files_matched,
        total_polls = acc.total_polls,
        notable_polls,
        spawn_locs = acc.by_loc.len(),
        "health: returning ({files_folded}/{files_matched} files, {notable_polls} polls above floor)"
    );

    // Compute time span.
    let time_span_ns = match (acc.min_ts, acc.max_ts) {
        (Some(min), Some(max)) => (max - min).max(1),
        _ => 1,
    };

    // Build response: per-spawn-loc with durations sorted descending.
    let mut by_spawn_loc: Vec<SpawnLocHealth> = acc
        .by_loc
        .into_iter()
        .map(|(loc, mut la)| {
            la.durations
                .sort_unstable_by_key(|&(d, _)| std::cmp::Reverse(d));
            let durations_ns = la.durations.iter().map(|(d, _)| *d).collect();
            let classes = la.durations.iter().map(|(_, c)| *c).collect();
            SpawnLocHealth {
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

    Ok(Json(HealthResponse {
        time_span_ns,
        total_polls: acc.total_polls,
        bucket: agg.source_bucket.clone(),
        by_spawn_loc,
        coverage: Some(aggregate::Coverage {
            files_matched: ordered.len(),
            files_folded,
            samples_folded: files_read,
        }),
    }))
}

// ─── Internal types ──────────────────────────────────────────────────────────

#[derive(Default)]
struct HealthAccum {
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
    acc: &mut HealthAccum,
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
        let mut acc = HealthAccum::default();
        read_polls_part(&buf, &scope, "test-key", &mut acc).unwrap();

        assert_eq!(acc.total_polls, polls.len() as u64);
        let notable: usize = acc.by_loc.values().map(|la| la.durations.len()).sum();
        assert!(notable > 0, "expected polls above 100µs floor");
        // Check exemplars exist for locations with notable polls.
        let with_exemplar = acc.by_loc.values().filter(|la| la.worst_by_class.iter().any(|e| e.is_some())).count();
        assert!(with_exemplar > 0);
        eprintln!(
            "health: {} total, {} above floor, {} locs with exemplars",
            acc.total_polls, notable, with_exemplar
        );
    }
}

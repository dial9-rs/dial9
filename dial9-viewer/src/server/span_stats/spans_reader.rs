//! Typed reader seam for folded `spans/` Parquet part-files.
//!
//! This module owns Arrow schema adaptation, record-batch traversal, row
//! validation, and occurrence-time filtering. Rows are passed to a caller-owned
//! sink as they are decoded, allowing transactional staging without retaining
//! an unbounded, fully materialized Parquet part.

use arrow::array::{
    Array, BooleanArray, FixedSizeBinaryArray, Int64Array, MapArray, StringArray, UInt32Array,
};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};

use super::{Exemplar, ExemplarAttribute, TimeComposition};
use crate::server::metrics::SpanStatsPhaseDurations;

pub(super) struct ExemplarOnlyConfig {
    pub min_ns: Option<i64>,
    pub max_ns: Option<i64>,
    pub max_exemplars: usize,
}

impl ExemplarOnlyConfig {
    fn matches(&self, elapsed_ns: i64) -> bool {
        self.min_ns.is_none_or(|min| elapsed_ns >= min)
            && self.max_ns.is_none_or(|max| elapsed_ns <= max)
    }

    fn has_bounds(&self) -> bool {
        self.min_ns.is_some() || self.max_ns.is_some()
    }
}

const SPAN_STATS_COLUMNS: &[&str] = &[
    "span_uid",
    "span_type_uid",
    "kind",
    "name",
    "target",
    "callsite_file",
    "callsite_line",
    "start_ns",
    "end_ns",
    "elapsed_ns",
    "details_complete",
    "on_cpu_ns_est",
    "blocked_ns_est",
    "async_wait_ns",
    "scheduler_delay_ns",
    "unknown_ns",
    "attributes",
    "source_key",
    "host",
];

fn projected_reader(data: Vec<u8>) -> parquet::errors::Result<ParquetRecordBatchReader> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(data))?;
    let root_indices = builder
        .parquet_schema()
        .root_schema()
        .get_fields()
        .iter()
        .enumerate()
        .filter_map(|(index, field)| SPAN_STATS_COLUMNS.contains(&field.name()).then_some(index))
        .collect::<Vec<_>>();
    let projection = ProjectionMask::roots(builder.parquet_schema(), root_indices);
    builder
        .with_batch_size(4096)
        .with_projection(projection)
        .build()
}

pub(super) enum SpanStatsInput {
    Row(Box<SpanStatsRow>),
    ExemplarOnlySummary(ExemplarOnlySummary),
}

pub(super) struct ExemplarOnlySummary {
    pub span_type_uid: [u8; 16],
    pub kind: String,
    pub name: String,
    pub target: Option<String>,
    pub callsite_file: Option<String>,
    pub callsite_line: Option<u32>,
    pub count: u64,
    pub selected_duration_count: Option<u64>,
}

pub(super) struct SpanStatsRow {
    pub span_type_uid: [u8; 16],
    pub kind: String,
    pub name: String,
    pub target: Option<String>,
    pub callsite_file: Option<String>,
    pub callsite_line: Option<u32>,
    pub elapsed_ns: i64,
    pub exemplar: Option<Exemplar>,
    pub attributes: Vec<(String, String)>,
    pub composition: Option<RowComposition>,
    pub details_complete: Option<bool>,
}

pub(super) struct RowComposition {
    pub on_cpu_ns: i64,
    pub blocked_ns: i64,
    pub async_wait_ns: i64,
    pub scheduler_delay_ns: i64,
    pub unknown_ns: i64,
}

pub(super) struct SpansBatchReader {
    start_ns: Option<i64>,
    end_ns: Option<i64>,
    span_type_uid: Option<[u8; 16]>,
    exemplar_only: Option<ExemplarOnlyConfig>,
}

#[derive(Default)]
struct MaterializationCounts {
    rows: u64,
    attribute_entries: u64,
}
impl SpansBatchReader {
    pub(super) fn new(
        start_ns: Option<i64>,
        end_ns: Option<i64>,
        span_type_uid: Option<[u8; 16]>,
        exemplar_only: Option<ExemplarOnlyConfig>,
    ) -> Self {
        Self {
            start_ns,
            end_ns,
            span_type_uid,
            exemplar_only,
        }
    }

    pub(super) fn read(
        &self,
        data: Vec<u8>,
        mut consume: impl FnMut(SpanStatsInput),
    ) -> (SpanStatsPhaseDurations, anyhow::Result<()>) {
        use std::time::Instant;

        let mut phases = SpanStatsPhaseDurations {
            parquet_bytes: data.len() as u64,
            ..Default::default()
        };

        // ── Reader setup: Parquet footer parsing + Arrow schema negotiation ──
        let setup_started = Instant::now();
        let reader = projected_reader(data);
        let setup_elapsed = setup_started.elapsed();
        phases.reader_setup += setup_elapsed;
        phases.parse += setup_elapsed;

        let mut reader = match reader {
            Ok(reader) => reader,
            Err(error) => return (phases, Err(error.into())),
        };

        loop {
            // ── Batch decode: advance column-chunk decoding into RecordBatch ─
            let decode_started = Instant::now();
            let next = reader.next();
            let decode_elapsed = decode_started.elapsed();
            phases.batch_decode += decode_elapsed;
            phases.parse += decode_elapsed;

            let Some(batch) = next else {
                break;
            };
            let batch = match batch {
                Ok(batch) => batch,
                Err(error) => {
                    return (phases, Err(error.into()));
                }
            };
            phases.record_batches_decoded += 1;

            // ── Row materialize: walk Arrow arrays → owned SpanStatsRow ──────
            let materialize_started = Instant::now();
            let mut counts = MaterializationCounts::default();
            let rows = self.read_batch(&batch, &mut counts);
            let materialize_elapsed = materialize_started.elapsed();
            phases.row_materialize += materialize_elapsed;
            phases.parse += materialize_elapsed;
            phases.rows_materialized += counts.rows;
            phases.attribute_entries += counts.attribute_entries;
            let rows = match rows {
                Ok(rows) => rows,
                Err(error) => return (phases, Err(error)),
            };

            // ── Query: feed rows into the caller's accumulator ───────────────
            let query_started = Instant::now();
            for row in rows {
                consume(row);
            }
            phases.query += query_started.elapsed();
        }
        (phases, Ok(()))
    }

    fn read_batch(
        &self,
        batch: &RecordBatch,
        counts: &mut MaterializationCounts,
    ) -> anyhow::Result<Vec<SpanStatsInput>> {
        let Some(type_uid_column) = batch.column_by_name("span_type_uid") else {
            // Preserve compatibility with old/non-span parts: a batch without a
            // type UID contributes no span rows.
            return Ok(Vec::new());
        };
        let Some(type_uid_arr) = type_uid_column
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
        else {
            anyhow::bail!(
                "span_type_uid column has wrong type: expected FixedSizeBinary, got {}",
                type_uid_column.data_type()
            );
        };
        if type_uid_arr.value_length() != 16 {
            anyhow::bail!(
                "span_type_uid column has wrong width: expected 16 bytes, got {}",
                type_uid_arr.value_length()
            );
        }

        let elapsed_arr = required_column::<Int64Array>(batch, "elapsed_ns")?;
        let name_arr = required_column::<StringArray>(batch, "name")?;
        let kind_arr = required_column::<StringArray>(batch, "kind")?;
        let end_ns_col = column::<Int64Array>(batch, "end_ns");
        let has_time_filter = self.start_ns.is_some() || self.end_ns.is_some();
        if has_time_filter && end_ns_col.is_none() {
            anyhow::bail!(
                "time filter active but spans part is missing end_ns column; \
                     cannot apply occurrence-time filter"
            );
        }

        let span_uid_col = column::<FixedSizeBinaryArray>(batch, "span_uid");
        if let Some(uid_arr) = span_uid_col
            && uid_arr.value_length() != 16
        {
            anyhow::bail!(
                "span_uid column has wrong width: expected 16 bytes, got {}",
                uid_arr.value_length()
            );
        }

        let target_col = column::<StringArray>(batch, "target");
        let file_col = column::<StringArray>(batch, "callsite_file");
        let line_col = column::<UInt32Array>(batch, "callsite_line");
        let start_ns_col = column::<Int64Array>(batch, "start_ns");
        let source_key_col = column::<StringArray>(batch, "source_key");
        let host_col = column::<StringArray>(batch, "host");
        let attributes_col = column::<MapArray>(batch, "attributes");
        let on_cpu_col = column::<Int64Array>(batch, "on_cpu_ns_est");
        let blocked_col = column::<Int64Array>(batch, "blocked_ns_est");
        let async_wait_col = column::<Int64Array>(batch, "async_wait_ns");
        let sched_delay_col = column::<Int64Array>(batch, "scheduler_delay_ns");
        let unknown_col = column::<Int64Array>(batch, "unknown_ns");
        let details_complete_col = column::<BooleanArray>(batch, "details_complete");

        let scoped_row = |row_index: usize| -> anyhow::Result<Option<(i64, Option<i64>)>> {
            if type_uid_arr.is_null(row_index)
                || elapsed_arr.is_null(row_index)
                || name_arr.is_null(row_index)
                || kind_arr.is_null(row_index)
            {
                return Ok(None);
            }
            if self
                .span_type_uid
                .is_some_and(|expected| type_uid_arr.value(row_index) != expected.as_slice())
            {
                return Ok(None);
            }

            let elapsed_ns = elapsed_arr.value(row_index);
            if elapsed_ns < 0 {
                return Ok(None);
            }

            let end_ns = match end_ns_col {
                Some(end_arr) if end_arr.is_null(row_index) => {
                    if has_time_filter {
                        anyhow::bail!(
                            "null end_ns at row {row_index} with time filter active; \
                                 part file is malformed"
                        );
                    }
                    None
                }
                Some(end_arr) => {
                    let value = end_arr.value(row_index);
                    if value < 0 {
                        anyhow::bail!(
                            "negative end_ns ({value}) at row {row_index}; part file is malformed"
                        );
                    }
                    Some(value)
                }
                None => None,
            };
            if end_ns.is_some_and(|value| {
                self.start_ns.is_some_and(|start| value < start)
                    || self.end_ns.is_some_and(|end| value >= end)
            }) {
                return Ok(None);
            }
            Ok(Some((elapsed_ns, end_ns)))
        };

        let materialize_row = |row_index: usize, elapsed_ns: i64, end_ns: Option<i64>| {
            let mut span_type_uid = [0; 16];
            span_type_uid.copy_from_slice(type_uid_arr.value(row_index));

            let composition =
                unknown_col
                    .filter(|array| !array.is_null(row_index))
                    .map(|unknown_arr| RowComposition {
                        on_cpu_ns: optional_i64(on_cpu_col, row_index).unwrap_or(0),
                        blocked_ns: optional_i64(blocked_col, row_index).unwrap_or(0),
                        async_wait_ns: optional_i64(async_wait_col, row_index).unwrap_or(0),
                        scheduler_delay_ns: optional_i64(sched_delay_col, row_index).unwrap_or(0),
                        unknown_ns: unknown_arr.value(row_index),
                    });

            let attributes = attributes_col
                .map(|array| parse_map_column(array, row_index))
                .unwrap_or_default();

            // Attach this instance's own metadata (composition + attributes) to
            // the exemplar so the viewer can show each instance's makeup, not
            // just the span type's aggregate.
            let exemplar = span_uid_col
                .filter(|array| !array.is_null(row_index))
                .map(|uid_arr| Exemplar {
                    elapsed_ns,
                    span_uid: hex::encode(uid_arr.value(row_index)),
                    callsite_file: optional_string(file_col, row_index),
                    callsite_line: optional_u32(line_col, row_index),
                    host: optional_string(host_col, row_index).unwrap_or_default(),
                    start_ns: optional_i64(start_ns_col, row_index).unwrap_or(0),
                    end_ns: end_ns.unwrap_or(0),
                    source_key: optional_string(source_key_col, row_index).unwrap_or_default(),
                    // A single instance's composition needs no equal-weighting,
                    // so the per-instance fraction fields stay zero (omitted on
                    // the wire).
                    composition: composition.as_ref().map(|c| TimeComposition {
                        on_cpu_ns: c.on_cpu_ns,
                        blocked_ns: c.blocked_ns,
                        async_wait_ns: c.async_wait_ns,
                        scheduler_delay_ns: c.scheduler_delay_ns,
                        unknown_ns: c.unknown_ns,
                        instance_count: 0,
                        on_cpu_frac_sum: 0.0,
                        blocked_frac_sum: 0.0,
                        async_wait_frac_sum: 0.0,
                        scheduler_delay_frac_sum: 0.0,
                        unknown_frac_sum: 0.0,
                    }),
                    attributes: attributes
                        .iter()
                        .map(|(key, value)| ExemplarAttribute {
                            key: key.clone(),
                            value: value.clone(),
                        })
                        .collect(),
                });

            SpanStatsRow {
                span_type_uid,
                kind: kind_arr.value(row_index).to_string(),
                name: name_arr.value(row_index).to_string(),
                target: optional_string(target_col, row_index),
                callsite_file: optional_string(file_col, row_index),
                callsite_line: optional_u32(line_col, row_index),
                elapsed_ns,
                exemplar,
                attributes,
                composition,
                details_complete: optional_bool(details_complete_col, row_index),
            }
        };

        if let Some(config) = &self.exemplar_only {
            debug_assert!(self.span_type_uid.is_some());
            let mut total_count = 0_u64;
            let mut matching_count = 0_u64;
            let mut first_row = None;
            let mut candidates: Vec<(usize, i64)> = Vec::with_capacity(config.max_exemplars);

            for row_index in 0..batch.num_rows() {
                let Some((elapsed_ns, _)) = scoped_row(row_index)? else {
                    continue;
                };
                total_count += 1;
                first_row.get_or_insert(row_index);
                if !config.matches(elapsed_ns) {
                    continue;
                }
                matching_count += 1;
                if span_uid_col.is_none_or(|array| array.is_null(row_index))
                    || config.max_exemplars == 0
                {
                    continue;
                }
                if candidates.len() < config.max_exemplars {
                    candidates.push((row_index, elapsed_ns));
                } else if let Some((min_position, (_, min_elapsed))) = candidates
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, (_, candidate_elapsed))| *candidate_elapsed)
                    && elapsed_ns > *min_elapsed
                {
                    candidates[min_position] = (row_index, elapsed_ns);
                }
            }

            // Per-batch top-K is sufficient for exact global top-K: a row outside
            // its batch's top K cannot belong to the top K of the union. Preserve
            // wire order among retained rows so equal-duration tie behavior stays
            // identical to eager materialization.
            candidates.sort_unstable_by_key(|(row_index, _)| *row_index);
            let materialized_count = candidates.len() as u64;
            let mut inputs = Vec::with_capacity(candidates.len() + 1);
            for (row_index, elapsed_ns) in candidates {
                let (_, end_ns) = scoped_row(row_index)?.expect("retained row remains in scope");
                let row = materialize_row(row_index, elapsed_ns, end_ns);
                counts.rows += 1;
                counts.attribute_entries += row.attributes.len() as u64;
                inputs.push(SpanStatsInput::Row(Box::new(row)));
            }

            let summary_count = total_count - materialized_count;
            if summary_count > 0 {
                let row_index = first_row.expect("non-zero count has a source row");
                let mut span_type_uid = [0; 16];
                span_type_uid.copy_from_slice(type_uid_arr.value(row_index));
                inputs.push(SpanStatsInput::ExemplarOnlySummary(ExemplarOnlySummary {
                    span_type_uid,
                    kind: kind_arr.value(row_index).to_string(),
                    name: name_arr.value(row_index).to_string(),
                    target: optional_string(target_col, row_index),
                    callsite_file: optional_string(file_col, row_index),
                    callsite_line: optional_u32(line_col, row_index),
                    count: summary_count,
                    selected_duration_count: config
                        .has_bounds()
                        .then_some(matching_count - materialized_count),
                }));
            }
            return Ok(inputs);
        }

        let mut inputs = Vec::with_capacity(batch.num_rows());
        for row_index in 0..batch.num_rows() {
            let Some((elapsed_ns, end_ns)) = scoped_row(row_index)? else {
                continue;
            };
            let row = materialize_row(row_index, elapsed_ns, end_ns);
            counts.rows += 1;
            counts.attribute_entries += row.attributes.len() as u64;
            inputs.push(SpanStatsInput::Row(Box::new(row)));
        }
        Ok(inputs)
    }
}

fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> Option<&'a T> {
    batch
        .column_by_name(name)
        .and_then(|array| array.as_any().downcast_ref())
}

fn required_column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> anyhow::Result<&'a T> {
    column(batch, name)
        .ok_or_else(|| anyhow::anyhow!("spans part is missing required column: {name}"))
}

fn optional_i64(array: Option<&Int64Array>, row: usize) -> Option<i64> {
    array.and_then(|array| (!array.is_null(row)).then(|| array.value(row)))
}

fn optional_u32(array: Option<&UInt32Array>, row: usize) -> Option<u32> {
    array.and_then(|array| (!array.is_null(row)).then(|| array.value(row)))
}

fn optional_bool(array: Option<&BooleanArray>, row: usize) -> Option<bool> {
    array.and_then(|array| (!array.is_null(row)).then(|| array.value(row)))
}

fn optional_string(array: Option<&StringArray>, row: usize) -> Option<String> {
    array.and_then(|array| (!array.is_null(row)).then(|| array.value(row).to_string()))
}

/// Parse an Arrow map row into owned key/value pairs.
pub(super) fn parse_map_column(map_array: &MapArray, row: usize) -> Vec<(String, String)> {
    let offsets = map_array.offsets();
    let start = offsets[row] as usize;
    let end = offsets[row + 1] as usize;
    if start == end {
        return Vec::new();
    }

    let entries = map_array.entries();
    let keys_col = entries.column(0).as_any().downcast_ref::<StringArray>();
    let values_col = entries.column(1).as_any().downcast_ref::<StringArray>();
    let (Some(keys), Some(values)) = (keys_col, values_col) else {
        return Vec::new();
    };

    let mut result = Vec::with_capacity(end - start);
    for index in start..end {
        if keys.is_null(index) {
            continue;
        }
        let value = if values.is_null(index) {
            String::new()
        } else {
            values.value(index).to_string()
        };
        result.push((keys.value(index).to_string(), value));
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{FixedSizeBinaryArray, Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use parquet::arrow::ArrowWriter;

    use super::*;

    #[test]
    fn reader_projects_only_span_stats_columns() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("span_type_uid", DataType::FixedSizeBinary(16), false),
            Field::new("name", DataType::Utf8, false),
            Field::new("kind", DataType::Utf8, false),
            Field::new("elapsed_ns", DataType::Int64, false),
            Field::new("active_ns", DataType::Int64, false),
        ]));
        let uid = [7_u8; 16];
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(
                    FixedSizeBinaryArray::try_from_iter([uid.as_slice()].into_iter()).unwrap(),
                ),
                Arc::new(StringArray::from(vec!["test"])),
                Arc::new(StringArray::from(vec!["tracing"])),
                Arc::new(Int64Array::from(vec![10_i64])),
                Arc::new(Int64Array::from(vec![9_i64])),
            ],
        )
        .unwrap();
        let mut data = Vec::new();
        {
            let mut writer = ArrowWriter::try_new(&mut data, schema, None).unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();
        }

        let mut reader = projected_reader(data).unwrap();
        let projected = reader.next().unwrap().unwrap();
        assert!(projected.column_by_name("span_type_uid").is_some());
        assert!(projected.column_by_name("elapsed_ns").is_some());
        assert!(projected.column_by_name("active_ns").is_none());
    }
}

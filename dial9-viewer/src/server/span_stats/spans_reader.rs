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

use super::SlowExemplar;

pub(super) struct SpanStatsRow {
    pub span_type_uid: [u8; 16],
    pub kind: String,
    pub name: String,
    pub target: Option<String>,
    pub callsite_file: Option<String>,
    pub callsite_line: Option<u32>,
    pub elapsed_ns: i64,
    pub exemplar: Option<SlowExemplar>,
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
}

impl SpansBatchReader {
    pub(super) fn new(start_ns: Option<i64>, end_ns: Option<i64>) -> Self {
        Self { start_ns, end_ns }
    }

    pub(super) fn read(
        &self,
        data: Vec<u8>,
        mut consume: impl FnMut(SpanStatsRow),
    ) -> anyhow::Result<()> {
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReader::try_new(
            bytes::Bytes::from(data),
            4096,
        )?;
        for batch in reader {
            self.read_batch(&batch?, &mut consume)?;
        }
        Ok(())
    }

    fn read_batch(
        &self,
        batch: &RecordBatch,
        consume: &mut impl FnMut(SpanStatsRow),
    ) -> anyhow::Result<()> {
        let Some(type_uid_arr) = column::<FixedSizeBinaryArray>(batch, "span_type_uid") else {
            // Preserve compatibility with old/non-span parts: a batch without a
            // recognizable type UID contributes no span rows.
            return Ok(());
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

        for row_index in 0..batch.num_rows() {
            if type_uid_arr.is_null(row_index)
                || elapsed_arr.is_null(row_index)
                || name_arr.is_null(row_index)
                || kind_arr.is_null(row_index)
            {
                continue;
            }

            let elapsed_ns = elapsed_arr.value(row_index);
            if elapsed_ns < 0 {
                continue;
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
                continue;
            }

            let mut span_type_uid = [0; 16];
            span_type_uid.copy_from_slice(type_uid_arr.value(row_index));

            let exemplar = span_uid_col
                .filter(|array| !array.is_null(row_index))
                .map(|uid_arr| SlowExemplar {
                    elapsed_ns,
                    span_uid: hex::encode(uid_arr.value(row_index)),
                    callsite_file: optional_string(file_col, row_index),
                    callsite_line: optional_u32(line_col, row_index),
                    host: optional_string(host_col, row_index).unwrap_or_default(),
                    start_ns: optional_i64(start_ns_col, row_index).unwrap_or(0),
                    end_ns: end_ns.unwrap_or(0),
                    source_key: optional_string(source_key_col, row_index).unwrap_or_default(),
                });

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

            consume(SpanStatsRow {
                span_type_uid,
                kind: kind_arr.value(row_index).to_string(),
                name: name_arr.value(row_index).to_string(),
                target: optional_string(target_col, row_index),
                callsite_file: optional_string(file_col, row_index),
                callsite_line: optional_u32(line_col, row_index),
                elapsed_ns,
                exemplar,
                attributes: attributes_col
                    .map(|array| parse_map_column(array, row_index))
                    .unwrap_or_default(),
                composition,
                details_complete: optional_bool(details_complete_col, row_index),
            });
        }
        Ok(())
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

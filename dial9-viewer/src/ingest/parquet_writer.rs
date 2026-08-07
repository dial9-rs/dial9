//! Write samples and stacks dictionary as Parquet files.

use arrow::array::{
    ArrayRef, BooleanBuilder, FixedSizeBinaryBuilder, Int64Builder, ListBuilder, StringBuilder,
    StructBuilder, UInt8Builder, UInt32Builder, UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Fields, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use super::decode::ResolvedPoll;
use super::decode::ResolvedSample;
use super::decode::ResolvedSpan;

/// Write samples to a Parquet file.
///
/// Does NOT include partition columns (service, date, hour, host) — those are
/// inferred from the file path.
pub fn write_samples<W: Write + Send>(
    writer: W,
    samples: &[ResolvedSample],
    metadata: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let schema = samples_schema();
    let props = WriterProperties::builder()
        .set_dictionary_enabled(true)
        .set_max_row_group_size(128 * 1024)
        .build();

    let mut arrow_writer = ArrowWriter::try_new(writer, schema.clone(), Some(props))?;

    // Build arrays
    let n = samples.len();
    let mut ts_builder = arrow::array::Int64Builder::with_capacity(n);
    let mut stack_id_builder = FixedSizeBinaryBuilder::with_capacity(n, 16);
    // Nullable: null = the sample is not attributed to a runtime worker
    // (off-runtime). There is no in-band sentinel value.
    let mut worker_id_builder = UInt32Builder::with_capacity(n);
    let mut source_builder = UInt8Builder::with_capacity(n);
    let mut source_key_builder = StringBuilder::with_capacity(n, 128 * n);
    let mut host_builder = StringBuilder::with_capacity(n, 64 * n);
    let mut service_builder = StringBuilder::with_capacity(n, 32 * n);
    let mut date_builder = StringBuilder::with_capacity(n, 10 * n);
    let mut poll_duration_builder = arrow::array::Int64Builder::with_capacity(n);
    let mut spawn_location_builder = StringBuilder::with_capacity(n, 64 * n);

    // Metadata map: keys and values builders
    let map_keys_builder = StringBuilder::new();
    let map_values_builder = StringBuilder::new();
    let mut map_builder = arrow::array::MapBuilder::new(None, map_keys_builder, map_values_builder);

    // Enclosing spans: LIST<STRUCT<span_uid, span_type_uid, elapsed_ns, details_complete>>
    let es_fields = enclosing_span_fields();
    let mut enclosing_spans_builder = ListBuilder::new(StructBuilder::from_fields(es_fields, 4));

    for sample in samples {
        ts_builder.append_value(sample.timestamp_ns as i64);
        stack_id_builder.append_value(sample.stack_id)?;
        worker_id_builder.append_option(sample.worker_id);
        source_builder.append_value(sample.source);
        source_key_builder.append_value(&sample.source_key);
        host_builder.append_value(&sample.host);
        service_builder.append_value(&sample.service);
        date_builder.append_value(&sample.date);
        poll_duration_builder.append_option(sample.poll_duration_ns.map(|d| d as i64));
        spawn_location_builder.append_option(sample.spawn_location.as_deref());

        // Append metadata map for this row
        map_builder.keys().append_value("source_key");
        map_builder.values().append_value(&sample.source_key);
        for (k, v) in metadata {
            map_builder.keys().append_value(k);
            map_builder.values().append_value(v);
        }
        map_builder.append(true)?;

        // Append enclosing spans list for this row
        let struct_builder = enclosing_spans_builder.values();
        for es in &sample.enclosing_spans {
            struct_builder
                .field_builder::<FixedSizeBinaryBuilder>(0)
                .unwrap()
                .append_value(es.span_uid)?;
            struct_builder
                .field_builder::<FixedSizeBinaryBuilder>(1)
                .unwrap()
                .append_value(es.span_type_uid)?;
            struct_builder
                .field_builder::<Int64Builder>(2)
                .unwrap()
                .append_value(es.elapsed_ns as i64);
            struct_builder
                .field_builder::<BooleanBuilder>(3)
                .unwrap()
                .append_value(es.details_complete);
            struct_builder.append(true);
        }
        enclosing_spans_builder.append(true);
    }

    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(ts_builder.finish()) as ArrayRef,
            Arc::new(stack_id_builder.finish()) as ArrayRef,
            Arc::new(worker_id_builder.finish()) as ArrayRef,
            Arc::new(source_builder.finish()) as ArrayRef,
            Arc::new(source_key_builder.finish()) as ArrayRef,
            Arc::new(host_builder.finish()) as ArrayRef,
            Arc::new(service_builder.finish()) as ArrayRef,
            Arc::new(date_builder.finish()) as ArrayRef,
            Arc::new(poll_duration_builder.finish()) as ArrayRef,
            Arc::new(spawn_location_builder.finish()) as ArrayRef,
            Arc::new(map_builder.finish()) as ArrayRef,
            Arc::new(enclosing_spans_builder.finish()) as ArrayRef,
        ],
    )?;

    arrow_writer.write(&batch)?;
    arrow_writer.close()?;
    Ok(())
}

/// Write the stacks dictionary to a Parquet file.
pub fn write_stacks_dict<W: Write + Send>(
    writer: W,
    stacks: &HashMap<[u8; 16], Vec<String>>,
) -> anyhow::Result<()> {
    let schema = stacks_schema();
    let props = WriterProperties::builder()
        .set_dictionary_enabled(true)
        .build();

    let mut arrow_writer = ArrowWriter::try_new(writer, schema.clone(), Some(props))?;

    let n = stacks.len();
    let mut stack_id_builder = FixedSizeBinaryBuilder::with_capacity(n, 16);
    let mut frames_builder = ListBuilder::new(StringBuilder::new());

    for (stack_id, frames) in stacks {
        stack_id_builder.append_value(stack_id)?;
        for frame in frames {
            frames_builder.values().append_value(frame);
        }
        frames_builder.append(true);
    }

    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(stack_id_builder.finish()) as ArrayRef,
            Arc::new(frames_builder.finish()) as ArrayRef,
        ],
    )?;

    arrow_writer.write(&batch)?;
    arrow_writer.close()?;
    Ok(())
}

/// Write poll spans to a Parquet file.
pub fn write_polls<W: Write + Send>(writer: W, polls: &[ResolvedPoll]) -> anyhow::Result<()> {
    let schema = polls_schema();
    let props = WriterProperties::builder()
        .set_dictionary_enabled(true)
        .build();
    let mut arrow_writer = ArrowWriter::try_new(writer, schema.clone(), Some(props))?;

    let n = polls.len();
    let mut start_builder = arrow::array::Int64Builder::with_capacity(n);
    let mut end_builder = arrow::array::Int64Builder::with_capacity(n);
    let mut duration_builder = arrow::array::Int64Builder::with_capacity(n);
    let mut worker_id_builder = UInt32Builder::with_capacity(n);
    let mut task_id_builder = UInt64Builder::with_capacity(n);
    let mut spawn_loc_builder = StringBuilder::with_capacity(n, 64 * n);
    let mut ready_at_builder = arrow::array::Int64Builder::with_capacity(n);
    let mut scheduling_delay_builder = arrow::array::Int64Builder::with_capacity(n);
    let mut scheduling_kind_builder = UInt8Builder::with_capacity(n);
    let mut waker_task_id_builder = UInt64Builder::with_capacity(n);
    let mut task_instrumented_builder = arrow::array::BooleanBuilder::with_capacity(n);
    let mut cpu_count_builder = UInt32Builder::with_capacity(n);
    let mut sched_count_builder = UInt32Builder::with_capacity(n);
    let mut host_builder = StringBuilder::with_capacity(n, 64 * n);
    let mut service_builder = StringBuilder::with_capacity(n, 32 * n);
    let mut date_builder = StringBuilder::with_capacity(n, 10 * n);

    for poll in polls {
        start_builder.append_value(poll.start_ns as i64);
        end_builder.append_value(poll.end_ns as i64);
        duration_builder.append_value(poll.duration_ns as i64);
        worker_id_builder.append_value(poll.worker_id);
        task_id_builder.append_value(poll.task_id);
        spawn_loc_builder.append_option(poll.spawn_loc.as_deref());
        ready_at_builder.append_option(poll.ready_at_ns.map(|value| value as i64));
        scheduling_delay_builder.append_option(poll.scheduling_delay_ns.map(|value| value as i64));
        scheduling_kind_builder.append_option(poll.scheduling_delay_kind.map(|kind| kind as u8));
        waker_task_id_builder.append_option(poll.waker_task_id);
        task_instrumented_builder.append_option(poll.task_instrumented);
        cpu_count_builder.append_value(poll.cpu_sample_count);
        sched_count_builder.append_value(poll.sched_sample_count);
        host_builder.append_value(&poll.host);
        service_builder.append_value(&poll.service);
        date_builder.append_value(&poll.date);
    }

    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(start_builder.finish()) as ArrayRef,
            Arc::new(end_builder.finish()) as ArrayRef,
            Arc::new(duration_builder.finish()) as ArrayRef,
            Arc::new(worker_id_builder.finish()) as ArrayRef,
            Arc::new(task_id_builder.finish()) as ArrayRef,
            Arc::new(spawn_loc_builder.finish()) as ArrayRef,
            Arc::new(ready_at_builder.finish()) as ArrayRef,
            Arc::new(scheduling_delay_builder.finish()) as ArrayRef,
            Arc::new(scheduling_kind_builder.finish()) as ArrayRef,
            Arc::new(waker_task_id_builder.finish()) as ArrayRef,
            Arc::new(task_instrumented_builder.finish()) as ArrayRef,
            Arc::new(cpu_count_builder.finish()) as ArrayRef,
            Arc::new(sched_count_builder.finish()) as ArrayRef,
            Arc::new(host_builder.finish()) as ArrayRef,
            Arc::new(service_builder.finish()) as ArrayRef,
            Arc::new(date_builder.finish()) as ArrayRef,
        ],
    )?;

    arrow_writer.write(&batch)?;
    arrow_writer.close()?;
    Ok(())
}

fn samples_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("timestamp_ns", DataType::Int64, false),
        Field::new("stack_id", DataType::FixedSizeBinary(16), false),
        // Nullable: null = off-runtime (not attributed to a worker).
        Field::new("worker_id", DataType::UInt32, true),
        Field::new("source", DataType::UInt8, false),
        Field::new("source_key", DataType::Utf8, false),
        Field::new("host", DataType::Utf8, false),
        Field::new("service", DataType::Utf8, false),
        Field::new("date", DataType::Utf8, false),
        // Nullable: null = sample not inside a poll (off-worker or between polls).
        Field::new("poll_duration_ns", DataType::Int64, true),
        // Nullable: null = sample not inside a poll or task has no spawn location.
        Field::new("spawn_location", DataType::Utf8, true),
        Field::new(
            "metadata",
            DataType::Map(
                Arc::new(Field::new(
                    "entries",
                    DataType::Struct(
                        vec![
                            Field::new("keys", DataType::Utf8, false),
                            Field::new("values", DataType::Utf8, true),
                        ]
                        .into(),
                    ),
                    false,
                )),
                false, // keys_sorted
            ),
            false,
        ),
        // Tracing spans enclosing this sample. Typically 0–2 entries.
        // Old part-files will not have this column; readers must handle absence.
        Field::new(
            "enclosing_spans",
            DataType::List(Arc::new(Field::new(
                "item",
                DataType::Struct(enclosing_span_fields()),
                true,
            ))),
            false, // list itself is non-null; empty list = no enclosing spans
        ),
    ]))
}

fn stacks_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("stack_id", DataType::FixedSizeBinary(16), false),
        Field::new(
            "frames",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            false,
        ),
    ]))
}

fn polls_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("start_ns", DataType::Int64, false),
        Field::new("end_ns", DataType::Int64, false),
        Field::new("duration_ns", DataType::Int64, false),
        Field::new("worker_id", DataType::UInt32, false),
        Field::new("task_id", DataType::UInt64, false),
        Field::new("spawn_loc", DataType::Utf8, true),
        // Nullable means the segment had no safe readiness evidence for this poll.
        Field::new("ready_at_ns", DataType::Int64, true),
        Field::new("scheduling_delay_ns", DataType::Int64, true),
        Field::new("scheduling_delay_kind", DataType::UInt8, true),
        Field::new("waker_task_id", DataType::UInt64, true),
        Field::new("task_instrumented", DataType::Boolean, true),
        Field::new("cpu_sample_count", DataType::UInt32, false),
        Field::new("sched_sample_count", DataType::UInt32, false),
        Field::new("host", DataType::Utf8, false),
        Field::new("service", DataType::Utf8, false),
        Field::new("date", DataType::Utf8, false),
    ]))
}

/// Fields of the enclosing_spans struct within the samples table.
/// Compact OTAP-aligned: only identity, duration, and completeness for the hot
/// flamegraph filter path. Full metadata lives in `spans/` only.
fn enclosing_span_fields() -> Fields {
    vec![
        Field::new("span_uid", DataType::FixedSizeBinary(16), false),
        Field::new("span_type_uid", DataType::FixedSizeBinary(16), false),
        Field::new("elapsed_ns", DataType::Int64, false),
        Field::new("details_complete", DataType::Boolean, false),
    ]
    .into()
}

/// Arrow schema for the `spans/` table (one row per span close summary).
fn spans_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("span_uid", DataType::FixedSizeBinary(16), false),
        Field::new("span_type_uid", DataType::FixedSizeBinary(16), false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("target", DataType::Utf8, true),
        Field::new("callsite_file", DataType::Utf8, true),
        Field::new("callsite_line", DataType::UInt32, true),
        Field::new("start_ns", DataType::Int64, false),
        Field::new("end_ns", DataType::Int64, false),
        Field::new("elapsed_ns", DataType::Int64, false),
        Field::new("active_ns", DataType::Int64, true),
        Field::new("observed_active_wall_ns", DataType::Int64, false),
        Field::new("detail_coverage_ns", DataType::Int64, false),
        Field::new("details_complete", DataType::Boolean, false),
        Field::new("concurrent", DataType::Boolean, false),
        Field::new("parent_span_uid", DataType::FixedSizeBinary(16), true),
        // Five-way time attribution (nullable until metadata consumed)
        Field::new("on_cpu_ns_est", DataType::Int64, true),
        Field::new("blocked_ns_est", DataType::Int64, true),
        Field::new("async_wait_ns", DataType::Int64, true),
        Field::new("scheduler_delay_ns", DataType::Int64, true),
        Field::new("unknown_ns", DataType::Int64, false),
        Field::new("cpu_sample_count", DataType::UInt32, false),
        Field::new("sched_sample_count", DataType::UInt32, false),
        Field::new("attribution_version", DataType::UInt16, false),
        Field::new("attribution_flags", DataType::UInt32, false),
        Field::new("unbalanced_exits", DataType::UInt32, false),
        Field::new("unbalanced_enters", DataType::UInt32, false),
        Field::new("identity_quality", DataType::Utf8, false),
        // Span attributes as Map<String, String>. Current producers always emit
        // String values (OTAP evolution to typed values remains documented but is
        // not yet implemented). Empty maps are written as empty maps (non-null).
        Field::new(
            "attributes",
            DataType::Map(
                Arc::new(Field::new(
                    "entries",
                    DataType::Struct(
                        vec![
                            Field::new("keys", DataType::Utf8, false),
                            Field::new("values", DataType::Utf8, true),
                        ]
                        .into(),
                    ),
                    false,
                )),
                false, // keys_sorted
            ),
            false,
        ),
        Field::new("source_key", DataType::Utf8, false),
        Field::new("host", DataType::Utf8, false),
        Field::new("service", DataType::Utf8, false),
        Field::new("date", DataType::Utf8, false),
    ]))
}

const SPAN_BATCH_ROWS: usize = 8 * 1024;

/// Write span close summaries to a Parquet file (the `spans/` table).
pub fn write_spans<W: Write + Send>(writer: W, spans: &[ResolvedSpan]) -> anyhow::Result<()> {
    write_spans_with_batch_size(writer, spans, SPAN_BATCH_ROWS)
}

fn write_spans_with_batch_size<W: Write + Send>(
    writer: W,
    spans: &[ResolvedSpan],
    batch_rows: usize,
) -> anyhow::Result<()> {
    anyhow::ensure!(batch_rows > 0, "span batch size must be positive");
    let schema = spans_schema();
    let props = WriterProperties::builder()
        .set_dictionary_enabled(true)
        .set_max_row_group_size(128 * 1024)
        .build();
    let mut arrow_writer = ArrowWriter::try_new(writer, schema.clone(), Some(props))?;

    if spans.is_empty() {
        arrow_writer.write(&build_spans_batch(schema, spans)?)?;
    } else {
        for chunk in spans.chunks(batch_rows) {
            arrow_writer.write(&build_spans_batch(schema.clone(), chunk)?)?;
        }
    }
    arrow_writer.close()?;
    Ok(())
}

fn build_spans_batch(schema: Arc<Schema>, spans: &[ResolvedSpan]) -> anyhow::Result<RecordBatch> {
    let n = spans.len();
    let mut span_uid_builder = FixedSizeBinaryBuilder::with_capacity(n, 16);
    let mut span_type_uid_builder = FixedSizeBinaryBuilder::with_capacity(n, 16);
    let mut kind_builder = StringBuilder::with_capacity(n, 8 * n);
    let mut name_builder = StringBuilder::with_capacity(n, 64 * n);
    let mut target_builder = StringBuilder::with_capacity(n, 64 * n);
    let mut callsite_file_builder = StringBuilder::with_capacity(n, 128 * n);
    let mut callsite_line_builder = UInt32Builder::with_capacity(n);
    let mut start_ns_builder = Int64Builder::with_capacity(n);
    let mut end_ns_builder = Int64Builder::with_capacity(n);
    let mut elapsed_ns_builder = Int64Builder::with_capacity(n);
    let mut active_ns_builder = Int64Builder::with_capacity(n);
    let mut observed_active_wall_ns_builder = Int64Builder::with_capacity(n);
    let mut detail_coverage_ns_builder = Int64Builder::with_capacity(n);
    let mut details_complete_builder = BooleanBuilder::with_capacity(n);
    let mut concurrent_builder = BooleanBuilder::with_capacity(n);
    let mut parent_span_uid_builder = FixedSizeBinaryBuilder::with_capacity(n, 16);
    let mut on_cpu_ns_est_builder = Int64Builder::with_capacity(n);
    let mut blocked_ns_est_builder = Int64Builder::with_capacity(n);
    let mut async_wait_ns_builder = Int64Builder::with_capacity(n);
    let mut scheduler_delay_ns_builder = Int64Builder::with_capacity(n);
    let mut unknown_ns_builder = Int64Builder::with_capacity(n);
    let mut cpu_sample_count_builder = UInt32Builder::with_capacity(n);
    let mut sched_sample_count_builder = UInt32Builder::with_capacity(n);
    let mut attribution_version_builder = arrow::array::UInt16Builder::with_capacity(n);
    let mut attribution_flags_builder = UInt32Builder::with_capacity(n);
    let mut unbalanced_exits_builder = UInt32Builder::with_capacity(n);
    let mut unbalanced_enters_builder = UInt32Builder::with_capacity(n);
    let mut identity_quality_builder = StringBuilder::with_capacity(n, 10 * n);

    // Attributes: Map<String, String>
    let attr_keys_builder = StringBuilder::new();
    let attr_values_builder = StringBuilder::new();
    let mut attr_map_builder =
        arrow::array::MapBuilder::new(None, attr_keys_builder, attr_values_builder);

    let mut source_key_builder = StringBuilder::with_capacity(n, 128 * n);
    let mut host_builder = StringBuilder::with_capacity(n, 64 * n);
    let mut service_builder = StringBuilder::with_capacity(n, 32 * n);
    let mut date_builder = StringBuilder::with_capacity(n, 10 * n);

    /// Convert u64 ns to i64, rejecting values that exceed i64::MAX.
    fn to_i64(val: u64) -> anyhow::Result<i64> {
        i64::try_from(val).map_err(|_| anyhow::anyhow!("timestamp {val} exceeds i64::MAX"))
    }

    for span in spans {
        span_uid_builder.append_value(span.span_uid)?;
        span_type_uid_builder.append_value(span.span_type_uid)?;
        kind_builder.append_value(&span.kind);
        name_builder.append_value(&span.name);
        target_builder.append_value(&span.target);
        callsite_file_builder.append_option(span.callsite_file.as_deref());
        callsite_line_builder.append_option(span.callsite_line);
        start_ns_builder.append_value(to_i64(span.start_ns)?);
        end_ns_builder.append_value(to_i64(span.end_ns)?);
        elapsed_ns_builder.append_value(to_i64(span.elapsed_ns)?);
        active_ns_builder.append_option(span.active_ns.map(to_i64).transpose()?);
        observed_active_wall_ns_builder.append_value(to_i64(span.observed_active_wall_ns)?);
        detail_coverage_ns_builder.append_value(to_i64(span.detail_coverage_ns)?);
        details_complete_builder.append_value(span.details_complete);
        concurrent_builder.append_value(span.concurrent);
        match span.parent_span_uid {
            Some(uid) => parent_span_uid_builder.append_value(uid)?,
            None => parent_span_uid_builder.append_null(),
        }
        on_cpu_ns_est_builder.append_option(span.on_cpu_ns_est.map(to_i64).transpose()?);
        blocked_ns_est_builder.append_option(span.blocked_ns_est.map(to_i64).transpose()?);
        async_wait_ns_builder.append_option(span.async_wait_ns.map(to_i64).transpose()?);
        scheduler_delay_ns_builder.append_option(span.scheduler_delay_ns.map(to_i64).transpose()?);
        unknown_ns_builder.append_value(to_i64(span.unknown_ns)?);
        cpu_sample_count_builder.append_value(span.cpu_sample_count);
        sched_sample_count_builder.append_value(span.sched_sample_count);
        attribution_version_builder.append_value(span.attribution_version);
        attribution_flags_builder.append_value(span.attribution_flags);
        unbalanced_exits_builder.append_value(span.unbalanced_exits);
        unbalanced_enters_builder.append_value(span.unbalanced_enters);
        identity_quality_builder.append_value(span.identity_quality);

        // Append attributes map
        for (k, v) in &span.attributes {
            attr_map_builder.keys().append_value(k);
            attr_map_builder.values().append_value(v);
        }
        attr_map_builder.append(true)?;

        source_key_builder.append_value(&span.source_key);
        host_builder.append_value(&span.host);
        service_builder.append_value(&span.service);
        date_builder.append_value(&span.date);
    }

    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(span_uid_builder.finish()) as ArrayRef,
            Arc::new(span_type_uid_builder.finish()) as ArrayRef,
            Arc::new(kind_builder.finish()) as ArrayRef,
            Arc::new(name_builder.finish()) as ArrayRef,
            Arc::new(target_builder.finish()) as ArrayRef,
            Arc::new(callsite_file_builder.finish()) as ArrayRef,
            Arc::new(callsite_line_builder.finish()) as ArrayRef,
            Arc::new(start_ns_builder.finish()) as ArrayRef,
            Arc::new(end_ns_builder.finish()) as ArrayRef,
            Arc::new(elapsed_ns_builder.finish()) as ArrayRef,
            Arc::new(active_ns_builder.finish()) as ArrayRef,
            Arc::new(observed_active_wall_ns_builder.finish()) as ArrayRef,
            Arc::new(detail_coverage_ns_builder.finish()) as ArrayRef,
            Arc::new(details_complete_builder.finish()) as ArrayRef,
            Arc::new(concurrent_builder.finish()) as ArrayRef,
            Arc::new(parent_span_uid_builder.finish()) as ArrayRef,
            Arc::new(on_cpu_ns_est_builder.finish()) as ArrayRef,
            Arc::new(blocked_ns_est_builder.finish()) as ArrayRef,
            Arc::new(async_wait_ns_builder.finish()) as ArrayRef,
            Arc::new(scheduler_delay_ns_builder.finish()) as ArrayRef,
            Arc::new(unknown_ns_builder.finish()) as ArrayRef,
            Arc::new(cpu_sample_count_builder.finish()) as ArrayRef,
            Arc::new(sched_sample_count_builder.finish()) as ArrayRef,
            Arc::new(attribution_version_builder.finish()) as ArrayRef,
            Arc::new(attribution_flags_builder.finish()) as ArrayRef,
            Arc::new(unbalanced_exits_builder.finish()) as ArrayRef,
            Arc::new(unbalanced_enters_builder.finish()) as ArrayRef,
            Arc::new(identity_quality_builder.finish()) as ArrayRef,
            Arc::new(attr_map_builder.finish()) as ArrayRef,
            Arc::new(source_key_builder.finish()) as ArrayRef,
            Arc::new(host_builder.finish()) as ArrayRef,
            Arc::new(service_builder.finish()) as ArrayRef,
            Arc::new(date_builder.finish()) as ArrayRef,
        ],
    )?;

    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::decode::EnclosingSpanSummary;

    #[test]
    fn test_write_and_read_samples() {
        let samples = vec![ResolvedSample {
            timestamp_ns: 1000,
            stack_id: [1u8; 16],
            worker_id: Some(1),
            source: 0,
            source_key: "2026-06-19/1450/shale/myhost/boot-1/123-0.bin.gz".to_string(),
            host: "myhost".to_string(),
            service: "shale".to_string(),
            date: "2026-06-19".to_string(),
            poll_duration_ns: Some(5_000_000),
            spawn_location: Some("src/main.rs:42".to_string()),
            enclosing_spans: vec![
                EnclosingSpanSummary {
                    span_uid: [2u8; 16],
                    span_type_uid: [3u8; 16],
                    elapsed_ns: 100,
                    details_complete: true,
                },
                EnclosingSpanSummary {
                    span_uid: [4u8; 16],
                    span_type_uid: [5u8; 16],
                    elapsed_ns: 200,
                    details_complete: false,
                },
            ],
        }];
        let metadata = HashMap::from([("version".to_string(), "1.0".to_string())]);

        let mut buf = Vec::new();
        write_samples(&mut buf, &samples, &metadata).unwrap();

        // Verify we can read it back
        let reader = ::parquet::arrow::arrow_reader::ParquetRecordBatchReader::try_new(
            bytes::Bytes::from(buf),
            1024,
        )
        .unwrap();
        let batches: Vec<_> = reader.into_iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert_eq!(batch.num_rows(), 1);

        let lists = batch
            .column_by_name("enclosing_spans")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::ListArray>()
            .unwrap();
        assert_eq!(lists.value_offsets(), &[0, 2]);
        let entries = lists
            .values()
            .as_any()
            .downcast_ref::<arrow::array::StructArray>()
            .unwrap();
        let span_uids = entries
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::FixedSizeBinaryArray>()
            .unwrap();
        let span_type_uids = entries
            .column(1)
            .as_any()
            .downcast_ref::<arrow::array::FixedSizeBinaryArray>()
            .unwrap();
        let elapsed = entries
            .column(2)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap();
        let complete = entries
            .column(3)
            .as_any()
            .downcast_ref::<arrow::array::BooleanArray>()
            .unwrap();
        assert_eq!(span_uids.value(0), &[2u8; 16]);
        assert_eq!(span_uids.value(1), &[4u8; 16]);
        assert_eq!(span_type_uids.value(0), &[3u8; 16]);
        assert_eq!(span_type_uids.value(1), &[5u8; 16]);
        assert_eq!((elapsed.value(0), elapsed.value(1)), (100, 200));
        assert!(complete.value(0));
        assert!(!complete.value(1));
    }

    #[test]
    fn test_write_and_read_polls_with_scheduling_evidence() {
        use crate::ingest::decode::SchedulingDelayKind;
        use arrow::array::Array;

        let polls = vec![
            ResolvedPoll {
                start_ns: 1_500,
                end_ns: 1_600,
                duration_ns: 100,
                worker_id: 2,
                task_id: 42,
                spawn_loc: Some("src/main.rs:10".to_string()),
                cpu_sample_count: 1,
                sched_sample_count: 0,
                host: "host-a".to_string(),
                service: "svc".to_string(),
                date: "2026-07-21".to_string(),
                ready_at_ns: Some(1_000),
                scheduling_delay_ns: Some(500),
                scheduling_delay_kind: Some(SchedulingDelayKind::Wake),
                waker_task_id: Some(7),
                task_instrumented: Some(true),
            },
            ResolvedPoll {
                start_ns: 2_000,
                end_ns: 2_100,
                duration_ns: 100,
                worker_id: 2,
                task_id: 42,
                spawn_loc: None,
                cpu_sample_count: 0,
                sched_sample_count: 0,
                host: "host-a".to_string(),
                service: "svc".to_string(),
                date: "2026-07-21".to_string(),
                ready_at_ns: None,
                scheduling_delay_ns: None,
                scheduling_delay_kind: None,
                waker_task_id: None,
                task_instrumented: None,
            },
        ];

        let mut buf = Vec::new();
        write_polls(&mut buf, &polls).unwrap();
        let mut reader = ::parquet::arrow::arrow_reader::ParquetRecordBatchReader::try_new(
            bytes::Bytes::from(buf),
            1024,
        )
        .unwrap();
        let batch = reader.next().unwrap().unwrap();
        let delay = batch
            .column_by_name("scheduling_delay_ns")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap();
        let kind = batch
            .column_by_name("scheduling_delay_kind")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::UInt8Array>()
            .unwrap();
        let instrumented = batch
            .column_by_name("task_instrumented")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::BooleanArray>()
            .unwrap();

        assert_eq!(delay.value(0), 500);
        assert_eq!(kind.value(0), SchedulingDelayKind::Wake as u8);
        assert!(instrumented.value(0));
        assert!(delay.is_null(1));
        assert!(kind.is_null(1));
        assert!(instrumented.is_null(1));
    }

    #[test]
    fn test_write_and_read_stacks() {
        let mut stacks: HashMap<[u8; 16], Vec<String>> = HashMap::new();
        stacks.insert(
            [2u8; 16],
            vec!["leaf".into(), "middle".into(), "root".into()],
        );

        let mut buf = Vec::new();
        write_stacks_dict(&mut buf, &stacks).unwrap();

        let reader = ::parquet::arrow::arrow_reader::ParquetRecordBatchReader::try_new(
            bytes::Bytes::from(buf),
            1024,
        )
        .unwrap();
        let batches: Vec<_> = reader.into_iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 1);
    }

    #[test]
    fn test_write_and_read_spans() {
        use crate::ingest::decode::ResolvedSpan;

        let span = ResolvedSpan {
            span_uid: [1u8; 16],
            span_type_uid: [2u8; 16],
            kind: "tracing".to_string(),
            name: "handle_request".to_string(),
            target: "my_crate".to_string(),
            callsite_file: Some("src/main.rs".to_string()),
            callsite_line: Some(42),
            start_ns: 1000,
            end_ns: 2000,
            elapsed_ns: 1000,
            active_ns: Some(800),
            observed_active_wall_ns: 800,
            detail_coverage_ns: 800,
            details_complete: true,
            concurrent: false,
            parent_span_uid: None,
            attributes: vec![("user_id".to_string(), "42".to_string())],
            on_cpu_ns_est: None,
            blocked_ns_est: None,
            async_wait_ns: None,
            scheduler_delay_ns: None,
            unknown_ns: 1000,
            cpu_sample_count: 3,
            sched_sample_count: 1,
            attribution_version: 1,
            attribution_flags: 0b1111,
            unbalanced_exits: 0,
            unbalanced_enters: 0,
            identity_quality: "metadata",
            source_key: "test".to_string(),
            host: "myhost".to_string(),
            service: "shale".to_string(),
            date: "2026-06-19".to_string(),
        };
        let spans = vec![span.clone(), span.clone(), span];

        let mut buf = Vec::new();
        write_spans_with_batch_size(&mut buf, &spans, 2).unwrap();

        let reader = ::parquet::arrow::arrow_reader::ParquetRecordBatchReader::try_new(
            bytes::Bytes::from(buf),
            1024,
        )
        .unwrap();
        let batches: Vec<_> = reader.into_iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(batches.iter().map(RecordBatch::num_rows).sum::<usize>(), 3);

        // Verify elapsed_ns is readable
        let elapsed_col = batches[0]
            .column_by_name("elapsed_ns")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap();
        assert_eq!(elapsed_col.value(0), 1000);
    }

    /// Verify that non-default span fields are persisted in their schema slots.
    #[test]
    fn test_write_and_read_spans_non_default_fields() {
        use crate::ingest::decode::ResolvedSpan;

        let spans = vec![ResolvedSpan {
            span_uid: [3u8; 16],
            span_type_uid: [4u8; 16],
            kind: "tracing".to_string(),
            name: "non_default_span".to_string(),
            target: "my_target".to_string(),
            callsite_file: Some("src/lib.rs".to_string()),
            callsite_line: Some(99),
            start_ns: 5000,
            end_ns: 15000,
            elapsed_ns: 10000,
            active_ns: Some(7000),
            observed_active_wall_ns: 6500,
            detail_coverage_ns: 6500,
            details_complete: false,
            concurrent: true,
            parent_span_uid: Some([5u8; 16]),
            attributes: vec![
                ("http.method".to_string(), "GET".to_string()),
                ("http.status".to_string(), "200".to_string()),
            ],
            on_cpu_ns_est: Some(3000),
            blocked_ns_est: Some(1000),
            async_wait_ns: Some(2000),
            scheduler_delay_ns: Some(500),
            unknown_ns: 3500,
            cpu_sample_count: 12,
            sched_sample_count: 4,
            attribution_version: 2,
            attribution_flags: 0b0101,
            unbalanced_exits: 3,
            unbalanced_enters: 7,
            identity_quality: "path",
            source_key: "test/key".to_string(),
            host: "host-b".to_string(),
            service: "svc-b".to_string(),
            date: "2026-07-15".to_string(),
        }];

        let mut buf = Vec::new();
        write_spans(&mut buf, &spans).unwrap();

        let reader = ::parquet::arrow::arrow_reader::ParquetRecordBatchReader::try_new(
            bytes::Bytes::from(buf),
            1024,
        )
        .unwrap();
        let batches: Vec<_> = reader.into_iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert_eq!(batch.num_rows(), 1);

        let expected_fields = [
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
            "active_ns",
            "observed_active_wall_ns",
            "detail_coverage_ns",
            "details_complete",
            "concurrent",
            "parent_span_uid",
            "on_cpu_ns_est",
            "blocked_ns_est",
            "async_wait_ns",
            "scheduler_delay_ns",
            "unknown_ns",
            "cpu_sample_count",
            "sched_sample_count",
            "attribution_version",
            "attribution_flags",
            "unbalanced_exits",
            "unbalanced_enters",
            "identity_quality",
            "attributes",
            "source_key",
            "host",
            "service",
            "date",
        ];
        assert_eq!(
            batch
                .schema()
                .fields()
                .iter()
                .map(|field| field.name().as_str())
                .collect::<Vec<_>>(),
            expected_fields
        );

        macro_rules! column {
            ($name:literal, $ty:ty) => {
                batch
                    .column_by_name($name)
                    .unwrap()
                    .as_any()
                    .downcast_ref::<$ty>()
                    .unwrap()
            };
        }

        assert_eq!(
            column!("span_uid", arrow::array::FixedSizeBinaryArray).value(0),
            &[3u8; 16]
        );
        assert_eq!(
            column!("span_type_uid", arrow::array::FixedSizeBinaryArray).value(0),
            &[4u8; 16]
        );
        assert_eq!(
            column!("kind", arrow::array::StringArray).value(0),
            "tracing"
        );
        assert_eq!(
            column!("name", arrow::array::StringArray).value(0),
            "non_default_span"
        );
        assert_eq!(
            column!("target", arrow::array::StringArray).value(0),
            "my_target"
        );
        assert_eq!(
            column!("callsite_file", arrow::array::StringArray).value(0),
            "src/lib.rs"
        );
        assert_eq!(
            column!("callsite_line", arrow::array::UInt32Array).value(0),
            99
        );
        assert_eq!(column!("start_ns", arrow::array::Int64Array).value(0), 5000);
        assert_eq!(column!("end_ns", arrow::array::Int64Array).value(0), 15000);
        assert_eq!(
            column!("elapsed_ns", arrow::array::Int64Array).value(0),
            10000
        );
        assert_eq!(
            column!("active_ns", arrow::array::Int64Array).value(0),
            7000
        );
        assert_eq!(
            column!("observed_active_wall_ns", arrow::array::Int64Array).value(0),
            6500
        );
        assert_eq!(
            column!("detail_coverage_ns", arrow::array::Int64Array).value(0),
            6500
        );
        assert!(!column!("details_complete", arrow::array::BooleanArray).value(0));
        assert!(column!("concurrent", arrow::array::BooleanArray).value(0));
        assert_eq!(
            column!("parent_span_uid", arrow::array::FixedSizeBinaryArray).value(0),
            &[5u8; 16]
        );
        assert_eq!(
            column!("on_cpu_ns_est", arrow::array::Int64Array).value(0),
            3000
        );
        assert_eq!(
            column!("blocked_ns_est", arrow::array::Int64Array).value(0),
            1000
        );
        assert_eq!(
            column!("async_wait_ns", arrow::array::Int64Array).value(0),
            2000
        );
        assert_eq!(
            column!("scheduler_delay_ns", arrow::array::Int64Array).value(0),
            500
        );
        assert_eq!(
            column!("unknown_ns", arrow::array::Int64Array).value(0),
            3500
        );
        assert_eq!(
            column!("cpu_sample_count", arrow::array::UInt32Array).value(0),
            12
        );
        assert_eq!(
            column!("sched_sample_count", arrow::array::UInt32Array).value(0),
            4
        );
        assert_eq!(
            column!("attribution_version", arrow::array::UInt16Array).value(0),
            2
        );
        assert_eq!(
            column!("attribution_flags", arrow::array::UInt32Array).value(0),
            0b0101
        );
        assert_eq!(
            column!("unbalanced_exits", arrow::array::UInt32Array).value(0),
            3
        );
        assert_eq!(
            column!("unbalanced_enters", arrow::array::UInt32Array).value(0),
            7
        );
        assert_eq!(
            column!("identity_quality", arrow::array::StringArray).value(0),
            "path"
        );
        assert_eq!(
            column!("source_key", arrow::array::StringArray).value(0),
            "test/key"
        );
        assert_eq!(
            column!("host", arrow::array::StringArray).value(0),
            "host-b"
        );
        assert_eq!(
            column!("service", arrow::array::StringArray).value(0),
            "svc-b"
        );
        assert_eq!(
            column!("date", arrow::array::StringArray).value(0),
            "2026-07-15"
        );

        let attributes = column!("attributes", arrow::array::MapArray);
        assert_eq!(attributes.value_offsets(), &[0, 2]);
        let entries = attributes.value(0);
        let keys = entries
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        let values = entries
            .column(1)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        assert_eq!((keys.value(0), values.value(0)), ("http.method", "GET"));
        assert_eq!((keys.value(1), values.value(1)), ("http.status", "200"));
    }

    /// Finding 4: Build an actual old-schema Parquet fixture WITHOUT the
    /// `enclosing_spans` column (simulating a pre-v4 part-file). The fixture
    /// faithfully includes the non-null metadata Map column that was present in
    /// the historical v3 samples schema, so the only difference from the
    /// current schema is the absent `enclosing_spans` column.
    /// Verify that `span_filter_matches` fails closed on this fixture.
    #[test]
    fn test_old_schema_parquet_fixture_no_enclosing_spans() {
        use arrow::array::{
            ArrayRef, FixedSizeBinaryBuilder, Int64Builder, MapBuilder, StringBuilder,
            UInt8Builder, UInt32Builder,
        };
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        use parquet::file::properties::WriterProperties;
        use std::sync::Arc;

        // Build old v3 schema: includes metadata Map column but omits
        // enclosing_spans (which was added in v4).
        let old_schema = Arc::new(Schema::new(vec![
            Field::new("timestamp_ns", DataType::Int64, false),
            Field::new("stack_id", DataType::FixedSizeBinary(16), false),
            Field::new("worker_id", DataType::UInt32, true),
            Field::new("source", DataType::UInt8, false),
            Field::new("source_key", DataType::Utf8, false),
            Field::new("host", DataType::Utf8, false),
            Field::new("service", DataType::Utf8, false),
            Field::new("date", DataType::Utf8, false),
            Field::new("poll_duration_ns", DataType::Int64, true),
            Field::new("spawn_location", DataType::Utf8, true),
            // The metadata Map column was present in v3.
            Field::new(
                "metadata",
                DataType::Map(
                    Arc::new(Field::new(
                        "entries",
                        DataType::Struct(
                            vec![
                                Field::new("keys", DataType::Utf8, false),
                                Field::new("values", DataType::Utf8, true),
                            ]
                            .into(),
                        ),
                        false,
                    )),
                    false, // keys_sorted
                ),
                false,
            ),
        ]));

        let mut ts_builder = Int64Builder::with_capacity(1);
        let mut stack_id_builder = FixedSizeBinaryBuilder::with_capacity(1, 16);
        let mut worker_id_builder = UInt32Builder::with_capacity(1);
        let mut source_builder = UInt8Builder::with_capacity(1);
        let mut source_key_builder = StringBuilder::with_capacity(1, 64);
        let mut host_builder = StringBuilder::with_capacity(1, 64);
        let mut service_builder = StringBuilder::with_capacity(1, 32);
        let mut date_builder = StringBuilder::with_capacity(1, 10);
        let mut poll_duration_builder = Int64Builder::with_capacity(1);
        let mut spawn_location_builder = StringBuilder::with_capacity(1, 64);

        // Build metadata map column with a representative entry.
        let map_keys_builder = StringBuilder::new();
        let map_values_builder = StringBuilder::new();
        let mut map_builder = MapBuilder::new(None, map_keys_builder, map_values_builder);

        ts_builder.append_value(1000);
        stack_id_builder.append_value([1u8; 16]).unwrap();
        worker_id_builder.append_value(1);
        source_builder.append_value(0);
        source_key_builder.append_value("old/key/path.bin.gz");
        host_builder.append_value("old-host");
        service_builder.append_value("old-svc");
        date_builder.append_value("2024-01-01");
        poll_duration_builder.append_value(5_000_000);
        spawn_location_builder.append_value("src/old.rs:10");

        // Populate metadata map with a source_key entry (matching historical behavior).
        map_builder.keys().append_value("source_key");
        map_builder.values().append_value("old/key/path.bin.gz");
        map_builder.append(true).unwrap();

        let batch = RecordBatch::try_new(
            old_schema.clone(),
            vec![
                Arc::new(ts_builder.finish()) as ArrayRef,
                Arc::new(stack_id_builder.finish()) as ArrayRef,
                Arc::new(worker_id_builder.finish()) as ArrayRef,
                Arc::new(source_builder.finish()) as ArrayRef,
                Arc::new(source_key_builder.finish()) as ArrayRef,
                Arc::new(host_builder.finish()) as ArrayRef,
                Arc::new(service_builder.finish()) as ArrayRef,
                Arc::new(date_builder.finish()) as ArrayRef,
                Arc::new(poll_duration_builder.finish()) as ArrayRef,
                Arc::new(spawn_location_builder.finish()) as ArrayRef,
                Arc::new(map_builder.finish()) as ArrayRef,
            ],
        )
        .unwrap();

        let props = WriterProperties::builder()
            .set_dictionary_enabled(true)
            .build();
        let mut buf = Vec::new();
        let mut arrow_writer =
            ArrowWriter::try_new(&mut buf, old_schema.clone(), Some(props)).unwrap();
        arrow_writer.write(&batch).unwrap();
        arrow_writer.close().unwrap();

        // Verify: reading back the old-schema file, the enclosing_spans column is absent
        // but metadata is present.
        let reader = ::parquet::arrow::arrow_reader::ParquetRecordBatchReader::try_new(
            bytes::Bytes::from(buf.clone()),
            1024,
        )
        .unwrap();
        let batches: Vec<_> = reader.into_iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(batches.len(), 1);
        let read_batch = &batches[0];
        assert_eq!(read_batch.num_rows(), 1);
        // Confirm metadata column IS present (faithful v3 representation).
        assert!(
            read_batch.column_by_name("metadata").is_some(),
            "old v3 schema fixture must have the metadata Map column"
        );
        // Confirm enclosing_spans column is absent.
        assert!(
            read_batch.column_by_name("enclosing_spans").is_none(),
            "old v3 schema fixture must NOT have enclosing_spans column"
        );

        // Now test that the span_filter_matches function fails closed.
        // Import from aggregate module.
        use crate::ingest::aggregate::span_filter_matches;
        let wanted_uid = [42u8; 16];
        let result = span_filter_matches(read_batch, 0, &wanted_uid, None, None);
        assert!(
            !result,
            "span_filter_matches must fail closed (return false) for old v3 schema without enclosing_spans"
        );
    }
}

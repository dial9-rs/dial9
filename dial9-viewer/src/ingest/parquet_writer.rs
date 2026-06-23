//! Write samples and stacks dictionary as Parquet files.

use arrow::array::{
    ArrayRef, FixedSizeBinaryBuilder, ListBuilder, StringBuilder, UInt8Builder, UInt32Builder,
    UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use super::decode::ResolvedPoll;
use super::decode::ResolvedSample;

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
        Field::new("cpu_sample_count", DataType::UInt32, false),
        Field::new("sched_sample_count", DataType::UInt32, false),
        Field::new("host", DataType::Utf8, false),
        Field::new("service", DataType::Utf8, false),
        Field::new("date", DataType::Utf8, false),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(batches[0].num_rows(), 1);
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
}

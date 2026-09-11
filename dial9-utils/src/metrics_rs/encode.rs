//! Writing captured metrics into the trace, one event per metric.
//!
//! Schema registration is name-keyed and immutable for the life of a segment,
//! re-registering a name with a different field list fails.

use std::collections::HashMap;
use std::time::Duration;

use dial9_core::encoder::ThreadLocalEncoder;
use dial9_core::rate_limited;
use dial9_trace_format::encoder::Schema;
use dial9_trace_format::schema::{FieldAnnotation, FieldDef, SchemaEntry};
use dial9_trace_format::types::{FieldType, FieldValue};

use super::capture::{Field, MetricSample, SampleValue, UNIT_ANNOTATION_KEY};

/// Prefix on every schema this source writes.
const SCHEMA_PREFIX: &str = "metricsrs:";

/// Annotation key telling the viewer how to chart a field. Matches the key the
/// `TraceEvent` derive emits for `#[traceevent(kind = "...")]`.
const KIND_ANNOTATION_KEY: &str = "kind";

/// Charted as a running total the viewer differences itself.
const KIND_COUNTER: &str = "counter";
/// Charted as the value observed.
const KIND_GAUGE: &str = "gauge";

/// Per-metric schemas, built on first sight and reused after.
#[derive(Debug, Default)]
pub(crate) struct Schemas {
    by_metric: HashMap<String, Schema>,
}

impl Schemas {
    /// How many metrics have a schema. Bounded by the source's `max_metrics`,
    /// which is what keeps the encoder's own schema cache from thrashing.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_metric.len()
    }

    fn get_or_build(&mut self, sample: &MetricSample) -> &Schema {
        self.by_metric
            .entry(sample.name.clone())
            .or_insert_with(|| build_schema(&sample.name, &sample.fields))
    }
}

/// Write one metric's event. Returns whether it was recorded.
pub(crate) fn write(
    encoder: &mut ThreadLocalEncoder<'_>,
    schemas: &mut Schemas,
    timestamp_ns: u64,
    sample: &MetricSample,
    values: &mut Vec<FieldValue>,
) -> bool {
    values.clear();
    values.extend(sample.fields.iter().map(|field| field_value(&field.value)));

    let schema = schemas.get_or_build(sample);
    match encoder.write_event(schema, timestamp_ns, values) {
        Ok(()) => true,
        Err(error) => {
            rate_limited!(Duration::from_secs(60), {
                tracing::error!(
                    metric = %sample.name,
                    "encoder rejected a metrics.rs event; dropped: {error}"
                );
            });
            false
        }
    }
}

fn build_schema(metric: &str, fields: &[Field]) -> Schema {
    let mut defs = Vec::with_capacity(fields.len());
    let mut annotations = Vec::with_capacity(fields.len());

    for (index, field) in fields.iter().enumerate() {
        // A metric's field count is fixed and small (one, or two plus the
        // configured percentiles), so this never truncates in practice.
        let index = index as u16;
        annotations.push(FieldAnnotation::new(
            index,
            KIND_ANNOTATION_KEY,
            kind_annotation_value(&field.value),
        ));
        if let Some(unit) = field.unit {
            annotations.push(FieldAnnotation::new(index, UNIT_ANNOTATION_KEY, unit));
        }
        defs.push(FieldDef::new(field.name.as_ref(), field_type(&field.value)));
    }

    Schema::from_entry(SchemaEntry::with_annotations(
        format!("{SCHEMA_PREFIX}{metric}"),
        defs,
        annotations,
    ))
}

/// The wire type a field carries for every event of its metric. Fixed by the
/// value's shape, which is fixed by the metric's type.
fn field_type(value: &SampleValue) -> FieldType {
    match value {
        SampleValue::CounterDelta(_) => FieldType::Varint,
        SampleValue::Gauge(_) => FieldType::F64,
        SampleValue::OptionalGauge(_) => FieldType::OptionalF64,
    }
}

fn kind_annotation_value(value: &SampleValue) -> &'static str {
    match value {
        SampleValue::CounterDelta(_) => KIND_COUNTER,
        SampleValue::Gauge(_) | SampleValue::OptionalGauge(_) => KIND_GAUGE,
    }
}

fn field_value(value: &SampleValue) -> FieldValue {
    match value {
        SampleValue::CounterDelta(v) => FieldValue::Varint(*v),
        SampleValue::Gauge(v) => FieldValue::F64(*v),
        SampleValue::OptionalGauge(Some(v)) => FieldValue::F64(*v),
        SampleValue::OptionalGauge(None) => FieldValue::None,
    }
}

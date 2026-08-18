//! Tracing subscriber layer that emits span events into dial9 traces.
//!
//! Requires the `tracing-layer` feature.
//!
//! # Usage
//!
//! ```ignore
//! use dial9_utils::tracing_layer::Dial9TracingLayer;
//! use tracing_subscriber::prelude::*;
//!
//! tracing_subscriber::registry()
//!     .with(Dial9TracingLayer::new())
//!     .init();
//! ```
//!
//! The layer emits events only on threads with an enabled dial9 handle. The
//! Tokio runtime integration installs that handle on its runtime threads.
//! Events carry the current Tokio task ID when available; outside a Tokio task,
//! the task ID is explicitly absent.
//!
//! # High-frequency spans
//!
//! Every span enter and exit produces a trace event. If you instrument tight
//! loops, the volume can be large. Libraries like the AWS SDK emit many
//! internal spans (`deserialization`, `serialization`, `try_attempt`, etc.)
//! that can produce over 100K span events per second and quickly fill the
//! trace buffer.
//!
//! You can use per-layer `tracing_subscriber::filter::Targets` filter to restrict
//! which spans reach the dial9 layer. This keeps your fmt/logging layer
//! unaffected while controlling trace volume:
//!
//! ```ignore
//! use dial9_utils::tracing_layer::Dial9TracingLayer;
//! use tracing_subscriber::prelude::*;
//!
//! tracing_subscriber::registry()
//!     .with(tracing_subscriber::fmt::layer())
//!     .with(
//!         Dial9TracingLayer::new().with_filter(
//!             tracing_subscriber::filter::Targets::new()
//!                 .with_target("my_app", tracing::Level::DEBUG)
//!                 .with_default(tracing::Level::WARN),
//!         ),
//!     )
//!     .init();
//! ```
//!
//! This captures all spans from `my_app` while filtering out noisy
//! third-party spans (AWS SDK, hyper, tower, etc.).
//!
//! # Overhead
//!
//! Each span enter+exit pair costs roughly **650-800ns** total (tracing
//! dispatch plus dial9 encoding), of which dial9 encoding is the majority:
//! the same span through a bare `tracing_subscriber::registry()` costs
//! ~100-200ns. Measured with an in-memory writer on a `current_thread`
//! runtime pinned to one core of an AMD EPYC 9R14 (see the `single_thread`
//! group in `benches/tracing_layer_bench.rs`); absolute numbers scale with
//! hardware. Cost grows with the number of span fields and scales linearly
//! with nesting depth, so the layer is suitable for production use with
//! appropriate span filtering.

use dial9_core::clock::clock_monotonic_ns;
use dial9_core::handle::Dial9Handle;
use dial9_trace_format::TraceEvent;
use dial9_trace_format::encoder::Schema;
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};
use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use tracing::callsite::Identifier;
use tracing::span;
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

const TOKIO_TASK_ID_FIELD: &str = "dial9.tokio.task_id";

/// Emitted once when a span closes, so the viewer can recycle its id. Mirrors
/// the `SpanCloseEvent` the ad-hoc span wrappers emit (same schema, distinct
/// producer).
#[derive(TraceEvent)]
#[traceevent(wire_slot)]
struct SpanCloseEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    span_id: u64,
}

// ── Per-callsite schema cache ───────────────────────────────────────────────

/// Cached schemas for a single callsite (one for enter, one for exit).
#[derive(Clone)]
struct CallsiteSchemas {
    enter: Schema,
    exit: Schema,
    /// Field names from the callsite metadata, in order.
    field_names: Vec<&'static str>,
}

/// Build the enter and exit schemas for a callsite.
fn build_callsite_schemas(meta: &'static tracing::Metadata<'static>) -> CallsiteSchemas {
    let file = meta.file().unwrap_or("unknown");
    let line = meta.line().unwrap_or(0);
    let schema_id = format!("{}::{}:{}:{}", meta.target(), meta.name(), file, line);

    // Base fields present on all span events
    let mut enter_fields = vec![
        FieldDef::new(TOKIO_TASK_ID_FIELD, FieldType::OptionalVarint),
        FieldDef::new("span_id", FieldType::Varint),
        FieldDef::new("parent_span_id", FieldType::OptionalVarint),
        FieldDef::new("span_name", FieldType::PooledString),
    ];
    let mut exit_fields = vec![
        FieldDef::new(TOKIO_TASK_ID_FIELD, FieldType::OptionalVarint),
        FieldDef::new("span_id", FieldType::Varint),
        FieldDef::new("span_name", FieldType::PooledString),
    ];

    // Add user-defined fields as optional interned strings
    let mut field_names = Vec::new();
    for field in meta.fields() {
        let name = field.name();
        field_names.push(name);
        let def = FieldDef::new(name.to_string(), FieldType::OptionalPooledString);
        enter_fields.push(def.clone());
        exit_fields.push(def);
    }

    CallsiteSchemas {
        enter: Schema::new(&format!("SpanEnter:{schema_id}"), enter_fields),
        exit: Schema::new(&format!("SpanExit:{schema_id}"), exit_fields),
        field_names,
    }
}

// ── Per-span storage ────────────────────────────────────────────────────────

/// Data stored in span extensions, captured at `on_new_span` and updated by `on_record`.
struct SpanData {
    meta: &'static tracing::Metadata<'static>,
    parent_id: Option<span::Id>,
    /// Field values keyed by field name.
    field_values: Vec<(&'static str, String)>,
}

impl fmt::Debug for SpanData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpanData")
            .field("name", &self.meta.name())
            .finish_non_exhaustive()
    }
}

/// Visitor that collects span field values.
struct FieldVisitor<'a> {
    values: &'a mut Vec<(&'static str, String)>,
}

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.upsert(field.name(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.upsert(field.name(), value.to_owned());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.upsert(field.name(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.upsert(field.name(), value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.upsert(field.name(), value.to_string());
    }
}

impl FieldVisitor<'_> {
    fn upsert(&mut self, name: &'static str, value: String) {
        if let Some(entry) = self.values.iter_mut().find(|(k, _)| *k == name) {
            entry.1 = value;
        } else {
            self.values.push((name, value));
        }
    }
}

// ── Layer ───────────────────────────────────────────────────────────────────

/// A [`tracing_subscriber::Layer`] that emits span enter/exit events into
/// the dial9 trace buffer.
///
/// Each unique callsite gets its own wire schema with typed fields,
/// avoiding the overhead of a generic `StringMap` encoding. Field values
/// are interned as `PooledString`s for compact wire representation.
///
/// # Setup
///
/// ```ignore
/// use dial9_utils::tracing_layer::Dial9TracingLayer;
/// use tracing_subscriber::prelude::*;
///
/// tracing_subscriber::registry()
///     .with(Dial9TracingLayer::new())
///     .init();
/// ```
pub struct Dial9TracingLayer {
    schemas: Mutex<HashMap<Identifier, CallsiteSchemas>>,
}

impl fmt::Debug for Dial9TracingLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dial9TracingLayer").finish_non_exhaustive()
    }
}

impl Dial9TracingLayer {
    /// Create a new layer.
    pub fn new() -> Self {
        Self {
            schemas: Mutex::new(HashMap::new()),
        }
    }

    fn get_schemas(&self, meta: &'static tracing::Metadata<'static>) -> CallsiteSchemas {
        let id = meta.callsite();
        let mut cache = self.schemas.lock().unwrap();
        cache
            .entry(id)
            .or_insert_with(|| build_callsite_schemas(meta))
            .clone()
    }
}

impl Default for Dial9TracingLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Look up a field value by name in the span's field list.
fn field_value<'a>(fields: &'a [(&str, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.as_str())
}

/// The current Tokio task id, converted to the same `u64` used by dial9's
/// runtime poll events. `tokio::task::Id` is intentionally opaque, but its
/// `Hash` implementation writes the underlying id as one `u64`.
fn current_task_id() -> Option<u64> {
    use std::hash::{Hash, Hasher};

    struct U64Extractor(u64);

    impl Hasher for U64Extractor {
        fn finish(&self) -> u64 {
            self.0
        }

        fn write(&mut self, _bytes: &[u8]) {
            debug_assert!(false, "tokio task id hashed as bytes")
        }

        fn write_u64(&mut self, value: u64) {
            self.0 = value;
        }
    }

    tokio::task::try_id().map(|id| {
        let mut extractor = U64Extractor(0);
        id.hash(&mut extractor);
        extractor.finish()
    })
}

impl<S> Layer<S> for Dial9TracingLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        if !Dial9Handle::current().is_enabled() {
            return;
        }
        let mut field_values = Vec::new();
        attrs.record(&mut FieldVisitor {
            values: &mut field_values,
        });

        let data = SpanData {
            meta: attrs.metadata(),
            parent_id: attrs.parent().cloned(),
            field_values,
        };

        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(data);
        }
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        if !Dial9Handle::current().is_enabled() {
            return;
        }
        if let Some(span) = ctx.span(id) {
            let mut extensions = span.extensions_mut();
            if let Some(data) = extensions.get_mut::<SpanData>() {
                values.record(&mut FieldVisitor {
                    values: &mut data.field_values,
                });
            }
        }
    }

    fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
        let handle = Dial9Handle::current();
        if !handle.is_enabled() {
            return;
        }
        handle.with_encoder(|enc| {
            let task_id = current_task_id();
            let span_id = id.into_u64();
            let ts = clock_monotonic_ns();

            let Some(span_ref) = ctx.span(id) else { return };
            let ext = span_ref.extensions();
            let Some(data) = ext.get::<SpanData>() else {
                return;
            };

            let schemas = self.get_schemas(data.meta);

            // We only use explicit parents (span!(parent: &x, ..)), not contextual
            // parents (ctx.current_span()), because contextual parenting is
            // unreliable across tasks on the same worker thread. See:
            // https://chesedo.me/blog/rust-tracing-incorrect-parent-spans-async-futures-joinset/
            // The viewer infers nesting from timestamp containment instead.
            let parent_span_id = data.parent_id.as_ref().map(|id| id.into_u64());
            let span_name = data.meta.name();

            // Encode directly into the thread-local buffer (no clone needed)
            let mut values = Vec::with_capacity(4 + schemas.field_names.len());
            match task_id {
                Some(task_id) => values.push(FieldValue::Varint(task_id)),
                None => values.push(FieldValue::None),
            }
            values.push(FieldValue::Varint(span_id));
            match parent_span_id {
                Some(pid) => values.push(FieldValue::Varint(pid)),
                None => values.push(FieldValue::None),
            }
            values.push(FieldValue::PooledString(enc.intern_string(span_name)));
            for &name in &schemas.field_names {
                match field_value(&data.field_values, name) {
                    Some(v) => values.push(FieldValue::PooledString(enc.intern_string(v))),
                    None => values.push(FieldValue::None),
                }
            }
            if let Err(e) = enc.write_event(&schemas.enter, ts, &values) {
                dial9_core::rate_limit::rate_limited!(std::time::Duration::from_secs(60), {
                    tracing::error!(span = %span_name, "dropping span-enter event: {e}");
                });
            }
        });
    }

    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        let handle = Dial9Handle::current();
        if !handle.is_enabled() {
            return;
        }
        handle.with_encoder(|enc| {
            let task_id = current_task_id();
            let span_id = id.into_u64();
            let ts = clock_monotonic_ns();

            let Some(span_ref) = ctx.span(id) else { return };
            let ext = span_ref.extensions();
            let Some(data) = ext.get::<SpanData>() else {
                return;
            };

            let schemas = self.get_schemas(data.meta);
            let span_name = data.meta.name();

            let mut values = Vec::with_capacity(3 + schemas.field_names.len());
            match task_id {
                Some(task_id) => values.push(FieldValue::Varint(task_id)),
                None => values.push(FieldValue::None),
            }
            values.push(FieldValue::Varint(span_id));
            values.push(FieldValue::PooledString(enc.intern_string(span_name)));
            for &name in &schemas.field_names {
                match field_value(&data.field_values, name) {
                    Some(v) => values.push(FieldValue::PooledString(enc.intern_string(v))),
                    None => values.push(FieldValue::None),
                }
            }
            if let Err(e) = enc.write_event(&schemas.exit, ts, &values) {
                dial9_core::rate_limit::rate_limited!(std::time::Duration::from_secs(60), {
                    tracing::error!(span = %span_name, "dropping span-exit event: {e}");
                });
            }
        });
    }

    fn on_close(&self, id: span::Id, _ctx: Context<'_, S>) {
        let Some(handle) = Dial9Handle::try_current() else {
            return;
        };

        handle.record_event(SpanCloseEvent {
            timestamp_ns: clock_monotonic_ns(),
            span_id: id.into_u64(),
        });
    }
}

//! Tracing subscriber layer that emits span events into dial9 traces.
//!
//! Requires the `tracing-layer` feature.
//!
//! # Usage
//!
//! ```ignore
//! use dial9_tokio_telemetry::tracing_layer::Dial9TracingLayer;
//! use tracing_subscriber::prelude::*;
//!
//! tracing_subscriber::registry()
//!     .with(Dial9TracingLayer::new())
//!     .init();
//! ```
//!
//! The layer emits events only on threads owned by a dial9-traced runtime.
//! On other threads, span enter/exit is silently skipped.
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
//! use dial9_tokio_telemetry::tracing_layer::Dial9TracingLayer;
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
//! Each span enter+exit pair costs roughly **300ns** total (tracing dispatch
//! plus dial9 encoding), of which **~50-100ns** is the dial9 encoding overhead.
//! Measured with an in-memory writer on a `current_thread` runtime to
//! isolate encoding from I/O. This scales linearly with nesting depth and is
//! comparable to the cost of a single poll event, so the layer is suitable
//! for production use with appropriate span filtering.

use crate::telemetry::events::current_tid;
use crate::telemetry::{Dial9Handle, clock_monotonic_ns, current_worker_id};
use dial9_trace_format::TraceEvent;
use dial9_trace_format::encoder::Schema;
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};
use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::callsite::Identifier;
use tracing::span;
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

/// Process-local monotonically increasing counter for stable span instance
/// identity. Unlike `tracing::span::Id`, these are never recycled.
///
/// Starts at 1 so that 0 is reserved as a sentinel for "telemetry was disabled
/// at span creation" (see the minimal close event in `on_close`). The counter
/// will not wrap: allocating one ID per nanosecond would exhaust u64 after ~584
/// years. If exhaustion ever occurs (e.g. a bug calling fetch_add in a tight
/// loop), the check in `allocate_instance_id` panics rather than silently
/// reusing ID 0 or any previous ID.
static NEXT_SPAN_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a new unique span instance ID. Panics if the counter is exhausted
/// (would wrap to zero) — this would violate the uniqueness invariant and mean
/// the process has created 2^64 - 1 spans, which is physically impossible under
/// normal operation.
///
/// Uses `fetch_update` (CAS loop) instead of `fetch_add` so the counter is
/// **never mutated** on exhaustion: if the current value is `u64::MAX`, the
/// increment would wrap, so the CAS fails without writing and we panic with an
/// un-corrupted counter.
fn allocate_instance_id() -> u64 {
    NEXT_SPAN_INSTANCE_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("span instance ID counter exhausted (would wrap to zero); this is a bug")
}

#[derive(TraceEvent)]
#[traceevent(wire_slot)]
struct SpanCloseEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    /// Existing tracing wire ID (kept for old viewer reconstruction).
    span_id: u64,
    /// Process-local stable instance identity (never recycled).
    span_instance_id: u64,
    /// Lifecycle creation timestamp from `on_new_span`.
    start_timestamp_ns: u64,
    /// Timestamp of the first `on_enter`, or `None` if the span was never entered.
    first_enter_timestamp_ns: Option<u64>,
    /// Accumulated active thread time (sum of balanced enter→exit intervals).
    #[traceevent(unit = "ns")]
    active_ns: u64,
    /// Span name from tracing metadata.
    span_name: String,
    /// Tracing target (module path).
    target: String,
    /// Source file of the callsite, if available.
    file: Option<String>,
    /// Source line of the callsite, if available.
    line: Option<u32>,
    /// Stable instance ID of the explicit parent span, if any.
    parent_span_instance_id: Option<u64>,
    /// Final field values as key-value pairs at close time.
    attributes: Vec<(String, String)>,
    /// Number of unbalanced enters observed (quality flag).
    /// Non-zero indicates the `active_ns` may be inaccurate.
    unbalanced_enters: u32,
    /// Whether the span was entered concurrently from multiple threads.
    concurrent: u32,
    /// Whether `active_ns` saturated (hit `u64::MAX`) during accumulation.
    /// Non-zero indicates the reported `active_ns` is a lower bound.
    saturated: u32,
    /// Whether exact drop/loss counts are available for this span's events.
    /// When `0`, loss is **unobservable**: the backend must classify dropped
    /// enter/exit events as Unknown rather than assuming zero loss. The producer
    /// cannot currently observe drops in the thread-local buffer path, so this
    /// field is always `0` in the current implementation.
    loss_observable: u32,
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
        FieldDef::new("worker_id", FieldType::Varint),
        FieldDef::new("span_id", FieldType::Varint),
        FieldDef::new("span_instance_id", FieldType::Varint),
        FieldDef::new("tid", FieldType::Varint),
        FieldDef::new("parent_span_id", FieldType::OptionalVarint),
        FieldDef::new("span_name", FieldType::PooledString),
    ];
    let mut exit_fields = vec![
        FieldDef::new("worker_id", FieldType::Varint),
        FieldDef::new("span_id", FieldType::Varint),
        FieldDef::new("span_instance_id", FieldType::Varint),
        FieldDef::new("tid", FieldType::Varint),
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

/// Per-thread LIFO stack of enter timestamps for one span instance.
///
/// Supports same-thread re-entry (the same thread entering the span while
/// already inside it) and concurrent multi-thread entry. Each `on_enter`
/// pushes a timestamp keyed by tid; each `on_exit` pops the most recent
/// entry for the calling tid. This guarantees correct `active_ns`
/// accumulation even under re-entrant or concurrent use.
#[derive(Debug, Default)]
struct EnterStack {
    /// (tid, enter_timestamp_ns) — most recent entries at the end.
    entries: Vec<(u64, u64)>,
}

impl EnterStack {
    /// Push an enter event for `tid` at `ts`.
    fn push(&mut self, tid: u64, ts: u64) {
        self.entries.push((tid, ts));
    }

    /// Pop the most recent enter for `tid` (LIFO within the same tid).
    /// Returns `Some(enter_ts)` on success, `None` if no matching enter exists.
    fn pop(&mut self, tid: u64) -> Option<u64> {
        // Search from the back for the most recent entry with this tid.
        if let Some(pos) = self.entries.iter().rposition(|(t, _)| *t == tid) {
            let (_, ts) = self.entries.remove(pos);
            Some(ts)
        } else {
            None
        }
    }

    /// Number of outstanding (un-exited) enters across all threads.
    fn outstanding(&self) -> u32 {
        self.entries.len() as u32
    }

    /// Number of distinct tids currently in the stack.
    fn distinct_tids(&self) -> usize {
        let mut seen = Vec::new();
        for &(tid, _) in &self.entries {
            if !seen.contains(&tid) {
                seen.push(tid);
            }
        }
        seen.len()
    }
}

/// Data stored in span extensions, captured at `on_new_span` and updated by `on_record`.
struct SpanData {
    meta: &'static tracing::Metadata<'static>,
    /// Stable process-local instance ID (never recycled).
    instance_id: u64,
    parent_id: Option<span::Id>,
    /// Stable instance ID of the explicit parent span (resolved at creation time).
    parent_instance_id: Option<u64>,
    /// Field values keyed by field name.
    field_values: Vec<(&'static str, String)>,
    /// Monotonic timestamp when `on_new_span` was called.
    start_timestamp_ns: u64,
    /// Timestamp of the first `on_enter`, or `None` if never entered.
    first_enter_timestamp_ns: Option<u64>,
    /// Accumulated active thread time in nanoseconds (sum of enter→exit intervals).
    /// Uses saturating arithmetic; an overflow sets the `saturated` quality flag.
    active_ns: u64,
    /// Per-tid LIFO enter stack. Correctly handles:
    /// - Same-thread re-entry (multiple enters before exit on one tid)
    /// - Multi-thread concurrent entry (different tids entering simultaneously)
    enter_stack: EnterStack,
    /// Count of unbalanced exits detected (exit without matching enter).
    unbalanced_enters: u32,
    /// Whether this span was entered concurrently from multiple threads.
    concurrent: bool,
    /// Whether `active_ns` saturated during accumulation (quality flag).
    saturated: bool,
    /// Handle captured at span creation time. Used by `on_close` so the close
    /// event is emitted even when the final span handle is dropped on a thread
    /// that is not owned by a dial9 runtime.
    handle: Dial9Handle,
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
/// use dial9_tokio_telemetry::tracing_layer::Dial9TracingLayer;
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

impl<S> Layer<S> for Dial9TracingLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let handle = Dial9Handle::current();
        if !handle.is_enabled() {
            return;
        }
        let mut field_values = Vec::new();
        attrs.record(&mut FieldVisitor {
            values: &mut field_values,
        });

        let instance_id = allocate_instance_id();
        let start_timestamp_ns = clock_monotonic_ns();

        // Resolve explicit parent's stable instance ID if available.
        let parent_instance_id = attrs.parent().and_then(|parent_id| {
            ctx.span(parent_id).and_then(|parent_span| {
                parent_span
                    .extensions()
                    .get::<SpanData>()
                    .map(|d| d.instance_id)
            })
        });

        let data = SpanData {
            meta: attrs.metadata(),
            instance_id,
            parent_id: attrs.parent().cloned(),
            parent_instance_id,
            field_values,
            start_timestamp_ns,
            first_enter_timestamp_ns: None,
            active_ns: 0,
            enter_stack: EnterStack::default(),
            unbalanced_enters: 0,
            concurrent: false,
            saturated: false,
            handle,
        };

        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(data);
        }
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
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
        let ts = clock_monotonic_ns();
        let tid = current_tid();

        let Some(span_ref) = ctx.span(id) else { return };
        let handle;
        let instance_id;
        let parent_span_id;
        let meta;
        {
            let mut extensions = span_ref.extensions_mut();
            let Some(data) = extensions.get_mut::<SpanData>() else {
                return;
            };
            handle = data.handle.clone();
            instance_id = data.instance_id;
            meta = data.meta;

            // Track enter for active_ns accumulation using per-tid LIFO stack.
            if data.first_enter_timestamp_ns.is_none() {
                data.first_enter_timestamp_ns = Some(ts);
            }
            // Detect concurrent entry: multiple distinct tids in the stack, or
            // this tid is different from existing entries.
            if data.enter_stack.distinct_tids() > 0
                && !data
                    .enter_stack
                    .entries
                    .iter()
                    .all(|(t, _)| *t == tid as u64)
            {
                data.concurrent = true;
            }
            data.enter_stack.push(tid as u64, ts);

            // We only use explicit parents (span!(parent: &x, ..)), not contextual
            // parents (ctx.current_span()), because contextual parenting is
            // unreliable across tasks on the same worker thread. See:
            // https://chesedo.me/blog/rust-tracing-incorrect-parent-spans-async-futures-joinset/
            // The viewer infers nesting from timestamp containment instead.
            parent_span_id = data.parent_id.as_ref().map(|id| id.into_u64());
        }

        handle.with_encoder(|enc| {
            let worker_id = current_worker_id();
            let span_id = id.into_u64();

            let schemas = self.get_schemas(meta);
            let span_name = meta.name();

            // Encode directly into the thread-local buffer (no clone needed)
            let mut values = Vec::with_capacity(7 + schemas.field_names.len());
            values.push(FieldValue::Varint(ts));
            values.push(FieldValue::Varint(worker_id.as_u64()));
            values.push(FieldValue::Varint(span_id));
            values.push(FieldValue::Varint(instance_id));
            values.push(FieldValue::Varint(tid as u64));
            match parent_span_id {
                Some(pid) => values.push(FieldValue::Varint(pid)),
                None => values.push(FieldValue::None),
            }
            values.push(FieldValue::PooledString(enc.intern_string(span_name)));

            let ext = span_ref.extensions();
            let field_values = ext.get::<SpanData>().map(|d| &d.field_values[..]);
            for &name in &schemas.field_names {
                match field_values.and_then(|fv| field_value(fv, name)) {
                    Some(v) => values.push(FieldValue::PooledString(enc.intern_string(v))),
                    None => values.push(FieldValue::None),
                }
            }
            enc.write_event(&schemas.enter, &values);
        });
    }

    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        let ts = clock_monotonic_ns();
        let tid = current_tid();

        let Some(span_ref) = ctx.span(id) else { return };
        let handle;
        let instance_id;
        let meta;
        {
            let mut extensions = span_ref.extensions_mut();
            let Some(data) = extensions.get_mut::<SpanData>() else {
                return;
            };
            handle = data.handle.clone();
            instance_id = data.instance_id;
            meta = data.meta;

            // Accumulate active time from the matching enter (per-tid LIFO pop).
            if let Some(enter_ts) = data.enter_stack.pop(tid as u64) {
                let interval = ts.saturating_sub(enter_ts);
                let new_active = data.active_ns.saturating_add(interval);
                if new_active == u64::MAX && interval > 0 {
                    data.saturated = true;
                }
                data.active_ns = new_active;
            } else {
                // Unbalanced exit — more exits than enters for this tid.
                data.unbalanced_enters += 1;
            }
        }

        handle.with_encoder(|enc| {
            let worker_id = current_worker_id();
            let span_id = id.into_u64();

            let schemas = self.get_schemas(meta);
            let span_name = meta.name();

            let mut values = Vec::with_capacity(6 + schemas.field_names.len());
            values.push(FieldValue::Varint(ts));
            values.push(FieldValue::Varint(worker_id.as_u64()));
            values.push(FieldValue::Varint(span_id));
            values.push(FieldValue::Varint(instance_id));
            values.push(FieldValue::Varint(tid as u64));
            values.push(FieldValue::PooledString(enc.intern_string(span_name)));

            let ext = span_ref.extensions();
            let field_values = ext.get::<SpanData>().map(|d| &d.field_values[..]);
            for &name in &schemas.field_names {
                match field_values.and_then(|fv| field_value(fv, name)) {
                    Some(v) => values.push(FieldValue::PooledString(enc.intern_string(v))),
                    None => values.push(FieldValue::None),
                }
            }
            enc.write_event(&schemas.exit, &values);
        });
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        let close_ts = clock_monotonic_ns();

        let Some(span_ref) = ctx.span(&id) else {
            return;
        };
        let mut extensions = span_ref.extensions_mut();
        let Some(data) = extensions.get_mut::<SpanData>() else {
            // No SpanData means on_new_span was skipped (telemetry was disabled
            // at span creation). Nothing to emit — the span was never tracked.
            return;
        };

        // Use the handle captured at span creation time. This ensures the close
        // event is emitted even when the final span handle is dropped on a
        // thread that is not owned by a dial9 runtime.
        let handle = data.handle.clone();

        // Any outstanding enters at close time are unbalanced
        let unbalanced = data.unbalanced_enters + data.enter_stack.outstanding();

        // Build final attributes from field_values
        let attributes: Vec<(String, String)> = data
            .field_values
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();

        handle.record_event(SpanCloseEvent {
            timestamp_ns: close_ts,
            span_id: id.into_u64(),
            span_instance_id: data.instance_id,
            start_timestamp_ns: data.start_timestamp_ns,
            first_enter_timestamp_ns: data.first_enter_timestamp_ns,
            active_ns: data.active_ns,
            span_name: data.meta.name().to_string(),
            target: data.meta.target().to_string(),
            file: data.meta.file().map(|s| s.to_string()),
            line: data.meta.line(),
            parent_span_instance_id: data.parent_instance_id,
            attributes,
            unbalanced_enters: unbalanced,
            concurrent: u32::from(data.concurrent),
            saturated: u32::from(data.saturated),
            // The producer cannot currently observe whether enter/exit events
            // were dropped from the thread-local buffer. Loss is therefore
            // unobservable, and the backend must classify any gap as Unknown.
            loss_observable: 0,
        });
    }
}

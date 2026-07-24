//! Ad-hoc span wrappers that emit span metadata directly into a dial9 trace,
//! with no `tracing` subscriber and no `tracing` dependency.
//!
//! Where [`Dial9TracingLayer`](crate::tracing_layer::Dial9TracingLayer) needs
//! a `tracing-subscriber` registry wired up and `tracing` instrumentation in
//! the code, these wrappers are point-of-use: add one combinator where you
//! care and a span appears in the viewer.
//!
//! Three shapes cover the common cases:
//!
//! - [`span`] + [`Span::entered`] — a synchronous RAII guard for blocking or
//!   CPU-bound sections. Emits one enter/exit pair.
//! - [`SpanFutureExt::in_span`] — the async workhorse. Enters/exits around
//!   each poll, so awaits render as idle and each on-CPU segment is attributed
//!   to the worker that ran it.
//! - `Dial9SpanLayer` — a `tower` middleware (behind the `tower` feature)
//!   that wraps one span per request.
//!
//! All three reuse the existing standardized span wire events
//! (`SpanEnter:*` / `SpanExit:*` / `SpanCloseEvent`), so no viewer change is
//! required.
//!
//! ```ignore
//! use dial9_tokio_telemetry::span::{span, SpanFutureExt as _};
//!
//! // async
//! let order = load_order(id).in_span(span("db.load_order").field("id", id)).await;
//!
//! // sync
//! let guard = span("pricing.compute").entered();
//! let quote = compute_quote();
//! drop(guard);
//! ```
//!
//! # Off-runtime threads
//!
//! Like the tracing layer, every emit checks whether the current thread is
//! owned by a dial9 runtime (`Dial9Handle::is_enabled`). On other threads the
//! wrappers are no-ops, so they are safe to leave in code paths that
//! sometimes run outside a traced runtime.

use crate::telemetry::{Dial9Handle, clock_monotonic_ns, current_worker_id};
use dial9_trace_format::TraceEvent;
use dial9_trace_format::encoder::Schema;
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::panic::Location;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{Context, Poll};

use pin_project_lite::pin_project;

#[cfg(feature = "tower")]
mod tower;
#[cfg(feature = "tower")]
#[cfg_attr(docsrs, doc(cfg(feature = "tower")))]
pub use tower::Dial9SpanLayer;

// ── Span close wire event ────────────────────────────────────────────────────

/// Wire event that finalizes a span so the viewer can recycle its id.
///
/// Shared by the ad-hoc wrappers here and by
/// [`Dial9TracingLayer`](crate::tracing_layer::Dial9TracingLayer), so both
/// producers emit the identical `SpanCloseEvent` schema.
#[derive(TraceEvent)]
#[traceevent(wire_slot)]
pub(crate) struct SpanCloseEvent {
    #[traceevent(timestamp)]
    pub(crate) timestamp_ns: u64,
    pub(crate) span_id: u64,
}

// ── Span id allocation ───────────────────────────────────────────────────────

/// Top bit, set on every ad-hoc span id so they never collide with the
/// tracing layer's subscriber-allocated ids (small integers from 1).
const ADHOC_ID_BIT: u64 = 1 << 63;

static NEXT_SPAN_ID: AtomicU64 = AtomicU64::new(0);

fn alloc_span_id() -> u64 {
    // 2^63 ids does not wrap in practice (at 1B spans/sec, ~292 years).
    ADHOC_ID_BIT | NEXT_SPAN_ID.fetch_add(1, Ordering::Relaxed)
}

/// Identifier for an ad-hoc span, used for explicit parenting via
/// [`Span::parent`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SpanId(u64);

impl SpanId {
    /// The raw span id as written to the wire.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

// ── Per-callsite schema cache ────────────────────────────────────────────────

/// Cached enter/exit schemas for one `(callsite, field-name list)`.
#[derive(Clone)]
struct CallsiteSchemas {
    enter: Schema,
    exit: Schema,
}

/// Global cache keyed on the callsite location plus the exact field-name list.
///
/// Keying on the field names as well as the location means a callsite that
/// conditionally adds a field gets two stable schemas rather than dropped
/// fields, and it is how late fields work: an exit whose field set differs
/// from the enter's resolves a different (stable) schema. Schema count stays
/// bounded by code, not data.
#[allow(clippy::type_complexity)]
static SCHEMAS: LazyLock<
    Mutex<HashMap<(&'static Location<'static>, Vec<&'static str>), CallsiteSchemas>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn build_schemas(
    location: &'static Location<'static>,
    field_names: &[&'static str],
) -> CallsiteSchemas {
    // The schema id is the callsite plus the field-name list, never the span
    // name: names are `impl Into<String>` and may be dynamic, so the name
    // rides in the `span_name` field (which is what the viewer reads).
    //
    // The field names must be part of the id, not just the cache key. A single
    // callsite legitimately emits different field sets — an exit carrying a
    // late field recorded via `SpanHandle::record` has one more field than the
    // exit of an earlier poll that ran before the record. Those are different
    // schema definitions, so they need different names or the encoder rejects
    // the second one ("schema already registered with different definition").
    // Cardinality stays bounded by code: the set of field-name lists a
    // callsite can produce is fixed by the source, not by the data.
    let schema_id = if field_names.is_empty() {
        format!("adhoc::{}:{}", location.file(), location.line())
    } else {
        format!(
            "adhoc::{}:{}:{}",
            location.file(),
            location.line(),
            field_names.join(",")
        )
    };

    // Base fields present on all span events (mirrors the tracing layer).
    let mut enter_fields = vec![
        FieldDef::new("worker_id", FieldType::Varint),
        FieldDef::new("span_id", FieldType::Varint),
        FieldDef::new("parent_span_id", FieldType::OptionalVarint),
        FieldDef::new("span_name", FieldType::PooledString),
    ];
    let mut exit_fields = vec![
        FieldDef::new("worker_id", FieldType::Varint),
        FieldDef::new("span_id", FieldType::Varint),
        FieldDef::new("span_name", FieldType::PooledString),
    ];

    // User fields as optional interned strings, in call order.
    for &name in field_names {
        let def = FieldDef::new(name.to_string(), FieldType::OptionalPooledString);
        enter_fields.push(def.clone());
        exit_fields.push(def);
    }

    CallsiteSchemas {
        enter: Schema::new(&format!("SpanEnter:{schema_id}"), enter_fields),
        exit: Schema::new(&format!("SpanExit:{schema_id}"), exit_fields),
    }
}

fn get_schemas(
    location: &'static Location<'static>,
    field_names: &[&'static str],
) -> CallsiteSchemas {
    let key = (location, field_names.to_vec());
    let mut cache = SCHEMAS.lock().unwrap();
    cache
        .entry(key)
        .or_insert_with(|| build_schemas(location, field_names))
        .clone()
}

// ── Field helpers and emit ───────────────────────────────────────────────────

fn upsert(fields: &mut Vec<(&'static str, String)>, name: &'static str, value: String) {
    if let Some(entry) = fields.iter_mut().find(|(k, _)| *k == name) {
        entry.1 = value;
    } else {
        fields.push((name, value));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EventKind {
    Enter,
    Exit,
}

/// Emit an enter or exit event. No-op when the current thread has no live
/// dial9 handle (the encoder closure simply never runs).
fn emit_event(
    handle: &Dial9Handle,
    kind: EventKind,
    id: u64,
    name: &str,
    location: &'static Location<'static>,
    parent: Option<u64>,
    fields: &[(&'static str, String)],
) {
    handle.with_encoder(|enc| {
        let ts = clock_monotonic_ns();
        let worker_id = current_worker_id();

        let field_names: Vec<&'static str> = fields.iter().map(|(k, _)| *k).collect();
        let schemas = get_schemas(location, &field_names);
        let schema = match kind {
            EventKind::Enter => &schemas.enter,
            EventKind::Exit => &schemas.exit,
        };

        let mut values = Vec::with_capacity(5 + fields.len());
        values.push(FieldValue::Varint(ts));
        values.push(FieldValue::Varint(worker_id.as_u64()));
        values.push(FieldValue::Varint(id));
        if kind == EventKind::Enter {
            match parent {
                Some(pid) => values.push(FieldValue::Varint(pid)),
                None => values.push(FieldValue::None),
            }
        }
        values.push(FieldValue::PooledString(enc.intern_string(name)));
        for (_, v) in fields {
            values.push(FieldValue::PooledString(enc.intern_string(v)));
        }
        enc.write_event(schema, &values);
    });
}

fn emit_close(handle: &Dial9Handle, id: u64) {
    handle.record_event(SpanCloseEvent {
        timestamp_ns: clock_monotonic_ns(),
        span_id: id,
    });
}

// ── Span description ─────────────────────────────────────────────────────────

/// Start describing a span. Nothing is emitted until the span is
/// [`entered`](Span::entered) or driven via
/// [`in_span`](SpanFutureExt::in_span).
///
/// The callsite is captured here via `#[track_caller]` and becomes the span's
/// schema id.
#[track_caller]
pub fn span(name: impl Into<String>) -> Span {
    Span::new(name.into(), Some(Location::caller()))
}

/// A described-but-not-yet-entered span.
///
/// Build it with [`span`], attach [`field`](Span::field)s and an explicit
/// [`parent`](Span::parent), then either [`entered`](Span::entered) it (sync)
/// or hand it to [`in_span`](SpanFutureExt::in_span) (async).
#[derive(Debug)]
pub struct Span {
    id: u64,
    name: String,
    location: Option<&'static Location<'static>>,
    parent: Option<u64>,
    fields: Vec<(&'static str, String)>,
}

impl Span {
    fn new(name: String, location: Option<&'static Location<'static>>) -> Self {
        Self {
            id: alloc_span_id(),
            name,
            location,
            parent: None,
            fields: Vec::new(),
        }
    }

    /// Attach a field. Names are `&'static str` (they become wire-schema field
    /// names, so must be low-cardinality); values are anything `Display`.
    pub fn field(mut self, name: &'static str, value: impl fmt::Display) -> Self {
        upsert(&mut self.fields, name, value.to_string());
        self
    }

    /// Set an explicit parent span. There is no implicit/contextual parenting;
    /// the viewer otherwise infers nesting from timestamp containment.
    pub fn parent(mut self, parent: SpanId) -> Self {
        self.parent = Some(parent.as_u64());
        self
    }

    /// This span's pre-allocated id, for parenting another span before this
    /// one is entered.
    pub fn id(&self) -> SpanId {
        SpanId(self.id)
    }

    /// Enter the span, emitting the enter event immediately. The returned
    /// guard emits the exit and close events on drop (or [`exit`](EnteredSpan::exit)).
    #[track_caller]
    pub fn entered(self) -> EnteredSpan {
        let location = self.location.unwrap_or_else(Location::caller);
        let handle = Dial9Handle::current();
        emit_event(
            &handle,
            EventKind::Enter,
            self.id,
            &self.name,
            location,
            self.parent,
            &self.fields,
        );
        EnteredSpan {
            id: self.id,
            name: self.name,
            location,
            fields: self.fields,
            closed: false,
            _not_send: PhantomData,
        }
    }
}

impl From<&str> for Span {
    fn from(name: &str) -> Self {
        // No location captured here: the blanket `Into` used by
        // `.in_span("name")` is not `#[track_caller]`, so `in_span` stamps the
        // real callsite instead (see `SpanFutureExt::in_span`).
        Span::new(name.to_owned(), None)
    }
}

impl From<String> for Span {
    fn from(name: String) -> Self {
        Span::new(name, None)
    }
}

// ── Sync RAII guard ──────────────────────────────────────────────────────────

/// A guard for a synchronous, on-thread span. Emits the exit and close events
/// when dropped (or via [`exit`](Self::exit)).
///
/// `EnteredSpan` is `!Send` and must not be held across `.await`: the exit
/// event has to land on the entering thread's stream, ordered after the enter.
/// For async work use [`in_span`](SpanFutureExt::in_span) instead.
pub struct EnteredSpan {
    id: u64,
    name: String,
    location: &'static Location<'static>,
    fields: Vec<(&'static str, String)>,
    closed: bool,
    /// Makes the guard `!Send` (a raw pointer is neither `Send` nor `Sync`).
    _not_send: PhantomData<*const ()>,
}

impl fmt::Debug for EnteredSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnteredSpan")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl EnteredSpan {
    /// Record a field discovered mid-span. It is emitted on the exit event.
    pub fn record(&mut self, name: &'static str, value: impl fmt::Display) {
        upsert(&mut self.fields, name, value.to_string());
    }

    /// Exit the span now (rather than at end of scope). Equivalent to dropping
    /// the guard.
    pub fn exit(self) {
        // Drop runs here, emitting exit + close.
    }
}

impl Drop for EnteredSpan {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let handle = Dial9Handle::current();
        emit_event(
            &handle,
            EventKind::Exit,
            self.id,
            &self.name,
            self.location,
            None,
            &self.fields,
        );
        emit_close(&handle, self.id);
    }
}

// ── Async future wrapper ─────────────────────────────────────────────────────

/// Extension trait adding [`in_span`](Self::in_span) to every future.
pub trait SpanFutureExt: Future + Sized {
    /// Wrap this future so it is entered/exited around each poll and closed
    /// when it completes or is cancelled.
    ///
    /// Accepts a [`Span`] or, via `From<&str>`, a bare name (`fut.in_span("x")`).
    #[track_caller]
    fn in_span(self, span: impl Into<Span>) -> SpanFuture<Self> {
        let mut span = span.into();
        // Stamp the callsite for the `&str`/`String` shorthand, whose `From`
        // could not capture it.
        if span.location.is_none() {
            span.location = Some(Location::caller());
        }
        SpanFuture::new(self, span)
    }
}

impl<F: Future> SpanFutureExt for F {}

/// Shared, cloneable state behind a [`SpanFuture`] and its [`SpanHandle`]s.
struct SpanInner {
    id: u64,
    name: String,
    location: &'static Location<'static>,
    parent: Option<u64>,
    /// Fields known at construction, emitted on every enter.
    base_fields: Vec<(&'static str, String)>,
    /// Fields recorded after the fact via [`SpanHandle::record`]; merged into
    /// each exit event.
    late_fields: Mutex<Vec<(&'static str, String)>>,
}

impl SpanInner {
    fn from_span(span: Span) -> Arc<Self> {
        let location = span
            .location
            .expect("span location is set by span() or stamped by in_span()");
        Arc::new(Self {
            id: span.id,
            name: span.name,
            location,
            parent: span.parent,
            base_fields: span.fields,
            late_fields: Mutex::new(Vec::new()),
        })
    }

    fn emit_enter(&self, handle: &Dial9Handle) {
        emit_event(
            handle,
            EventKind::Enter,
            self.id,
            &self.name,
            self.location,
            self.parent,
            &self.base_fields,
        );
    }

    /// Exit carries base fields merged with any late fields recorded so far.
    fn emit_exit(&self, handle: &Dial9Handle) {
        let mut fields = self.base_fields.clone();
        {
            let late = self.late_fields.lock().unwrap();
            for (k, v) in late.iter() {
                upsert(&mut fields, k, v.clone());
            }
        }
        emit_event(
            handle,
            EventKind::Exit,
            self.id,
            &self.name,
            self.location,
            None,
            &fields,
        );
    }
}

pin_project! {
    /// A future wrapped with a span. Output is the inner future's output,
    /// unchanged.
    ///
    /// Emits one enter/exit pair per poll (matching what `tracing::Instrument`
    /// + the tracing layer produce), and a single close when the future
    /// resolves or is dropped.
    pub struct SpanFuture<F> {
        #[pin]
        inner: F,
        shared: Arc<SpanInner>,
        closed: bool,
    }

    impl<F> PinnedDrop for SpanFuture<F> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if !*this.closed {
                *this.closed = true;
                emit_close(&Dial9Handle::current(), this.shared.id);
            }
        }
    }
}

impl<F> fmt::Debug for SpanFuture<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpanFuture")
            .field("name", &self.shared.name)
            .field("id", &self.shared.id)
            .finish_non_exhaustive()
    }
}

impl<F: Future> SpanFuture<F> {
    fn new(inner: F, span: Span) -> Self {
        Self {
            inner,
            shared: SpanInner::from_span(span),
            closed: false,
        }
    }

    /// A cheap, `Send`, cloneable handle for recording fields after the fact
    /// (e.g. a status code once the response head is known). Late fields ride
    /// the next poll's exit event.
    pub fn handle(&self) -> SpanHandle {
        SpanHandle {
            inner: Arc::clone(&self.shared),
        }
    }
}

impl<F: Future> Future for SpanFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.project();
        let handle = Dial9Handle::current();

        this.shared.emit_enter(&handle);
        let result = this.inner.poll(cx);
        this.shared.emit_exit(&handle);

        if result.is_ready() && !*this.closed {
            *this.closed = true;
            emit_close(&handle, this.shared.id);
        }

        result
    }
}

/// A cheap, `Send`, cloneable handle to a live [`SpanFuture`] for recording
/// fields after the fact.
///
/// A field recorded after the future's final poll has no later exit to ride
/// and is dropped; record before or during the last poll.
#[derive(Clone)]
pub struct SpanHandle {
    inner: Arc<SpanInner>,
}

impl fmt::Debug for SpanHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpanHandle")
            .field("id", &self.inner.id)
            .field("name", &self.inner.name)
            .finish_non_exhaustive()
    }
}

impl SpanHandle {
    /// Record a field. Picked up at the next poll's exit event; recording off
    /// the polling thread is allowed.
    pub fn record(&self, name: &'static str, value: impl fmt::Display) {
        let mut late = self.inner.late_fields.lock().unwrap();
        upsert(&mut late, name, value.to_string());
    }

    /// This span's id, for explicit parenting.
    pub fn id(&self) -> SpanId {
        SpanId(self.inner.id)
    }
}

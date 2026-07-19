//! [`Dial9Stream`]: the `EntryIoStream` sink that walks descriptor-aware
//! metrique entries and encodes their `Emit`-tagged fields into the dial9
//! trace.

use super::schema::{self, CachedDescriptor, ExpectedScalar, FieldRole, PayloadWire};
use crate::rate_limit::rate_limited;
use crate::telemetry::{Dial9Handle, ThreadLocalEncoder};
use dial9_trace_format::types::FieldValue;
use metrique::writer::core::DescriptorId;
use metrique::writer::value::{Observation, Value, ValueWriter};
use metrique::writer::{
    Entry, EntryConfig, EntryIoStream, EntryWriter, IoStreamError, MetricFlags, Unit,
};
use std::borrow::Cow;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// A metrique [`EntryIoStream`] that records every `Emit`-tagged field of a
/// descriptor-aware entry into the dial9 trace, correlated via a flattened
/// [`Dial9Context`](super::Dial9Context).
///
/// Compose with an existing metrique pipeline via
/// [`tee`](metrique::writer::stream::tee):
///
/// ```ignore
/// use metrique::writer::stream::tee;
/// let stream = tee(emf_stream, Dial9Stream::new(&handle));
/// ```
///
/// Entries with no descriptor (hand-written `Entry` impls), with `Emit`
/// fields but no flattened `Dial9Context`, or with unsupported field shapes
/// are diagnosed (see the module docs) and either dropped or recorded with a
/// fallback header — they never panic the pipeline.
#[derive(Debug)]
pub struct Dial9Stream {
    handle: Dial9Handle,
    cache: HashMap<Vec<DescriptorId>, CachedDescriptor>,
    stats: Dial9StreamStats,
}

#[derive(Debug, Default)]
struct Dial9StreamStats {
    descriptors_seen: AtomicU64,
    descriptors_skipped: AtomicU64,
    events_emitted: AtomicU64,
    events_dropped: AtomicU64,
}

const DEBUG_SUMMARY_SAMPLE: u64 = 10_000;

impl Dial9Stream {
    /// Build a stream that records into `handle`'s trace. Cheap to
    /// construct; the handle is cloned (it's a lightweight `Arc`-backed
    /// type).
    pub fn new(handle: &Dial9Handle) -> Self {
        Self {
            handle: handle.clone(),
            cache: HashMap::new(),
            stats: Dial9StreamStats::default(),
        }
    }

    fn log_build_diagnostics(&self, build: &schema::BuildResult) {
        let name = build.cached.schema.name().to_string();
        if build.emit_without_context {
            debug_assert!(
                false,
                "dial9 metrique sink: entry '{name}' has Emit-tagged fields but no flattened \
                 Dial9Context (or a #[cfg]-gated one that isn't compiled in); recording with a \
                 WorkerId::UNKNOWN / no-task fallback header"
            );
            tracing::error!(
                entry = %name,
                "dial9 metrique sink: entry has Emit fields but no Dial9Context; recording with fallback header"
            );
        }
        if build.interned_on_non_string {
            debug_assert!(
                false,
                "dial9 metrique sink: entry '{name}' has an `Interned` flag on a non-string field; \
                 the flag is ignored for that field"
            );
            tracing::error!(
                entry = %name,
                "dial9 metrique sink: Interned flag on non-string field ignored"
            );
        }
        if build.opaque_emit_field {
            debug_assert!(
                false,
                "dial9 metrique sink: entry '{name}' has an Emit-tagged field with an unsupported \
                 (Opaque or unrecognized) shape; that field is skipped on the wire"
            );
            tracing::error!(
                entry = %name,
                "dial9 metrique sink: Emit field with unsupported shape skipped"
            );
        }
        if build.duplicate_field_name {
            debug_assert!(
                false,
                "dial9 metrique sink: entry '{name}' has two Context/Emit-tagged fields that \
                 resolve to the same written name; the later one silently overrides the earlier \
                 one's role"
            );
            tracing::error!(
                entry = %name,
                "dial9 metrique sink: duplicate field name among Context/Emit-tagged fields"
            );
        }
    }

    fn maybe_log_summary(&self) {
        let emitted = self.stats.events_emitted.load(Ordering::Relaxed);
        if emitted != 0 && emitted.is_multiple_of(DEBUG_SUMMARY_SAMPLE) {
            tracing::debug!(
                descriptors_seen = self.stats.descriptors_seen.load(Ordering::Relaxed),
                descriptors_skipped = self.stats.descriptors_skipped.load(Ordering::Relaxed),
                events_emitted = emitted,
                events_dropped = self.stats.events_dropped.load(Ordering::Relaxed),
                "dial9 metrique sink summary"
            );
        }
    }
}

impl EntryIoStream for Dial9Stream {
    fn next(&mut self, entry: &impl Entry) -> Result<(), IoStreamError> {
        if !self.handle.is_enabled() {
            return Ok(());
        }

        let Some(key) = schema::descriptor_cache_key(entry) else {
            self.stats
                .descriptors_skipped
                .fetch_add(1, Ordering::Relaxed);
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!(
                    "dial9 metrique sink: entry has no descriptor (hand-written `Entry` impl?); \
                     dropping it from the dial9 trace (other sinks are unaffected)"
                );
            });
            return Ok(());
        };

        if !self.cache.contains_key(&key) {
            let Some(build) = schema::build(entry) else {
                // descriptors() flipped to Unavailable between the two calls
                // above -- entries don't change shape mid-flight in
                // practice, but handle it defensively rather than panic.
                return Ok(());
            };
            self.log_build_diagnostics(&build);
            self.stats.descriptors_seen.fetch_add(1, Ordering::Relaxed);
            self.cache.insert(key.clone(), build.cached);
        }
        let cached = self.cache.get(&key).expect("just inserted above");

        let stats = &self.stats;
        self.handle.with_encoder(|enc| {
            let mut writer = Dial9EntryWriter::new(cached, enc);
            let write_result = catch_unwind(AssertUnwindSafe(|| entry.write(&mut writer)));
            match write_result {
                Ok(()) => {
                    if writer.ok {
                        let values = writer.finish();
                        enc.write_event(&cached.schema, &values);
                        stats.events_emitted.fetch_add(1, Ordering::Relaxed);
                    } else {
                        stats.events_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(_) => {
                    stats.events_dropped.fetch_add(1, Ordering::Relaxed);
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(
                            entry = cached.schema.name(),
                            "dial9 metrique sink: panic while writing entry, dropping this event"
                        );
                    });
                }
            }
        });
        self.maybe_log_summary();

        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // Encoding happens synchronously in `next` via `Dial9Handle::with_encoder`;
        // there's no buffered I/O of our own to flush here.
        Ok(())
    }
}

/// Header fields captured from `Dial9Context`'s tagged fields, keyed by
/// name. Defaults represent "no context captured" — used verbatim when an
/// entry has `Emit` fields but no flattened `Dial9Context` (diagnosed
/// separately at schema-build time).
struct EventHeader {
    worker_id: u64,
    task_id: Option<u64>,
    monotonic_start: u64,
    monotonic_end: u64,
}

impl EventHeader {
    fn fallback(has_context: bool) -> Self {
        let now = if has_context {
            0
        } else {
            crate::telemetry::clock_monotonic_ns()
        };
        Self {
            worker_id: crate::telemetry::WorkerId::UNKNOWN.as_u64(),
            task_id: None,
            monotonic_start: now,
            monotonic_end: now,
        }
    }
}

/// Walks one entry's `write()` call, routing `Context`-tagged fields into
/// the event header and `Emit`-tagged fields into the payload, by looking
/// up each `value()` callback's *name* in the role map built once per
/// descriptor shape (see `schema.rs`) — not by callback position, which
/// isn't reliably aligned with descriptor field order.
struct Dial9EntryWriter<'w, 'enc> {
    cached: &'w CachedDescriptor,
    enc: &'w mut ThreadLocalEncoder<'enc>,
    header: EventHeader,
    payload: Vec<FieldValue>,
    ok: bool,
}

impl<'w, 'enc> Dial9EntryWriter<'w, 'enc> {
    fn new(cached: &'w CachedDescriptor, enc: &'w mut ThreadLocalEncoder<'enc>) -> Self {
        let payload_len = cached.schema.fields().len().saturating_sub(3);
        Self {
            cached,
            enc,
            header: EventHeader::fallback(cached.has_context),
            payload: vec![FieldValue::None; payload_len],
            ok: true,
        }
    }

    /// Assemble the final `values` slice for `Encoder::write_event`:
    /// `[timestamp, worker_id, task_id, monotonic_ns_end, ...payload]`.
    fn finish(self) -> Vec<FieldValue> {
        let mut out = Vec::with_capacity(4 + self.payload.len());
        out.push(FieldValue::Varint(self.header.monotonic_start));
        out.push(FieldValue::Varint(self.header.worker_id));
        out.push(match self.header.task_id {
            Some(v) => FieldValue::Varint(v),
            None => FieldValue::None,
        });
        out.push(FieldValue::Varint(self.header.monotonic_end));
        out.extend(self.payload);
        out
    }

    fn report_anomaly(&self, what: &'static str) {
        let name = self.cached.schema.name().to_string();
        rate_limited!(Duration::from_secs(60), {
            tracing::warn!(
                entry = %name,
                reason = what,
                "dial9 metrique sink: runtime field anomaly, dropping this event"
            );
        });
    }
}

impl<'a, 'w, 'enc> EntryWriter<'a> for Dial9EntryWriter<'w, 'enc> {
    fn timestamp(&mut self, _timestamp: SystemTime) {
        // `#[metrics(timestamp)]` fields are excluded from
        // `descriptors().fields()`, so there's no role to look up here.
        // dial9's own event timestamp comes from `monotonic_ns_start`
        // (Dial9Context), not from metrique's optional wall-clock field.
    }

    fn value(&mut self, name: impl Into<Cow<'a, str>>, value: &(impl Value + ?Sized)) {
        let name = name.into();
        let role = self
            .cached
            .field_roles
            .get(name.as_ref())
            .copied()
            .unwrap_or(FieldRole::Skip);

        match role {
            FieldRole::Skip => {}
            FieldRole::ContextWorkerId => {
                let (result, _mismatch) =
                    capture_value(self.enc, ExpectedScalar::UInt, false, value);
                if let Some(FieldValue::Varint(v)) = result {
                    self.header.worker_id = v;
                }
            }
            FieldRole::ContextTaskId => {
                let (result, _mismatch) =
                    capture_value(self.enc, ExpectedScalar::UInt, false, value);
                self.header.task_id = match result {
                    Some(FieldValue::Varint(v)) => Some(v),
                    _ => None,
                };
            }
            FieldRole::ContextMonotonicStart => {
                let (result, _mismatch) =
                    capture_value(self.enc, ExpectedScalar::UInt, false, value);
                if let Some(FieldValue::Varint(v)) = result {
                    self.header.monotonic_start = v;
                }
            }
            FieldRole::ContextMonotonicEnd => {
                let (result, _mismatch) =
                    capture_value(self.enc, ExpectedScalar::UInt, false, value);
                if let Some(FieldValue::Varint(v)) = result {
                    self.header.monotonic_end = v;
                }
            }
            FieldRole::Payload { schema_index, wire } => {
                let payload_index = schema_index - 3;
                let PayloadWire {
                    expected,
                    optional,
                    interned,
                } = wire;
                let (result, mismatch) = capture_value(self.enc, expected, interned, value);
                match (result, mismatch) {
                    (Some(fv), false) => self.payload[payload_index] = fv,
                    (None, false) if optional => self.payload[payload_index] = FieldValue::None,
                    _ => {
                        self.ok = false;
                        self.report_anomaly("field shape mismatch at write time");
                    }
                }
            }
        }
    }

    fn config(&mut self, _config: &'a dyn EntryConfig) {}
}

/// Capture a single leaf value into a [`FieldValue`], coercing it to
/// `expected` (the shape recorded for this field at schema-build time).
/// Returns `(value, mismatch)`; `mismatch` means the concrete `Value::write`
/// call didn't match `expected` — either a bug in that type's `Value` impl,
/// or (for a list-shaped field) `metrique`'s `ForceFlag` wrapper doing its
/// documented comma-join, which `capture_value`'s caller has already
/// resolved by declaring list fields as `ExpectedScalar::Str` (see
/// `schema.rs`), so this should not fire for `Emit`-tagged list fields.
fn capture_value(
    enc: &mut ThreadLocalEncoder<'_>,
    expected: ExpectedScalar,
    intern: bool,
    value: &(impl Value + ?Sized),
) -> (Option<FieldValue>, bool) {
    let mut result = None;
    let mut mismatch = false;
    let capture = ValueCapture {
        enc,
        expected,
        intern,
        result: &mut result,
        mismatch: &mut mismatch,
    };
    value.write(capture);
    (result, mismatch)
}

/// Normalized numeric observation, sidestepping repeated matches on the
/// `#[non_exhaustive]` `Observation` enum.
enum Numeric {
    U(u64),
    F(f64),
}

struct ValueCapture<'w, 'enc> {
    enc: &'w mut ThreadLocalEncoder<'enc>,
    expected: ExpectedScalar,
    intern: bool,
    result: &'w mut Option<FieldValue>,
    mismatch: &'w mut bool,
}

impl<'w, 'enc> ValueWriter for ValueCapture<'w, 'enc> {
    fn string(self, value: &str) {
        match self.expected {
            ExpectedScalar::Str => {
                *self.result = Some(if self.intern {
                    FieldValue::PooledString(self.enc.intern_string(value))
                } else {
                    FieldValue::String(value.to_owned())
                });
            }
            ExpectedScalar::Bytes => {
                *self.result = Some(FieldValue::Bytes(value.as_bytes().to_vec()));
            }
            _ => *self.mismatch = true,
        }
    }

    fn metric<'a>(
        self,
        distribution: impl IntoIterator<Item = Observation>,
        _unit: Unit,
        _dimensions: impl IntoIterator<Item = (&'a str, &'a str)>,
        _flags: MetricFlags<'_>,
    ) {
        let Some(obs) = distribution.into_iter().next() else {
            return;
        };
        // `Observation` is `#[non_exhaustive]`; normalize once here (any
        // future variant falls back to its most natural `f64` total) rather
        // than repeating a wildcard-guarded match per expected shape below.
        let numeric = match obs {
            Observation::Unsigned(v) => Numeric::U(v),
            Observation::Floating(v) => Numeric::F(v),
            Observation::Repeated { total, .. } => Numeric::F(total),
            _ => Numeric::F(0.0),
        };
        let fv = match self.expected {
            ExpectedScalar::Bool => FieldValue::Bool(match numeric {
                Numeric::U(v) => v != 0,
                Numeric::F(v) => v != 0.0,
            }),
            ExpectedScalar::UInt => FieldValue::Varint(match numeric {
                Numeric::U(v) => v,
                Numeric::F(v) => v.max(0.0) as u64,
            }),
            ExpectedScalar::SInt => FieldValue::I64(match numeric {
                Numeric::U(v) => v as i64,
                Numeric::F(v) => v as i64,
            }),
            ExpectedScalar::Float => FieldValue::F64(match numeric {
                Numeric::U(v) => v as f64,
                Numeric::F(v) => v,
            }),
            ExpectedScalar::Str | ExpectedScalar::Bytes => {
                *self.mismatch = true;
                return;
            }
        };
        *self.result = Some(fv);
    }

    fn error(self, _error: metrique::writer::ValidationError) {
        // Leave `result` unwritten: the caller treats "nothing captured" as
        // `FieldValue::None` for an Optional field, or an anomaly otherwise.
    }

    fn values<'a, V: Value + 'a>(self, _values: impl IntoIterator<Item = &'a V>) {
        // Unreachable for any field this sink actually captures: every
        // captured field carries at least one `#[metrics(flags(...))]` tag
        // (`Emit`/`Context`), and metrique wraps flagged fields' `Value` in
        // `ForceFlag<T, _>`, whose writer wrapper overrides `.string()`/
        // `.metric()`/`.error()` but not `.values()` — so a list-shaped
        // field's `writer.values(...)` call is intercepted by `ForceFlag`'s
        // own inherited default (comma-join to a string) before it ever
        // reaches us. `schema.rs` accounts for this by declaring list
        // fields as `ExpectedScalar::Str`, not by routing them here. Kept
        // as a defensive fallback in case that upstream behavior changes.
        *self.mismatch = true;
    }
}

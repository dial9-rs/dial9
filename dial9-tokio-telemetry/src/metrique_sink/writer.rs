//! The flush-thread `Entry::write` walk: routes each metrique value callback
//! to the event header, a payload slot, or nowhere, per the cached plan.

use std::borrow::Cow;
use std::time::{Duration, SystemTime};

use dial9_trace_format::types::FieldValue;
use metrique_writer::{EntryConfig, EntryWriter, Observation, Value, ValueWriter};

use crate::rate_limit::rate_limited;
use crate::telemetry::{ThreadLocalEncoder, WorkerId, clock_monotonic_ns};

use super::plan::{ContextRole, FieldAction, HEADER_FIELDS, Header, Plan, ScalarKind, ValueKind};

/// Captured event-header values, filled in by [`ContextRole`]-routed fields
/// and the `EntryWriter::timestamp` callback as the walk encounters them.
#[derive(Debug, Default)]
struct ContextValues {
    worker_id: Option<u64>,
    task_id: Option<u64>,
    monotonic_start: Option<u64>,
    monotonic_end: Option<u64>,
    /// From `#[metrics(timestamp)]`, via the `timestamp` callback.
    wall_clock_ns: Option<u64>,
}

/// [`EntryWriter`] that walks one entry against its cached [`Plan`].
///
/// Value callbacks are consumed positionally: the `i`-th callback takes
/// `plan.actions[i]`, relying on metrique's guarantee that `Entry::write`
/// emits values in descriptor order. [`fill_values`](Self::fill_values)
/// assembles the final wire value vector.
pub(crate) struct EntryWalk<'p, 'enc> {
    plan: &'p Plan,
    enc: &'p mut ThreadLocalEncoder<'enc>,
    /// Payload capture slots, reused across entries by the caller.
    slots: &'p mut Vec<Option<FieldValue>>,
    ctx: ContextValues,
    /// Index of the next `value` callback into `plan.actions`.
    next: usize,
    /// More `value` callbacks fired than the descriptor declares.
    overflowed: bool,
}

/// Why [`EntryWalk::fill_values`] refused to assemble an event. The caller
/// must drop it either way.
#[derive(Debug)]
pub(crate) enum WalkError<'p> {
    /// `Entry::write` emitted a different number of value callbacks than the
    /// descriptor declares (in either direction). That breaks metrique's
    /// descriptor/write-order contract, so positional routing cannot be
    /// trusted. A mid-walk omission shifts every later value one slot left,
    /// so counting is what turns silent misattribution into a detectable
    /// mismatch.
    PlanMismatch,
    /// A non-optional payload field produced no value (a shape/value
    /// mismatch); the wire format has no absent encoding for required
    /// fields.
    MissingRequired { field: &'p str },
}

impl<'p, 'enc> EntryWalk<'p, 'enc> {
    pub(crate) fn new(
        plan: &'p Plan,
        enc: &'p mut ThreadLocalEncoder<'enc>,
        slots: &'p mut Vec<Option<FieldValue>>,
    ) -> Self {
        slots.clear();
        slots.resize_with(plan.payload_optional.len(), || None);
        Self {
            plan,
            enc,
            slots,
            ctx: ContextValues::default(),
            next: 0,
            overflowed: false,
        }
    }

    /// Assemble the wire value vector into `out`: the implicit timestamp,
    /// one value per [`Header`] in `Header::ALL` order, then the payload
    /// slots.
    ///
    /// Also where the walk is validated, so a caller cannot encode a
    /// mis-routed event by forgetting a separate check.
    pub(crate) fn fill_values(&mut self, out: &mut Vec<FieldValue>) -> Result<(), WalkError<'p>> {
        if self.overflowed || self.next != self.plan.actions.len() {
            return Err(WalkError::PlanMismatch);
        }

        fn opt(v: Option<u64>) -> FieldValue {
            match v {
                Some(v) => FieldValue::Varint(v),
                None => FieldValue::None,
            }
        }

        out.clear();
        // Timestamp: request start, or the flush-thread clock as fallback.
        out.push(FieldValue::Varint(
            self.ctx.monotonic_start.unwrap_or_else(clock_monotonic_ns),
        ));
        for header in Header::ALL {
            out.push(match header {
                Header::WorkerId => {
                    FieldValue::Varint(self.ctx.worker_id.unwrap_or(WorkerId::UNKNOWN.as_u64()))
                }
                Header::TaskId => opt(self.ctx.task_id),
                Header::MonotonicEnd => opt(self.ctx.monotonic_end),
                Header::WallClock => opt(self.ctx.wall_clock_ns),
            });
        }

        // Payload slot i is schema field HEADER_FIELDS + i. Optionality
        // comes from the plan's cache; the schema is consulted only on the
        // cold path for the field name.
        let plan: &'p Plan = self.plan;
        for (i, (slot, optional)) in self
            .slots
            .iter_mut()
            .zip(&plan.payload_optional)
            .enumerate()
        {
            match slot.take() {
                Some(value) => out.push(value),
                None if *optional => out.push(FieldValue::None),
                None => {
                    return Err(WalkError::MissingRequired {
                        field: plan.schema.fields()[HEADER_FIELDS + i].name(),
                    });
                }
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for EntryWalk<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryWalk")
            .field("entry", &self.plan.entry_name)
            .field("next", &self.next)
            .finish_non_exhaustive()
    }
}

impl<'a> EntryWriter<'a> for EntryWalk<'_, '_> {
    fn timestamp(&mut self, timestamp: SystemTime) {
        self.ctx.wall_clock_ns = timestamp
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        if self.ctx.wall_clock_ns.is_none() {
            // Pre-epoch or absurdly far-future timestamp; the header field
            // stays absent rather than carrying a fabricated value.
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!(
                    entry = %self.plan.entry_name,
                    ?timestamp,
                    "metrique timestamp not representable as u64 nanoseconds since epoch; \
                     wall_clock_ns left absent"
                );
            });
        }
    }

    fn value(&mut self, _name: impl Into<Cow<'a, str>>, value: &(impl Value + ?Sized)) {
        let Some(action) = self.plan.actions.get(self.next).copied() else {
            self.overflowed = true;
            return;
        };
        self.next += 1;
        match action {
            FieldAction::Skip => {}
            FieldAction::Context(role) => {
                let captured = capture_u64(value);
                // TaskId is legitimately absent off-task; the others always
                // write a u64, so a missing capture means a broken context
                // field and a silently degraded event header.
                if captured.is_none() && !matches!(role, ContextRole::TaskId) {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(
                            entry = %self.plan.entry_name,
                            ?role,
                            "dial9 context field produced no value; event header degraded"
                        );
                    });
                }
                match role {
                    ContextRole::WorkerId => self.ctx.worker_id = captured,
                    ContextRole::TaskId => self.ctx.task_id = captured,
                    ContextRole::MonotonicStart => self.ctx.monotonic_start = captured,
                    ContextRole::MonotonicEnd => self.ctx.monotonic_end = captured,
                }
            }
            FieldAction::Payload { slot, kind } => {
                let mut out = None;
                value.write(ValueCapture {
                    out: &mut out,
                    kind,
                    entry_name: &self.plan.entry_name,
                    enc: self.enc,
                });
                self.slots[slot] = out;
            }
        }
    }

    fn config(&mut self, _config: &'a dyn EntryConfig) {
        // Format-specific configuration (EMF dimensions etc.); nothing maps
        // to the trace encoding today.
    }
}

/// Extract a single unsigned observation from a value (context fields are
/// all `u64` / `Option<u64>`). Anything else leaves the slot empty.
fn capture_u64(value: &(impl Value + ?Sized)) -> Option<u64> {
    struct U64Capture<'a>(&'a mut Option<u64>);

    impl ValueWriter for U64Capture<'_> {
        fn string(self, _value: &str) {}

        fn metric<'a>(
            self,
            distribution: impl IntoIterator<Item = Observation>,
            _unit: metrique_writer::Unit,
            _dimensions: impl IntoIterator<Item = (&'a str, &'a str)>,
            _flags: metrique_writer::MetricFlags<'_>,
        ) {
            if let Some(Observation::Unsigned(v)) = single_observation(distribution) {
                *self.0 = Some(v);
            }
        }

        fn error(self, _error: metrique_writer::ValidationError) {}
    }

    let mut out = None;
    value.write(U64Capture(&mut out));
    out
}

/// The distribution's only observation, or `None` when it is empty or
/// carries more than one.
fn single_observation(distribution: impl IntoIterator<Item = Observation>) -> Option<Observation> {
    let mut iter = distribution.into_iter();
    let first = iter.next();
    if iter.next().is_some() { None } else { first }
}

/// [`ValueWriter`] that captures one value as a [`FieldValue`] according to
/// its planned [`ValueKind`]. Shape/value mismatches leave the slot empty;
/// the caller decides whether that is legal (optional field) or drops the
/// event.
struct ValueCapture<'a, 'enc> {
    out: &'a mut Option<FieldValue>,
    kind: ValueKind,
    entry_name: &'a str,
    enc: &'a mut ThreadLocalEncoder<'enc>,
}

impl<'a, 'enc> ValueCapture<'a, 'enc> {
    /// Rewrap as a scalar capture, for scalar callbacks arriving on a
    /// scalar-planned field.
    fn into_scalar(self, kind: ScalarKind) -> ScalarCapture<'a, 'enc> {
        ScalarCapture {
            out: self.out,
            kind,
            entry_name: self.entry_name,
            enc: self.enc,
        }
    }
}

impl ValueWriter for ValueCapture<'_, '_> {
    fn string(self, value: &str) {
        match self.kind {
            ValueKind::Scalar(kind) => self.into_scalar(kind).string(value),
            ValueKind::List { .. } => shape_mismatch(self.entry_name, self.kind),
        }
    }

    fn metric<'a>(
        self,
        distribution: impl IntoIterator<Item = Observation>,
        unit: metrique_writer::Unit,
        dimensions: impl IntoIterator<Item = (&'a str, &'a str)>,
        flags: metrique_writer::MetricFlags<'_>,
    ) {
        match self.kind {
            ValueKind::Scalar(kind) => {
                self.into_scalar(kind)
                    .metric(distribution, unit, dimensions, flags)
            }
            ValueKind::List { .. } => shape_mismatch(self.entry_name, self.kind),
        }
    }

    fn values<'v, V: Value + 'v>(self, values: impl IntoIterator<Item = &'v V>) {
        let ValueKind::List { elem } = self.kind else {
            shape_mismatch(self.entry_name, self.kind);
            return;
        };
        let values = values.into_iter();
        // Cap the pre-allocation: the hint comes from user iterator code,
        // and an absurd lower bound must not abort the flush thread with an
        // allocation failure (which catch_unwind cannot contain).
        let mut items = Vec::with_capacity(values.size_hint().0.min(1024));
        for value in values {
            let mut item = None;
            value.write(ScalarCapture {
                out: &mut item,
                kind: elem,
                entry_name: self.entry_name,
                enc: &mut *self.enc,
            });
            // Absent optional elements write nothing and are omitted, the
            // same way metrique's own formats leave them out of their
            // arrays.
            if let Some(item) = item {
                items.push(item);
            }
        }
        *self.out = Some(FieldValue::List(items));
    }

    fn error(self, error: metrique_writer::ValidationError) {
        validation_error(self.entry_name, &error);
    }
}

/// A value wrote through a callback that does not match its declared shape
/// (a list callback for a scalar shape or vice versa). The value is lost;
/// the descriptor's `SHAPE` and the `Value::write` impl disagree.
fn shape_mismatch(entry_name: &str, kind: ValueKind) {
    rate_limited!(Duration::from_secs(60), {
        tracing::warn!(
            entry = %entry_name,
            ?kind,
            "metrique value callback did not match its declared shape; value lost"
        );
    });
}

/// A value reported a validation error instead of data; its slot stays
/// empty.
fn validation_error(entry_name: &str, error: &metrique_writer::ValidationError) {
    rate_limited!(Duration::from_secs(60), {
        tracing::warn!(
            entry = %entry_name,
            %error,
            "metrique value failed validation; field left absent"
        );
    });
}

/// [`ValueWriter`] that captures one scalar observation as a [`FieldValue`]
/// according to its planned [`ScalarKind`]. Serves scalar payload fields and
/// each element of a list field. Mismatches leave the slot empty.
struct ScalarCapture<'a, 'enc> {
    out: &'a mut Option<FieldValue>,
    kind: ScalarKind,
    entry_name: &'a str,
    enc: &'a mut ThreadLocalEncoder<'enc>,
}

impl ValueWriter for ScalarCapture<'_, '_> {
    fn string(self, value: &str) {
        match self.kind {
            ScalarKind::Str { interned: true } => {
                *self.out = Some(FieldValue::PooledString(self.enc.intern_string(value)));
            }
            ScalarKind::Str { interned: false } => {
                *self.out = Some(FieldValue::String(value.to_owned()));
            }
            _ => {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        entry = %self.entry_name,
                        kind = ?self.kind,
                        "metrique value wrote a string for a non-string shape; value lost"
                    );
                });
            }
        }
    }

    fn metric<'a>(
        self,
        distribution: impl IntoIterator<Item = Observation>,
        _unit: metrique_writer::Unit,
        _dimensions: impl IntoIterator<Item = (&'a str, &'a str)>,
        _flags: metrique_writer::MetricFlags<'_>,
    ) {
        // Planned kinds are all single-observation scalars (distribution
        // shapes are Opaque and never planned), so a multi-observation
        // callback falls through to the mismatch warn below.
        *self.out = match (single_observation(distribution), self.kind) {
            (None, _) => None,
            (Some(Observation::Unsigned(v)), ScalarKind::Bool) => Some(FieldValue::Bool(v != 0)),
            (Some(Observation::Unsigned(v)), ScalarKind::Uint) => Some(FieldValue::Varint(v)),
            (Some(Observation::Unsigned(v)), ScalarKind::Int) => {
                i64::try_from(v).ok().map(FieldValue::I64)
            }
            // `Observation` has no signed variant, so signed-shape values
            // (necessarily custom `Value` impls) arrive as floats.
            (Some(Observation::Floating(v)), ScalarKind::Int) if v.fract() == 0.0 => {
                // Asymmetric bounds on purpose: -2^63 is exactly
                // representable in f64 (>= is exact), while i64::MAX rounds
                // up to 2^63 as f64, so the upper bound must be strict.
                (v >= i64::MIN as f64 && v < i64::MAX as f64).then_some(FieldValue::I64(v as i64))
            }
            (Some(Observation::Unsigned(v)), ScalarKind::Float) => Some(FieldValue::F64(v as f64)),
            (Some(Observation::Floating(v)), ScalarKind::Float) => Some(FieldValue::F64(v)),
            // A pre-summed observation (e.g. metrique's `Observation` value
            // type, shape F64); carry the total like other formats do.
            (Some(Observation::Repeated { total, .. }), ScalarKind::Float) => {
                Some(FieldValue::F64(total))
            }
            _ => None,
        };
        if self.out.is_none() {
            // A metric callback fired but could not be mapped to the planned
            // shape; unlike an absent optional (which fires no callback at
            // all), this is data loss worth reporting.
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!(
                    entry = %self.entry_name,
                    kind = ?self.kind,
                    "metrique observation did not match its declared shape; value lost"
                );
            });
        }
    }

    fn error(self, error: metrique_writer::ValidationError) {
        validation_error(self.entry_name, &error);
    }
}

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
/// emits values in descriptor order. Payload callbacks also arrive in
/// payload order, so captured values append straight to the wire value
/// vector behind reserved timestamp/header placeholders, which
/// [`finish`](Self::finish) validates and fills in.
pub(crate) struct EntryWalk<'p, 'enc> {
    plan: &'p Plan,
    enc: &'p mut ThreadLocalEncoder<'enc>,
    /// The wire value vector under construction (caller-owned, reused
    /// across entries): `[timestamp, headers..., payload...]`.
    values: &'p mut Vec<FieldValue>,
    ctx: ContextValues,
    /// Index of the next `value` callback into `plan.actions`.
    next: usize,
    /// Payload actions consumed so far; doubles as the payload index for
    /// the missing-required diagnostic.
    payload_seen: usize,
    /// First required payload field that produced no value, if any.
    missing_required: Option<usize>,
    /// More `value` callbacks fired than the descriptor declares.
    overflowed: bool,
}

/// Why [`EntryWalk::finish`] refused to assemble an event. The caller
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
        values: &'p mut Vec<FieldValue>,
    ) -> Self {
        values.clear();
        // Placeholders for the implicit timestamp and the header fields;
        // `finish` overwrites them once the walk has captured the context.
        values.resize_with(1 + HEADER_FIELDS, || FieldValue::None);
        Self {
            plan,
            enc,
            values,
            ctx: ContextValues::default(),
            next: 0,
            payload_seen: 0,
            missing_required: None,
            overflowed: false,
        }
    }

    /// Validate the walk and fill the timestamp/header placeholders,
    /// completing the wire value vector handed to [`Self::new`]. Consumes
    /// the walk, releasing its borrows for the encode call.
    pub(crate) fn finish(self) -> Result<(), WalkError<'p>> {
        if self.overflowed || self.next != self.plan.actions.len() {
            return Err(WalkError::PlanMismatch);
        }
        if let Some(index) = self.missing_required {
            return Err(WalkError::MissingRequired {
                field: self.plan.schema.fields()[HEADER_FIELDS + index].name(),
            });
        }

        fn opt(v: Option<u64>) -> FieldValue {
            match v {
                Some(v) => FieldValue::Varint(v),
                None => FieldValue::None,
            }
        }

        // Timestamp: request start, or the flush-thread clock as fallback.
        self.values[0] =
            FieldValue::Varint(self.ctx.monotonic_start.unwrap_or_else(clock_monotonic_ns));
        for (i, header) in Header::ALL.into_iter().enumerate() {
            self.values[1 + i] = match header {
                Header::WorkerId => {
                    FieldValue::Varint(self.ctx.worker_id.unwrap_or(WorkerId::UNKNOWN.as_u64()))
                }
                Header::TaskId => opt(self.ctx.task_id),
                // Absent unless the context captured both timestamps; the
                // flush-thread fallback timestamp would make a nonsense
                // duration. `MonotonicAtClose` fires after `capture` on the
                // same clock, so the subtraction cannot wrap in practice.
                Header::Duration => opt(self
                    .ctx
                    .monotonic_start
                    .zip(self.ctx.monotonic_end)
                    .map(|(start, end)| end.saturating_sub(start))),
                Header::WallClock => opt(self.ctx.wall_clock_ns),
            };
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
            FieldAction::Payload { optional, kind } => {
                let index = self.payload_seen;
                self.payload_seen += 1;
                let mut out = Captured::Absent;
                value.write(ValueCapture {
                    out: &mut out,
                    kind,
                    entry_name: &self.plan.entry_name,
                    enc: self.enc,
                });
                match out {
                    Captured::Value(value) => self.values.push(value),
                    // Absent and mismatched are treated alike: legal for an
                    // optional field, an event drop otherwise (in `finish`).
                    Captured::Absent | Captured::Mismatched if optional => {
                        self.values.push(FieldValue::None)
                    }
                    Captured::Absent | Captured::Mismatched => {
                        if self.missing_required.is_none() {
                            self.missing_required = Some(index);
                        }
                    }
                }
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

/// Outcome of one capture.
enum Captured {
    /// No data callback fired; an absent optional writes nothing.
    Absent,
    Value(FieldValue),
    /// A callback fired but did not match the planned shape (warned,
    /// rate-limited, at the capture site).
    Mismatched,
}

/// [`ValueWriter`] that captures one value as a [`FieldValue`] according to
/// its planned [`ValueKind`]. The caller decides whether a missing value is
/// legal (optional field) or drops the event.
struct ValueCapture<'a, 'enc> {
    out: &'a mut Captured,
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
            ValueKind::List { .. } => {
                shape_mismatch(self.entry_name, self.kind);
                *self.out = Captured::Mismatched;
            }
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
            ValueKind::List { .. } => {
                shape_mismatch(self.entry_name, self.kind);
                *self.out = Captured::Mismatched;
            }
        }
    }

    fn values<'v, V: Value + 'v>(self, values: impl IntoIterator<Item = &'v V>) {
        let ValueKind::List { elem } = self.kind else {
            shape_mismatch(self.entry_name, self.kind);
            *self.out = Captured::Mismatched;
            return;
        };
        let values = values.into_iter();
        // Cap the pre-allocation: the hint comes from user iterator code,
        // and an absurd lower bound must not abort the flush thread with an
        // allocation failure (which catch_unwind cannot contain).
        let mut items = Vec::with_capacity(values.size_hint().0.min(1024));
        for value in values {
            let mut item = Captured::Absent;
            value.write(ScalarCapture {
                out: &mut item,
                kind: elem,
                entry_name: self.entry_name,
                enc: &mut *self.enc,
            });
            match item {
                Captured::Value(item) => items.push(item),
                // Absent optional elements write nothing and are omitted,
                // the same way metrique's own formats leave them out of
                // their arrays.
                Captured::Absent => {}
                // Recording a partial list would misrepresent the data;
                // poison the whole field instead (boxed entries reaching
                // dial9 through metrique's dyn bridge stringify numeric
                // elements and land here; see the module docs' limitations).
                Captured::Mismatched => {
                    *self.out = Captured::Mismatched;
                    return;
                }
            }
        }
        *self.out = Captured::Value(FieldValue::List(items));
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
/// each element of a list field.
struct ScalarCapture<'a, 'enc> {
    out: &'a mut Captured,
    kind: ScalarKind,
    entry_name: &'a str,
    enc: &'a mut ThreadLocalEncoder<'enc>,
}

impl ValueWriter for ScalarCapture<'_, '_> {
    fn string(self, value: &str) {
        match self.kind {
            ScalarKind::Str { interned: true } => {
                *self.out =
                    Captured::Value(FieldValue::PooledString(self.enc.intern_string(value)));
            }
            ScalarKind::Str { interned: false } => {
                *self.out = Captured::Value(FieldValue::String(value.to_owned()));
            }
            _ => {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        entry = %self.entry_name,
                        kind = ?self.kind,
                        "metrique value wrote a string for a non-string shape; value lost"
                    );
                });
                *self.out = Captured::Mismatched;
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
        let captured = match (single_observation(distribution), self.kind) {
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
        *self.out = match captured {
            Some(value) => Captured::Value(value),
            None => {
                // A metric callback fired but could not be mapped to the
                // planned shape; unlike an absent optional (which fires no
                // callback at all), this is data loss worth reporting.
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        entry = %self.entry_name,
                        kind = ?self.kind,
                        "metrique observation did not match its declared shape; value lost"
                    );
                });
                Captured::Mismatched
            }
        };
    }

    fn error(self, error: metrique_writer::ValidationError) {
        validation_error(self.entry_name, &error);
    }
}

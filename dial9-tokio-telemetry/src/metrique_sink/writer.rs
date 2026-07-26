//! The flush-thread `Entry::write` walk: routes each metrique value callback
//! to the event header, a payload slot, or nowhere, per the cached plan.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use dial9_trace_format::types::FieldValue;
use metrique_writer::{EntryConfig, EntryWriter, Observation, Value, ValueWriter};

use crate::rate_limit::rate_limited;
use crate::telemetry::{ThreadLocalEncoder, WorkerId, clock_monotonic_ns};

use super::plan::{ContextRole, FieldAction, HEADER_FIELDS, Plan, ValueKind};

/// Captured event-header values, filled in by [`ContextRole`]-routed fields
/// as the walk encounters them.
#[derive(Debug, Default)]
struct ContextValues {
    worker_id: Option<u64>,
    task_id: Option<u64>,
    monotonic_start: Option<u64>,
    monotonic_end: Option<u64>,
    /// From the `EntryWriter::timestamp` callback (`#[metrics(timestamp)]`).
    wall_clock_ns: Option<u64>,
}

/// How `value` callbacks are routed to actions.
///
/// A resolving walk routes by name (correct in any order) and records the
/// order it saw; the cached walk replays that order. A metrique upgrade that
/// changes write order is re-resolved at process start, so the cache only
/// needs the count guard against dynamic fields.
enum Dispatch<'p> {
    /// First walk of an entry type: look actions up by field name and record
    /// them in write order for subsequent walks.
    Resolve {
        by_name: &'p HashMap<String, FieldAction>,
        recorded: Vec<FieldAction>,
    },
    /// Steady state: index the recorded actions by callback position.
    Cached(&'p [FieldAction]),
}

/// [`EntryWriter`] that walks one entry against its cached [`Plan`].
///
/// Counts callbacks and flags unknown names so [`aligned`](Self::aligned)
/// can reject entries with dynamic fields; [`fill_values`](Self::fill_values)
/// assembles the final wire value vector.
pub(crate) struct EntryWalk<'p, 'enc> {
    plan: &'p Plan,
    enc: &'p mut ThreadLocalEncoder<'enc>,
    /// Payload capture slots, reused across entries by the caller.
    slots: &'p mut Vec<Option<FieldValue>>,
    dispatch: Dispatch<'p>,
    ctx: ContextValues,
    /// Number of `value` callbacks seen so far.
    seen: usize,
    /// A callback fired for a field name the descriptor does not declare.
    saw_unknown: bool,
}

impl<'p, 'enc> EntryWalk<'p, 'enc> {
    pub(crate) fn new(
        plan: &'p Plan,
        enc: &'p mut ThreadLocalEncoder<'enc>,
        slots: &'p mut Vec<Option<FieldValue>>,
    ) -> Self {
        slots.clear();
        slots.resize_with(plan.payload_optional.len(), || None);
        let dispatch = match &plan.positional {
            Some(actions) => Dispatch::Cached(actions),
            None => Dispatch::Resolve {
                by_name: &plan.actions,
                recorded: Vec::with_capacity(plan.expected_values),
            },
        };
        Self {
            plan,
            enc,
            slots,
            dispatch,
            ctx: ContextValues::default(),
            seen: 0,
            saw_unknown: false,
        }
    }

    /// Whether the observed `value` callbacks matched the plan: exactly the
    /// declared number, all with declared names.
    ///
    /// A mismatch means the entry emitted dynamic fields (e.g. a `Flex`
    /// map) and routing cannot be trusted to be complete; the caller must
    /// drop the event.
    pub(crate) fn aligned(&self) -> bool {
        !self.saw_unknown && self.seen == self.plan.expected_values
    }

    /// The write-order action sequence recorded by a resolving walk, for the
    /// caller to cache. `None` for cached-dispatch walks.
    pub(crate) fn take_recorded(&mut self) -> Option<Vec<FieldAction>> {
        match &mut self.dispatch {
            Dispatch::Resolve { recorded, .. } => Some(std::mem::take(recorded)),
            Dispatch::Cached(_) => None,
        }
    }

    /// Assemble the wire value vector into `out`: `[timestamp, worker_id,
    /// task_id, monotonic_ns_end, wall_clock_ns, payload...]`.
    ///
    /// Returns `Err(field_name)` when a non-optional payload field produced
    /// no value (a shape/value mismatch); the caller must drop the event,
    /// since the wire format has no absent encoding for required fields.
    pub(crate) fn fill_values(&mut self, out: &mut Vec<FieldValue>) -> Result<(), String> {
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
        out.push(FieldValue::Varint(
            self.ctx.worker_id.unwrap_or(WorkerId::UNKNOWN.as_u64()),
        ));
        out.push(opt(self.ctx.task_id));
        out.push(opt(self.ctx.monotonic_end));
        out.push(opt(self.ctx.wall_clock_ns));

        // Schema fields exclude the timestamp: payload slot i is schema
        // field HEADER_FIELDS + i.
        for (i, (slot, optional)) in self
            .slots
            .iter_mut()
            .zip(&self.plan.payload_optional)
            .enumerate()
        {
            match slot.take() {
                Some(value) => out.push(value),
                None if *optional => out.push(FieldValue::None),
                None => {
                    let name = match self.plan.schema.fields().get(HEADER_FIELDS + i) {
                        Some(field) => field.name().to_owned(),
                        // Slot/schema misalignment is a sink bug; make it visible.
                        None => format!("<payload slot {i}>"),
                    };
                    return Err(name);
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
            .field("seen", &self.seen)
            .finish_non_exhaustive()
    }
}

impl<'a> EntryWriter<'a> for EntryWalk<'_, '_> {
    fn timestamp(&mut self, timestamp: SystemTime) {
        self.ctx.wall_clock_ns = timestamp
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
    }

    fn value(&mut self, name: impl Into<Cow<'a, str>>, value: &(impl Value + ?Sized)) {
        let index = self.seen;
        self.seen += 1;
        let action = match &mut self.dispatch {
            Dispatch::Cached(actions) => match actions.get(index) {
                Some(action) => *action,
                None => {
                    // Dynamic fields appeared; `aligned` rejects the event.
                    self.saw_unknown = true;
                    return;
                }
            },
            Dispatch::Resolve { by_name, recorded } => {
                let name = name.into();
                match by_name.get(name.as_ref()) {
                    Some(action) => {
                        recorded.push(*action);
                        *action
                    }
                    None => {
                        // Field the descriptor does not declare (e.g. Flex).
                        self.saw_unknown = true;
                        return;
                    }
                }
            }
        };
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
            let mut iter = distribution.into_iter();
            let first = iter.next();
            if iter.next().is_some() {
                return;
            }
            if let Some(Observation::Unsigned(v)) = first {
                *self.0 = Some(v);
            }
        }

        fn error(self, _error: metrique_writer::ValidationError) {}
    }

    let mut out = None;
    value.write(U64Capture(&mut out));
    out
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

impl ValueWriter for ValueCapture<'_, '_> {
    fn string(self, value: &str) {
        match self.kind {
            ValueKind::Str { interned: true } => {
                *self.out = Some(FieldValue::PooledString(self.enc.intern_string(value)));
            }
            ValueKind::Str { interned: false } => {
                *self.out = Some(FieldValue::String(value.to_owned()));
            }
            _ => {}
        }
    }

    fn metric<'a>(
        self,
        distribution: impl IntoIterator<Item = Observation>,
        _unit: metrique_writer::Unit,
        _dimensions: impl IntoIterator<Item = (&'a str, &'a str)>,
        _flags: metrique_writer::MetricFlags<'_>,
    ) {
        let mut iter = distribution.into_iter();
        let first = iter.next();
        let single = iter.next().is_none();
        // Planned kinds are all single-observation scalars (distribution
        // shapes are Opaque and never planned), so anything else falls
        // through to the mismatch warn below.
        *self.out = match (first.filter(|_| single), self.kind) {
            (None, _) => None,
            (Some(Observation::Unsigned(v)), ValueKind::Bool) => Some(FieldValue::Bool(v != 0)),
            (Some(Observation::Unsigned(v)), ValueKind::Uint) => Some(FieldValue::Varint(v)),
            (Some(Observation::Unsigned(v)), ValueKind::Int) => {
                i64::try_from(v).ok().map(FieldValue::I64)
            }
            // `Observation` has no signed variant, so signed-shape values
            // (necessarily custom `Value` impls) arrive as floats.
            (Some(Observation::Floating(v)), ValueKind::Int) if v.fract() == 0.0 => {
                (v >= i64::MIN as f64 && v <= i64::MAX as f64).then_some(FieldValue::I64(v as i64))
            }
            (Some(Observation::Unsigned(v)), ValueKind::Float) => Some(FieldValue::F64(v as f64)),
            (Some(Observation::Floating(v)), ValueKind::Float) => Some(FieldValue::F64(v)),
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
        rate_limited!(Duration::from_secs(60), {
            tracing::warn!(
                entry = %self.entry_name,
                %error,
                "metrique value failed validation; field left absent"
            );
        });
    }

    // No `values()` override: list data arrives via the default comma-joined
    // `string()` fallback; see `ValueKind::Str`.
}

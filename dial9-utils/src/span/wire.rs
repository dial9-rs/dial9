//! On-wire identity helpers for ad-hoc spans.
//!
//! Span enter/exit events are emitted through per-call-site generated
//! [`TraceEvent`](dial9_trace_format::TraceEvent) structs (see the
//! [`dial9_span!`](crate::dial9_span) macro and [`Dial9Span::new`](super::Dial9Span::new)).
//! Each call site registers two schemas, named `SpanEnter:<id>` and
//! `SpanExit:<id>`, where `<id>` is unique per call site (the viewer keys on the
//! `SpanEnter:` / `SpanExit:` prefix and groups by it). This matches the format
//! emitted by the `tracing` layer (`dial9_tokio_telemetry::tracing_layer`), so the viewer renders
//! ad-hoc spans and `tracing` spans on the same timeline.

use dial9_trace_format::types::EventEncoder;
use dial9_trace_format::{TraceEvent, TraceField, types::FieldType};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

/// A span's process-unique identity, handed out by [`Span::id`](super::Span::id)
/// and accepted by [`Span::with_parent_id`](super::Span::with_parent_id).
///
/// A newtype rather than a bare `u64` so a span id cannot be confused with a
/// task id, worker id, or any other id that crosses the same call sites. Rides
/// the wire exactly as the underlying `u64`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SpanId(u64);

impl SpanId {
    /// The raw id, for logging or for correlating with a decoded trace.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl TraceField for SpanId {
    fn field_type() -> FieldType {
        <u64 as TraceField>::field_type()
    }

    fn encode<W: io::Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        <u64 as TraceField>::encode(&self.0, enc)
    }
}

/// Emitted once when a span is finalized (dropped). Tells the viewer the span
/// will receive no further enter/exit segments.
///
/// Shared with the `tracing` layer (`dial9_tokio_telemetry::tracing_layer`) so both producers
/// emit the identical `SpanCloseEvent` schema.
#[derive(TraceEvent)]
#[traceevent(wire_slot)]
pub(crate) struct SpanCloseEvent {
    #[traceevent(timestamp)]
    pub(crate) timestamp_ns: u64,
    pub(crate) span_id: SpanId,
}

/// Ad-hoc spans live in a separate id space from `tracing` span ids (which are
/// small and assigned per-subscriber). Setting the top bit keeps the two from
/// colliding when both appear in the same trace.
const ADHOC_SPAN_ID_BIT: u64 = 1 << 63;

static NEXT_SPAN_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a process-unique id for an ad-hoc span. Used by the
/// [`dial9_span!`](crate::dial9_span) expansion; not a stable API.
#[doc(hidden)]
pub(crate) fn next_span_id() -> SpanId {
    SpanId(NEXT_SPAN_ID.fetch_add(1, Ordering::Relaxed) | ADHOC_SPAN_ID_BIT)
}

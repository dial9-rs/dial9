//! Wall-clock encoding cost of a span event for the `dial9_span!` macro path,
//! against the cost of the field set itself.
//!
//! - `macro_span_event`: what the `dial9_span!` macro generates — a per-call-site
//!   `#[derive(TraceEvent)]` struct written via `Encoder::write`. This is the
//!   recommended, fast path and is identical to a hand-written ("raw") event.
//! - `macro_span_event_inline`: the same with the user field written inline
//!   (owned `String`, no string-pool lookup) — what `dial9_span!(..., id = ~v)`
//!   generates.
//!
//! Usage:
//!   cargo bench --bench span_encode_bench
//!
//! The schema registration / first-string-interning cost is amortized by
//! writing a batch of events per measured iteration (steady state is what the
//! hot path actually pays).

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::{InternedString, TraceEvent};
use std::hint::black_box;

const BATCH: u64 = 512;

/// The fields shared by both encoders: worker id, span id, the span name, and
/// one user-defined field.
const SPAN_NAME: &str = "handle_request";
const FIELD_VALUE: &str = "req-0123456789";

/// Hand-written event a user would derive to emit spans "manually" — the field
/// set the macro generates, with the user field interned.
#[derive(TraceEvent)]
struct RawSpanEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u64,
    span_id: u64,
    span_name: InternedString,
    request_id: InternedString,
}

/// Hand-written event with the user field written **inline** (not interned) —
/// what `dial9_span!(..., id = ~value)` generates.
#[derive(TraceEvent)]
struct RawInlineSpanEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u64,
    span_id: u64,
    span_name: InternedString,
    request_id: String,
}

/// Encode one event through the fast derived path (interned user field).
#[inline]
fn write_raw(enc: &mut Encoder<Vec<u8>>, ts: u64) {
    let name = enc.intern_string_infallible(SPAN_NAME);
    let value = enc.intern_string_infallible(FIELD_VALUE);
    enc.write_infallible(&RawSpanEvent {
        timestamp_ns: ts,
        worker_id: 7,
        span_id: 0x8000_0000_0000_0001,
        span_name: name,
        request_id: value,
    });
}

/// Encode one event with the user field inline (owned `String`, no pool lookup).
#[inline]
fn write_raw_inline(enc: &mut Encoder<Vec<u8>>, ts: u64) {
    let name = enc.intern_string_infallible(SPAN_NAME);
    enc.write_infallible(&RawInlineSpanEvent {
        timestamp_ns: ts,
        worker_id: 7,
        span_id: 0x8000_0000_0000_0001,
        span_name: name,
        request_id: FIELD_VALUE.to_string(),
    });
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("span_encode");
    group.throughput(Throughput::Elements(BATCH));

    group.bench_function("macro_span_event", |b| {
        b.iter_batched_ref(
            Encoder::new,
            |enc| {
                for i in 0..BATCH {
                    write_raw(enc, 1_000 + i * 10);
                }
                black_box(&*enc);
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("macro_span_event_inline", |b| {
        b.iter_batched_ref(
            Encoder::new,
            |enc| {
                for i in 0..BATCH {
                    write_raw_inline(enc, 1_000 + i * 10);
                }
                black_box(&*enc);
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);

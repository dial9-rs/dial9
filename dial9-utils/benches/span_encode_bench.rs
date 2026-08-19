//! Head-to-head: the [`dial9_span!`](dial9_utils::dial9_span) macro
//! vs. hand-written `#[derive(TraceEvent)]` span events.
//!
//! Both paths allocate a span id and emit enter → exit → close through the same
//! `Dial9Handle`/encoder, with the same typed fields (two `u64`s). The macro is
//! meant to be a zero-cost abstraction over the hand-written form, so the two
//! bars should sit on top of each other.
//!
//! Usage:
//!   cargo bench --bench span_encode_bench

use criterion::{Criterion, criterion_group, criterion_main};
use dial9_core::buffer::MemoryBuffer;
use dial9_core::clock::clock_monotonic_ns;
use dial9_core::handle::{Dial9Handle, set_tl_handle};
use dial9_core::recorder::recorder;
use dial9_trace_format::{InternedString, TraceEvent};
use dial9_utils::dial9_span;
use dial9_utils::span::Span as _;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};

// ── Hand-written baseline: exactly what the macro generates, written by hand ──

#[derive(TraceEvent)]
#[traceevent(name = "SpanEnter:handbench")]
struct HandEnter {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    #[traceevent(name = "dial9.tokio.task_id", role = "tokio.task_id")]
    task_id: Option<u64>,
    span_id: u64,
    parent_span_id: Option<u64>,
    #[traceevent(role = "span.name")]
    span_name: InternedString,
    x: u64,
    y: u64,
}

#[derive(TraceEvent)]
#[traceevent(name = "SpanExit:handbench")]
struct HandExit {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    #[traceevent(name = "dial9.tokio.task_id", role = "tokio.task_id")]
    task_id: Option<u64>,
    span_id: u64,
    #[traceevent(role = "span.name")]
    span_name: InternedString,
    active_ns: u64,
    idle_ns: u64,
    poll_count: u64,
    completed: bool,
    x: u64,
    y: u64,
}

#[derive(TraceEvent)]
#[traceevent(wire_slot)]
struct HandClose {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    span_id: u64,
}

const ADHOC_ID_BIT: u64 = 1 << 63;
static HAND_NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn hand_span(x: u64, y: u64) {
    let span_id = HAND_NEXT_ID.fetch_add(1, Ordering::Relaxed) | ADHOC_ID_BIT;
    // Optimal hand-written span, matching what the macro compiles to: one handle
    // for the enter/exit pair (same thread), a fresh one for the close (a
    // future's drop may land on another thread).
    let handle = Dial9Handle::current();
    let enter_ns = clock_monotonic_ns();
    handle.with_encoder(|enc| {
        let span_name = enc.intern_string("handbench");
        enc.encode(&HandEnter {
            timestamp_ns: clock_monotonic_ns(),
            task_id: None,
            span_id,
            parent_span_id: None,
            span_name,
            x,
            y,
        });
    });
    // Measure the scope's active duration, exactly as the macro's guard does.
    let active_ns = clock_monotonic_ns().saturating_sub(enter_ns);
    handle.with_encoder(|enc| {
        let span_name = enc.intern_string("handbench");
        enc.encode(&HandExit {
            timestamp_ns: clock_monotonic_ns(),
            task_id: None,
            span_id,
            span_name,
            active_ns,
            idle_ns: 0,
            poll_count: 1,
            completed: true,
            x,
            y,
        });
    });
    let close_handle = Dial9Handle::current();
    if close_handle.is_enabled() {
        close_handle.record_event(HandClose {
            timestamp_ns: clock_monotonic_ns(),
            span_id,
        });
    }
}

// ── The macro path: construct, enter (emit enter), drop guard (emit exit),
//    drop span (emit close). Same three events, typed fields. ──

fn macro_span(x: u64, y: u64) {
    let span = dial9_span!("handbench", x: u64 = x, y: u64 = y);
    let entered = span.enter();
    drop(entered);
    // `span` drops here → close.
}

fn bench_span_emit(c: &mut Criterion) {
    // Enabled in-memory recorder installed on this (the bench) thread, so
    // `Dial9Handle::current()` is live and events actually encode.
    let rec = recorder(MemoryBuffer::new(1 << 24).expect("memory buffer")).build();
    set_tl_handle(rec.handle().clone());

    // Guard against silently measuring disabled no-ops: the handle must be live
    // and the encoder closure must actually run on this thread.
    let handle = Dial9Handle::current();
    assert!(handle.is_enabled(), "bench handle is not recording");
    let mut encoded = false;
    handle.with_encoder(|_enc| encoded = true);
    assert!(
        encoded,
        "with_encoder did not run — events would not encode"
    );

    // Warm up: first emit registers this thread's encoder buffer.
    hand_span(1, 2);
    macro_span(1, 2);

    let mut group = c.benchmark_group("span_emit");
    group.bench_function("hand_written", |b| {
        b.iter(|| hand_span(black_box(7), black_box(11)))
    });
    group.bench_function("dial9_span_macro", |b| {
        b.iter(|| macro_span(black_box(7), black_box(11)))
    });
    group.finish();
}

criterion_group!(benches, bench_span_emit);
criterion_main!(benches);

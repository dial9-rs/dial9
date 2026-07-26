//! Benchmarks for the metrique → dial9 sink.
//!
//! Measures the costs that matter:
//! - `Dial9Context::capture()` and the close-time clock read: the
//!   caller-thread (request-path) costs.
//! - `Dial9Stream::next()` steady state: per-entry encode on the flush
//!   thread with a cached plan, in three payload variants (scalars only,
//!   all strings interned, one allocating string) plus the boxed
//!   global-sink path.
//! - `Dial9Stream::next()` with a paused recorder and with a disabled
//!   handle: the wiring-left-in costs.
//!
//! Usage:
//!   cargo bench --bench metrique_sink_bench --features metrique-sink

use criterion::{Criterion, criterion_group, criterion_main};
use dial9_tokio_telemetry::metrique_sink::{Dial9Context, Dial9Stream, Emit, Interned};
use dial9_tokio_telemetry::telemetry::{Dial9Handle, MemoryBuffer, recorder};
use metrique::unit::Microsecond;
use metrique::unit_of_work::metrics;
use metrique_writer::EntryIoStream;
use std::hint::black_box;

#[metrics(rename_all = "PascalCase", default_flags(Emit))]
struct RequestMetrics {
    #[metrics(flatten)]
    dial9: Dial9Context,
    #[metrics(flags(Interned))]
    route: String,
    operation: &'static str,
    #[metrics(unit = Microsecond)]
    latency_us: u64,
    retries: Option<u64>,
    success: bool,
    load: f64,
}

fn sample() -> RequestMetrics {
    RequestMetrics {
        dial9: Dial9Context::capture(),
        route: "/pets".to_owned(),
        operation: "GetPet",
        latency_us: 1500,
        retries: Some(2),
        success: true,
        load: 0.5,
    }
}

fn closed_entry() -> impl metrique_writer::Entry {
    metrique::RootEntry::new(metrique::CloseValue::close(sample()))
}

fn bench_capture(c: &mut Criterion) {
    c.bench_function("dial9_context_capture", |b| {
        b.iter(|| black_box(Dial9Context::capture()));
    });
    // The full request-path cost: capture plus the end-timestamp clock read
    // that `MonotonicAtClose` performs when the entry closes.
    c.bench_function("dial9_context_capture_and_close", |b| {
        b.iter(|| {
            let ctx = black_box(Dial9Context::capture());
            black_box(metrique::CloseValue::close(ctx))
        });
    });
}

/// Scalars only: the machinery floor, no per-entry allocations.
#[metrics(rename_all = "PascalCase", default_flags(Emit))]
struct ScalarMetrics {
    #[metrics(flatten)]
    dial9: Dial9Context,
    latency_us: u64,
    retries: Option<u64>,
    success: bool,
    load: f64,
}

/// Low-cardinality labels routed through the string pool: no allocations.
#[metrics(rename_all = "PascalCase", default_flags(Emit))]
struct InternedMetrics {
    #[metrics(flatten)]
    dial9: Dial9Context,
    #[metrics(flags(Interned))]
    route: String,
    #[metrics(flags(Interned))]
    operation: &'static str,
    latency_us: u64,
    success: bool,
}

fn scalar_entry() -> impl metrique_writer::Entry {
    metrique::RootEntry::new(metrique::CloseValue::close(ScalarMetrics {
        dial9: Dial9Context::capture(),
        latency_us: 1500,
        retries: Some(2),
        success: true,
        load: 0.5,
    }))
}

fn interned_entry() -> impl metrique_writer::Entry {
    metrique::RootEntry::new(metrique::CloseValue::close(InternedMetrics {
        dial9: Dial9Context::capture(),
        route: "/pets".to_owned(),
        operation: "GetPet",
        latency_us: 1500,
        success: true,
    }))
}

fn recording_stream() -> Dial9Stream {
    let writer = MemoryBuffer::new(64 * 1024 * 1024).unwrap();
    let recorder = recorder(writer).build();
    recorder.enable();
    let stream = Dial9Stream::new(recorder.handle());
    // The recorder must outlive the stream for recording to stay on.
    std::mem::forget(recorder);
    stream
}

fn bench_stream_next(c: &mut Criterion) {
    let mut stream = recording_stream();

    // One allocating (non-interned) string: the realistic mixed case.
    let entry = closed_entry();
    stream.next(&entry).unwrap();
    c.bench_function("stream_next_steady_state", |b| {
        b.iter(|| stream.next(black_box(&entry)).unwrap());
    });

    let scalar = scalar_entry();
    stream.next(&scalar).unwrap();
    c.bench_function("stream_next_scalar_only", |b| {
        b.iter(|| stream.next(black_box(&scalar)).unwrap());
    });

    let interned = interned_entry();
    stream.next(&interned).unwrap();
    c.bench_function("stream_next_all_interned", |b| {
        b.iter(|| stream.next(black_box(&interned)).unwrap());
    });

    // The global-sink path (`ServiceMetrics::sink()`) boxes entries and
    // routes every value through metrique's dyn bridge.
    let boxed = metrique_writer::Entry::boxed(closed_entry());
    stream.next(&boxed).unwrap();
    c.bench_function("stream_next_boxed", |b| {
        b.iter(|| stream.next(black_box(&boxed)).unwrap());
    });
}

fn bench_inactive(c: &mut Criterion) {
    let handle = Dial9Handle::disabled();
    let mut stream = Dial9Stream::new(&handle);
    let entry = closed_entry();

    c.bench_function("stream_next_disabled", |b| {
        b.iter(|| stream.next(black_box(&entry)).unwrap());
    });

    // Connected but paused: must be as cheap as disabled.
    let writer = MemoryBuffer::new(1024 * 1024).unwrap();
    let recorder = recorder(writer).build();
    recorder.disable();
    let mut stream = Dial9Stream::new(recorder.handle());
    c.bench_function("stream_next_paused", |b| {
        b.iter(|| stream.next(black_box(&entry)).unwrap());
    });
}

criterion_group!(benches, bench_capture, bench_stream_next, bench_inactive);
criterion_main!(benches);

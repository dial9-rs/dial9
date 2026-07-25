//! Benchmarks for the metrique → dial9 sink.
//!
//! Measures the three costs that matter:
//! - `Dial9Context::capture()`: the only caller-thread (request-path) cost.
//! - `Dial9Stream::next()` steady state: per-entry encode on the flush
//!   thread with a cached plan.
//! - `Dial9Stream::next()` with a disabled handle: the wiring-left-in cost.
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
}

fn bench_stream_next(c: &mut Criterion) {
    let writer = MemoryBuffer::new(64 * 1024 * 1024).unwrap();
    let recorder = recorder(writer).build();
    recorder.enable();
    let mut stream = Dial9Stream::new(recorder.handle());

    let entry = closed_entry();
    // Prime the plan cache.
    stream.next(&entry).unwrap();

    c.bench_function("stream_next_steady_state", |b| {
        b.iter(|| stream.next(black_box(&entry)).unwrap());
    });
}

fn bench_disabled(c: &mut Criterion) {
    let handle = Dial9Handle::disabled();
    let mut stream = Dial9Stream::new(&handle);
    let entry = closed_entry();

    c.bench_function("stream_next_disabled", |b| {
        b.iter(|| stream.next(black_box(&entry)).unwrap());
    });
}

criterion_group!(benches, bench_capture, bench_stream_next, bench_disabled);
criterion_main!(benches);

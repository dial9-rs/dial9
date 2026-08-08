//! Wall-clock benches for the tracing layer.
//!
//! `single_thread`: per-span enter+exit cost through the full dial9 encode
//! path (recorder + in-memory writer on a `current_thread` runtime), with a
//! `tracing_only` comparator to isolate the dial9 encoding overhead. This is
//! the wall-clock companion to `tracing_layer_iai.rs`.
//!
//! `multi_thread`: span emission throughput as N threads share one
//! `Dial9TracingLayer` (and therefore one schemas Mutex). Each thread
//! emits 100 spans after a barrier sync; the iteration is timed end to
//! end across N ∈ {1, 2, 4, 8, 16, 32}.
//!
//! Usage:
//!   cargo bench -p dial9-utils --bench tracing_layer_bench --features tracing-layer

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dial9_tokio_telemetry::telemetry::{
    Dial9Handle, Dial9HandleTokioExt, MemoryBuffer, TokioAttachOptions, recorder,
};
use dial9_utils::tracing_layer::Dial9TracingLayer;
use std::sync::{Arc, Barrier};
use tracing::Dispatch;
use tracing_subscriber::prelude::*;

fn bench_single_thread(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_thread");
    group.measurement_time(std::time::Duration::from_secs(5));

    // Recorder with an in-memory writer and a current_thread runtime, so
    // block_on runs spans on a dial9-claimed thread and the layer encodes
    // for real (no disk I/O in the measurement).
    let recorder = recorder(MemoryBuffer::new(64 * 1024 * 1024).unwrap()).build();
    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    let runtime = recorder
        .handle()
        .attach_tokio_runtime(builder, TokioAttachOptions::default())
        .unwrap();

    // Sanity check: the bench must exercise the enabled encode path.
    runtime.block_on(async {
        assert!(
            Dial9Handle::current().is_enabled(),
            "bench thread is not claimed by the dial9 runtime; \
             the measurement would cover only the disabled early-return"
        );
    });

    // Comparator: identical span workload with no dial9 layer, to separate
    // tracing dispatch cost from dial9 encoding cost.
    let tracing_only = Dispatch::new(tracing_subscriber::registry());
    group.bench_function("tracing_only/span_enter_exit", |b| {
        let _g = tracing::dispatcher::set_default(&tracing_only);
        runtime.block_on(async {
            b.iter(|| {
                let span = tracing::info_span!("bench", key = "value");
                let _enter = span.enter();
            });
        });
    });

    let with_dial9 = Dispatch::new(tracing_subscriber::registry().with(Dial9TracingLayer::new()));
    group.bench_function("with_dial9/span_enter_exit", |b| {
        let _g = tracing::dispatcher::set_default(&with_dial9);
        runtime.block_on(async {
            b.iter(|| {
                let span = tracing::info_span!("bench", key = "value");
                let _enter = span.enter();
            });
        });
    });

    group.bench_function("with_dial9/span_enter_exit_fields", |b| {
        let _g = tracing::dispatcher::set_default(&with_dial9);
        runtime.block_on(async {
            b.iter(|| {
                let span = tracing::info_span!(
                    "fielded",
                    user_id = 42,
                    method = "GET",
                    path = "/api/v1/users"
                );
                let _enter = span.enter();
            });
        });
    });

    group.finish();
}

fn bench_multi_thread(c: &mut Criterion) {
    let mut group = c.benchmark_group("multi_thread");
    group.measurement_time(std::time::Duration::from_secs(8));

    // One shared subscriber (one Dial9TracingLayer, one schemas Mutex).
    // All threads dispatch through this single layer, contending on the mutex.
    let dispatch = Dispatch::new(tracing_subscriber::registry().with(Dial9TracingLayer::new()));

    let spans_per_thread = 100;

    for threads in [1, 2, 4, 8, 16, 32] {
        group.bench_with_input(
            BenchmarkId::new("schema_contention", threads),
            &threads,
            |b, &n| {
                b.iter(|| {
                    let barrier = Arc::new(Barrier::new(n));
                    std::thread::scope(|s| {
                        for _ in 0..n {
                            let d = dispatch.clone();
                            let bar = barrier.clone();
                            s.spawn(move || {
                                let _g = tracing::dispatcher::set_default(&d);
                                bar.wait();
                                for _ in 0..spans_per_thread {
                                    let span = tracing::info_span!("contended", key = "value");
                                    let _enter = span.enter();
                                }
                            });
                        }
                    });
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_single_thread, bench_multi_thread);
criterion_main!(benches);

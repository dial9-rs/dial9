//! IAI callgrind micro-benchmark for the traced-task poll path.
//!
//! Two shapes, because the wrapper charges in two places:
//! - `poll_one_task`: one task yielding N times, so the count is dominated by
//!   the per-poll path (`WakeTraced::poll`).
//! - `spawn_many_tasks`: N tasks that finish on their first poll, so the count
//!   is dominated by per-task setup (`TracedFuture` -> `WakeTraced::new`).
//!
//! A `tokio::spawn` variant of each runs alongside as the uninstrumented
//! floor; the interesting number is the gap, not the absolute.
//!
//! Usage:
//!   cargo bench --bench traced_poll_iai
//!
//! Gated on `--cfg iai_enabled` so plain `cargo test --all-targets`
//! compiles to a no-op stub `fn main()` instead of spawning
//! `iai-callgrind-runner`. CI iai jobs set RUSTFLAGS to enable.

#![cfg_attr(not(iai_enabled), allow(unused))]

#[cfg(not(iai_enabled))]
fn main() {}

use dial9_core::recording::Recorder;
use dial9_tokio_telemetry::telemetry::{
    Dial9HandleTokioExt, MemoryBuffer, TokioAttachOptions, recorder, spawn,
};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use std::hint::black_box;

/// Large enough that the workload never fills the buffer: a full buffer would
/// start dropping events and change the instruction count under measurement.
const CAPTURE_SIZE: u64 = 64 * 1024 * 1024;

/// An attached `current_thread` runtime and the recorder feeding it. Built in
/// bench setup, so neither attach nor shutdown lands in the counts.
struct Attached {
    runtime: tokio::runtime::Runtime,
    /// Held only to keep recording alive for the run. Declared after `runtime`
    /// so the runtime stops first.
    _recorder: Recorder,
}

fn attached() -> Attached {
    let recorder = recorder(MemoryBuffer::new(CAPTURE_SIZE).expect("fixed size is valid")).build();
    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    let runtime = recorder
        .handle()
        .attach_tokio_runtime(
            builder,
            TokioAttachOptions::builder().runtime_name("bench").build(),
        )
        .expect("build tokio runtime");
    Attached {
        runtime,
        _recorder: recorder,
    }
}

/// One dial9-spawned task yielding `n` times: `n + 1` polls through the wrapper.
fn poll_traced(at: &Attached, n: usize) {
    at.runtime.block_on(async move {
        spawn(async move {
            for _ in 0..n {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("task completes")
    });
}

/// The same shape through `tokio::spawn`: no wrapper, so the difference is
/// what the wrapper costs.
fn poll_raw(at: &Attached, n: usize) {
    at.runtime.block_on(async move {
        tokio::spawn(async move {
            for _ in 0..n {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("task completes")
    });
}

/// `n` dial9-spawned tasks that finish on their first poll.
fn spawn_traced(at: &Attached, n: usize) {
    at.runtime.block_on(async move {
        let mut handles = Vec::with_capacity(n);
        for _ in 0..n {
            handles.push(spawn(async {}));
        }
        for h in handles {
            h.await.expect("task completes");
        }
    });
}

fn spawn_raw(at: &Attached, n: usize) {
    at.runtime.block_on(async move {
        let mut handles = Vec::with_capacity(n);
        for _ in 0..n {
            handles.push(tokio::spawn(async {}));
        }
        for h in handles {
            h.await.expect("task completes");
        }
    });
}

#[library_benchmark]
#[bench::polls_1000(attached(), 1000)]
fn poll_one_task(at: Attached, n: usize) {
    poll_traced(black_box(&at), black_box(n));
}

#[library_benchmark]
#[bench::polls_1000(attached(), 1000)]
fn poll_one_task_untraced(at: Attached, n: usize) {
    poll_raw(black_box(&at), black_box(n));
}

#[library_benchmark]
#[bench::tasks_1000(attached(), 1000)]
fn spawn_many_tasks(at: Attached, n: usize) {
    spawn_traced(black_box(&at), black_box(n));
}

#[library_benchmark]
#[bench::tasks_1000(attached(), 1000)]
fn spawn_many_tasks_untraced(at: Attached, n: usize) {
    spawn_raw(black_box(&at), black_box(n));
}

library_benchmark_group!(
    name = traced_poll_group;
    benchmarks =
        poll_one_task,
        poll_one_task_untraced,
        spawn_many_tasks,
        spawn_many_tasks_untraced
);

#[cfg(iai_enabled)]
main!(library_benchmark_groups = traced_poll_group);

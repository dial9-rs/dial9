//! Generates a trace whose CPU samples fan out through rayon's work-stealing
//! scheduler — the shape that motivated the compact `/api/flamegraph` wire
//! formats and, eventually, zone eliding.
//!
//! Rayon's `join` and parallel-iterator bridge recurse to split work, so the
//! sampler catches each thread at a different split depth. A handful of (very
//! long, monomorphized) `join_context` / `bridge_producer_consumer::helper`
//! symbols therefore recombine into *thousands* of distinct root→leaf paths:
//!
//! ```text
//! handle_request → [thousands of paths through rayon] → score_document
//! ```
//!
//! The result is a flamegraph with enormous path cardinality and almost no
//! visibly wide leaf, which is what makes it a useful fixture for payload-size
//! and readability work in the viewer.
//!
//! Run it:
//!
//! ```sh
//! cargo run --release -p rayon-fanout
//! ```
//!
//! Release matters: a debug build spends its samples in unoptimized helpers
//! rather than the leaf work. The workspace `.cargo/config.toml` already sets
//! `--cfg tokio_unstable -C force-frame-pointers=yes`, which the unwinder needs.
//! CPU sampling also needs `perf_event_paranoid <= 2`:
//!
//! ```sh
//! cat /proc/sys/kernel/perf_event_paranoid
//! echo 2 | sudo tee /proc/sys/kernel/perf_event_paranoid
//! ```

use dial9::cpu::CpuProfilingConfig;
use dial9::{Dial9HandleTokioExt, DiskBuffer, RecorderPerfExt, TokioAttachOptions, recorder};
use rayon::prelude::*;
use std::time::Duration;

const TRACE_DIR: &str = "/tmp/rayon-fanout-traces";

/// How deep the recursive `join` tree splits. Each level doubles the leaf count
/// and adds a `join_context` frame, so this is the main dial on path
/// cardinality: depth 12 is 4,096 leaves reached over ~4,096 distinct paths.
const JOIN_DEPTH: u32 = 12;

/// Requests dispatched to the rayon pool, sequentially — each is internally
/// parallel. More requests means more samples per path rather than more paths,
/// so this is the dial on trace size. Override with `RAYON_FANOUT_REQUESTS`.
const REQUESTS: usize = 400;

/// Leaf work. `#[inline(never)]` keeps these as distinct frames so the
/// flamegraph shows where the real time went once the rayon scaffolding above
/// them is collapsed.
#[inline(never)]
fn score_document(seed: u64) -> u64 {
    let mut x = seed | 1;
    for _ in 0..24_000 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
    }
    std::hint::black_box(x)
}

#[inline(never)]
fn hash_block(seed: u64) -> u64 {
    let mut x = seed ^ 0x9e3779b97f4a7c15;
    for _ in 0..8_000 {
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51afd7ed558ccd);
    }
    std::hint::black_box(x)
}

/// Recursive `rayon::join` tree: every level adds another `join_context` frame,
/// and work stealing means each thread is at a different depth when sampled.
fn split_and_score(depth: u32, seed: u64) -> u64 {
    if depth == 0 {
        return score_document(seed);
    }
    let (left, right) = rayon::join(
        || split_and_score(depth - 1, seed.wrapping_mul(31)),
        || split_and_score(depth - 1, seed.wrapping_add(0x5bf0_3635)),
    );
    left ^ right
}

/// The parallel section of one request: a `par_iter` bridge plus a `join` tree,
/// so the trace carries both of rayon's recursive split paths.
fn parallel_scan(request: usize) -> u64 {
    let bridged: u64 = (0..4_096u64)
        .into_par_iter()
        .map(|i| hash_block(i ^ request as u64))
        .reduce(|| 0, |a, b| a ^ b);
    bridged ^ split_and_score(JOIN_DEPTH, request as u64)
}

/// One traced request. Rayon's `join` and `for_each` block the calling thread
/// until the parallel section finishes, so the dispatch goes to the blocking
/// pool rather than stalling a runtime worker.
async fn handle_request(request: usize) {
    let scanned = tokio::task::spawn_blocking(move || parallel_scan(request))
        .await
        .expect("parallel scan panicked");
    std::hint::black_box(scanned);
}

fn main() {
    let writer = DiskBuffer::builder()
        .base_path(TRACE_DIR)
        .max_file_size(20_000_000)
        .max_total_size(200_000_000)
        .build()
        .expect("build disk buffer");
    let recorder = recorder(writer)
        .with_cpu_profiling(CpuProfilingConfig::default())
        .build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(4);
    let rt = recorder
        .handle()
        .attach_tokio_runtime(
            builder,
            TokioAttachOptions::builder()
                .task_tracking_enabled(true)
                .build(),
        )
        .expect("build tokio runtime");

    let requests = std::env::var("RAYON_FANOUT_REQUESTS")
        .ok()
        .map(|v| v.parse().expect("RAYON_FANOUT_REQUESTS must be a number"))
        .unwrap_or(REQUESTS);
    eprintln!(
        "dispatching {requests} requests through rayon ({} threads, join depth {JOIN_DEPTH})...",
        rayon::current_num_threads()
    );
    rt.block_on(async {
        for request in 0..requests {
            handle_request(request).await;
        }
        // Let the flush thread drain the sampler's ring buffer.
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    // Graceful shutdown seals the segment and waits for the background worker to
    // symbolize it — without this the trace carries raw addresses, so the
    // flamegraph has no frame names to fan out.
    eprintln!("symbolizing (up to 60s)...");
    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(60));

    eprintln!("\n✓ Trace written to: {TRACE_DIR}");
    eprintln!("  cargo run --release -p dial9-viewer -- serve --local-dir {TRACE_DIR}");
    eprintln!("  then open http://localhost:3000");
}

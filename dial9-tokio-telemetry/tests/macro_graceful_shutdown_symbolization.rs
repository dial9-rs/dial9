//! Integration test: the `#[dial9_tokio_telemetry::main]` macro's implicit
//! graceful shutdown drains the background worker, which symbolizes the trace.
//!
//! Before the implicit graceful shutdown, a macro-based program never drained
//! the worker, so its trace contained no symbolized stacks. Here we use a
//! single large segment (large budget + default per-file size) so the segment
//! is sealed only at shutdown finalize — the worker has nothing to symbolize
//! mid-run. So the presence of any `SymbolTableEntry` event proves the macro's
//! implicit `graceful_shutdown` drained the worker. The test never calls
//! `graceful_shutdown` itself: `run_workload()` is the only call.
#![cfg(all(feature = "cpu-profiling", target_os = "linux"))]

use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::telemetry::cpu_profile::CpuProfilingConfig;
use dial9_trace_format::decoder::Decoder;
use flate2::read::GzDecoder;
use std::io::Read;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static TRACE_DIR: OnceLock<PathBuf> = OnceLock::new();
static OUTPUT_DIR: OnceLock<PathBuf> = OnceLock::new();

fn macro_test_config() -> Dial9Config {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.bin");
    let output = dir.path().join("output");
    TRACE_DIR.get_or_init(|| dir.path().to_path_buf());
    OUTPUT_DIR.get_or_init(|| output.clone());
    std::mem::forget(dir);

    Dial9Config::builder()
        .on_disk_buffer(&path)
        // Large budget + default per-file size => a single segment that is
        // sealed only at shutdown, so symbolization can't run mid-workload.
        .max_total_size(256 * 1024 * 1024)
        // Generous deadline so the drain finishes symbolizing the segment.
        .graceful_shutdown(Duration::from_secs(10))
        .with_runtime(|r| {
            r.with_cpu_profiling(CpuProfilingConfig::default().frequency_hz(999))
                .with_custom_pipeline(move |p| p.symbolize().gzip().write_back_to(output.clone()))
        })
        .build()
        .unwrap()
}

/// Burn CPU for a fixed window so the profiler reliably captures stack samples.
///
/// `#[inline(never)]` so it shows up as a stable frame to symbolize.
#[inline(never)]
fn burn_cpu_work() {
    let start = Instant::now();
    let mut x: u64 = 1;
    while start.elapsed() < Duration::from_millis(500) {
        for i in 0..10_000u64 {
            x = x.wrapping_mul(i | 1).wrapping_add(7);
        }
        std::hint::black_box(x);
    }
}

#[dial9_tokio_telemetry::main(config = macro_test_config)]
async fn run_workload() {
    let mut handles = Vec::new();
    for _ in 0..4 {
        handles.push(tokio::spawn(tokio::task::spawn_blocking(burn_cpu_work)));
    }
    for h in handles {
        let _ = h.await;
    }
}

#[test]
fn macro_implicit_graceful_shutdown_symbolizes_trace() {
    // The only call. The macro drops the runtime and drains the worker on return.
    run_workload();

    let output_dir = OUTPUT_DIR.get().expect("OUTPUT_DIR not set");

    let mut symbol_table_entries = 0usize;
    for entry in std::fs::read_dir(output_dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if !name.ends_with(".bin") && !name.ends_with(".bin.gz") {
            continue;
        }
        let raw = std::fs::read(&path).unwrap();
        if raw.is_empty() {
            continue;
        }
        // Worker-processed segments are gzip-compressed; fall back to raw.
        let bytes = decompress_gzip(&raw).unwrap_or(raw);
        let Some(mut dec) = Decoder::new(&bytes) else {
            continue;
        };
        dec.for_each_event(|ev| {
            if ev.name == "SymbolTableEntry" {
                symbol_table_entries += 1;
            }
        })
        .ok();
    }

    assert!(
        symbol_table_entries > 0,
        "expected SymbolTableEntry events: the macro's implicit graceful shutdown \
         should have drained the worker and symbolized the trace, but found none"
    );
}

fn decompress_gzip(data: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = GzDecoder::new(data);
    let mut buf = Vec::new();
    decoder.read_to_end(&mut buf).ok()?;
    Some(buf)
}

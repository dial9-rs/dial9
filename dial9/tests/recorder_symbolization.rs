//! Linux-only: sampling needs `perf_event_open`.
#![cfg(all(feature = "cpu-profiling", feature = "pipeline", target_os = "linux"))]

use dial9::core::pipeline::SymbolizeProcessor;
use dial9::core::worker::processors::WriteBackProcessor;
use dial9::cpu::{CpuProfiler, CpuProfilingConfig};
use dial9::{DiskBuffer, recorder};
use dial9_trace_format::decoder::Decoder;
use std::time::{Duration, Instant};

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

#[test]
fn recorder_symbolizes_cpu_samples() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = dir.path().join("output");
    std::fs::create_dir_all(&output).expect("output dir");

    // `single_file` seals only at shutdown, so the whole run lands in one
    // segment the worker symbolizes on drain.
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).expect("writer");
    let profiler = CpuProfiler::start(CpuProfilingConfig::default().frequency_hz(999))
        .expect("start cpu profiler");

    let traced = recorder(writer)
        .source(profiler)
        .pipe(SymbolizeProcessor::new())
        .pipe(WriteBackProcessor::to_dir(output.clone()))
        .build_and_start();

    // Burn CPU on several process threads; the process-wide profiler samples
    // them via perf `inherit`, no per-thread registration needed.
    let handles: Vec<_> = (0..4).map(|_| std::thread::spawn(burn_cpu_work)).collect();
    for h in handles {
        h.join().expect("burn thread");
    }

    traced
        .graceful_shutdown(Duration::from_secs(10))
        .expect("graceful shutdown");

    let mut symbol_table_entries = 0usize;
    for entry in std::fs::read_dir(&output).expect("read output dir") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().unwrap().to_string_lossy();
        if !name.ends_with(".bin") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("read segment");
        if bytes.is_empty() {
            continue;
        }
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
        "expected SymbolTableEntry events from the symbolize pipeline, found none"
    );
}

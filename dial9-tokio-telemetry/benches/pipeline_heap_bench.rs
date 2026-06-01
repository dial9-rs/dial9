//! pipeline_heap_bench, peak heap of the writer + worker pipeline across
//! disk / mem (with and without cpu-profiling).
//!
//! Workload (async sleep + CPU spin + child wakes) runs under a `System`-
//! wrapping global allocator that tracks live + peak bytes. Output: peak
//! heap at baseline (pre-build), steady-state (end of workload), and
//! post-shutdown. Pipeline is `gzip + noop`.
//!
//! ```bash
//! cargo bench --bench pipeline_heap_bench --features cpu-profiling
//!
//! # single mode for the cleanest baseline (no residual from prior runs)
//! cargo bench --bench pipeline_heap_bench --features cpu-profiling -- --mode disk
//!
//! # BMF JSON (uploaded by .github/workflows/benchmarks.yml on main pushes)
//! cargo bench --bench pipeline_heap_bench --features cpu-profiling -- --bmf
//! ```

#![cfg(feature = "cpu-profiling")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dial9_tokio_telemetry::background_task::{ProcessError, SegmentData, SegmentProcessor};
use dial9_tokio_telemetry::telemetry::cpu_profile::CpuProfilingConfig;
use dial9_tokio_telemetry::telemetry::{
    DiskWriter, InMemoryWriter, TelemetryHandle, TracedRuntime,
};

// ── Tracking allocator ─────────────────────────────────────────────────────

struct TrackingAllocator {
    inner: System,
    live: AtomicUsize,
    peak: AtomicUsize,
}

impl TrackingAllocator {
    const fn new() -> Self {
        Self {
            inner: System,
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }
    fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
    /// Reset the peak watermark to the current live value. Use between
    /// phases to measure per-phase deltas instead of run-cumulative peaks.
    fn reset_peak(&self) {
        let cur = self.live.load(Ordering::Relaxed);
        self.peak.store(cur, Ordering::Relaxed);
    }
    fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }
    fn bump(&self, n: usize) {
        let new_live = self.live.fetch_add(n, Ordering::Relaxed) + n;
        self.peak.fetch_max(new_live, Ordering::Relaxed);
    }
    fn drop(&self, n: usize) {
        self.live.fetch_sub(n, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.inner.alloc(layout) };
        if !p.is_null() {
            self.bump(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.inner.dealloc(ptr, layout) };
        self.drop(layout.size());
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.inner.alloc_zeroed(layout) };
        if !p.is_null() {
            self.bump(layout.size());
        }
        p
    }
    unsafe fn realloc(&self, ptr: *mut u8, old_layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { self.inner.realloc(ptr, old_layout, new_size) };
        if !p.is_null() {
            let old = old_layout.size();
            if new_size >= old {
                self.bump(new_size - old);
            } else {
                self.drop(old - new_size);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOC: TrackingAllocator = TrackingAllocator::new();

// ── Workload ───────────────────────────────────────────────────────────────

const WORKLOAD_TASKS: usize = 32;
const WORKLOAD_SECS: u64 = 10;
const TOTAL_BUDGET: u64 = 16 * 1024 * 1024;
const SEGMENT_SIZE: u64 = 512 * 1024;
const WORKER_THREADS: usize = 4;
/// Short rotation so the workload exercises rotation + ring-handoff.
const ROTATION_PERIOD: Duration = Duration::from_secs(3);

async fn workload(handle: TelemetryHandle, tasks_done: Arc<AtomicU64>) {
    let stop_at = Instant::now() + Duration::from_secs(WORKLOAD_SECS);
    let joins: Vec<_> = (0..WORKLOAD_TASKS)
        .map(|id| {
            let done = tasks_done.clone();
            handle.spawn(async move {
                let mut local: u64 = id as u64;
                while Instant::now() < stop_at {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    for i in 0..50_000u64 {
                        local = local.wrapping_add(i.wrapping_mul(31));
                    }
                    std::hint::black_box(local);
                    let (tx, rx) = tokio::sync::oneshot::channel::<u64>();
                    tokio::spawn(async move {
                        let _ = tx.send(local);
                    });
                    let _ = rx.await;
                    done.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for j in joins {
        let _ = j.await;
    }
}

/// Terminal processor: drops the payload, returns Ok.
struct NoopSink;
impl SegmentProcessor for NoopSink {
    fn name(&self) -> &'static str {
        "Noop"
    }
    fn process(
        &mut self,
        mut data: SegmentData,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SegmentData, ProcessError>> + Send + '_>,
    > {
        let _ = data.take_payload();
        Box::pin(async move { Ok(data) })
    }
}

// ── Per-mode measurement ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum Mode {
    Disk,
    Mem,
    DiskCpu,
    MemCpu,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Disk => "disk",
            Mode::Mem => "mem",
            Mode::DiskCpu => "disk-cpu-profiling",
            Mode::MemCpu => "mem-cpu-profiling",
        }
    }
}

#[derive(Debug, Default, Clone)]
struct Sample {
    baseline: usize,
    steady_state: usize,
    post_shutdown: usize,
}

fn measure(mode: Mode) -> Sample {
    ALLOC.reset_peak();
    let baseline = ALLOC.peak();

    let tasks_done = Arc::new(AtomicU64::new(0));

    // Scope writer/runtime so they drop before we sample post_shutdown.
    let steady_state = {
        let mut tk = tokio::runtime::Builder::new_multi_thread();
        tk.worker_threads(WORKER_THREADS).enable_all();

        let (runtime, guard) = match mode {
            Mode::Disk => {
                let tmp = tempfile::tempdir().unwrap();
                let trace_path = tmp.path().join("trace.bin");
                let writer = DiskWriter::builder()
                    .base_path(trace_path.to_str().unwrap())
                    .max_file_size(SEGMENT_SIZE)
                    .max_total_size(TOTAL_BUDGET)
                    .rotation_period(ROTATION_PERIOD)
                    .build()
                    .unwrap();
                let r = TracedRuntime::builder()
                    .with_task_tracking(true)
                    .with_trace_path(&trace_path)
                    .with_custom_pipeline(|p| p.gzip().pipe(NoopSink))
                    .build_and_start(tk, writer)
                    .expect("build_and_start (disk)");
                // Keep tmp alive for the duration; leak it intentionally so
                // its Drop doesn't show up in the measurement window.
                std::mem::forget(tmp);
                r
            }
            Mode::DiskCpu => {
                let tmp = tempfile::tempdir().unwrap();
                let trace_path = tmp.path().join("trace.bin");
                let writer = DiskWriter::builder()
                    .base_path(trace_path.to_str().unwrap())
                    .max_file_size(SEGMENT_SIZE)
                    .max_total_size(TOTAL_BUDGET)
                    .rotation_period(ROTATION_PERIOD)
                    .build()
                    .unwrap();
                let r = TracedRuntime::builder()
                    .with_task_tracking(true)
                    .with_cpu_profiling(CpuProfilingConfig::default().frequency_hz(199))
                    .with_trace_path(&trace_path)
                    .with_custom_pipeline(|p| p.symbolize().gzip().pipe(NoopSink))
                    .build_and_start(tk, writer)
                    .expect("build_and_start (disk+cpu)");
                std::mem::forget(tmp);
                r
            }
            Mode::Mem => {
                let writer = InMemoryWriter::builder()
                    .max_total_size(TOTAL_BUDGET)
                    .max_segment_size(SEGMENT_SIZE)
                    .rotation_period(ROTATION_PERIOD)
                    .build()
                    .expect("InMemoryWriter build");
                TracedRuntime::builder()
                    .with_task_tracking(true)
                    .with_custom_pipeline(|p| p.gzip().pipe(NoopSink))
                    .build_and_start(tk, writer)
                    .expect("build_and_start (mem)")
            }
            Mode::MemCpu => {
                let writer = InMemoryWriter::builder()
                    .max_total_size(TOTAL_BUDGET)
                    .max_segment_size(SEGMENT_SIZE)
                    .rotation_period(ROTATION_PERIOD)
                    .build()
                    .expect("InMemoryWriter build");
                TracedRuntime::builder()
                    .with_task_tracking(true)
                    .with_cpu_profiling(CpuProfilingConfig::default().frequency_hz(199))
                    .with_custom_pipeline(|p| p.symbolize().gzip().pipe(NoopSink))
                    .build_and_start(tk, writer)
                    .expect("build_and_start (mem+cpu)")
            }
        };
        guard.enable();
        let handle = guard.handle();
        runtime.block_on(workload(handle, tasks_done.clone()));
        let steady = ALLOC.peak();
        guard
            .graceful_shutdown(Duration::from_secs(30))
            .expect("graceful_shutdown");
        drop(runtime);
        steady
    };

    // After drop, give the system a beat to finalize any background releases.
    std::thread::sleep(Duration::from_millis(50));
    let post_shutdown = ALLOC.live();

    Sample {
        baseline,
        steady_state,
        post_shutdown,
    }
}

// ── Output ─────────────────────────────────────────────────────────────────

fn fmt_kib(bytes: usize) -> String {
    format!("{:.1} KiB", bytes as f64 / 1024.0)
}

fn fmt_mib(bytes: usize) -> String {
    format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
}

fn print_table(rows: &[(Mode, Sample)]) {
    eprintln!();
    eprintln!(
        "{:<24} {:>12} {:>12} {:>14} {:>14}",
        "mode", "baseline", "steady-peak", "workload Δ", "post-shutdown"
    );
    eprintln!("{}", "-".repeat(82));
    for (mode, s) in rows {
        let delta = s.steady_state.saturating_sub(s.baseline);
        eprintln!(
            "{:<24} {:>12} {:>12} {:>14} {:>14}",
            mode.label(),
            fmt_mib(s.baseline),
            fmt_mib(s.steady_state),
            fmt_mib(delta),
            fmt_kib(s.post_shutdown),
        );
    }
    eprintln!();
    eprintln!("config: {WORKLOAD_TASKS} tasks × {WORKLOAD_SECS}s, {WORKER_THREADS} worker threads");
    eprintln!(
        "        budget={} MiB total, {} KiB / segment, rotation={}s",
        TOTAL_BUDGET / (1024 * 1024),
        SEGMENT_SIZE / 1024,
        ROTATION_PERIOD.as_secs(),
    );
    eprintln!();
    eprintln!("note: 'baseline' is residual live bytes at the start of each per-mode");
    eprintln!("      measurement; later modes carry holdovers from earlier runs in this");
    eprintln!("      process. 'workload Δ' = steady-peak − baseline is the apples-to-apples");
    eprintln!("      per-mode allocation cost. For cleanest numbers, run each mode in a");
    eprintln!("      fresh process: pass `--mode disk|mem|disk-cpu-profiling|mem-cpu-profiling`.");
}

fn print_bmf(rows: &[(Mode, Sample)]) {
    let mut top = BTreeMap::new();
    for (mode, s) in rows {
        let mut measures = BTreeMap::new();
        for (name, value) in [
            ("baseline_bytes", s.baseline),
            ("steady_peak_bytes", s.steady_state),
            ("post_shutdown_bytes", s.post_shutdown),
        ] {
            measures.insert(name.to_string(), serde_json::json!({ "value": value }));
        }
        top.insert(format!("pipeline_heap/{}", mode.label()), measures);
    }
    println!("{}", serde_json::to_string_pretty(&top).unwrap());
}

fn parse_mode(s: &str) -> Mode {
    match s {
        "disk" => Mode::Disk,
        "mem" => Mode::Mem,
        "disk-cpu-profiling" | "disk-cpu" => Mode::DiskCpu,
        "mem-cpu-profiling" | "mem-cpu" => Mode::MemCpu,
        other => {
            eprintln!(
                "unknown mode: {other}; expected disk|mem|disk-cpu-profiling|mem-cpu-profiling"
            );
            std::process::exit(2);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bmf = args.iter().any(|a| a == "--bmf");
    let single_mode = args
        .windows(2)
        .find_map(|w| (w[0] == "--mode").then(|| parse_mode(&w[1])));
    let modes: Vec<Mode> = match single_mode {
        Some(m) => vec![m],
        None => vec![Mode::Disk, Mode::Mem, Mode::DiskCpu, Mode::MemCpu],
    };

    let mut rows = Vec::new();
    for mode in modes {
        eprintln!("== measuring {} ==", mode.label());
        let s = measure(mode);
        let delta = s.steady_state.saturating_sub(s.baseline);
        eprintln!(
            "  baseline={}, steady-peak={}, workload Δ={}, post-shutdown live={}",
            fmt_mib(s.baseline),
            fmt_mib(s.steady_state),
            fmt_mib(delta),
            fmt_kib(s.post_shutdown),
        );
        rows.push((mode, s));
    }

    print_table(&rows);
    if bmf {
        print_bmf(&rows);
    }
}

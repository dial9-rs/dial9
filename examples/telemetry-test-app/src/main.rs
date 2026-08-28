use clap::Parser;
#[cfg(target_os = "linux")]
use dial9::RecorderPerfExt;
#[cfg(target_os = "linux")]
use dial9::cpu::CpuProfilingConfig;
use dial9::format::TraceEvent;
use dial9::{Dial9Handle, Dial9HandleTokioExt, DiskBuffer, TaskDumpConfig, TokioAttachOptions};
use dial9_utils::dial9_span;
use dial9_utils::span::{Instrument as _, Span as _};
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const DEFAULT_CYCLES: u64 = 40;
const WARMUP_CYCLES: u64 = 4;
const CPU_QUANTUM: Duration = Duration::from_millis(10);
const WAIT_QUANTUM: Duration = Duration::from_millis(10);
const MAX_TRACE_SIZE: u64 = 100_000_000;

const MIXED_CYCLE: &str = "dial9_fixture_mixed_cycle";
const MIXED_INNER: &str = "dial9_fixture_mixed_inner";
const CPU_OUTER: &str = "dial9_fixture_cpu_outer_weight_1";
const CPU_INNER: &str = "dial9_fixture_cpu_inner_weight_3";
const WAIT_OUTER: &str = "dial9_fixture_wait_outer_weight_1";
const WAIT_INNER: &str = "dial9_fixture_wait_inner_weight_2";
const SPAN_CYCLE: &str = "dial9_fixture_span_cycle";
const SPAN_INNER: &str = "dial9_fixture_span_inner";

#[derive(Debug, Parser)]
#[command(about = "Generate a self-describing dial9 integration-test trace")]
struct Args {
    /// Directory in which trace segments are written.
    #[arg(long)]
    trace_dir: PathBuf,

    /// Number of mixed workload cycles in the measurement window.
    #[arg(long, default_value_t = DEFAULT_CYCLES)]
    cycles: u64,
}

#[derive(TraceEvent)]
struct TelemetryFixtureExpectationEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    feature: String,
    name: String,
    parent: Option<String>,
    active_span: Option<String>,
}

#[derive(TraceEvent)]
struct TelemetryFixtureMarkerEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    phase: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let writer = DiskBuffer::builder()
        .base_path(&args.trace_dir)
        .max_total_size(MAX_TRACE_SIZE)
        .build()?;

    let recorder_builder = dial9::recorder(writer);
    #[cfg(target_os = "linux")]
    let recorder_builder = recorder_builder.with_cpu_profiling(CpuProfilingConfig::default());
    let recorder = recorder_builder.build();

    let mut runtime_builder = tokio::runtime::Builder::new_multi_thread();
    runtime_builder.enable_all().worker_threads(2);
    let runtime = recorder.handle().attach_tokio_runtime(
        runtime_builder,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .task_dump_config(
                TaskDumpConfig::builder()
                    .idle_threshold(Duration::from_millis(1))
                    .rng_seed(1)
                    .build(),
            )
            .build(),
    )?;

    let handle = recorder.handle().clone();
    runtime.block_on(async move {
        dial9::spawn(async move {
            run_cycles(WARMUP_CYCLES).await;
            emit_expectations(&handle);
            emit_marker(&handle, "measurement_start");
            run_cycles(args.cycles).await;
            emit_marker(&handle, "measurement_end");
        })
        .await
        .expect("fixture task should complete");
    });

    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(5));

    println!("Trace written to: {}", args.trace_dir.display());
    Ok(())
}

fn emit_expectations(handle: &Dial9Handle) {
    emit_expectation(
        handle,
        "cpu",
        CPU_OUTER,
        Some(MIXED_CYCLE),
        Some(SPAN_CYCLE),
    );
    emit_expectation(
        handle,
        "task_dump",
        WAIT_OUTER,
        Some(MIXED_CYCLE),
        Some(SPAN_CYCLE),
    );
    emit_expectation(
        handle,
        "cpu",
        CPU_INNER,
        Some(MIXED_INNER),
        Some(SPAN_INNER),
    );
    emit_expectation(
        handle,
        "task_dump",
        WAIT_INNER,
        Some(MIXED_INNER),
        Some(SPAN_INNER),
    );
    emit_expectation(handle, "span", SPAN_CYCLE, None, None);
    emit_expectation(handle, "span", SPAN_INNER, Some(SPAN_CYCLE), None);
}

fn emit_expectation(
    handle: &Dial9Handle,
    feature: &str,
    name: &str,
    parent: Option<&str>,
    active_span: Option<&str>,
) {
    handle.record_event(TelemetryFixtureExpectationEvent {
        timestamp_ns: dial9::core::clock::clock_monotonic_ns(),
        feature: feature.to_owned(),
        name: name.to_owned(),
        parent: parent.map(str::to_owned),
        active_span: active_span.map(str::to_owned),
    });
}

fn emit_marker(handle: &Dial9Handle, phase: &str) {
    handle.record_event(TelemetryFixtureMarkerEvent {
        timestamp_ns: dial9::core::clock::clock_monotonic_ns(),
        phase: phase.to_owned(),
    });
}

async fn run_cycles(cycles: u64) {
    for cycle in 0..cycles {
        dial9_fixture_mixed_cycle(cycle).await;
    }
}

#[inline(never)]
async fn dial9_fixture_mixed_cycle(cycle: u64) {
    let cycle_span = dial9_span!(SPAN_CYCLE, cycle: u64 = cycle);
    let cycle_span_id = cycle_span.id();

    async move {
        dial9_fixture_cpu_outer_weight_1();
        dial9_fixture_wait_outer_weight_1().await;
        dial9_fixture_mixed_inner(cycle_span_id).await;
    }
    .instrument(cycle_span)
    .await;
}

#[inline(never)]
async fn dial9_fixture_mixed_inner(parent_span_id: dial9_utils::span::SpanId) {
    let inner_span = dial9_span!(SPAN_INNER).with_parent_id(parent_span_id);
    async {
        dial9_fixture_cpu_inner_weight_3();
        dial9_fixture_wait_inner_weight_2().await;
    }
    .instrument(inner_span)
    .await;
}

#[inline(never)]
fn dial9_fixture_cpu_outer_weight_1() {
    busy_for(CPU_QUANTUM);
    black_box(1_u64);
}

#[inline(never)]
fn dial9_fixture_cpu_inner_weight_3() {
    busy_for(CPU_QUANTUM * 3);
    black_box(3_u64);
}

#[inline(never)]
async fn dial9_fixture_wait_outer_weight_1() {
    tokio::time::sleep(WAIT_QUANTUM).await;
    // Give task-dump capture a distinct poll boundary before the inner wait.
    tokio::task::yield_now().await;
    black_box(1_u64);
}

#[inline(never)]
async fn dial9_fixture_wait_inner_weight_2() {
    tokio::time::sleep(WAIT_QUANTUM * 2).await;
    black_box(2_u64);
}

#[inline(never)]
fn busy_for(duration: Duration) {
    let start = Instant::now();
    let mut value = 0_u64;
    while start.elapsed() < duration {
        for i in 0..1_000 {
            value = value.wrapping_add(black_box(i));
        }
        black_box(value);
    }
}

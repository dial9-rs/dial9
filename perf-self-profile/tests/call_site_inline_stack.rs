//! Real CPU samples must retain the inline caller, rather than the destructor
//! that executes after the sampled call returns.
//! Run with `--release` to reproduce the bogus destructor stack without the fix;
//! unoptimized code can keep the return address inside the inline caller.

#![cfg(all(target_os = "linux", feature = "cpu-profiling"))]

use dial9_core::buffer::MemoryBuffer;
use dial9_core::recorder::recorder;
use dial9_core::test_util::drain_encoded_batches;
use dial9_perf_self_profile::offline_symbolize::symbolize_trace_with_maps;
use dial9_perf_self_profile::{CpuProfiler, CpuProfilingConfig, read_proc_maps};
use dial9_trace_format::decoder::Decoder;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// The ctimer backend owns process-wide signal handlers and sample buffers.
static SAMPLER: Mutex<()> = Mutex::new(());
static DROPS: AtomicU64 = AtomicU64::new(0);

struct AfterReturn;

impl Drop for AfterReturn {
    #[inline(always)]
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline(never)]
fn sampled_work(deadline: Instant) {
    let mut sum = 0u64;
    while Instant::now() < deadline {
        for i in 0..100_000 {
            sum = std::hint::black_box(sum.wrapping_add(i));
        }
    }
}

#[inline(always)]
fn inline_caller(deadline: Instant) {
    sampled_work(deadline);
}

#[inline(never)]
fn caller_then_drop(deadline: Instant) {
    let after_return = AfterReturn;
    inline_caller(deadline);
    // The unadjusted return address can resolve to this inlined destructor.
    drop(after_return);
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum DecodedEvent {
    CpuSampleEvent {
        callchain: Vec<u64>,
    },
    SymbolTableEntry(Symbol),
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct Symbol {
    addr: u64,
    symbol_name: String,
    inline_depth: u64,
}

fn check_inline_stack(config: CpuProfilingConfig, backend: &str) {
    let _lock = SAMPLER.lock().expect("sampler lock");
    let profiler = match CpuProfiler::start(config.frequency_hz(999)) {
        Ok(profiler) => profiler,
        Err(error)
            if backend == "perf"
                && matches!(
                    error.raw_os_error(),
                    Some(libc::EACCES | libc::EPERM | libc::ENOSYS | libc::EOPNOTSUPP)
                )
                && std::env::var_os("DIAL9_REQUIRE_PERF").is_none() =>
        {
            eprintln!("skipping perf: {error}; set DIAL9_REQUIRE_PERF=1 to require it");
            return;
        }
        Err(error) => panic!("failed to start {backend} profiler: {error}"),
    };
    let rec = recorder(MemoryBuffer::new(16 << 20).expect("memory buffer"))
        .source(profiler)
        .paused()
        .build();
    let shared = rec.shared().expect("live recorder").clone();

    caller_then_drop(Instant::now() + Duration::from_millis(300));
    shared.flush_sources();
    let batches = drain_encoded_batches(&shared);
    rec.graceful_shutdown(Duration::ZERO);

    let maps = read_proc_maps();
    let mut checked = 0;
    for (batch, mut trace) in batches.into_iter().enumerate() {
        let mut symbols = Vec::new();
        symbolize_trace_with_maps(&trace, &maps, &mut symbols).expect("offline symbolization");
        trace.extend_from_slice(&symbols);

        if let Some(dir) = std::env::var_os("DIAL9_CALL_SITE_TRACE_DIR") {
            std::fs::create_dir_all(&dir).expect("create trace directory");
            std::fs::write(
                std::path::Path::new(&dir).join(format!("{backend}-{batch}.bin")),
                &trace,
            )
            .expect("write integration trace");
        }

        let mut samples = Vec::new();
        let mut by_addr: BTreeMap<u64, Vec<Symbol>> = BTreeMap::new();
        Decoder::new(&trace)
            .expect("trace header")
            .for_each_event(|raw| match raw.deserialize().expect("decode event") {
                DecodedEvent::CpuSampleEvent { callchain } => samples.push(callchain),
                DecodedEvent::SymbolTableEntry(symbol) => {
                    by_addr.entry(symbol.addr).or_default().push(symbol);
                }
                DecodedEvent::Other => {}
            })
            .expect("decode symbolized trace");
        for symbols in by_addr.values_mut() {
            symbols.sort_by_key(|symbol| symbol.inline_depth);
        }

        for chain in samples {
            let Some(work) = chain.iter().position(|addr| {
                by_addr.get(addr).is_some_and(|symbols| {
                    symbols
                        .iter()
                        .any(|symbol| symbol.symbol_name.ends_with("::sampled_work"))
                })
            }) else {
                continue;
            };
            let caller_addr = chain
                .get(work + 1)
                .expect("sampled_work must have a caller frame");
            let caller = by_addr.get(caller_addr).expect("caller must symbolize");
            let names: Vec<_> = caller
                .iter()
                .map(|symbol| symbol.symbol_name.as_str())
                .collect();
            assert!(
                names
                    .first()
                    .is_some_and(|name| name.ends_with("::caller_then_drop")),
                "{backend}: wrong physical caller: {names:?}"
            );
            assert!(
                names.iter().any(|name| name.ends_with("::inline_caller")),
                "{backend}: lost the actual inline caller, got {names:?}"
            );
            assert!(
                names
                    .iter()
                    .all(|name| !name.contains("AfterReturn") && !name.contains("::drop")),
                "{backend}: destructor after return appeared on the sampled stack: {names:?}"
            );
            if checked == 0 {
                eprintln!("{backend}: caller stack {names:?}");
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "{backend}: no samples inside sampled_work");
    eprintln!("{backend}: checked {checked} sampled inline caller stacks");
}

#[test]
fn perf_inline_stack_points_to_call_site() {
    check_inline_stack(CpuProfilingConfig::with_perf_backend(), "perf");
}

#[test]
fn ctimer_inline_stack_points_to_call_site() {
    check_inline_stack(CpuProfilingConfig::with_ctimer_backend(), "ctimer");
}

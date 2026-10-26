//! Identical workloads linked with two different process-global allocators.

use dial9_trace_format::decoder::Decoder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::hint::black_box;
use std::io::Read;
use std::time::Instant;

#[derive(Deserialize, Serialize)]
struct Event {
    id: u64,
    route: String,
    tags: Vec<String>,
    payload: String,
    values: Vec<u64>,
}

fn event_inputs() -> Vec<Vec<u8>> {
    (0..1024)
        .map(|id| {
            serde_json::to_vec(&Event {
                id,
                route: format!("/api/route/{}", id % 32),
                tags: vec![format!("tenant-{}", id % 64), "ingestion".into()],
                payload: "x".repeat(32 + (id as usize % 8) * 64),
                values: (0..(id % 16 + 1)).collect(),
            })
            .unwrap()
        })
        .collect()
}

// Parse owned events, move batches across a bounded channel, aggregate, and
// serialize the result. Runtime/input construction stays outside the timer.
async fn event_pipeline(inputs: &[Vec<u8>], count: usize) -> u64 {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<Event>>(8);
    let consumer = tokio::spawn(async move {
        let mut totals = BTreeMap::<String, u64>::new();
        let mut processed = 0usize;
        while let Some(batch) = rx.recv().await {
            for event in batch {
                let value = event.id
                    + event.payload.len() as u64
                    + event.tags.iter().map(|s| s.len() as u64).sum::<u64>()
                    + event.values.iter().sum::<u64>();
                *totals.entry(event.route).or_default() += value;
                processed += 1;
            }
        }
        assert_eq!(processed, count);
        let checksum = totals.values().sum();
        black_box(serde_json::to_vec(&totals).unwrap());
        checksum
    });
    let mut batch = Vec::new();
    for i in 0..count {
        batch.push(serde_json::from_slice(&inputs[i % inputs.len()]).unwrap());
        if batch.len() == 256 {
            tx.send(std::mem::take(&mut batch)).await.unwrap();
        }
    }
    if !batch.is_empty() {
        tx.send(batch).await.unwrap();
    }
    drop(tx);
    consumer.await.unwrap()
}

fn decode_trace(data: &[u8]) -> u64 {
    let mut decoder = Decoder::new(data).unwrap();
    let mut count = 0;
    decoder
        .for_each_event(|event| {
            // Exercise the production serde decoder's owned strings, vectors,
            // and maps, rather than only scanning frame headers.
            black_box(event.deserialize::<serde_json::Value>().unwrap());
            count += 1;
        })
        .unwrap();
    count
}

fn load_trace(path: &str) -> Vec<u8> {
    let data = std::fs::read(path).unwrap();
    if data.starts_with(&[0x1f, 0x8b]) {
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(data.as_slice())
            .read_to_end(&mut decoded)
            .unwrap();
        decoded
    } else {
        data
    }
}

/// Retained symbol for comparing the real global deallocation path with objdump.
///
/// # Safety
/// `ptr` must be allocated by the global allocator with the supplied layout.
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn allocator_probe_dealloc(ptr: *mut u8, size: usize, align: usize) {
    // SAFETY: forwarded from the caller's contract.
    unsafe {
        std::alloc::dealloc(
            ptr,
            std::alloc::Layout::from_size_align_unchecked(size, align),
        );
    }
}

fn install_profiler(config: &str) {
    use dial9_tokio_telemetry::memory_profiling::{MemoryProfiler, MemoryProfilingConfig};
    use dial9_tokio_telemetry::telemetry::{MemoryBuffer, recorder};

    let recorder = recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap()).build();
    let cfg = MemoryProfilingConfig::builder()
        .sample_rate_bytes(512 * 1024)
        .track_liveset(config == "sampling_with_liveset")
        .rng_seed(42)
        .build();
    let _guard = MemoryProfiler::from_config(cfg)
        .install(recorder.handle().clone())
        .expect("profiler install");
    // The profiler is process-permanent; keep its recorder alive as well.
    std::mem::forget(recorder);
}

pub fn run(allocator: &str, wrapped: bool) {
    let mut workload = "events".to_string();
    let mut events = 300_000usize;
    let mut iterations = 3usize;
    let mut trace = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../dial9-viewer/ui/public/demo-trace.bin"
    )
    .to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            // Cargo passes this even though the harness is not Criterion.
            "--bench" => {}
            "--workload" => workload = args.next().expect("--workload value"),
            "--events" => events = args.next().expect("--events value").parse().unwrap(),
            "--iterations" => {
                iterations = args.next().expect("--iterations value").parse().unwrap()
            }
            "--trace" => trace = args.next().expect("--trace value"),
            _ => panic!("unknown argument: {arg}"),
        }
    }
    assert!(events > 0 && iterations > 0);
    let config = std::env::var("BENCH_CONFIG").unwrap_or_else(|_| "none".into());
    match config.as_str() {
        "none" => {}
        "sampling_only" | "sampling_with_liveset" => {
            assert!(wrapped, "plain jemalloc cannot record allocation samples");
            install_profiler(&config);
        }
        _ => panic!("unknown BENCH_CONFIG: {config}"),
    }

    let layout = std::alloc::Layout::from_size_align(64, 8).unwrap();
    // SAFETY: valid layout and a matching deallocation of the returned pointer.
    unsafe {
        let ptr = std::alloc::alloc(layout);
        assert!(!ptr.is_null());
        allocator_probe_dealloc(ptr, layout.size(), layout.align());
    }

    let mut times_ns = Vec::with_capacity(iterations);
    let checksum;
    match workload.as_str() {
        "events" => {
            let inputs = event_inputs();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            checksum = runtime.block_on(event_pipeline(&inputs, events));
            for _ in 0..iterations {
                let start = Instant::now();
                let result = runtime.block_on(event_pipeline(&inputs, events));
                times_ns.push(start.elapsed().as_nanos());
                assert_eq!(result, checksum);
                black_box(result);
            }
        }
        "trace" => {
            let data = load_trace(&trace);
            checksum = decode_trace(&data);
            assert!(checksum > 0);
            for _ in 0..iterations {
                let start = Instant::now();
                let result = decode_trace(&data);
                times_ns.push(start.elapsed().as_nanos());
                assert_eq!(result, checksum);
                black_box(result);
            }
        }
        _ => panic!("unknown workload: {workload}"),
    }
    println!(
        "{}",
        serde_json::json!({
            "allocator": allocator,
            "config": config,
            "workload": workload,
            "events": if workload == "events" { events as u64 } else { checksum },
            "checksum": checksum,
            "times_ns": times_ns,
        })
    );
}

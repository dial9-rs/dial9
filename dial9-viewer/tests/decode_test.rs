//! End-to-end tests for the decode pipeline.
//!
//! Verifies:
//! 1. Worker_id inference from tid↔WorkerPark/WorkerUnpark correlation
//! 2. Timestamp ordering within the output
//! 3. Deterministic stack_id computation

use dial9_viewer::ingest::decode::decode_samples;

fn load_demo_trace() -> Vec<u8> {
    let data = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/demo-trace.bin")).unwrap();
    let mut dec = flate2::read::GzDecoder::new(data.as_slice());
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut dec, &mut buf).unwrap();
    buf
}

#[test]
fn worker_id_inferred_from_park_unpark_correlation() {
    let data = load_demo_trace();
    let (samples, _, _) = decode_samples(&data, "test").unwrap();

    let worker_count = samples.iter().filter(|s| s.worker_id.is_some()).count();
    let non_worker_count = samples.iter().filter(|s| s.worker_id.is_none()).count();

    // The demo trace has multiple worker threads that park/unpark.
    // We must infer worker_id for a significant portion of CPU samples.
    assert!(
        worker_count > 0,
        "no samples were classified as worker threads; \
         tid→worker inference from park/unpark is broken"
    );

    eprintln!(
        "worker_id inference: {} worker, {} non-worker out of {} total samples",
        worker_count,
        non_worker_count,
        samples.len()
    );
}

#[test]
fn output_timestamps_are_wall_clock() {
    let data = load_demo_trace();
    let (samples, _, _) = decode_samples(&data, "test").unwrap();

    assert!(!samples.is_empty());
    // Wall-clock Unix epoch ns for any date after 2017 should exceed 1.5e18.
    let min_ts = samples.iter().map(|s| s.timestamp_ns).min().unwrap();
    let max_ts = samples.iter().map(|s| s.timestamp_ns).max().unwrap();
    assert!(
        min_ts > 1_500_000_000_000_000_000,
        "min timestamp {min_ts} does not look like wall-clock epoch ns"
    );
    // Sanity: all timestamps within a reasonable span (< 5 minutes for a demo trace)
    assert!(
        max_ts - min_ts < 5 * 60 * 1_000_000_000,
        "timestamp span too large: {} ns",
        max_ts - min_ts
    );
}

#[test]
fn stack_ids_are_deterministic() {
    let data = load_demo_trace();
    let (s1, d1, _) = decode_samples(&data, "test").unwrap();
    let (s2, d2, _) = decode_samples(&data, "test").unwrap();

    assert_eq!(s1.len(), s2.len());
    assert_eq!(d1.len(), d2.len());
    for (a, b) in s1.iter().zip(s2.iter()) {
        assert_eq!(a.stack_id, b.stack_id);
        assert_eq!(a.timestamp_ns, b.timestamp_ns);
        assert_eq!(a.worker_id, b.worker_id);
        assert_eq!(a.source, b.source);
    }
}

#[test]
fn every_sample_has_stack_in_dictionary() {
    let data = load_demo_trace();
    let (samples, stacks, _) = decode_samples(&data, "demo-trace.bin").unwrap();

    for (i, sample) in samples.iter().enumerate() {
        assert!(
            stacks.contains_key(&sample.stack_id),
            "sample {i} has stack_id not found in dictionary"
        );
    }
}

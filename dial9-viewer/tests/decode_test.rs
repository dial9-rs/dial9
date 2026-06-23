//! Integration coverage for the decode pipeline's public API.
//!
//! Most decode behavior (worker_id inference, deterministic stack_ids, stack
//! dictionary integrity, wall-clock conversion) is covered by the unit tests in
//! `src/ingest/decode.rs`. This file keeps the one assertion that is *not*
//! covered there — that the wall-clock timestamp span of a single demo trace is
//! bounded — and doubles as a smoke test that `decode_samples` stays reachable
//! through the crate's public API.

use dial9_viewer::ingest::decode::decode_samples;

fn load_demo_trace() -> Vec<u8> {
    let data = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/demo-trace.bin")).unwrap();
    let mut dec = flate2::read::GzDecoder::new(data.as_slice());
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut dec, &mut buf).unwrap();
    buf
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

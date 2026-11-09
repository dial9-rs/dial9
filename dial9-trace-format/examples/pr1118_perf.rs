//! A/B harness for typed schema registration and steady-state encoding cost.
//!
//! Scenarios:
//! - `one_type_registration`: one non-`wire_slot` type registered per encoder reset.
//! - `two_unique_registrations`: two uniquely named non-`wire_slot` types per reset.
//! - `duplicate_name_same_schema`: two distinct Rust types sharing an event name
//!   and an identical schema, registered per reset.
//! - `steady_state`: non-`wire_slot` encoding after the initial registration.
//! - `steady_state_fast_slot`: `wire_slot` encoding, as a control.
//!
//! Reset scenarios alternate two reused `Vec` buffers through `reset_to`, so
//! fresh allocation is excluded from the measurement.
//!
//! To compare revisions, compile this exact source at each revision and run the
//! release binaries pinned to one CPU, alternating their order across runs
//! (for example `taskset -c 2 ./a; taskset -c 2 ./b; taskset -c 2 ./b; taskset -c 2 ./a`).
//!
//! Usage: cargo run --release -p dial9-trace-format --example pr1118_perf

use dial9_trace_format::TraceEvent;
use dial9_trace_format::encoder::Encoder;
use std::hint::black_box;
use std::time::{Duration, Instant};

const RESET_BATCHES: u64 = 200_000;
const STEADY_EVENTS: u64 = 20_000_000;

#[derive(TraceEvent)]
#[traceevent(name = "SameName")]
struct First {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    value: u64,
}

#[derive(TraceEvent)]
#[traceevent(name = "SameName")]
struct Second {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    value: u64,
}

#[derive(TraceEvent)]
#[traceevent(name = "UniqueName")]
struct Unique {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    value: u64,
}

#[derive(TraceEvent)]
#[traceevent(wire_slot)]
struct Fast {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    value: u64,
}

fn report(name: &str, ops: u64, elapsed: Duration, bytes: u64) {
    let ns_per_op = elapsed.as_nanos() as f64 / ops as f64;
    println!(
        "{name}: {ns_per_op:.2} ns/op, {:.3} s, bytes={bytes}",
        elapsed.as_secs_f64()
    );
}

/// Runs `body` once per encoder reset, alternating two reused buffers.
fn run_resets(name: &str, mut body: impl FnMut(&mut Encoder<Vec<u8>>, u64)) {
    let mut enc = Encoder::new();
    let mut spare = Vec::with_capacity(4096);
    let mut bytes = 0u64;
    let start = Instant::now();
    for i in 0..RESET_BATCHES {
        let mut old = enc.reset_to_infallible(black_box(spare));
        bytes += old.len() as u64;
        old.clear();
        spare = old;
        body(&mut enc, i);
    }
    let elapsed = start.elapsed();
    bytes += enc.finish().len() as u64;
    black_box(spare);
    report(name, RESET_BATCHES, elapsed, bytes);
}

/// Encodes `STEADY_EVENTS` events after one warm-up write registers the schema.
fn run_steady<T: TraceEvent>(name: &str, make: impl Fn(u64) -> T) {
    let mut enc = Encoder::new();
    enc.write_infallible(&make(0));
    let start = Instant::now();
    for i in 1..=STEADY_EVENTS {
        enc.write_infallible(black_box(&make(i)));
    }
    let elapsed = start.elapsed();
    let bytes = black_box(enc.finish()).len() as u64;
    report(name, STEADY_EVENTS, elapsed, bytes);
}

fn main() {
    run_resets("one_type_registration", |enc, i| {
        enc.write_infallible(black_box(&First {
            timestamp_ns: i,
            value: i,
        }));
    });
    run_resets("two_unique_registrations", |enc, i| {
        enc.write_infallible(black_box(&First {
            timestamp_ns: i,
            value: i,
        }));
        enc.write_infallible(black_box(&Unique {
            timestamp_ns: i,
            value: i,
        }));
    });
    run_resets("duplicate_name_same_schema", |enc, i| {
        enc.write_infallible(black_box(&First {
            timestamp_ns: i,
            value: i,
        }));
        enc.write_infallible(black_box(&Second {
            timestamp_ns: i,
            value: i,
        }));
    });
    run_steady("steady_state", |i| First {
        timestamp_ns: i,
        value: i,
    });
    run_steady("steady_state_fast_slot", |i| Fast {
        timestamp_ns: i,
        value: i,
    });
}

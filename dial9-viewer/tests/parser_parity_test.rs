//! Parser parity framework: run the SAME trace bytes through the Rust decoder
//! (`decode_samples`) and the JS reference parser (`trace_parser.js`), extract a
//! fixed set of language-neutral *trace properties* from each, and diff them.
//!
//! ## Why this exists
//!
//! `decode.rs` is a Rust port of the CPU-event decode logic that the JS viewer
//! (`trace_parser.js` + `trace_analysis.js` + `flamegraph.js`) has long
//! implemented. The port produced "clearly wrong" output, and ad-hoc spot
//! checks weren't catching it. This test makes the two decoders comparable on a
//! stable contract so any divergence is a hard failure with a readable diff —
//! and so the contract survives the *next* field we add (worker set, sched
//! series, CPU id, …), which is the actual point: a reliable parity harness.
//!
//! ## The properties (see `ui/trace_properties.js` for the canonical defn)
//!
//! Universe = samples with a non-empty callchain (both decoders drop empties).
//!
//! - `total_samples` — must match exactly.
//! - `by_source` — CpuProfile (0) vs SchedEvent (1) counts. These are different
//!   KINDS of samples; the on-CPU flamegraph shows only source 0.
//! - `cpu_profile.count` — on-CPU sample count; must match exactly.
//! - `cpu_profile.distinct_stacks` — distinct symbolized stacks (source 0).
//! - `cpu_profile.stack_sig_digest` — order-independent FNV-1a of the
//!   (stack-signature -> count) multiset.
//! - `cpu_profile.ts_delta_digest` — FNV-1a of sorted (ts - min) deltas;
//!   offset-invariant so monotonic (JS) and wall-clock (Rust) timestamps digest
//!   equal.
//!
//! Also asserted, now that `ResolvedSample` carries `Option<worker_id>`:
//!
//!   * `worker_set` — the set of real workers (the `Some` values). `None` is
//!     off-runtime; there is no in-band sentinel.
//!   * `on_off_by_source` — the on/off-runtime split per source, where on =
//!     `worker_id.is_some()`.
//!
//! NOTE (block-in-place): the JS reference rewrites samples inside a
//! block_in_place tid handoff to off-runtime; `decode.rs` does not do that gap
//! detection yet (rare in practice — see its TODO). The demo trace has no such
//! gaps, so parity holds; a trace that exercised them could diverge on the
//! on/off split until that TODO is closed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use dial9_viewer::ingest::decode::{ResolvedSample, decode_samples};

// ── Source codes (wire values), mirror `CpuSampleSource`. ───────────────────
const SOURCE_CPU_PROFILE: u8 = 0;
const SOURCE_SCHED_EVENT: u8 = 1;

/// Frame separator for stack signatures. MUST be NUL and MUST equal the JS
/// oracle's `FRAME_SEP` (`trace_properties.js`): symbol names contain spaces
/// (e.g. "<T as Trait>::method"), so a space would collide distinct stacks.
/// This is also the byte `decode.rs` hashes between frames (b"\x00").
const FRAME_SEP: &str = "\u{0}";

// ── FNV-1a 64-bit, matches trace_properties.js exactly. ─────────────────────
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn fnv1a_update(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}
fn fnv1a_hex(h: u64) -> String {
    format!("{h:016x}")
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = dial9-viewer; repo root is its parent.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn demo_trace_compressed() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui/demo-trace.bin")
}

fn load_demo_trace() -> Vec<u8> {
    let data = std::fs::read(demo_trace_compressed()).unwrap();
    let mut dec = flate2::read::GzDecoder::new(data.as_slice());
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut dec, &mut buf).unwrap();
    buf
}

/// The trace properties computed from the Rust *output* (`Vec<ResolvedSample>` +
/// stacks dict). Field names mirror trace_properties.js. Now that
/// `ResolvedSample` carries `Option<worker_id>`, the worker SET and the
/// per-source on/off split are recoverable and asserted at full parity.
#[derive(Debug)]
struct RustProperties {
    total_samples: usize,
    by_source: HashMap<u8, usize>,
    /// On/off-runtime split per source. on = `worker_id.is_some()`.
    on_off_by_source: HashMap<u8, (usize, usize)>, // source -> (on, off)
    /// The set of real workers observed (the `Some` values), sorted.
    worker_set: Vec<u32>,
    cpu_profile_count: usize,
    cpu_profile_distinct_stacks: usize,
    cpu_profile_stack_sig_digest: String,
    cpu_profile_ts_delta_digest: String,
}

fn rust_properties(
    samples: &[ResolvedSample],
    dict: &HashMap<[u8; 16], Vec<String>>,
) -> RustProperties {
    let mut by_source: HashMap<u8, usize> = HashMap::new();
    let mut on_off_by_source: HashMap<u8, (usize, usize)> = HashMap::new();
    let mut worker_set: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    // (stack signature -> count) over CpuProfile samples.
    let mut sig_counts: HashMap<String, usize> = HashMap::new();
    let mut ts_values: Vec<u64> = Vec::new();
    let mut min_ts: Option<u64> = None;

    for s in samples {
        *by_source.entry(s.source).or_default() += 1;
        let entry = on_off_by_source.entry(s.source).or_default();
        match s.worker_id {
            Some(w) => {
                entry.0 += 1;
                worker_set.insert(w);
            }
            None => entry.1 += 1,
        }
        if s.source == SOURCE_CPU_PROFILE {
            min_ts = Some(min_ts.map_or(s.timestamp_ns, |m| m.min(s.timestamp_ns)));
        }
    }

    for s in samples {
        if s.source != SOURCE_CPU_PROFILE {
            continue;
        }
        // Stack signature: the dict frame names joined by FRAME_SEP, exactly the
        // serialization trace_properties.js uses (symbolizeChain joined by NUL).
        let sig = dict
            .get(&s.stack_id)
            .map(|frames| frames.join(FRAME_SEP))
            .unwrap_or_default();
        *sig_counts.entry(sig).or_default() += 1;
        ts_values.push(s.timestamp_ns - min_ts.unwrap());
    }

    // Order-independent digest of the (signature -> count) multiset.
    let mut sigs: Vec<&String> = sig_counts.keys().collect();
    sigs.sort();
    let mut sig_hash = FNV_OFFSET;
    for sig in sigs {
        sig_hash = fnv1a_update(sig_hash, sig.as_bytes());
        sig_hash = fnv1a_update(sig_hash, format!("{}\n", sig_counts[sig]).as_bytes());
    }

    ts_values.sort_unstable();
    let mut ts_hash = FNV_OFFSET;
    for d in &ts_values {
        ts_hash = fnv1a_update(ts_hash, format!("{d}\n").as_bytes());
    }

    RustProperties {
        total_samples: samples.len(),
        by_source,
        on_off_by_source,
        worker_set: worker_set.into_iter().collect(),
        cpu_profile_count: ts_values.len(),
        cpu_profile_distinct_stacks: sig_counts.len(),
        cpu_profile_stack_sig_digest: fnv1a_hex(sig_hash),
        cpu_profile_ts_delta_digest: fnv1a_hex(ts_hash),
    }
}

/// Run the JS oracle on the same trace file. Returns the parsed JSON, or None if
/// node is unavailable (so the test still runs offline against the golden file).
fn js_properties(trace_path: &std::path::Path) -> Option<serde_json::Value> {
    let script = repo_root().join("dial9-viewer/ui/trace_properties.js");
    let out = Command::new("node")
        .arg(&script)
        .arg(trace_path)
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!(
            "node oracle failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// The committed golden snapshot (so the contract is pinned even without node).
fn golden_properties() -> serde_json::Value {
    let p =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/demo-trace.properties.json");
    let bytes = std::fs::read(p).expect("golden properties fixture missing");
    serde_json::from_slice(&bytes).expect("golden properties fixture is not valid JSON")
}

fn report(rust: &RustProperties, js: &serde_json::Value) {
    eprintln!("\n──────────────────────────── PARITY REPORT ────────────────────────────");
    eprintln!("property                         rust                 js (reference)");
    eprintln!("────────────────────────────────────────────────────────────────────");
    let js_total = js["total_samples"].as_u64().unwrap_or(0);
    eprintln!(
        "total_samples                    {:<20} {}",
        rust.total_samples, js_total
    );
    for src in [SOURCE_CPU_PROFILE, SOURCE_SCHED_EVENT] {
        let r = rust.by_source.get(&src).copied().unwrap_or(0);
        let j = js["by_source"][src.to_string()].as_u64().unwrap_or(0);
        let name = if src == SOURCE_CPU_PROFILE {
            "CpuProfile"
        } else {
            "SchedEvent"
        };
        eprintln!("by_source[{src}] ({name:<10})       {r:<20} {j}");
    }
    eprintln!(
        "cpu_profile.count                {:<20} {}",
        rust.cpu_profile_count,
        js["cpu_profile"]["count"].as_u64().unwrap_or(0)
    );
    eprintln!(
        "cpu_profile.distinct_stacks      {:<20} {}",
        rust.cpu_profile_distinct_stacks,
        js["cpu_profile"]["distinct_stacks"].as_u64().unwrap_or(0)
    );
    eprintln!(
        "cpu_profile.stack_sig_digest     {:<20} {}",
        rust.cpu_profile_stack_sig_digest,
        js["cpu_profile"]["stack_sig_digest"].as_str().unwrap_or("")
    );
    eprintln!(
        "cpu_profile.ts_delta_digest      {:<20} {}",
        rust.cpu_profile_ts_delta_digest,
        js["cpu_profile"]["ts_delta_digest"].as_str().unwrap_or("")
    );
    eprintln!("────────────────────────────────────────────────────────────────────");
    for src in [SOURCE_CPU_PROFILE, SOURCE_SCHED_EVENT] {
        let (on, off) = rust.on_off_by_source.get(&src).copied().unwrap_or((0, 0));
        let j = &js["on_off_by_source"][src.to_string()];
        eprintln!(
            "on/off source={src}                rust on={on} off={off}      js on={} off={}",
            j["on"].as_u64().unwrap_or(0),
            j["off"].as_u64().unwrap_or(0),
        );
    }
    eprintln!(
        "worker_set                       {:?}      js {}",
        rust.worker_set, js["worker_set"]
    );
    eprintln!("────────────────────────────────────────────────────────────────────\n");
}

/// Read + gunzip an arbitrary trace file (`.bin` or `.bin.gz`).
fn load_trace_file(path: &std::path::Path) -> Vec<u8> {
    let raw = std::fs::read(path).unwrap();
    if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        let mut dec = flate2::read::GzDecoder::new(raw.as_slice());
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut buf).unwrap();
        buf
    } else {
        raw
    }
}

/// Ad-hoc parity check against ANY trace file, pointed at via the
/// `DIAL9_PARITY_TRACE` env var. Compares the Rust decoder to the live JS oracle
/// on the same bytes and asserts full parity. Use this to reproduce a real-trace
/// divergence the demo trace can't expose:
///
///   DIAL9_PARITY_TRACE=/path/to/trace.bin.gz \
///     cargo test -p dial9-viewer --test parser_parity_test external -- --nocapture
///
/// The `source_key` passed to the decoder is the file's own path; the decoder
/// only parses date/service/host out of it for partition columns, which the
/// properties here don't compare.
#[test]
fn external_trace_parity() {
    let Ok(path) = std::env::var("DIAL9_PARITY_TRACE") else {
        eprintln!("DIAL9_PARITY_TRACE not set — skipping external trace parity");
        return;
    };
    let path = std::path::PathBuf::from(path);
    let data = load_trace_file(&path);
    let key = path.to_string_lossy().to_string();
    let (samples, dict, _) = decode_samples(&data, &key).unwrap();
    let rust = rust_properties(&samples, &dict);

    let js = js_properties(&path).expect("node JS oracle must be available for external parity");
    report(&rust, &js);

    let js_total = js["total_samples"].as_u64().unwrap() as usize;
    assert_eq!(rust.total_samples, js_total, "total_samples diverged");
    for src in [SOURCE_CPU_PROFILE, SOURCE_SCHED_EVENT] {
        let r = rust.by_source.get(&src).copied().unwrap_or(0);
        let j = js["by_source"][src.to_string()].as_u64().unwrap_or(0) as usize;
        assert_eq!(r, j, "by_source[{src}] diverged");
    }
    let js_cpu = &js["cpu_profile"];
    assert_eq!(
        rust.cpu_profile_stack_sig_digest,
        js_cpu["stack_sig_digest"].as_str().unwrap(),
        "stack-signature multiset diverged"
    );
    assert_eq!(
        rust.cpu_profile_ts_delta_digest,
        js_cpu["ts_delta_digest"].as_str().unwrap(),
        "timestamp series diverged"
    );
    let js_worker_set: Vec<u32> = js["worker_set"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    assert_eq!(rust.worker_set, js_worker_set, "worker_set diverged");
    for src in [SOURCE_CPU_PROFILE, SOURCE_SCHED_EVENT] {
        let (on, off) = rust.on_off_by_source.get(&src).copied().unwrap_or((0, 0));
        let j = &js["on_off_by_source"][src.to_string()];
        assert_eq!(on as u64, j["on"].as_u64().unwrap_or(0), "on src={src}");
        assert_eq!(off as u64, j["off"].as_u64().unwrap_or(0), "off src={src}");
    }
}

/// The core test: decode with Rust, extract properties, and diff against both
/// the golden snapshot and (when available) a live node run.
#[test]
fn rust_decode_matches_js_reference_properties() {
    let data = load_demo_trace();
    let (samples, dict, _) = decode_samples(&data, "demo-trace.bin").unwrap();
    let rust = rust_properties(&samples, &dict);

    let golden = golden_properties();
    // Prefer a live node run; fall back to the committed golden when offline.
    let js = js_properties(&demo_trace_compressed()).unwrap_or_else(|| {
        eprintln!("note: node unavailable — comparing against committed golden snapshot");
        golden.clone()
    });

    // The golden snapshot must itself agree with the live oracle (guards against
    // a stale fixture silently masking a JS-side change).
    assert_eq!(
        js["total_samples"], golden["total_samples"],
        "golden fixture is stale vs live JS oracle — regenerate it:\n  \
         node dial9-viewer/ui/trace_properties.js dial9-viewer/ui/demo-trace.bin \
         > dial9-viewer/tests/fixtures/demo-trace.properties.json"
    );

    report(&rust, &js);

    // ── MUST-MATCH invariants ──────────────────────────────────────────────
    let js_total = js["total_samples"].as_u64().unwrap() as usize;
    assert_eq!(
        rust.total_samples, js_total,
        "total sample count diverged (universe = non-empty callchain)"
    );

    for src in [SOURCE_CPU_PROFILE, SOURCE_SCHED_EVENT] {
        let r = rust.by_source.get(&src).copied().unwrap_or(0);
        let j = js["by_source"][src.to_string()].as_u64().unwrap_or(0) as usize;
        assert_eq!(
            r, j,
            "by_source[{src}] diverged — source conflation. CpuProfile (0) and \
             SchedEvent (1) are different sample kinds; the on-CPU flamegraph \
             shows only source 0."
        );
    }

    let js_cpu = &js["cpu_profile"];
    assert_eq!(
        rust.cpu_profile_count,
        js_cpu["count"].as_u64().unwrap() as usize,
        "CpuProfile sample count diverged"
    );
    assert_eq!(
        rust.cpu_profile_distinct_stacks,
        js_cpu["distinct_stacks"].as_u64().unwrap() as usize,
        "distinct CpuProfile stacks diverged — symbolization mismatch"
    );
    assert_eq!(
        rust.cpu_profile_stack_sig_digest,
        js_cpu["stack_sig_digest"].as_str().unwrap(),
        "CpuProfile stack-signature multiset diverged — the symbolized stacks \
         (and/or their counts) differ between decoders"
    );
    assert_eq!(
        rust.cpu_profile_ts_delta_digest,
        js_cpu["ts_delta_digest"].as_str().unwrap(),
        "CpuProfile timestamp series diverged (offset-invariant comparison, so \
         this is a real ordering/selection difference, not a clock offset)"
    );

    // ── Worker attribution (now that ResolvedSample carries Option<worker_id>) ─
    // The set of real workers must match exactly.
    let js_worker_set: Vec<u32> = js["worker_set"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    assert!(
        js_worker_set.len() >= 2,
        "reference trace should expose multiple workers"
    );
    assert_eq!(
        rust.worker_set, js_worker_set,
        "worker_set diverged — tid→worker attribution differs between decoders"
    );

    // The on/off-runtime split must match per source (on = worker_id.is_some()).
    for src in [SOURCE_CPU_PROFILE, SOURCE_SCHED_EVENT] {
        let (on, off) = rust.on_off_by_source.get(&src).copied().unwrap_or((0, 0));
        let j = &js["on_off_by_source"][src.to_string()];
        assert_eq!(
            on as u64,
            j["on"].as_u64().unwrap_or(0),
            "on-runtime count for source={src} diverged"
        );
        assert_eq!(
            off as u64,
            j["off"].as_u64().unwrap_or(0),
            "off-runtime count for source={src} diverged"
        );
    }
}

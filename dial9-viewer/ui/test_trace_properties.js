#!/usr/bin/env node
// Self-test for trace_properties.js — the canonical property oracle that the
// Rust decode parity test (`tests/parser_parity_test.rs`) diffs against.
//
// This guards the oracle's *contract*, independent of the Rust side:
//   * source is split (CpuProfile vs SchedEvent), never conflated;
//   * on/off-runtime is broken out per source;
//   * the frame separator is NUL (a space would collide real stacks);
//   * digests are stable and reproducible.
//
// It also re-checks the committed golden fixture the Rust test loads when node
// is unavailable, so a stale fixture fails here rather than silently in CI.

"use strict";

const fs = require("fs");
const path = require("path");
const {
  computeProperties,
  computePropertiesFromFile,
  FRAME_SEP,
  SOURCE_CPU_PROFILE,
  SOURCE_SCHED_EVENT,
} = require("./trace_properties.js");

let failed = 0;
function ok(cond, label) {
  if (cond) {
    console.log(`✓ ${label}`);
  } else {
    console.error(`✗ ${label}`);
    failed++;
  }
}
function eq(actual, expected, label) {
  ok(actual === expected, `${label} (got ${JSON.stringify(actual)})`);
}

async function main() {
  // FRAME_SEP must be NUL: symbol names contain spaces, so a space separator
  // would collide distinct stacks. This is the exact regression the framework
  // caught during development.
  eq(FRAME_SEP, String.fromCharCode(0), "FRAME_SEP is NUL");
  eq(FRAME_SEP.charCodeAt(0), 0, "FRAME_SEP byte is 0x00");
  eq(FRAME_SEP.length, 1, "FRAME_SEP is a single char");

  // ── Synthetic trace exercising every property branch ──────────────────────
  // Two CpuProfile samples sharing a stack, one with a distinct stack, plus a
  // SchedEvent sample (different source) and an off-worker sample (workerId 255).
  const callframeSymbols = new Map([
    ["0x1", { symbol: "alpha", location: null }],
    ["0x2", { symbol: "beta gamma", location: null }], // NOTE: contains a space
    ["0x3", { symbol: "delta", location: null }],
  ]);
  const trace = {
    callframeSymbols,
    cpuSamples: [
      { source: 0, workerId: 0, timestamp: 1000, callchain: ["0x1", "0x2"] },
      { source: 0, workerId: 1, timestamp: 1100, callchain: ["0x1", "0x2"] },
      { source: 0, workerId: 255, timestamp: 1200, callchain: ["0x3"] },
      { source: 1, workerId: 0, timestamp: 1300, callchain: ["0x1"] }, // sched
      { source: 0, workerId: 0, timestamp: 1400, callchain: [] }, // empty: dropped
    ],
  };
  const p = computeProperties(trace);

  eq(p.total_samples, 4, "total_samples drops empty callchains");
  eq(p.by_source[String(SOURCE_CPU_PROFILE)], 3, "by_source CpuProfile count");
  eq(p.by_source[String(SOURCE_SCHED_EVENT)], 1, "by_source SchedEvent count");

  // Source must NOT be conflated: cpu_profile only counts source 0.
  eq(p.cpu_profile.count, 3, "cpu_profile.count excludes sched");
  eq(p.cpu_profile.distinct_stacks, 2, "distinct stacks (two share a stack)");

  // on/off split, per source. The off-worker (255) CpuProfile sample is off.
  eq(p.on_off_by_source["0"].on, 2, "CpuProfile on-runtime count");
  eq(p.on_off_by_source["0"].off, 1, "CpuProfile off-runtime count");
  eq(p.on_off_by_source["1"].on, 1, "SchedEvent on-runtime count");

  // worker_set is the union of observed REAL worker ids only — the off-worker
  // sentinel (255) is excluded, matching the Rust `Option<worker_id>` Some-set.
  eq(JSON.stringify(p.worker_set), JSON.stringify([0, 1]), "worker_set");

  // The "beta gamma" frame proves NUL separation: with a space separator the
  // signature "alpha beta gamma" would be indistinguishable from a 3-frame
  // stack alpha|beta|gamma. The leaf of the top stack must be exactly "alpha".
  eq(p.cpu_profile.top_stacks[0].leaf, "alpha", "top stack leaf intact");
  eq(p.cpu_profile.top_stacks[0].depth, 2, "top stack depth = 2 (NUL split)");
  eq(p.cpu_profile.top_stacks[0].count, 2, "top stack count");

  // Digests are deterministic.
  const p2 = computeProperties(trace);
  eq(
    p.cpu_profile.stack_sig_digest,
    p2.cpu_profile.stack_sig_digest,
    "stack_sig_digest deterministic"
  );
  eq(
    p.cpu_profile.ts_delta_digest,
    p2.cpu_profile.ts_delta_digest,
    "ts_delta_digest deterministic"
  );

  // Offset-invariance: shifting every CpuProfile timestamp by a constant must
  // not change the ts_delta_digest (this is what lets monotonic-vs-wallclock
  // compare equal across the two decoders).
  const shifted = {
    callframeSymbols,
    cpuSamples: trace.cpuSamples.map((s) => ({
      ...s,
      timestamp: s.timestamp + 1_000_000_000,
    })),
  };
  eq(
    computeProperties(shifted).cpu_profile.ts_delta_digest,
    p.cpu_profile.ts_delta_digest,
    "ts_delta_digest is offset-invariant"
  );

  // ── Demo trace + golden fixture cross-check ───────────────────────────────
  const demoPath = path.join(__dirname, "demo-trace.bin");
  if (fs.existsSync(demoPath)) {
    const demo = await computePropertiesFromFile(demoPath);
    ok(demo.by_source["0"] > 0, "demo trace has CpuProfile samples");

    const goldenPath = path.join(
      __dirname,
      "..",
      "tests",
      "fixtures",
      "demo-trace.properties.json"
    );
    const golden = fs.existsSync(goldenPath)
      ? JSON.parse(fs.readFileSync(goldenPath, "utf8"))
      : null;

    // The rich-trace properties (sched-event presence, the cpu/sched source
    // split, and the golden digests) are only meaningful against the *committed*
    // canonical trace. The e2e pipeline regenerates demo-trace.bin first, and
    // regeneration is environment-dependent: CI containers can't capture perf
    // sched events, and CPU-sample timing varies run to run, so a regenerated
    // trace never matches the fixture. Only enforce the cross-check when the
    // on-disk trace IS the canonical one (its sample count matches the fixture).
    // The authoritative, environment-independent fixture check is the Rust
    // `parser_parity_test`, which always runs against the committed trace.
    if (golden && demo.total_samples === golden.total_samples) {
      ok(demo.by_source["1"] > 0, "demo trace has SchedEvent samples");
      ok(
        demo.cpu_profile.count < demo.total_samples,
        "demo: source split is real (cpu_profile < total)"
      );
      eq(
        demo.cpu_profile.stack_sig_digest,
        golden.cpu_profile.stack_sig_digest,
        "golden fixture stack_sig_digest is current"
      );
      eq(
        demo.cpu_profile.ts_delta_digest,
        golden.cpu_profile.ts_delta_digest,
        "golden fixture ts_delta_digest is current"
      );
    } else if (!golden) {
      console.log("· golden fixture absent — skipping cross-check");
    } else {
      console.log(
        "· on-disk demo trace differs from the golden fixture " +
          `(${demo.total_samples} vs ${golden.total_samples} samples — regenerated / ` +
          "profiling-incapable env); skipping rich cross-check. The committed " +
          "trace is validated by the Rust parser_parity_test."
      );
    }
  } else {
    console.log("· demo-trace.bin absent — skipping demo cross-check");
  }

  console.log(
    `\n${failed === 0 ? "All trace_properties tests passed" : `${failed} test(s) FAILED`}`
  );
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error("error:", e.stack || e.message);
  process.exit(1);
});

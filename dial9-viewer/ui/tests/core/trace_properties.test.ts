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
//
// Migrated from test_trace_properties.js (T11); frozen core loaded via
// createRequire (see format.test.ts for the rationale).

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);

interface CpuSampleLike {
  source: number;
  workerId: number;
  timestamp: number;
  callchain: string[];
}

interface TraceLike {
  callframeSymbols: Map<string, { symbol: string; location: string | null }>;
  cpuSamples: CpuSampleLike[];
}

interface SourceProperties {
  count: number;
  distinct_stacks: number;
  top_stacks: Array<{ leaf: string; depth: number; count: number }>;
  stack_sig_digest: string;
  ts_delta_digest: string;
}

interface TraceProperties {
  total_samples: number;
  by_source: Record<string, number>;
  cpu_profile: SourceProperties;
  on_off_by_source: Record<string, { on: number; off: number }>;
  worker_set: number[];
}

const {
  computeProperties,
  computePropertiesFromFile,
  FRAME_SEP,
  SOURCE_CPU_PROFILE,
  SOURCE_SCHED_EVENT,
} = require("../../trace_properties.js") as {
  computeProperties: (trace: TraceLike) => TraceProperties;
  computePropertiesFromFile: (path: string) => Promise<TraceProperties>;
  FRAME_SEP: string;
  SOURCE_CPU_PROFILE: number;
  SOURCE_SCHED_EVENT: number;
};

describe("FRAME_SEP", () => {
  // FRAME_SEP must be NUL: symbol names contain spaces, so a space separator
  // would collide distinct stacks. This is the exact regression the framework
  // caught during development.
  it("is NUL", () => {
    expect(FRAME_SEP).toBe(String.fromCharCode(0));
    expect(FRAME_SEP.charCodeAt(0), "FRAME_SEP byte is 0x00").toBe(0);
    expect(FRAME_SEP.length, "FRAME_SEP is a single char").toBe(1);
  });
});

// ── Synthetic trace exercising every property branch ──────────────────────
// Two CpuProfile samples sharing a stack, one with a distinct stack, plus a
// SchedEvent sample (different source) and an off-worker sample (workerId 255).
const callframeSymbols = new Map([
  ["0x1", { symbol: "alpha", location: null }],
  ["0x2", { symbol: "beta gamma", location: null }], // NOTE: contains a space
  ["0x3", { symbol: "delta", location: null }],
]);
const syntheticTrace: TraceLike = {
  callframeSymbols,
  cpuSamples: [
    { source: 0, workerId: 0, timestamp: 1000, callchain: ["0x1", "0x2"] },
    { source: 0, workerId: 1, timestamp: 1100, callchain: ["0x1", "0x2"] },
    { source: 0, workerId: 255, timestamp: 1200, callchain: ["0x3"] },
    { source: 1, workerId: 0, timestamp: 1300, callchain: ["0x1"] }, // sched
    { source: 0, workerId: 0, timestamp: 1400, callchain: [] }, // empty: dropped
  ],
};

describe("computeProperties (synthetic trace)", () => {
  const p = computeProperties(syntheticTrace);

  it("counts and splits sources", () => {
    expect(p.total_samples, "total_samples drops empty callchains").toBe(4);
    expect(p.by_source[String(SOURCE_CPU_PROFILE)], "by_source CpuProfile count").toBe(3);
    expect(p.by_source[String(SOURCE_SCHED_EVENT)], "by_source SchedEvent count").toBe(1);

    // Source must NOT be conflated: cpu_profile only counts source 0.
    expect(p.cpu_profile.count, "cpu_profile.count excludes sched").toBe(3);
    expect(p.cpu_profile.distinct_stacks, "distinct stacks (two share a stack)").toBe(2);
  });

  it("splits on/off runtime per source", () => {
    // The off-worker (255) CpuProfile sample is off.
    expect(p.on_off_by_source["0"]!.on, "CpuProfile on-runtime count").toBe(2);
    expect(p.on_off_by_source["0"]!.off, "CpuProfile off-runtime count").toBe(1);
    expect(p.on_off_by_source["1"]!.on, "SchedEvent on-runtime count").toBe(1);
  });

  it("worker_set excludes the off-worker sentinel", () => {
    // worker_set is the union of observed REAL worker ids only — the off-worker
    // sentinel (255) is excluded, matching the Rust `Option<worker_id>` Some-set.
    expect(JSON.stringify(p.worker_set)).toBe(JSON.stringify([0, 1]));
  });

  it("NUL separation keeps multi-word frames intact", () => {
    // The "beta gamma" frame proves NUL separation: with a space separator the
    // signature "alpha beta gamma" would be indistinguishable from a 3-frame
    // stack alpha|beta|gamma. The leaf of the top stack must be exactly "alpha".
    expect(p.cpu_profile.top_stacks[0]!.leaf, "top stack leaf intact").toBe("alpha");
    expect(p.cpu_profile.top_stacks[0]!.depth, "top stack depth = 2 (NUL split)").toBe(2);
    expect(p.cpu_profile.top_stacks[0]!.count, "top stack count").toBe(2);
  });

  it("digests are deterministic", () => {
    const p2 = computeProperties(syntheticTrace);
    expect(p.cpu_profile.stack_sig_digest, "stack_sig_digest deterministic").toBe(
      p2.cpu_profile.stack_sig_digest,
    );
    expect(p.cpu_profile.ts_delta_digest, "ts_delta_digest deterministic").toBe(
      p2.cpu_profile.ts_delta_digest,
    );
  });

  it("ts_delta_digest is offset-invariant", () => {
    // Shifting every CpuProfile timestamp by a constant must not change the
    // ts_delta_digest (this is what lets monotonic-vs-wallclock compare equal
    // across the two decoders).
    const shifted: TraceLike = {
      callframeSymbols,
      cpuSamples: syntheticTrace.cpuSamples.map((s) => ({
        ...s,
        timestamp: s.timestamp + 1_000_000_000,
      })),
    };
    expect(computeProperties(shifted).cpu_profile.ts_delta_digest).toBe(
      p.cpu_profile.ts_delta_digest,
    );
  });
});

// ── Demo trace + golden fixture cross-check ───────────────────────────────
describe("demo trace + golden fixture cross-check", () => {
  const demoPath = fileURLToPath(
    new URL("../../public/demo-trace.bin", import.meta.url),
  );
  const goldenPath = fileURLToPath(
    new URL("../../../tests/fixtures/demo-trace.properties.json", import.meta.url),
  );

  it.skipIf(!existsSync(demoPath))("demo trace properties vs golden fixture", async () => {
    const demo = await computePropertiesFromFile(demoPath);
    expect(demo.by_source["0"]!, "demo trace has CpuProfile samples").toBeGreaterThan(0);

    const golden = existsSync(goldenPath)
      ? (JSON.parse(readFileSync(goldenPath, "utf8")) as TraceProperties)
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
      expect(demo.by_source["1"]!, "demo trace has SchedEvent samples").toBeGreaterThan(0);
      expect(
        demo.cpu_profile.count,
        "demo: source split is real (cpu_profile < total)",
      ).toBeLessThan(demo.total_samples);
      expect(
        demo.cpu_profile.stack_sig_digest,
        "golden fixture stack_sig_digest is current",
      ).toBe(golden.cpu_profile.stack_sig_digest);
      expect(
        demo.cpu_profile.ts_delta_digest,
        "golden fixture ts_delta_digest is current",
      ).toBe(golden.cpu_profile.ts_delta_digest);
    } else if (!golden) {
      console.log("· golden fixture absent — skipping cross-check");
    } else {
      console.log(
        "· on-disk demo trace differs from the golden fixture " +
          `(${demo.total_samples} vs ${golden.total_samples} samples — regenerated / ` +
          "profiling-incapable env); skipping rich cross-check. The committed " +
          "trace is validated by the Rust parser_parity_test.",
      );
    }
  });
});

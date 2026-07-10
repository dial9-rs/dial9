// Tests for the flamegraph recipe predicates documented in SKILL.md.
// Each recipe filter is run against demo-trace.bin to verify it produces
// non-empty results (where appropriate).
//
// Migrated from test_flamegraph_recipes.js (T11); frozen core loaded via
// createRequire (see format.test.ts for the rationale). This suite was never
// registered in CI before; results are data-dependent on the committed demo
// trace (see the it.fails note below).

import { describe, it, expect, beforeAll } from "vitest";
import { createRequire } from "node:module";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);

interface SymbolEntry {
  symbol: string;
  location: string | null;
}

interface CpuSample {
  source: number;
  timestamp: number;
  callchain: string[];
  spawnLoc?: string | null;
}

interface Poll {
  taskId?: number;
  start: number;
  end: number;
  cpuSamples?: CpuSample[];
}

interface ParsedTraceLike {
  events: { eventType: number; workerId: number; timestamp: number }[];
  cpuSamples: CpuSample[];
  callframeSymbols: Map<string, SymbolEntry | SymbolEntry[]>;
  maxTs: number;
  minTs: number;
  blockInPlaceGaps: unknown[];
}

const { parseTrace, EVENT_TYPES } = require("../../trace_parser.js") as {
  parseTrace: (buf: Buffer) => Promise<ParsedTraceLike>;
  EVENT_TYPES: Record<string, number>;
};
const TraceAnalysis = require("../../trace_analysis.js") as {
  buildWorkerSpans: (
    events: unknown[],
    workerIds: number[],
    maxTs: number,
    gaps?: unknown[],
  ) => { workerSpans: Record<number, { polls: Poll[] }> };
  attachCpuSamples: (
    samples: CpuSample[],
    workerSpans: Record<number, { polls: Poll[] }>,
  ) => unknown;
};

const tracePath = fileURLToPath(
  new URL("../../public/demo-trace.bin", import.meta.url),
);

let trace: ParsedTraceLike;
let buf: Buffer;
let workerIds: number[];
let workerSpans: Record<number, { polls: Poll[] }>;

beforeAll(async () => {
  expect(existsSync(tracePath), `demo-trace.bin not found at ${tracePath}`).toBe(true);
  buf = readFileSync(tracePath);
  trace = await parseTrace(buf);

  // Build worker spans and attach CPU samples (required setup for all recipes)
  const wSet = new Set<number>();
  trace.events.forEach((e) => {
    if (
      e.eventType !== EVENT_TYPES["QueueSample"] &&
      e.eventType !== EVENT_TYPES["WakeEvent"]
    )
      wSet.add(e.workerId);
  });
  workerIds = [...wSet].sort((a, b) => a - b);
  const spanResult = TraceAnalysis.buildWorkerSpans(
    trace.events,
    workerIds,
    trace.maxTs,
    trace.blockInPlaceGaps,
  );
  workerSpans = spanResult.workerSpans;
  TraceAnalysis.attachCpuSamples(trace.cpuSamples, workerSpans);
});

describe("flamegraph recipes", { timeout: 60_000 }, () => {
  // ── Recipe 1: Total on-CPU flamegraph ──
  it("Recipe on-CPU total", () => {
    const samples = trace.cpuSamples.filter((s) => s.source === 0);
    expect(
      samples.length,
      `expected >0 samples, got ${samples.length}`,
    ).toBeGreaterThan(0);
  });

  // ── Recipe 2: Off-CPU (scheduling) samples only ──
  it("Recipe off-CPU", () => {
    const samples = trace.cpuSamples.filter((s) => s.source === 1);
    expect(
      samples.length,
      `expected >0 samples, got ${samples.length}`,
    ).toBeGreaterThan(0);
  });

  // ── Recipe 3: Just task X (via poll spans) ──
  it("Recipe task X via poll spans", () => {
    let targetTaskId: number | null = null;
    for (const wid of workerIds) {
      for (const p of workerSpans[wid]!.polls) {
        if (p.taskId && p.cpuSamples && p.cpuSamples.length > 0) {
          targetTaskId = p.taskId;
          break;
        }
      }
      if (targetTaskId) break;
    }
    expect(targetTaskId, "no task with CPU samples found").toBeTruthy();
    const taskPolls: Poll[] = [];
    for (const wid of workerIds) {
      for (const p of workerSpans[wid]!.polls) {
        if (p.taskId === targetTaskId) taskPolls.push(p);
      }
    }
    const samples = trace.cpuSamples.filter(
      (s) =>
        s.source === 0 &&
        taskPolls.some((p) => s.timestamp >= p.start && s.timestamp <= p.end),
    );
    expect(
      samples.length,
      `expected >0 samples for task ${targetTaskId}`,
    ).toBeGreaterThan(0);
  });

  // ── Recipe 4: Polls > N ms ──
  //
  // PRE-EXISTING FAILURE (pre-ruled in the T11 ticket): this recipe finds no
  // on-CPU samples inside >5ms polls in the COMMITTED demo trace, and already
  // failed on the untouched lineage (verified by the T04 implementer against
  // main; the suite was never registered in CI). Data-dependent on
  // demo-trace.bin: a regenerated demo trace may legitimately make the recipe
  // find samples again, at which point drop the `.fails` modifier.
  it.fails("Recipe polls > 5ms", () => {
    const THRESHOLD_NS = 5_000_000;
    const longPolls: Poll[] = [];
    for (const wid of workerIds) {
      for (const p of workerSpans[wid]!.polls) {
        if (p.end - p.start > THRESHOLD_NS) longPolls.push(p);
      }
    }
    const samples = trace.cpuSamples.filter(
      (s) =>
        s.source === 0 &&
        longPolls.some((p) => s.timestamp >= p.start && s.timestamp <= p.end),
    );
    expect(samples.length, `expected >0 samples for long polls`).toBeGreaterThan(0);
  });

  // ── Recipe 5: One specific poll instance (worst sampled poll for a task) ──
  it("Recipe worst sampled poll for a task", () => {
    let targetTaskId: number | null = null;
    let worstPoll: Poll | null = null;
    for (const wid of workerIds) {
      for (const p of workerSpans[wid]!.polls) {
        if (p.taskId && p.cpuSamples && p.cpuSamples.length > 0) {
          if (!worstPoll || p.end - p.start > worstPoll.end - worstPoll.start) {
            worstPoll = p;
            targetTaskId = p.taskId;
          }
        }
      }
    }
    expect(worstPoll, "no sampled poll found").toBeTruthy();
    const samples = trace.cpuSamples.filter(
      (s) =>
        s.source === 0 &&
        s.timestamp >= worstPoll!.start &&
        s.timestamp <= worstPoll!.end,
    );
    expect(
      samples.length,
      `expected >0 samples for task ${targetTaskId}'s worst sampled poll`,
    ).toBeGreaterThan(0);
  });

  // ── Recipe 6: Leaf-frame search ──
  it("Recipe leaf-frame search", () => {
    let searchTerm: string | null = null;
    for (const [, v] of trace.callframeSymbols) {
      const sym = Array.isArray(v) ? v[0]!.symbol : v.symbol;
      if (sym && sym.includes("rustls")) {
        searchTerm = "rustls";
        break;
      }
    }
    if (!searchTerm) {
      for (const [, v] of trace.callframeSymbols) {
        const sym = Array.isArray(v) ? v[0]!.symbol : v.symbol;
        if (sym && sym.length > 5 && !sym.startsWith("0x")) {
          searchTerm = sym.slice(0, 10);
          break;
        }
      }
    }
    expect(searchTerm, "no symbols found to search").toBeTruthy();
    // Leaf-frame may legitimately return 0, verify any-frame works at minimum
    const anyFrame = trace.cpuSamples.filter((s) =>
      s.callchain.some((addr) => {
        const sym = trace.callframeSymbols.get(addr);
        if (!sym) return false;
        const name = Array.isArray(sym) ? sym[0]!.symbol : sym.symbol;
        return name && name.includes(searchTerm!);
      }),
    );
    expect(
      anyFrame.length,
      `expected >0 samples containing '${searchTerm}'`,
    ).toBeGreaterThan(0);
  });

  // ── Recipe 7: Multi-trace union (parse same trace twice, merge symbols) ──
  it("Recipe multi-trace union", async () => {
    const trace2 = await parseTrace(buf);
    const merged = new Map(trace.callframeSymbols);
    for (const [k, v] of trace2.callframeSymbols) {
      if (!merged.has(k)) merged.set(k, v);
    }
    const union = [...trace.cpuSamples, ...trace2.cpuSamples].filter(
      (s) => s.source === 0,
    );
    expect(union.length, "should have on-CPU samples").toBeGreaterThan(0);
    expect(
      merged.size,
      "merged symbols should be >= original",
    ).toBeGreaterThanOrEqual(trace.callframeSymbols.size);
  });

  // ── Recipe 8: spawnLoc-based filter ──
  it("Recipe spawnLoc filter", () => {
    const locCounts = new Map<string, number>();
    trace.cpuSamples.forEach((s) => {
      if (s.spawnLoc) locCounts.set(s.spawnLoc, (locCounts.get(s.spawnLoc) || 0) + 1);
    });
    const topLoc = [...locCounts.entries()].sort((a, b) => b[1] - a[1])[0];
    expect(topLoc, "no spawnLocs in trace").toBeTruthy();
    const samples = trace.cpuSamples.filter((s) => s.spawnLoc === topLoc![0]);
    expect(samples.length, `expected >0 samples for spawnLoc`).toBeGreaterThan(0);
  });

  // ── Type contract: trace timestamps are Numbers, not BigInts ──
  it("Skill type contract: trace timestamps are plain Numbers", () => {
    const checks: Array<[string, unknown]> = [
      ["trace.minTs", trace.minTs],
      ["trace.maxTs", trace.maxTs],
      ["cpuSamples[0].timestamp", trace.cpuSamples[0]?.timestamp],
      ["events[0].timestamp", trace.events[0]?.timestamp],
    ];
    for (const [label, v] of checks) {
      if (v !== undefined) {
        expect(typeof v, `${label} should be number, got ${typeof v}`).toBe("number");
      }
    }
    // Verify the documented arithmetic doesn't throw
    const sum = trace.minTs + 3_900_000_000;
    expect(Number.isFinite(sum)).toBe(true);
  });
});

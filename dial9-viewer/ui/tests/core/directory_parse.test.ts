// Tests for parseTrace's directory mode: async iteration over a directory of
// .bin segments, the .d9-cache JSON cache (creation, invalidation, force,
// atomic writes), sampling, and the downstream analysis pipeline.
//
// Migrated from test_directory_parse.js (T11); frozen core loaded via
// createRequire (see format.test.ts for the rationale). Temp directories are
// seeded with copies of the demo trace, exactly as before.

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);

interface ParsedTraceLoose {
  magic: string;
  events: { eventType: number; workerId: number; timestamp: number }[];
  cpuSamples: { timestamp: number }[];
  taskSpawnTimes: Map<unknown, unknown>;
  callframeSymbols: Map<unknown, unknown>;
  segmentMetadata: Map<unknown, unknown>;
  maxTs: number;
  [key: string]: unknown;
}

interface DirParseOpts {
  sample?: number;
  force?: boolean;
  cache?: boolean;
  parallel?: boolean;
  onParseProgress?: (p: unknown) => void;
}

const { parseTrace, EVENT_TYPES } = require("../../trace_parser.js") as {
  parseTrace: (
    input: string | Buffer,
    opts?: DirParseOpts,
  ) => AsyncIterable<ParsedTraceLoose> & Promise<ParsedTraceLoose>;
  EVENT_TYPES: Record<string, number>;
};

const { buildWorkerSpans, attachCpuSamples, computeSchedulingDelays } =
  require("../../trace_analysis.js") as {
    buildWorkerSpans: (
      events: unknown[],
      workerIds: number[],
      maxTs: number,
    ) => {
      workerSpans: Record<number, { polls: unknown[] }>;
      wakesByTask: unknown;
    };
    attachCpuSamples: (
      samples: unknown[],
      workerSpans: Record<number, { polls: unknown[] }>,
    ) => { pollsWithCpuSamples: number };
    computeSchedulingDelays: (
      workerSpans: Record<number, { polls: unknown[] }>,
      workerIds: number[],
      wakesByTask: unknown,
    ) => unknown[];
  };

// analyze.js lives in the dial9-toolkit skill's scripts, exactly where the
// original resolved it from the ui/ root.
const analyzeJsPath = fileURLToPath(
  new URL("../../../skills/dial9-toolkit/scripts/analyze.js", import.meta.url),
);

const demoPath = fileURLToPath(
  new URL("../../public/demo-trace.bin", import.meta.url),
);

/** Collect first trace from the async iterable. */
async function first(
  input: string,
  opts?: DirParseOpts,
): Promise<ParsedTraceLoose> {
  for await (const t of parseTrace(input, opts)) return t;
  throw new Error("no traces");
}

/** Collect all traces from the async iterable. */
async function collect(
  input: string,
  opts?: DirParseOpts,
): Promise<ParsedTraceLoose[]> {
  const all: ParsedTraceLoose[] = [];
  for await (const t of parseTrace(input, opts)) all.push(t);
  return all;
}

function setupDir(n: number): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "d9-test-dir-"));
  for (let i = 0; i < n; i++) {
    fs.copyFileSync(demoPath, path.join(dir, `trace-${String(i).padStart(3, "0")}.bin`));
  }
  return dir;
}
function cleanup(dir: string): void {
  fs.rmSync(dir, { recursive: true, force: true });
}

describe("directory parsing", { timeout: 120_000 }, () => {
  it("demo trace exists", () => {
    expect(fs.existsSync(demoPath), "demo-trace.bin not found").toBe(true);
  });

  // ── Single file: async iterable yields one ParsedTrace ──
  it("parseTrace with file path", async () => {
    const trace = await first(demoPath);
    expect(trace.magic, "file: valid trace").toBe("D9TF");
    expect(trace.events.length, "file: has events").toBeGreaterThan(0);
    expect(trace.taskSpawnTimes instanceof Map, "file: taskSpawnTimes is Map").toBe(true);
    expect(
      trace.callframeSymbols instanceof Map,
      "file: callframeSymbols is Map",
    ).toBe(true);
    expect(trace.cpuSamples.length, "file: has cpuSamples").toBeGreaterThan(0);
  });

  // ── Buffer: returns Promise<ParsedTrace> (backwards compatible) ──
  it("parseTrace with buffer", async () => {
    const buf = fs.readFileSync(demoPath);
    const trace = await parseTrace(buf);
    expect(trace.magic, "buffer: valid trace").toBe("D9TF");
    expect(trace.events.length, "buffer: has events").toBeGreaterThan(0);
  });

  // ── Directory: yields one ParsedTrace per file ──
  it("parseTrace with directory", async () => {
    const dir = setupDir(3);
    try {
      const traces = await collect(dir);
      expect(traces.length, `dir: 3 traces (got ${traces.length})`).toBe(3);
      for (const trace of traces) {
        expect(trace.magic, "dir: valid trace").toBe("D9TF");
        expect(trace.events.length, "dir: has events").toBeGreaterThan(0);
        expect(
          trace.taskSpawnTimes instanceof Map,
          "dir: taskSpawnTimes is Map",
        ).toBe(true);
        expect(
          trace.callframeSymbols instanceof Map,
          "dir: callframeSymbols is Map",
        ).toBe(true);
        expect(trace.cpuSamples.length, "dir: has cpuSamples").toBeGreaterThan(0);
      }
    } finally {
      cleanup(dir);
    }
  });

  // ── Same shape for file and directory ──
  it("unified shape", async () => {
    const dir = setupDir(1);
    try {
      const fromFile = await first(demoPath);
      const fromDir = await first(dir);
      const keys = [
        "magic",
        "events",
        "cpuSamples",
        "taskSpawnTimes",
        "callframeSymbols",
        "customEvents",
        "segmentMetadata",
      ];
      for (const k of keys) {
        expect(k in fromFile && k in fromDir, `unified: both have '${k}'`).toBe(true);
      }
    } finally {
      cleanup(dir);
    }
  });

  // ── Caching ──
  it("caching", async () => {
    const dir = setupDir(2);
    try {
      await collect(dir);
      const cacheDir = path.join(dir, ".d9-cache");
      expect(fs.existsSync(cacheDir), "cache: .d9-cache created").toBe(true);
      const cached = fs.readdirSync(cacheDir).filter((f) => f.endsWith(".json"));
      expect(cached.length, `cache: 2 files (got ${cached.length})`).toBe(2);

      // Warm run
      const warm = await collect(dir);
      expect(warm.length, "cache hit: 2 traces").toBe(2);
      expect(warm[0]!.events.length, "cache hit: has events").toBeGreaterThan(0);
      expect(
        warm[0]!.taskSpawnTimes instanceof Map,
        "cache hit: Maps reconstructed",
      ).toBe(true);
      expect(
        warm[0]!.segmentMetadata instanceof Map,
        "cache hit: segmentMetadata is Map",
      ).toBe(true);
    } finally {
      cleanup(dir);
    }
  });

  // ── Capability fields survive the cache round-trip ──
  it("cached capability fields", async () => {
    const dir = setupDir(1);
    const caps = ["hasFullTaskCoverage", "hasLocalQueueDepth", "hasTaskLifetimes"];
    try {
      const cold = await first(demoPath);
      const warm = await first(dir);
      for (const k of caps) {
        expect(warm[k], `cache: ${k} preserved`).toBe(cold[k]);
        expect(typeof warm[k], `cache: ${k} is boolean`).toBe("boolean");
      }

      // Pre-capability cache file: strip the fields from the meta record and
      // check the loader re-derives them from the cached segmentMetadata.
      const cacheDir = path.join(dir, ".d9-cache");
      const cp = path.join(cacheDir, fs.readdirSync(cacheDir)[0]!);
      const lines = fs.readFileSync(cp, "utf8").split("\n");
      const meta = JSON.parse(lines[0]!) as { d: Record<string, unknown> };
      for (const k of caps) delete meta.d[k];
      lines[0] = JSON.stringify(meta);
      fs.writeFileSync(cp, lines.join("\n"));
      const legacy = await first(dir);
      for (const k of caps) {
        expect(legacy[k], `backfill: ${k} re-derived`).toBe(cold[k]);
      }
    } finally {
      cleanup(dir);
    }
  });

  // ── Cache invalidation ──
  it("cache invalidation", async () => {
    const dir = setupDir(1);
    try {
      await collect(dir);
      const cacheDir = path.join(dir, ".d9-cache");
      const cp = path.join(cacheDir, fs.readdirSync(cacheDir)[0]!);
      const mtimeBefore = fs.statSync(cp).mtimeMs;
      await new Promise((r) => setTimeout(r, 50));
      const src = path.join(
        dir,
        fs.readdirSync(dir).filter((f) => f.endsWith(".bin"))[0]!,
      );
      fs.utimesSync(src, new Date(), new Date());
      await collect(dir);
      expect(fs.statSync(cp).mtimeMs, "invalidation: cache updated").toBeGreaterThan(
        mtimeBefore,
      );
    } finally {
      cleanup(dir);
    }
  });

  // ── Force ──
  it("force re-parse rewrites cache", async () => {
    const dir = setupDir(1);
    try {
      await collect(dir);
      const cacheDir = path.join(dir, ".d9-cache");
      const cp = path.join(cacheDir, fs.readdirSync(cacheDir)[0]!);
      const mtimeBefore = fs.statSync(cp).mtimeMs;
      await new Promise((r) => setTimeout(r, 50));
      await collect(dir, { force: true });
      expect(fs.statSync(cp).mtimeMs, "force: cache rewritten").toBeGreaterThan(
        mtimeBefore,
      );
    } finally {
      cleanup(dir);
    }
  });

  // ── Sample ──
  it("sample option limits iterated files", async () => {
    const dir = setupDir(10);
    try {
      const traces = await collect(dir, { sample: 3 });
      expect(traces.length, `sample: 3 traces (got ${traces.length})`).toBe(3);
    } finally {
      cleanup(dir);
    }
  });

  // ── Sample validation ──
  it("sample=0 throws", async () => {
    const dir = setupDir(3);
    try {
      let threw = false;
      try {
        await collect(dir, { sample: 0 });
      } catch (e) {
        threw = true;
        expect(
          (e as Error).message.includes("sample"),
          `sample=0: ${(e as Error).message}`,
        ).toBe(true);
      }
      expect(threw, "sample=0: throws").toBe(true);
    } finally {
      cleanup(dir);
    }
  });

  // ── Cache disabled ──
  it("cache disabled", async () => {
    const dir = setupDir(2);
    try {
      const traces = await collect(dir, { cache: false });
      expect(traces.length, "no-cache: 2 traces").toBe(2);
      expect(traces[0]!.events.length, "no-cache: has events").toBeGreaterThan(0);
      expect(
        fs.existsSync(path.join(dir, ".d9-cache")),
        "no-cache: no .d9-cache",
      ).toBe(false);
    } finally {
      cleanup(dir);
    }
  });

  // ── Empty directory ──
  it("empty directory throws", async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "d9-test-empty-"));
    try {
      let threw = false;
      try {
        await collect(dir);
      } catch (e) {
        threw = true;
        expect(
          (e as Error).message.includes("No .bin"),
          `empty: ${(e as Error).message}`,
        ).toBe(true);
      }
      expect(threw, "empty: throws").toBe(true);
    } finally {
      cleanup(dir);
    }
  });

  // ── Progress ──
  it("progress callbacks fire", async () => {
    const dir = setupDir(3);
    try {
      const progress: unknown[] = [];
      await collect(dir, { onParseProgress: (p) => progress.push(p) });
      expect(
        progress.length,
        `progress: >= 3 calls (got ${progress.length})`,
      ).toBeGreaterThanOrEqual(3);
    } finally {
      cleanup(dir);
    }
  });

  // ── Parallel=false ──
  it("parallel=false still yields all traces", async () => {
    const dir = setupDir(3);
    try {
      const traces = await collect(dir, { parallel: false });
      expect(traces.length, "sequential: 3 traces").toBe(3);
      expect(traces[0]!.events.length, "sequential: has events").toBeGreaterThan(0);
    } finally {
      cleanup(dir);
    }
  });

  // ── Atomic writes ──
  it("atomic writes leave no .tmp files", async () => {
    const dir = setupDir(1);
    try {
      await collect(dir);
      const cacheDir = path.join(dir, ".d9-cache");
      const tmps = fs.readdirSync(cacheDir).filter((f) => f.endsWith(".tmp"));
      expect(tmps.length, "atomic: no .tmp files").toBe(0);
    } finally {
      cleanup(dir);
    }
  });

  // ── Full analysis pipeline works on each yielded trace ──
  it("analysis pipeline on each yielded trace", async () => {
    const dir = setupDir(2);
    try {
      for await (const trace of parseTrace(dir)) {
        const workerIds = [
          ...new Set(
            trace.events
              .filter(
                (e) =>
                  e.eventType !== EVENT_TYPES["QueueSample"] &&
                  e.eventType !== EVENT_TYPES["WakeEvent"],
              )
              .map((e) => e.workerId),
          ),
        ].sort((a, b) => a - b);
        expect(workerIds.length, "pipeline: has workers").toBeGreaterThan(0);
        const maxTs = trace.maxTs;
        const spans = buildWorkerSpans(trace.events, workerIds, maxTs);
        expect(
          spans.workerSpans[workerIds[0]!]!.polls.length,
          "pipeline: has polls",
        ).toBeGreaterThan(0);
        const { pollsWithCpuSamples } = attachCpuSamples(
          trace.cpuSamples,
          spans.workerSpans,
        );
        expect(pollsWithCpuSamples, "pipeline: attachCpuSamples works").toBeGreaterThan(0);
        const schedDelays = computeSchedulingDelays(
          spans.workerSpans,
          workerIds,
          spans.wakesByTask,
        );
        expect(schedDelays.length, "pipeline: has schedDelays").toBeGreaterThan(0);
      }
    } finally {
      cleanup(dir);
    }
  });

  // ── analyzeTraces respects sample option ──
  it("analyzeTraces respects sample option", async () => {
    const { analyzeTraces } = require(analyzeJsPath) as {
      analyzeTraces: (
        dir: string,
        opts?: { sample?: number; force?: boolean },
      ) => Promise<{ eventCount: number }>;
    };
    const dir = setupDir(6);
    try {
      // Full run to populate cache for all 6 files
      const full = await analyzeTraces(dir);
      const fullEvents = full.eventCount;

      // Sampled run should only analyze 3 files, not all 6 cached
      const sampled = await analyzeTraces(dir, { sample: 3 });
      expect(sampled.eventCount, "sample: has events").toBeGreaterThan(0);
      expect(
        sampled.eventCount,
        `sample: fewer events than full (${sampled.eventCount} < ${fullEvents})`,
      ).toBeLessThan(fullEvents);
      // Each file has the same event count, so sampled should be ~half
      const expectedRatio = 3 / 6;
      const actualRatio = sampled.eventCount / fullEvents;
      expect(
        Math.abs(actualRatio - expectedRatio),
        `sample: event ratio ~0.5 (got ${actualRatio.toFixed(3)})`,
      ).toBeLessThan(0.01);
    } finally {
      cleanup(dir);
    }
  });

  // ── analyzeTraces respects force option ──
  it("analyzeTraces respects force option", async () => {
    const { analyzeTraces } = require(analyzeJsPath) as {
      analyzeTraces: (
        dir: string,
        opts?: { sample?: number; force?: boolean },
      ) => Promise<{ eventCount: number }>;
    };
    const dir = setupDir(2);
    try {
      // First run populates cache
      await analyzeTraces(dir);
      const cacheDir = path.join(dir, ".d9-cache");
      const cacheFile = path.join(
        cacheDir,
        fs.readdirSync(cacheDir).filter((f) => f.endsWith(".json"))[0]!,
      );
      const mtimeBefore = fs.statSync(cacheFile).mtimeMs;
      await new Promise((r) => setTimeout(r, 50));

      // Force should re-parse even though cache is valid
      await analyzeTraces(dir, { force: true });
      const mtimeAfter = fs.statSync(cacheFile).mtimeMs;
      expect(mtimeAfter, "force: cache file was rewritten").toBeGreaterThan(mtimeBefore);
    } finally {
      cleanup(dir);
    }
  });
});

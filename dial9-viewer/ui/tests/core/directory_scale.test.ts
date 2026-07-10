// Scale test for directory parsing: cold parse, warm (cached) parse, sampled
// parse, and the full analysis pipeline over a directory of demo-trace copies.
//
// Migrated from test_directory_scale.js (T11). The original's `--large` argv
// flag (200 files instead of 20) is now the D9_SCALE_LARGE=1 env var, since
// Vitest owns argv. Tests in this file run sequentially and share the seeded
// directory: warm-vs-cold timing only means something in that order.

import { describe, it, expect, beforeAll, afterAll } from "vitest";
import { createRequire } from "node:module";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);

interface ParsedTraceLoose {
  events: { eventType: number; workerId: number; timestamp: number }[];
  cpuSamples: unknown[];
  taskSpawnTimes: Map<unknown, unknown>;
  maxTs: number;
}

const { parseTrace, EVENT_TYPES } = require("../../trace_parser.js") as {
  parseTrace: (
    input: string,
    opts?: { sample?: number },
  ) => AsyncIterable<ParsedTraceLoose>;
  EVENT_TYPES: Record<string, number>;
};

const { buildWorkerSpans, attachCpuSamples, computeSchedulingDelays } =
  require("../../trace_analysis.js") as {
    buildWorkerSpans: (
      events: unknown[],
      workerIds: number[],
      maxTs: number,
    ) => {
      workerSpans: Record<number, { polls: { start: number; end: number }[] }>;
      wakesByTask: unknown;
    };
    attachCpuSamples: (
      samples: unknown[],
      workerSpans: Record<number, { polls: { start: number; end: number }[] }>,
    ) => unknown;
    computeSchedulingDelays: (
      workerSpans: Record<number, { polls: { start: number; end: number }[] }>,
      workerIds: number[],
      wakesByTask: unknown,
    ) => unknown[];
  };

const LARGE = process.env["D9_SCALE_LARGE"] === "1";
const FILE_COUNT = LARGE ? 200 : 20;

const demoPath = fileURLToPath(
  new URL("../../public/demo-trace.bin", import.meta.url),
);

let dir: string;
let coldMs: number;

beforeAll(() => {
  expect(fs.existsSync(demoPath), "demo-trace.bin not found").toBe(true);
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "d9-scale-"));
  for (let i = 0; i < FILE_COUNT; i++) {
    fs.copyFileSync(
      demoPath,
      path.join(dir, `seg-${String(i).padStart(4, "0")}.bin`),
    );
  }
});

afterAll(() => {
  fs.rmSync(dir, { recursive: true, force: true });
});

describe(`directory scale (${FILE_COUNT} files)`, { timeout: 300_000 }, () => {
  // ── Cold parse ──
  it("cold parse iterates all files", async () => {
    const t0 = Date.now();
    let coldCount = 0;
    for await (const trace of parseTrace(dir)) {
      coldCount++;
      if (coldCount === 1) {
        expect(trace.events.length, "first file has events").toBeGreaterThan(0);
        expect(trace.taskSpawnTimes instanceof Map, "Maps intact").toBe(true);
      }
    }
    coldMs = Date.now() - t0;
    expect(coldCount, `cold: iterated all ${FILE_COUNT} files`).toBe(FILE_COUNT);
    console.log(`  Cold: ${coldMs}ms (${(coldMs / FILE_COUNT).toFixed(0)}ms/file)`);
  });

  // ── Warm parse ──
  it("warm parse hits the cache and is faster", async () => {
    const t1 = Date.now();
    let warmCount = 0;
    for await (const trace of parseTrace(dir)) {
      warmCount++;
      if (warmCount === 1)
        expect(trace.events.length, "cache hit: has events").toBeGreaterThan(0);
    }
    const warmMs = Date.now() - t1;
    expect(warmCount, `warm: iterated all ${FILE_COUNT} files`).toBe(FILE_COUNT);
    expect(
      warmMs,
      `warm faster than cold (${warmMs}ms < ${coldMs}ms)`,
    ).toBeLessThan(coldMs);
    console.log(`  Warm: ${warmMs}ms (${(warmMs / FILE_COUNT).toFixed(0)}ms/file)`);
  });

  // ── Sampled parse ──
  it("sampled parse iterates the sample count", async () => {
    const sampleN = Math.min(10, FILE_COUNT);
    const t2 = Date.now();
    let sampleCount = 0;
    for await (const _ of parseTrace(dir, { sample: sampleN })) {
      sampleCount++;
    }
    const sampleMs = Date.now() - t2;
    expect(sampleCount, `sample: iterated ${sampleN} files`).toBe(sampleN);
    console.log(`  Sampled (${sampleN}): ${sampleMs}ms`);
  });

  // ── Full pipeline on cached data ──
  it("full pipeline on cached data", async () => {
    const t3 = Date.now();
    let worstPollMs = 0;
    let totalDelays = 0;
    let pipelineCount = 0;

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
      const maxTs = trace.maxTs;
      const spans = buildWorkerSpans(trace.events, workerIds, maxTs);
      attachCpuSamples(trace.cpuSamples, spans.workerSpans);
      const schedDelays = computeSchedulingDelays(
        spans.workerSpans,
        workerIds,
        spans.wakesByTask,
      );
      totalDelays += schedDelays.length;

      for (const w of workerIds) {
        for (const p of spans.workerSpans[w]!.polls) {
          const dur = (p.end - p.start) / 1e6;
          if (dur > worstPollMs) worstPollMs = dur;
        }
      }
      pipelineCount++;
    }
    const pipelineMs = Date.now() - t3;
    expect(pipelineCount, `pipeline: processed all ${FILE_COUNT} files`).toBe(
      FILE_COUNT,
    );
    expect(
      worstPollMs,
      `pipeline: found worst poll (${worstPollMs.toFixed(1)}ms)`,
    ).toBeGreaterThan(0);
    expect(totalDelays, `pipeline: found scheduling delays`).toBeGreaterThan(0);
    console.log(
      `  Pipeline: ${pipelineMs}ms (${(pipelineMs / FILE_COUNT).toFixed(0)}ms/file)`,
    );
  });
});

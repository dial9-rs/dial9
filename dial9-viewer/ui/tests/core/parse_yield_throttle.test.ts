// Regression test for issue #595: the buffered trace parser (parseTraceBuffer,
// used for multi-trace `trace=` loads — a single trace streams instead) must
// throttle its expensive macrotask yield by WALL-CLOCK time, not by bytes.
//
// The bug: parseTraceBuffer used to `await setTimeout(0)` once per 100KB of
// decoded bytes. Each setTimeout(0) is clamped to ~4ms in browsers and forces a
// repaint, so a 10MB trace fired ~108 yields/repaints and parsed far slower
// than the single-trace streaming path (which already throttled by wall-clock).
//
// parseTraceStream already moved off the byte cadence; this asserts the buffered
// path matches that behavior. We mock the clock so the test is fully
// deterministic (no dependence on how fast the host actually parses):
//   • frozen clock  → ZERO macrotask yields, even though onProgress fires on
//     every 100KB milestone. (The old byte-cadence code yielded on each.)
//   • fast clock    → the parser still yields, so the spinner can repaint.
//
// Migrated from test_parse_yield_throttle.js (T11); frozen core loaded via
// createRequire (see format.test.ts for the rationale).

import { describe, it, expect, beforeAll } from "vitest";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";

const require = createRequire(import.meta.url);

interface ParsedTraceLike {
  events: unknown[];
}

const { parseTrace } = require("../../trace_parser.js") as {
  parseTrace: (
    buf: Uint8Array,
    opts?: { onParseProgress?: () => void },
  ) => Promise<ParsedTraceLike>;
};

const globalAny = globalThis as {
  setTimeout: typeof setTimeout;
  performance: unknown;
};

// Run `fn` with global setTimeout counted (delay-0 macrotask yields only) and
// performance.now driven by `clock()`. Restores both afterward.
async function withInstrumentedClock(
  clock: () => number,
  fn: () => Promise<void>,
): Promise<number> {
  const realSetTimeout = globalAny.setTimeout;
  const realPerformance = globalAny.performance;
  let yields = 0;
  globalAny.setTimeout = function (
    cb: () => void,
    delay?: number,
    ...rest: unknown[]
  ) {
    if (delay === 0 || delay == null) yields++;
    return (realSetTimeout as (...a: unknown[]) => unknown)(cb, delay, ...rest);
  } as typeof setTimeout;
  // parseTraceBuffer's nowMs closure reads `performance.now()` at call time, so
  // swapping the whole global picks up our clock. (performance.now itself is a
  // read-only property, so we can't reassign just the method.)
  globalAny.performance = { now: clock };
  try {
    await fn();
  } finally {
    globalAny.setTimeout = realSetTimeout;
    globalAny.performance = realPerformance;
  }
  return yields;
}

const tracePath = fileURLToPath(
  new URL("../../public/demo-trace.bin", import.meta.url),
);

const fileBytes = readFileSync(tracePath);
const raw = Uint8Array.from(
  fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
    ? gunzipSync(fileBytes)
    : Buffer.from(fileBytes),
);

let reference: ParsedTraceLike;

beforeAll(async () => {
  // Sanity: the demo is big enough to cross many 100KB progress milestones, so
  // a byte-cadence yield would fire many times. Otherwise the test is vacuous.
  const milestones = Math.floor(raw.length / (100 * 1024));
  expect(
    milestones,
    `demo trace spans ${milestones} progress milestones (need >= 20 for a meaningful test)`,
  ).toBeGreaterThanOrEqual(20);

  // Reference: events from an un-instrumented parse, to prove the yield change
  // doesn't alter decode output.
  reference = await parseTrace(raw);
  expect(reference.events.length, "reference has events").toBeGreaterThan(0);
});

describe("parse yield throttle (#595)", { timeout: 60_000 }, () => {
  // ── Frozen clock: the wall-clock throttle must suppress ALL macrotask yields
  //    while onProgress still fires on every 100KB milestone. The pre-#595
  //    byte-cadence code yielded once per milestone here. ──
  it("frozen clock: progress fires but zero macrotask yields", async () => {
    let progressFires = 0;
    const yields = await withInstrumentedClock(
      () => 1000, // never advances
      async () => {
        const t = await parseTrace(raw, {
          onParseProgress: () => {
            progressFires++;
          },
        });
        expect(
          t.events.length,
          "throttling must not change decoded event count",
        ).toBe(reference.events.length);
      },
    );
    expect(
      progressFires,
      `onProgress should fire on each 100KB milestone (got ${progressFires})`,
    ).toBeGreaterThanOrEqual(20);
    expect(
      yields,
      `frozen clock must yield 0 times, got ${yields} (byte-cadence regression: would be ~${progressFires})`,
    ).toBe(0);
  });

  // ── Fast clock: every milestone is >= PAINT_INTERVAL_MS apart, so the parser
  //    still hands the thread back so the spinner can repaint. ──
  it("fast clock: parser still yields so the spinner repaints", async () => {
    let progressFires = 0;
    let t = 0;
    const yields = await withInstrumentedClock(
      () => (t += 1000), // +1s per read >> 200ms paint interval
      async () => {
        await parseTrace(raw, {
          onParseProgress: () => {
            progressFires++;
          },
        });
      },
    );
    expect(
      yields,
      `fast clock should still yield at least once (got ${yields})`,
    ).toBeGreaterThanOrEqual(1);
    // Yields are clamped to the paint cadence: never more than one per progress
    // milestone (and with a real clock, far fewer).
    expect(
      yields,
      `yields (${yields}) must not exceed progress milestones (${progressFires})`,
    ).toBeLessThanOrEqual(progressFires);
  });

  // ── No progress callback: no yields at all (used by Node directory parsing
  //    and tests, where there is no spinner to tick). ──
  it("no onParseProgress: parser never yields", async () => {
    const yields = await withInstrumentedClock(
      () => 1000,
      async () => {
        await parseTrace(raw); // no onParseProgress
      },
    );
    expect(
      yields,
      `parse without onParseProgress must not yield (got ${yields})`,
    ).toBe(0);
  });
});

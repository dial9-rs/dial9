// Tests for sliceTrace (dial9-trace-format/js/slice.js): time-range slicing of
// raw trace bytes, absolute and relative modes, gzip handling, and symbol /
// task-table preservation.
//
// Migrated from test_slice.js (T11); the slicer and the frozen-core parser are
// loaded via createRequire (see format.test.ts for the rationale).

import { describe, it, expect, beforeAll } from "vitest";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";

const require = createRequire(import.meta.url);

interface SliceOpts {
  timeRange?: { startNs: string; endNs: string };
  relative?: boolean;
}

interface ParsedTraceLike {
  events: { timestamp: number }[];
  cpuSamples: { timestamp: number }[];
  customEvents: { timestamp: number }[];
  callframeSymbols: Map<unknown, unknown>;
  taskSpawnTimes: Map<unknown, unknown>;
  taskTerminateTimes: Map<unknown, unknown>;
  minTs: number;
  maxTs: number;
  recordMinTs: number | null;
  recordMaxTs: number | null;
}

const { sliceTrace } = require("../../../../dial9-trace-format/js/slice.js") as {
  sliceTrace: (input: Buffer | Uint8Array, opts?: SliceOpts) => Uint8Array;
};
const { parseTrace } = require("../../trace_parser.js") as {
  parseTrace: (buf: Buffer | Uint8Array) => Promise<ParsedTraceLike>;
};

const tracePath = fileURLToPath(
  new URL("../../public/demo-trace.bin", import.meta.url),
);

const input = readFileSync(tracePath);
const rawInput =
  input[0] === 0x1f && input[1] === 0x8b ? gunzipSync(input) : input;

let full: ParsedTraceLike;
let sliceMinTs: number;
let sliceMaxTs: number;
let fullEventCount: number;

beforeAll(async () => {
  // Parse the full trace for reference
  full = await parseTrace(input);
  sliceMinTs = full.recordMinTs ?? full.minTs;
  sliceMaxTs = full.recordMaxTs ?? full.maxTs;
  fullEventCount =
    full.events.length + full.cpuSamples.length + full.customEvents.length;
});

function countOf(parsed: ParsedTraceLike): number {
  return (
    parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length
  );
}

describe("sliceTrace", { timeout: 60_000 }, () => {
  // ── Test 1: Slicing with full range returns all events ──
  it("full-range slice preserves all events", async () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: sliceMinTs.toString(), endNs: sliceMaxTs.toString() },
    });
    const parsed = await parseTrace(sliced);
    expect(countOf(parsed)).toBe(fullEventCount);
  });

  // ── Test 2: Sub-range slice yields strictly smaller file ──
  it("sub-range slice is smaller and events are within range", async () => {
    const midTs = Math.floor((sliceMinTs + sliceMaxTs) / 2);
    const sliced = sliceTrace(input, {
      timeRange: { startNs: sliceMinTs.toString(), endNs: midTs.toString() },
    });
    expect(
      sliced.length,
      `${sliced.length} should be < ${rawInput.length}`,
    ).toBeLessThan(rawInput.length);

    const parsed = await parseTrace(sliced);
    expect(
      parsed.events.filter((e) => e.timestamp < sliceMinTs || e.timestamp > midTs)
        .length,
      "events out of range",
    ).toBe(0);
    expect(
      parsed.cpuSamples.filter(
        (s) => s.timestamp < sliceMinTs || s.timestamp > midTs,
      ).length,
      "cpuSamples out of range",
    ).toBe(0);
    expect(
      parsed.customEvents.filter(
        (c) => c.timestamp < sliceMinTs || c.timestamp > midTs,
      ).length,
      "customEvents out of range",
    ).toBe(0);
  });

  // ── Test 3: Sliced output is round-trip parseable ──
  it("quarter-range slice is parseable with fewer events", async () => {
    const quarterTs = Math.floor(sliceMinTs + (sliceMaxTs - sliceMinTs) / 4);
    const threeQuarterTs = Math.floor(
      sliceMinTs + (3 * (sliceMaxTs - sliceMinTs)) / 4,
    );
    const sliced = sliceTrace(input, {
      timeRange: { startNs: quarterTs.toString(), endNs: threeQuarterTs.toString() },
    });
    const parsed = await parseTrace(sliced);
    const count = countOf(parsed);
    expect(count, "should have some events").toBeGreaterThan(0);
    expect(count, `${count} should be < ${fullEventCount}`).toBeLessThan(
      fullEventCount,
    );
  });

  // ── Test 4: No-filter returns identical content ──
  it("no-filter slice preserves all events", async () => {
    const sliced = sliceTrace(input);
    const parsed = await parseTrace(sliced);
    expect(countOf(parsed)).toBe(fullEventCount);
  });

  // ── Test 5: Handles gzipped input ──
  it("gzipped input decompressed and sliced to raw output", () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: sliceMinTs.toString(), endNs: sliceMaxTs.toString() },
    });
    // Output is raw (starts with TRC magic)
    expect(sliced[0]).toBe(0x54);
    expect(sliced[1]).toBe(0x52);
    expect(sliced[2]).toBe(0x43);
  });

  // ── Test 6: Relative slice with [0, durationFull] returns all events ──
  // Note: relative mode anchors from the first timestamped event in stream order
  // (findMinTs), which may differ from parseTrace's minTs (the global minimum
  // across all events). Use a generous end to ensure we capture everything.
  it("relative full-range slice preserves all events", async () => {
    const duration = sliceMaxTs - sliceMinTs;
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "0", endNs: (duration + 1000000000).toString() },
      relative: true,
    });
    const parsed = await parseTrace(sliced);
    expect(countOf(parsed)).toBe(fullEventCount);
  });

  // ── Test 7: Relative slice with [0, halfDuration] returns ~half the events ──
  it("relative half-range slice: subset of events, all within range", async () => {
    const halfDuration = Math.floor((sliceMaxTs - sliceMinTs) / 2);
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "0", endNs: halfDuration.toString() },
      relative: true,
    });
    const parsed = await parseTrace(sliced);
    const slicedEventCount = countOf(parsed);
    expect(slicedEventCount, "should have events").toBeGreaterThan(0);
    expect(
      slicedEventCount,
      `${slicedEventCount} should be < ${fullEventCount}`,
    ).toBeLessThan(fullEventCount);
    const hi = sliceMinTs + halfDuration;
    expect(
      parsed.events.filter((e) => e.timestamp < sliceMinTs || e.timestamp > hi)
        .length,
      "events out of range",
    ).toBe(0);
    expect(
      parsed.cpuSamples.filter((s) => s.timestamp < sliceMinTs || s.timestamp > hi)
        .length,
      "cpuSamples out of range",
    ).toBe(0);
    expect(
      parsed.customEvents.filter(
        (c) => c.timestamp < sliceMinTs || c.timestamp > hi,
      ).length,
      "customEvents out of range",
    ).toBe(0);
  });

  // ── Test 8: Relative slice [3.9s, 4.05s] on demo-trace produces non-empty slice ──
  it("relative [3.9s, 4.05s] slice has events", async () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "3900000000", endNs: "4050000000" },
      relative: true,
    });
    const parsed = await parseTrace(sliced);
    expect(
      countOf(parsed),
      "should have events (this is the foot-gun case)",
    ).toBeGreaterThan(0);
  });

  // ── Test 9: Without --relative, small values produce 0-event slice (backward compat) ──
  it("absolute mode with small relative-looking values produces 0 events", async () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "3900000000", endNs: "4050000000" },
    });
    const parsed = await parseTrace(sliced);
    expect(countOf(parsed)).toBe(0);
  });

  // ── Test 10: Relative [0, 100ms] slice preserves all SymbolTableEntry events ──
  it("relative [0, 100ms] slice preserves all symbols", async () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "0", endNs: "100000000" },
      relative: true,
    });
    const parsed = await parseTrace(sliced);
    expect(parsed.callframeSymbols.size, "should have symbols").toBeGreaterThan(0);
    expect(parsed.callframeSymbols.size).toBe(full.callframeSymbols.size);
  });

  // ── Test 11: Half-range slice: time filter works AND symbols preserved ──
  it("half-range slice: time filter works AND symbols preserved", async () => {
    const halfDuration = Math.floor((sliceMaxTs - sliceMinTs) / 2);
    const sliceEnd = sliceMinTs + halfDuration;
    const sliced = sliceTrace(input, {
      timeRange: { startNs: sliceMinTs.toString(), endNs: sliceEnd.toString() },
    });
    const parsed = await parseTrace(sliced);
    expect(
      parsed.events.filter(
        (e) => e.timestamp < sliceMinTs || e.timestamp > sliceEnd,
      ).length,
      "events out of range",
    ).toBe(0);
    expect(
      parsed.cpuSamples.filter(
        (s) => s.timestamp < sliceMinTs || s.timestamp > sliceEnd,
      ).length,
      "cpuSamples out of range",
    ).toBe(0);
    expect(
      parsed.customEvents.filter(
        (c) => c.timestamp < sliceMinTs || c.timestamp > sliceEnd,
      ).length,
      "customEvents out of range",
    ).toBe(0);
    expect(parsed.callframeSymbols.size).toBe(full.callframeSymbols.size);
  });

  // ── Test 12: Full-range absolute slice preserves all events + symbols ──
  it("full-range absolute slice: all events and symbols preserved", async () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: sliceMinTs.toString(), endNs: sliceMaxTs.toString() },
    });
    const parsed = await parseTrace(sliced);
    expect(countOf(parsed)).toBe(fullEventCount);
    expect(parsed.callframeSymbols.size).toBe(full.callframeSymbols.size);
    expect(parsed.taskSpawnTimes.size).toBe(full.taskSpawnTimes.size);
    expect(parsed.taskTerminateTimes.size).toBe(full.taskTerminateTimes.size);
  });

  // ── Test 13: Relative [3.9s, 4.05s] slice has symbols for flamegraphs ──
  it("relative [3.9s, 4.05s] slice preserves all symbols", async () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "3900000000", endNs: "4050000000" },
      relative: true,
    });
    const parsed = await parseTrace(sliced);
    expect(parsed.callframeSymbols.size).toBe(full.callframeSymbols.size);
  });
});

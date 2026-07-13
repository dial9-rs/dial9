// reparse.ts tests: in-memory windowed re-parse against the real demo trace -
// no fetch involved.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import type { ParsedTrace } from "./load.js";
import { parseTraceBuffer } from "./load.js";
import { isRangeActive, reparseWithRange } from "./reparse.js";

let rawTrace: Uint8Array;
let full: ParsedTrace;

beforeAll(async () => {
  const fileBytes = readFileSync(
    fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url))
  );
  rawTrace =
    fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
      ? new Uint8Array(gunzipSync(fileBytes))
      : new Uint8Array(fileBytes);
  full = await parseTraceBuffer(rawTrace);
  expect(full.minTs).not.toBeNull();
  expect(full.maxTs).not.toBeNull();
});

describe("reparseWithRange (Set Range)", () => {
  it("filters events to the window and marks the trace time-filtered", async () => {
    const minTs = full.minTs!;
    const maxTs = full.maxTs!;
    const start = minTs + Math.floor((maxTs - minTs) / 4);
    const end = minTs + Math.floor((maxTs - minTs) / 2);
    const windowed = await reparseWithRange(rawTrace, {
      startNs: start,
      endNs: end,
    });
    expect(windowed.timeFiltered).toBe(true);
    expect(windowed.filterStartTime).toBe(start);
    expect(windowed.filterEndTime).toBe(end);
    expect(windowed.events.length).toBeGreaterThan(0);
    expect(windowed.events.length).toBeLessThan(full.events.length);
    for (const ev of [windowed.events[0]!, windowed.events.at(-1)!]) {
      expect(ev.timestamp).toBeGreaterThanOrEqual(start);
      expect(ev.timestamp).toBeLessThanOrEqual(end);
    }
  });

  it("open bounds are not forwarded (Clear Range = full trace)", async () => {
    const cleared = await reparseWithRange(rawTrace, {
      startNs: null,
      endNs: null,
    });
    expect(cleared.timeFiltered).toBe(false);
    expect(cleared.events.length).toBe(full.events.length);
  });

  it("a single open edge leaves the other side unfiltered", async () => {
    const mid = full.minTs! + Math.floor((full.maxTs! - full.minTs!) / 2);
    const tail = await reparseWithRange(rawTrace, { startNs: mid });
    expect(tail.timeFiltered).toBe(true);
    expect(tail.filterStartTime).toBe(mid);
    // Core contract: with an active filter, an open end edge surfaces as
    // the parser's Infinity default (not null - null means "unfiltered").
    expect(tail.filterEndTime).toBe(Infinity);
    expect(tail.events[0]!.timestamp).toBeGreaterThanOrEqual(mid);
  });
});

describe("isRangeActive", () => {
  it("false only when both edges are open", () => {
    expect(isRangeActive({})).toBe(false);
    expect(isRangeActive({ startNs: null, endNs: null })).toBe(false);
    expect(isRangeActive({ startNs: 1 })).toBe(true);
    expect(isRangeActive({ endNs: 1 })).toBe(true);
    // 0 is a valid closed edge, not "unset".
    expect(isRangeActive({ startNs: 0 })).toBe(true);
  });
});

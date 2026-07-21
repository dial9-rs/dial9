// Time-range filtering tests for parseTrace (startTime/endTime options),
// run against the demo trace.
//
// Migrated from test_time_range.js (T10); frozen core loaded via
// createRequire (see format.test.ts for the rationale).
//
// The original took an optional trace path as argv[2]; per ADR-0004 section 7
// trace-file-parameterized suites switch from argv to an env var under Vitest:
// set DIAL9_TRACE_PATH to run against a different trace file. Defaults to the
// checked-in demo trace at public/demo-trace.bin (path moved there by T04).

import { describe, it, expect, beforeAll } from "vitest";
import { createRequire } from "node:module";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);

interface TraceEvent {
  timestamp: number;
}

interface ParseResult {
  events: TraceEvent[];
  truncated: boolean;
  timeFiltered: boolean;
  filterStartTime?: number;
  filterEndTime?: number;
  minTs: number;
  maxTs: number;
  callframeSymbols: Map<unknown, unknown>;
  taskSpawnTimes: Map<unknown, unknown>;
}

const { parseTrace } = require("../../trace_parser.js") as {
  parseTrace: (
    buf: Buffer,
    opts?: { startTime?: number; endTime?: number; maxEvents?: number },
  ) => Promise<ParseResult>;
};

const tracePath =
  process.env["DIAL9_TRACE_PATH"] ??
  fileURLToPath(new URL("../../public/demo-trace.bin", import.meta.url));

// Events whose timestamp falls outside [lo, hi]; the original failed on the
// first offender, this reports how many there are.
function outsideRange(events: TraceEvent[], lo: number, hi: number): TraceEvent[] {
  return events.filter((e) => e.timestamp < lo || e.timestamp > hi);
}

describe("parseTrace time-range filtering", () => {
  let buf: Buffer;
  let full: ParseResult;
  let minTs: number;
  let maxTs: number;
  let midTs: number;

  beforeAll(async () => {
    expect(existsSync(tracePath), `Trace file not found: ${tracePath}`).toBe(
      true,
    );
    buf = readFileSync(tracePath);
    full = await parseTrace(buf);
    minTs = full.minTs;
    maxTs = full.maxTs;
    midTs = Math.floor((minTs + maxTs) / 2);
  });

  it("full parse: no truncation, no time filter", () => {
    expect(full.events.length, "No events in full parse").toBeGreaterThan(0);
    expect(full.truncated, "Full parse should not be truncated").toBe(false);
    expect(full.timeFiltered, "Full parse should not be time-filtered").toBe(
      false,
    );
  });

  it("halves are time-filtered, in range, and cover the full parse", async () => {
    // Filter first half
    const firstHalf = await parseTrace(buf, {
      startTime: minTs,
      endTime: midTs,
    });
    expect(firstHalf.events.length, "First half has no events").toBeGreaterThan(
      0,
    );
    expect(
      firstHalf.events.length,
      "First half should have fewer events than full",
    ).toBeLessThan(full.events.length);
    expect(firstHalf.timeFiltered, "First half should be time-filtered").toBe(
      true,
    );
    expect(firstHalf.filterStartTime, "filterStartTime mismatch").toBe(minTs);
    expect(firstHalf.filterEndTime, "filterEndTime mismatch").toBe(midTs);
    expect(
      outsideRange(firstHalf.events, minTs, midTs).length,
      `first-half events outside range [${minTs}, ${midTs}]`,
    ).toBe(0);

    // Filter second half
    const secondHalf = await parseTrace(buf, {
      startTime: midTs,
      endTime: maxTs,
    });
    expect(
      secondHalf.events.length,
      "Second half has no events",
    ).toBeGreaterThan(0);
    expect(
      outsideRange(secondHalf.events, midTs, maxTs).length,
      `second-half events outside range [${midTs}, ${maxTs}]`,
    ).toBe(0);

    // Halves should cover most events (boundary events may be missed due to
    // integer midpoint). With inclusive ranges and integer midpoint, at most a
    // few events at the exact boundary could be in both or neither.
    const totalHalves = firstHalf.events.length + secondHalf.events.length;
    const missed = full.events.length - totalHalves;
    expect(missed, `Halves missed ${missed} events (too many)`).toBeLessThanOrEqual(
      10,
    );

    // Symbol tables are uncapped frames and should always be present,
    // preserved across time filters.
    if (full.callframeSymbols.size > 0) {
      expect(
        firstHalf.callframeSymbols.size,
        "Symbol table size mismatch between filtered and full parse",
      ).toBe(full.callframeSymbols.size);
    }
  });

  it("narrow range (20% window) filters to in-range events", async () => {
    const narrowStart = minTs + Math.floor((maxTs - minTs) * 0.4);
    const narrowEnd = minTs + Math.floor((maxTs - minTs) * 0.6);
    const narrow = await parseTrace(buf, {
      startTime: narrowStart,
      endTime: narrowEnd,
    });
    expect(narrow.events.length, "Narrow range has no events").toBeGreaterThan(
      0,
    );
    expect(
      narrow.events.length,
      "Narrow range should have fewer events",
    ).toBeLessThan(full.events.length);
    expect(
      outsideRange(narrow.events, narrowStart, narrowEnd).length,
      "narrow events outside range",
    ).toBe(0);

    // Task spawn/terminate preserved: task spawn times should be preserved
    // regardless of time filter (uncapped frame).
    if (full.taskSpawnTimes.size > 0) {
      expect(
        narrow.taskSpawnTimes.size,
        "Task spawn times should be preserved (uncapped frame)",
      ).toBe(full.taskSpawnTimes.size);
    }
  });

  it("maxEvents still works", async () => {
    const limited = await parseTrace(buf, { maxEvents: 100 });
    expect(
      limited.events.length,
      "maxEvents=100 exceeded",
    ).toBeLessThanOrEqual(100);
    expect(limited.truncated, "Should be truncated with maxEvents=100").toBe(
      true,
    );
  });

  it("combined maxEvents + time range", async () => {
    const combined = await parseTrace(buf, {
      maxEvents: 50,
      startTime: minTs,
      endTime: midTs,
    });
    expect(combined.events.length, "Combined maxEvents exceeded").toBeLessThanOrEqual(
      50,
    );
    expect(
      outsideRange(combined.events, minTs, midTs).length,
      "Combined event outside time range",
    ).toBe(0);
  });

  it("empty range: 0 events", async () => {
    const empty = await parseTrace(buf, {
      startTime: maxTs + 1e9,
      endTime: maxTs + 2e9,
    });
    expect(empty.events.length, "Empty range should have 0 events").toBe(0);
  });
});

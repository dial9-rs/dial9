// The aggregated flamegraph's time-range readout

import { describe, it, expect } from "vitest";
import { utcRange } from "./api-mode.js";

const ns = (iso: string): number => Date.parse(iso) * 1e6;

describe("utcRange", () => {
  it("dates the start, times the end, and names the zone", () => {
    expect(
      utcRange(ns("2026-09-16T11:26:00Z"), ns("2026-09-16T11:34:04Z")),
    ).toBe("2026/09/16 11:26:00 → 11:34:04 UTC");
  });

  it("renders UTC wall-clock regardless of the host timezone", () => {
    const label = utcRange(ns("2026-01-01T00:30:00Z"), ns("2026-01-01T01:00:00Z"));
    expect(label).toBe("2026/01/01 00:30:00 → 01:00:00 UTC");
  });

  it("keeps 24h times past noon", () => {
    expect(
      utcRange(ns("2026-09-16T13:05:00Z"), ns("2026-09-16T23:59:59Z")),
    ).toBe("2026/09/16 13:05:00 → 23:59:59 UTC");
  });
});

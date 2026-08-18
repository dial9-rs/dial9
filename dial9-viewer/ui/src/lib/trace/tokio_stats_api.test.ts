import { describe, expect, it } from "vitest";
import {
  busynessHeat,
  hostBusyPct,
  hostWorkerCounts,
  latencyHeat,
} from "./tokio_stats_api.js";

describe("Tokio-stats heat scales", () => {
  it("classifies latency at the 1ms and 3ms boundaries", () => {
    expect(latencyHeat(999_999)).toBe("#3fb950");
    expect(latencyHeat(1_000_000)).toBe("#d29922");
    expect(latencyHeat(2_999_999)).toBe("#d29922");
    expect(latencyHeat(3_000_000)).toBe("#f85149");
  });

  it("classifies worker busyness at the 50% and 80% boundaries", () => {
    expect(busynessHeat(49.9)).toBe("#3fb950");
    expect(busynessHeat(50)).toBe("#d29922");
    expect(busynessHeat(79.9)).toBe("#d29922");
    expect(busynessHeat(80)).toBe("#f85149");
  });
});

describe("hostBusyPct", () => {
  it("pools busy and observed time instead of averaging worker ratios", () => {
    expect(
      hostBusyPct([
        { busy_ns: 90, span_ns: 100 },
        { busy_ns: 90, span_ns: 900 },
      ]),
    ).toBe(18);
  });

  it("handles saturated and empty hosts", () => {
    expect(
      hostBusyPct([
        { busy_ns: 100, span_ns: 100 },
        { busy_ns: 900, span_ns: 900 },
      ]),
    ).toBe(100);
    expect(hostBusyPct([])).toBe(0);
  });
});

describe("hostWorkerCounts", () => {
  it("distinguishes observed workers from the configured id range", () => {
    const workers = Array.from({ length: 23 }, (_, worker_id) => ({
      worker_id: worker_id === 22 ? 63 : worker_id,
    }));
    expect(hostWorkerCounts(workers)).toEqual({ active: 23, total: 64 });
  });

  it("falls back to the active count when worker ids are unavailable", () => {
    expect(hostWorkerCounts([{}, {}])).toEqual({
      active: 2,
      total: 2,
    });
    expect(hostWorkerCounts([])).toEqual({ active: 0, total: 0 });
  });
});

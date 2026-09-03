import { describe, it, expect } from "vitest";
import {
  computeRuntimeMetrics,
  EMPTY_RUNTIME_METRICS,
} from "./runtime-metrics-model.js";
import type { ParsedTrace, RuntimeMetricsSample } from "../../types/trace.js";

/** Minimal ParsedTrace carrying only the fields computeRuntimeMetrics reads. */
function traceWith(runtimeMetrics: RuntimeMetricsSample[]): ParsedTrace {
  return { runtimeMetrics } as unknown as ParsedTrace;
}

describe("computeRuntimeMetrics", () => {
  it("returns EMPTY for a trace with no runtime metrics (old format)", () => {
    expect(computeRuntimeMetrics(traceWith([]))).toBe(EMPTY_RUNTIME_METRICS);
    expect(computeRuntimeMetrics(null)).toBe(EMPTY_RUNTIME_METRICS);
  });

  it("groups samples by runtime and tracks the peak per runtime", () => {
    const m = computeRuntimeMetrics(
      traceWith([
        { t: 10, runtimeName: "", globalQueue: 5, aliveTasks: 100 },
        { t: 10, runtimeName: "io", globalQueue: 3, aliveTasks: 4 },
        { t: 20, runtimeName: "", globalQueue: 2, aliveTasks: 194 },
        { t: 20, runtimeName: "io", globalQueue: 6, aliveTasks: 2 },
      ]),
    );
    expect(m.present).toBe(true);

    const main = m.byRuntime.get("")!;
    expect(main.samples.length).toBe(2);
    expect(main.aliveTasksAvailable).toBe(true);
    expect(main.maxAliveTasks).toBe(194);
    expect(main.maxGlobalQueue).toBe(5);

    const io = m.byRuntime.get("io")!;
    expect(io.maxAliveTasks).toBe(4);
    expect(io.maxGlobalQueue).toBe(6);
  });

  it("marks alive-task counts unavailable when the old schema omitted them", () => {
    const m = computeRuntimeMetrics(
      traceWith([
        { t: 10, runtimeName: "", globalQueue: 5, aliveTasks: null },
        { t: 20, runtimeName: "", globalQueue: 2, aliveTasks: null },
      ]),
    );
    const main = m.byRuntime.get("")!;
    expect(main.aliveTasksAvailable).toBe(false);
    expect(main.maxAliveTasks).toBe(0);
    expect(main.maxGlobalQueue).toBe(5);
  });

  it("sums the global queue per cycle timestamp across runtimes", () => {
    const m = computeRuntimeMetrics(
      traceWith([
        { t: 10, runtimeName: "", globalQueue: 5, aliveTasks: 0 },
        { t: 10, runtimeName: "io", globalQueue: 3, aliveTasks: 0 },
        { t: 20, runtimeName: "", globalQueue: 2, aliveTasks: 0 },
      ]),
    );
    // Cycle 10: 5 + 3 = 8; cycle 20: 2. Sorted by t.
    expect(m.summedGlobalQueue).toEqual([
      { t: 10, global: 8 },
      { t: 20, global: 2 },
    ]);
  });
});

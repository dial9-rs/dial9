import { createRequire } from "node:module";
import { describe, expect, it } from "vitest";

const require = createRequire(import.meta.url);
const {
  decodeLegacyActiveTaskSample,
  decodeRuntimeMetricsSample,
} = require("../../trace_parser.js") as {
  decodeLegacyActiveTaskSample: (
    timestamp: number,
    fields: { active_tasks?: number | string | bigint | null },
  ) => { t: number; count: number } | null;
  decodeRuntimeMetricsSample: (
    timestamp: number,
    fields: {
      runtime_name?: string | null;
      global_queue_depth: number | string | bigint;
      alive_tasks?: number | string | bigint | null;
    },
  ) => {
    t: number;
    runtimeName: string;
    globalQueue: number;
    aliveTasks: number | null;
  };
};

describe("legacy QueueSample active-task decoding", () => {
  it("preserves a present active_tasks field", () => {
    expect(decodeLegacyActiveTaskSample(123, { active_tasks: "42" })).toEqual({
      t: 123,
      count: 42,
    });
  });

  it("does not invent zero for traces whose schema predates active_tasks", () => {
    expect(decodeLegacyActiveTaskSample(123, {})).toBeNull();
    expect(decodeLegacyActiveTaskSample(123, { active_tasks: null })).toBeNull();
  });
});

describe("RuntimeMetrics active-task decoding", () => {
  it("preserves a present alive_tasks field", () => {
    expect(
      decodeRuntimeMetricsSample(123, {
        runtime_name: "io",
        global_queue_depth: "7",
        alive_tasks: "42",
      }),
    ).toEqual({
      t: 123,
      runtimeName: "io",
      globalQueue: 7,
      aliveTasks: 42,
    });
  });

  it("does not invent zero for old RuntimeMetrics schemas", () => {
    expect(
      decodeRuntimeMetricsSample(123, {
        global_queue_depth: 7,
      }),
    ).toEqual({
      t: 123,
      runtimeName: "",
      globalQueue: 7,
      aliveTasks: null,
    });
  });
});

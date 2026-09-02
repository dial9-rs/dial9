import { createRequire } from "node:module";
import { describe, expect, it } from "vitest";

const require = createRequire(import.meta.url);
const { decodeLegacyActiveTaskSample } = require("../../trace_parser.js") as {
  decodeLegacyActiveTaskSample: (
    timestamp: number,
    fields: { active_tasks?: number | string | bigint | null },
  ) => { t: number; count: number } | null;
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

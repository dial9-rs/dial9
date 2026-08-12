import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

const { deriveCapabilities } = require("../../trace_parser.js") as {
  deriveCapabilities: (
    meta: Map<string, string>,
    taskSpawnTimes: Map<number, number>,
  ) => {
    hasFullTaskCoverage: boolean;
    hasLocalQueueDepth: boolean;
    hasTaskLifetimes: boolean;
  };
};

const spawns = new Map([[1, 100]]);
const noSpawns = new Map<number, number>();

describe("deriveCapabilities", () => {
  it("task lifetimes follow spawn-event evidence, not metadata", () => {
    const meta = new Map([["tokio.unstable", "true"]]);
    expect(deriveCapabilities(meta, spawns).hasTaskLifetimes).toBe(true);
    expect(deriveCapabilities(meta, noSpawns).hasTaskLifetimes).toBe(false);
    expect(deriveCapabilities(new Map(), spawns).hasTaskLifetimes).toBe(true);
    expect(deriveCapabilities(new Map(), noSpawns).hasTaskLifetimes).toBe(
      false,
    );
  });

  it("reads the metadata-declared capability keys", () => {
    const caps = deriveCapabilities(
      new Map([
        ["tokio.poll_coverage", "dial9-spawns-only"],
        ["tokio.local_queue", "false"],
      ]),
      noSpawns,
    );
    expect(caps.hasFullTaskCoverage).toBe(false);
    expect(caps.hasLocalQueueDepth).toBe(false);

    const full = deriveCapabilities(new Map(), noSpawns);
    expect(full.hasFullTaskCoverage).toBe(true);
    expect(full.hasLocalQueueDepth).toBe(true);
  });
});

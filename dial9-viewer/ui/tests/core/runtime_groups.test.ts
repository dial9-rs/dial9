// Tests for computeRuntimeGroups: grouping worker lanes by Tokio runtime.
//
// The default ("main") runtime is not named on the wire — its workers carry
// no runtime.<name> segment metadata — so it is inferred as the block of
// present workers that no named runtime claims.
//
// Migrated from test_runtime_groups.js (T11); frozen core loaded via
// createRequire (see format.test.ts for the rationale).

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

interface RuntimeGroup {
  name: string;
  inferred: boolean;
  workerIds: number[];
}

interface CpuSampleLike {
  workerId: number;
  callchain: number[];
  timestamp: number;
}

interface RuntimeFilterOption {
  name: string;
  sampleCount: number;
  inferred: boolean;
}

const { computeRuntimeGroups, buildRuntimeFilterData } =
  require("../../trace_analysis.js") as {
    computeRuntimeGroups: (
      workerIds: number[],
      runtimeWorkers?: Map<string, number[]>,
    ) => RuntimeGroup[];
    buildRuntimeFilterData: (
      samples: CpuSampleLike[],
      runtimeWorkers?: Map<string, number[]>,
      offWorkerId?: number,
    ) => { workerRuntime: Map<number, string>; options: RuntimeFilterOption[] };
  };

function names(groups: RuntimeGroup[]): string[] {
  return groups.map((g) => g.name);
}

describe("computeRuntimeGroups", () => {
  it("no runtime metadata at all -> single inferred 'main' group with everyone", () => {
    const groups = computeRuntimeGroups([0, 1, 2, 3], new Map());
    expect(groups.length, "one group when no metadata").toBe(1);
    expect(groups[0]!.name).toBe("main");
    expect(groups[0]!.inferred).toBe(true);
    expect(groups[0]!.workerIds).toEqual([0, 1, 2, 3]);
  });

  it("named 'journal' runtime plus unnamed main block: main inferred and leads", () => {
    // The reported bug: a named "journal" runtime (64..71) plus an unnamed
    // main block (0..7). Main must be inferred and lead the ordering.
    const wids = [0, 1, 2, 3, 4, 5, 6, 7, 64, 65, 66, 67, 68, 69, 70, 71];
    const rt = new Map([["journal", [64, 65, 66, 67, 68, 69, 70, 71]]]);
    const groups = computeRuntimeGroups(wids, rt);
    expect(names(groups), "main leads journal").toEqual(["main", "journal"]);
    expect(groups[0]!.workerIds).toEqual([0, 1, 2, 3, 4, 5, 6, 7]);
    expect(groups[0]!.inferred).toBe(true);
    expect(groups[1]!.workerIds).toEqual([64, 65, 66, 67, 68, 69, 70, 71]);
    expect(groups[1]!.inferred).toBe(false);
  });

  it("multiple named runtimes, no inferred main (every worker claimed)", () => {
    const rt = new Map([
      ["io", [2, 3]],
      ["main", [0, 1]],
    ]);
    const groups = computeRuntimeGroups([0, 1, 2, 3], rt);
    // Ordered by lowest worker id, so explicitly-named "main" (0,1) leads "io".
    expect(names(groups)).toEqual(["main", "io"]);
    expect(
      groups.find((g) => g.name === "main")!.inferred,
      "explicitly named main is not inferred",
    ).toBe(false);
  });

  it("named 'main' coexisting with an unnamed block does not collide", () => {
    // The inferred group must not collide with the real "main".
    const rt = new Map([["main", [10, 11]]]);
    const groups = computeRuntimeGroups([0, 1, 10, 11], rt);
    const inferred = groups.find((g) => g.inferred);
    expect(inferred!.name).toBe("main (untracked)");
    expect(inferred!.workerIds).toEqual([0, 1]);
    expect(groups.find((g) => g.name === "main")!.workerIds).toEqual([10, 11]);
  });

  it("metadata referencing absent workers: only present workers are grouped", () => {
    // Lanes always match rendered workers.
    const rt = new Map([["journal", [64, 65, 66, 67]]]);
    // Only 64 and 65 actually appear in the trace.
    const groups = computeRuntimeGroups([0, 1, 64, 65], rt);
    expect(groups.find((g) => g.name === "journal")!.workerIds).toEqual([64, 65]);
    expect(groups.find((g) => g.inferred)!.workerIds).toEqual([0, 1]);
  });

  it("empty trace -> no groups", () => {
    expect(computeRuntimeGroups([], new Map())).toEqual([]);
  });

  it("runtimeWorkers undefined (older parser / legacy ParsedTrace) -> all main", () => {
    const groups = computeRuntimeGroups([5, 6], undefined);
    expect(groups.length).toBe(1);
    expect(groups[0]!.name).toBe("main");
    expect(groups[0]!.workerIds).toEqual([5, 6]);
  });
});

// ── buildRuntimeFilterData (flamegraph runtime filter) ──

const sample = (workerId: number): CpuSampleLike => ({
  workerId,
  callchain: [1],
  timestamp: 0,
});

describe("buildRuntimeFilterData", () => {
  it("multi-runtime samples -> per-runtime counts and workerId->runtime map", () => {
    // Off-worker samples (255) are excluded.
    const rt = new Map([["journal", [64, 65]]]);
    const samples = [
      sample(0), sample(0), sample(1), // main: 3
      sample(64), sample(65),          // journal: 2
      sample(255),                     // off-worker: excluded
    ];
    const { workerRuntime, options } = buildRuntimeFilterData(samples, rt);
    expect(workerRuntime.get(0)).toBe("main");
    expect(workerRuntime.get(64)).toBe("journal");
    expect(workerRuntime.has(255), "off-worker not mapped").toBe(false);
    expect(options.map((o) => o.name)).toEqual(["main", "journal"]);
    const main = options.find((o) => o.name === "main");
    const journal = options.find((o) => o.name === "journal");
    expect(main!.sampleCount).toBe(3);
    expect(main!.inferred).toBe(true);
    expect(journal!.sampleCount).toBe(2);
    expect(journal!.inferred).toBe(false);
  });

  it("single runtime (only inferred main present) -> no options, empty map", () => {
    // The caller hides the filter.
    const samples = [sample(0), sample(1), sample(255)];
    const { workerRuntime, options } = buildRuntimeFilterData(samples, new Map());
    expect(options.length).toBe(0);
    expect(workerRuntime.size).toBe(0);
  });

  it("named runtime with no samples in this window -> filter hidden", () => {
    const rt = new Map([["journal", [64, 65]]]);
    const samples = [sample(0), sample(1)]; // no journal samples here
    const { options } = buildRuntimeFilterData(samples, rt);
    expect(options.length, "absent runtime does not appear").toBe(0);
  });

  it("runtimeWorkers undefined (e.g. heap view passes nothing) -> hidden", () => {
    const { options } = buildRuntimeFilterData([sample(0)], undefined);
    expect(options.length).toBe(0);
  });

  it("custom off-worker id is honored", () => {
    const rt = new Map([["io", [10, 11]]]);
    const samples = [sample(0), sample(10), sample(99)];
    const { workerRuntime, options } = buildRuntimeFilterData(samples, rt, 99);
    expect(workerRuntime.has(99), "custom off-worker excluded").toBe(false);
    expect(options.map((o) => o.name).sort()).toEqual(["io", "main"]);
  });
});

// Tests for the Task tab's pin model: which samples a pin folds, which tasks
// the lanes tint, and the fallbacks when the trace never recorded a spawn
// location. Sample gathering runs against the demo trace (real `spawnLoc`
// stamps from attachCpuSamples); the fallbacks run on hand-built stubs, since
// they are about absent data.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "../../lib/trace/index.js";
import { sharedDetectorInputs } from "../../lib/trace/derived.js";
import type { CpuSample, ParsedTrace, PollSpan } from "../../types/trace.js";
import {
  buildTaskFlamegraphView,
  isInPinnedFamily,
  parseSpawnPin,
  spawnLocationCpuSamples,
  spawnLocationOf,
  spawnScopeTaskIds,
  taskCpuSamples,
  taskFlamegraphCacheSignature,
  taskIdsAtSpawnLocation,
  tasksAtSpawnLocation,
} from "./task-flamegraph-model.js";

let trace: ParsedTrace;

beforeAll(async () => {
  const b = readFileSync(
    fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url)),
  );
  const raw = b[0] === 0x1f && b[1] === 0x8b ? new Uint8Array(gunzipSync(b)) : new Uint8Array(b);
  trace = await parseTraceBuffer(raw);
  // Stamps `spawnLoc` on every sample, which the spawn-location scope reads.
  sharedDetectorInputs(trace);
});

/** A spawn location the demo trace has FOLDABLE samples for, plus one of its
 *  tasks. Picking the first sample with any `spawnLoc` is not enough: it may be
 *  off-CPU or stackless, and the scope folds neither. */
function sampledLocation(): { location: string; taskId: number } {
  for (const s of trace.cpuSamples) {
    if (s.spawnLoc == null || s.source === 1 || s.callchain.length === 0) continue;
    const location = s.spawnLoc;
    for (const taskId of trace.taskSpawnLocs.keys()) {
      if (spawnLocationOf(trace, taskId) === location) return { location, taskId };
    }
  }
  throw new Error("demo trace has no sampled spawn location");
}

function sample(over: Partial<CpuSample> = {}): CpuSample {
  return {
    timestamp: 0,
    workerId: 0,
    tid: 1,
    source: 0,
    callchain: ["0x1"],
    cpu: null,
    ...over,
  } as CpuSample;
}

function poll(over: Partial<PollSpan> = {}): PollSpan {
  return { start: 0, end: 100, taskId: 1, spawnLocId: "L", spawnLoc: null, ...over } as PollSpan;
}

describe("parseSpawnPin", () => {
  it("keeps any non-empty location", () => {
    expect(parseSpawnPin("src/a.rs:1")).toBe("src/a.rs:1");
  });

  it("treats absent and blank as no pin", () => {
    expect(parseSpawnPin(null)).toBeNull();
    expect(parseSpawnPin("")).toBeNull();
    expect(parseSpawnPin("   ")).toBeNull();
  });
});

describe("taskCpuSamples", () => {
  it("gathers the on-CPU samples attached to the task's polls", () => {
    const kept = sample({ timestamp: 5 });
    const polls = [
      poll({ cpuSamples: [kept] }),
      poll({ start: 200, end: 300 }), // no samples attached
    ];
    expect(taskCpuSamples(polls)).toEqual([kept]);
  });

  it("drops stackless and off-CPU samples, which cannot be folded", () => {
    const polls = [
      poll({
        cpuSamples: [
          sample({ callchain: [] }), // no stack
          sample({ source: 1 }), // off-CPU
        ],
      }),
    ];
    expect(taskCpuSamples(polls)).toEqual([]);
  });
});

describe("spawn-location grouping (demo trace)", () => {
  it("folds every sample stamped with that spawn location", () => {
    const { location } = sampledLocation();
    const samples = spawnLocationCpuSamples(trace, location);
    expect(samples.length).toBeGreaterThan(0);
    for (const s of samples) {
      // attachCpuSamples stamps the poll's RESOLVED location, so this is a
      // plain string compare - no lookup through `spawnLocations`.
      expect(s.spawnLoc).toBe(location);
      expect(s.source).not.toBe(1);
      expect(s.callchain.length).toBeGreaterThan(0);
    }
  });

  it("counts the tasks spawned there, and lists the same set the lanes tint", () => {
    const { location } = sampledLocation();
    const ids = taskIdsAtSpawnLocation(trace, location);
    expect(ids.size).toBe(tasksAtSpawnLocation(trace, location));
    expect(ids.size).toBeGreaterThan(0);
  });

  it("memoizes the sibling set per (trace, location)", () => {
    const { location } = sampledLocation();
    expect(taskIdsAtSpawnLocation(trace, location)).toBe(
      taskIdsAtSpawnLocation(trace, location),
    );
  });

  it("resolves a task's spawn location from the task maps", () => {
    const { location, taskId } = sampledLocation();
    expect(spawnLocationOf(trace, taskId)).toBe(location);
  });
});

describe("spawn-location grouping reads the trace maps directly", () => {
  it("matches a sample's resolved spawnLoc without routing through spawnLocations", () => {
    const stub = {
      // Deliberately EMPTY: attachCpuSamples stamps the resolved location on
      // the sample, so folding must not depend on this map at all.
      spawnLocations: new Map<string, string>(),
      cpuSamples: [
        sample({ spawnLoc: "src/a.rs:1" }),
        sample({ spawnLoc: "src/b.rs:2" }),
        sample({ spawnLoc: null }),
      ],
    } as unknown as ParsedTrace;
    const kept = spawnLocationCpuSamples(stub, "src/a.rs:1");
    expect(kept).toHaveLength(1);
    expect(kept[0]!.spawnLoc).toBe("src/a.rs:1");
  });

  it("builds the sibling set from taskSpawnLocs, not the task index", () => {
    // No worker spans and no aggregates: taskIndexFor would throw on this stub,
    // which is the point - the lanes call this from a paint frame.
    const stub = {
      taskSpawnLocs: new Map([
        [1, "src/a.rs:1"],
        [2, "src/a.rs:1"],
        [3, "src/b.rs:2"],
      ]),
      spawnLocations: new Map([
        ["src/a.rs:1", "src/a.rs:1"],
        ["src/b.rs:2", "src/b.rs:2"],
      ]),
    } as unknown as ParsedTrace;
    expect([...taskIdsAtSpawnLocation(stub, "src/a.rs:1")]).toEqual([1, 2]);
    expect(tasksAtSpawnLocation(stub, "src/a.rs:1")).toBe(2);
  });
});

describe("spawnScopeTaskIds (what the lanes tint)", () => {
  it("is empty with no pin", () => {
    expect(spawnScopeTaskIds(trace, null).size).toBe(0);
  });

  it("is the whole family at the pinned location", () => {
    const { location, taskId } = sampledLocation();
    const ids = spawnScopeTaskIds(trace, location);
    expect(ids).toEqual(taskIdsAtSpawnLocation(trace, location));
    expect(ids.has(taskId)).toBe(true);
  });

  it("does not depend on the selection", () => {
    const { location } = sampledLocation();
    // The whole point of pinning: the tint is a function of the pin alone, so
    // there is no selection argument that could re-target it.
    expect(spawnScopeTaskIds(trace, location)).toEqual(
      taskIdsAtSpawnLocation(trace, location),
    );
  });

  it("is empty with no trace, or a location the trace never recorded", () => {
    expect(spawnScopeTaskIds(null, "src/a.rs:1").size).toBe(0);
    expect(spawnScopeTaskIds(trace, "nowhere.rs:1:1").size).toBe(0);
  });
});

describe("isInPinnedFamily", () => {
  it("is true for a task at the pinned location", () => {
    const { location, taskId } = sampledLocation();
    expect(isInPinnedFamily(trace, taskId, location)).toBe(true);
  });

  it("is false for a task from elsewhere, and with no pin", () => {
    const { taskId } = sampledLocation();
    expect(isInPinnedFamily(trace, taskId, "nowhere.rs:1:1")).toBe(false);
    expect(isInPinnedFamily(trace, taskId, null)).toBe(false);
    expect(isInPinnedFamily(trace, null, "src/a.rs:1")).toBe(false);
  });
});

describe("buildTaskFlamegraphView", () => {
  it("folds the task alone with no pin, titled by its hex id", () => {
    const kept = sample();
    const view = buildTaskFlamegraphView(trace, 0x2a, [poll({ cpuSamples: [kept] })], null);
    expect(view.isFamily).toBe(false);
    expect(view.samples).toEqual([kept]);
    expect(view.taskCount).toBe(1);
    expect(view.title).toContain("0x2a");
  });

  it("folds every task at the pinned location, titled by it", () => {
    const { location, taskId } = sampledLocation();
    const view = buildTaskFlamegraphView(trace, taskId, [], location);
    expect(view.isFamily).toBe(true);
    expect(view.samples.length).toBe(spawnLocationCpuSamples(trace, location).length);
    expect(view.taskCount).toBe(tasksAtSpawnLocation(trace, location));
    expect(view.title).toContain(location);
  });

  it("describes the selected task when the pin names another location", () => {
    // The tab is titled with the selected task, so it must not show a family
    // that task is not part of. The lanes and the rail still answer for the pin.
    const kept = sample();
    const view = buildTaskFlamegraphView(
      trace,
      0x2a,
      [poll({ cpuSamples: [kept] })],
      "nowhere.rs:1:1",
    );
    expect(view.isFamily).toBe(false);
    expect(view.samples).toEqual([kept]);
    expect(view.title).toContain("0x2a");
  });

  it("folds nothing with no trace and nothing selected", () => {
    expect(buildTaskFlamegraphView(trace, null, [], null).samples).toEqual([]);
    expect(buildTaskFlamegraphView(null, 1, [], null).samples).toEqual([]);
  });
});

describe("taskFlamegraphCacheSignature", () => {
  const base = {
    traceId: 1,
    taskId: 7,
    isFamily: false,
    pin: null as string | null,
    sampleCount: 3,
  };

  it("is stable for identical inputs", () => {
    expect(taskFlamegraphCacheSignature(base)).toBe(taskFlamegraphCacheSignature(base));
  });

  it("changes with the subject, the trace, and the sample count", () => {
    const sig = taskFlamegraphCacheSignature(base);
    expect(
      taskFlamegraphCacheSignature({ ...base, isFamily: true, pin: "src/a.rs:1" }),
    ).not.toBe(sig);
    expect(taskFlamegraphCacheSignature({ ...base, traceId: 2 })).not.toBe(sig);
    expect(taskFlamegraphCacheSignature({ ...base, sampleCount: 4 })).not.toBe(sig);
  });

  it("keys on the task for a single view and on the location for a family", () => {
    expect(taskFlamegraphCacheSignature({ ...base, taskId: 8 })).not.toBe(
      taskFlamegraphCacheSignature(base),
    );
    const fam = { ...base, isFamily: true, pin: "src/a.rs:1" as string | null };
    // Two tasks from the SAME location fold the same tree, so the signature
    // must not change with the task - re-selecting a sibling would rebuild it.
    expect(taskFlamegraphCacheSignature({ ...fam, taskId: 8 })).toBe(
      taskFlamegraphCacheSignature(fam),
    );
    expect(taskFlamegraphCacheSignature({ ...fam, pin: "src/b.rs:2" })).not.toBe(
      taskFlamegraphCacheSignature(fam),
    );
  });
});

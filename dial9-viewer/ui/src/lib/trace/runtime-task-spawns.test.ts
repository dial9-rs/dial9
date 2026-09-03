import { describe, expect, it } from "vitest";
import { deriveRuntimeTaskSpawns } from "./runtime-task-spawns.js";
import type { RuntimeGroup, WorkerLane } from "../../types/trace.js";

function lane(
  polls: readonly { start: number; end: number; taskId: number }[],
): WorkerLane {
  return {
    polls: polls.map((poll) => ({
      ...poll,
      spawnLocId: null,
      spawnLoc: null,
    })),
    parks: [],
    actives: [],
    cpuSampleTimes: [],
  };
}

describe("deriveRuntimeTaskSpawns", () => {
  it("attributes each spawn to the runtime owning its earliest observed poll", () => {
    const groups: RuntimeGroup[] = [
      { name: "main", inferred: true, workerIds: [0, 1] },
      { name: "io", inferred: false, workerIds: [2] },
    ];
    const result = deriveRuntimeTaskSpawns(
      new Map([
        [1, 90],
        [2, 40],
        [3, 10],
        [4, 5], // never polled, so no defensible runtime attribution
      ]),
      groups,
      {
        0: lane([{ start: 100, end: 110, taskId: 1 }]),
        1: lane([{ start: 50, end: 60, taskId: 2 }]),
        2: lane([
          { start: 20, end: 30, taskId: 3 },
          { start: 200, end: 210, taskId: 1 },
        ]),
      },
    );

    expect(result.taskRuntime).toEqual(
      new Map([
        [1, "main"],
        [2, "main"],
        [3, "io"],
      ]),
    );
    expect(result.byRuntime.get("main")).toEqual([40, 90]);
    expect(result.byRuntime.get("io")).toEqual([10]);
  });
});

// The "All from spawn" aggregation. The point of these tests is the missing-
// data discipline: a task with no recorded terminate must not read as a
// zero-length lifetime, and must not dilute the mean.

import { describe, expect, it } from "vitest";
import { aggregate, meanLifetimeNs } from "./task-scope-model.js";
import type { TaskIndexRow } from "./tasks-model.js";

function row(over: Partial<TaskIndexRow>): TaskIndexRow {
  return {
    taskId: 1,
    spawnLoc: "src/main.rs:10:5",
    spawnTs: 0,
    terminateTs: 100,
    lifetimeNs: 100,
    instrumented: true,
    pollCount: 1,
    totalPollNs: 10,
    longestPollNs: 10,
    firstPollStart: 0,
    firstPollWorker: 0,
    workerCount: 1,
    ...over,
  };
}

const LOC = "src/main.rs:10:5";

describe("spawn-family aggregation", () => {
  it("sums only the tasks at the location", () => {
    const stats = aggregate(
      [
        row({ taskId: 1, pollCount: 2, totalPollNs: 20, longestPollNs: 15 }),
        row({ taskId: 2, pollCount: 3, totalPollNs: 30, longestPollNs: 8 }),
        row({ taskId: 3, spawnLoc: "other.rs:1:1", pollCount: 99, totalPollNs: 990 }),
      ],
      LOC,
    );
    expect(stats.taskCount).toBe(2);
    expect(stats.pollCount).toBe(5);
    expect(stats.totalPollNs).toBe(50);
  });

  it("names the task owning the worst poll", () => {
    const stats = aggregate(
      [
        row({ taskId: 1, longestPollNs: 15 }),
        row({ taskId: 2, longestPollNs: 40 }),
        row({ taskId: 3, longestPollNs: 22 }),
      ],
      LOC,
    );
    expect(stats.longestPollNs).toBe(40);
    expect(stats.longestPollTaskId).toBe(2);
  });

  it("counts an unterminated task as running, not as a zero lifetime", () => {
    const stats = aggregate(
      [
        row({ taskId: 1, lifetimeNs: 100, terminateTs: 100 }),
        row({ taskId: 2, lifetimeNs: null, terminateTs: null }),
      ],
      LOC,
    );
    expect(stats.runningCount).toBe(1);
    expect(stats.lifetimeKnownCount).toBe(1);
    expect(stats.lifetimeSumNs).toBe(100);
    // 100, not 50: the running task is outside the denominator.
    expect(meanLifetimeNs(stats)).toBe(100);
  });

  it("reports no mean when no task has a known lifetime", () => {
    const stats = aggregate(
      [row({ taskId: 1, lifetimeNs: null, terminateTs: null })],
      LOC,
    );
    expect(meanLifetimeNs(stats)).toBeNull();
  });

  it("is empty for a location with no tasks", () => {
    const stats = aggregate([row({})], "nowhere.rs:1:1");
    expect(stats.taskCount).toBe(0);
    expect(stats.longestPollTaskId).toBeNull();
    expect(meanLifetimeNs(stats)).toBeNull();
  });
});

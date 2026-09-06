import type { LaneSpans } from "./columnar-worker-spans.js";
import type { RuntimeGroup } from "../../types/trace.js";

export interface RuntimeTaskSpawns {
  /** Runtime group name -> sorted task-spawn timestamps. */
  byRuntime: ReadonlyMap<string, readonly number[]>;
  /** Task id -> runtime group name, inferred from its earliest observed poll. */
  taskRuntime: ReadonlyMap<number, string>;
}

/**
 * Attribute task spawns to the runtime that owns each task's earliest observed
 * poll. TaskSpawnEvent predates per-event runtime identity, while poll worker
 * ownership is unambiguous and works for existing traces.
 */
export function deriveRuntimeTaskSpawns(
  taskSpawnTimes: ReadonlyMap<number, number>,
  groups: readonly RuntimeGroup[],
  workerSpans: Readonly<Record<number, LaneSpans>>,
): RuntimeTaskSpawns {
  const workerRuntime = new Map<number, string>();
  for (const group of groups) {
    for (const workerId of group.workerIds) workerRuntime.set(workerId, group.name);
  }

  const firstPoll = new Map<number, { t: number; runtime: string }>();
  for (const [workerId, runtime] of workerRuntime) {
    const polls = workerSpans[workerId]?.polls;
    if (polls === undefined) continue;
    for (let i = 0; i < polls.length; i++) {
      const poll = polls.at(i);
      if (poll?.taskId == null) continue;
      const previous = firstPoll.get(poll.taskId);
      if (previous === undefined || poll.start < previous.t) {
        firstPoll.set(poll.taskId, { t: poll.start, runtime });
      }
    }
  }

  const taskRuntime = new Map<number, string>();
  const byRuntime = new Map<string, number[]>();
  for (const [taskId, spawnTime] of taskSpawnTimes) {
    const placement = firstPoll.get(taskId);
    if (placement === undefined) continue;
    taskRuntime.set(taskId, placement.runtime);
    let times = byRuntime.get(placement.runtime);
    if (times === undefined) {
      times = [];
      byRuntime.set(placement.runtime, times);
    }
    times.push(spawnTime);
  }
  for (const times of byRuntime.values()) times.sort((a, b) => a - b);
  return { byRuntime, taskRuntime };
}

// Aggregates for the Task tab's "All from spawn" scope: what the whole family
// of tasks spawned at one location did, so the tab's numbers describe the same
// subject as its flamegraph and the lanes' tint.
//
// This reads taskIndexFor - the rail's per-task index, memoized on trace
// identity - which task-flamegraph-model deliberately must not touch (the lanes
// call into that file from a paint frame). Nothing here runs off a paint frame:
// the inspector renders from it.

import { taskIndexFor, type TaskIndexRow } from "./tasks-model.js";
import type { ParsedTrace } from "../../lib/trace/index.js";

/** What the Task tab shows for a whole spawn location. */
export interface SpawnFamilyStats {
  location: string;
  taskCount: number;
  pollCount: number;
  /** Summed poll time across the family. */
  totalPollNs: number;
  /** The single worst poll in the family, and the task that owned it. */
  longestPollNs: number;
  longestPollTaskId: number | null;
  /** Tasks with a recorded spawn and no recorded terminate. */
  runningCount: number;
  /** Tasks whose lifetime is known, and the sum over exactly those - a mean
   *  over a subset is only honest alongside its denominator. */
  lifetimeKnownCount: number;
  lifetimeSumNs: number;
}

const familyCache = new WeakMap<ParsedTrace, Map<string, SpawnFamilyStats>>();

/**
 * Aggregate every task spawned at `location`.
 *
 * Missing per-task values are counted as missing rather than folded in as
 * zero: a task with no recorded terminate is `running`, not one with a zero
 * lifetime, and it stays out of the lifetime denominator entirely.
 */
export function spawnFamilyStats(
  trace: ParsedTrace,
  location: string,
): SpawnFamilyStats {
  let byLocation = familyCache.get(trace);
  if (byLocation === undefined) {
    byLocation = new Map();
    familyCache.set(trace, byLocation);
  }
  const cached = byLocation.get(location);
  if (cached !== undefined) return cached;

  const stats = aggregate(taskIndexFor(trace).rows, location);
  byLocation.set(location, stats);
  return stats;
}

/** The pure reduction, exported for tests that build rows directly. */
export function aggregate(
  rows: readonly TaskIndexRow[],
  location: string,
): SpawnFamilyStats {
  const out: SpawnFamilyStats = {
    location,
    taskCount: 0,
    pollCount: 0,
    totalPollNs: 0,
    longestPollNs: 0,
    longestPollTaskId: null,
    runningCount: 0,
    lifetimeKnownCount: 0,
    lifetimeSumNs: 0,
  };
  for (const row of rows) {
    if (row.spawnLoc !== location) continue;
    out.taskCount += 1;
    out.pollCount += row.pollCount;
    out.totalPollNs += row.totalPollNs;
    if (row.longestPollNs > out.longestPollNs) {
      out.longestPollNs = row.longestPollNs;
      out.longestPollTaskId = row.taskId;
    }
    if (row.lifetimeNs != null) {
      out.lifetimeKnownCount += 1;
      out.lifetimeSumNs += row.lifetimeNs;
    } else if (row.terminateTs == null) {
      out.runningCount += 1;
    }
  }
  return out;
}

/** Mean lifetime over the tasks that have one, or null when none do. */
export function meanLifetimeNs(stats: SpawnFamilyStats): number | null {
  if (stats.lifetimeKnownCount === 0) return null;
  return stats.lifetimeSumNs / stats.lifetimeKnownCount;
}

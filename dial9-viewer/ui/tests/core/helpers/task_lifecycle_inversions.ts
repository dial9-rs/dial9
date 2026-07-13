// Shared test helper: the task-lifecycle inversion classifier.
//
// This function lived in test_trace_integrity.js (module.exports'd so
// test_task_lifecycle.js could unit-test it). Under Vitest a test file must
// not import another test file (importing would re-register its suites), so
// the helper moved here (T11); trace_integrity.test.ts and
// task_lifecycle.test.ts both import it.

export interface LifecycleInversion {
  taskId: number;
  delta: number;
}

/**
 * Find tasks whose terminate timestamp precedes their spawn timestamp.
 *
 * `TaskSpawnEvent` and `TaskTerminateEvent` timestamps are independent
 * `CLOCK_MONOTONIC` reads taken on whichever worker thread happened to run the
 * spawn versus the task's completion. `CLOCK_MONOTONIC` is guaranteed monotonic
 * per-CPU, but on virtualized hosts (e.g. CI runners) the clock can skew by a
 * small amount across cores. A short-lived task that spawns on one core and
 * finishes on another can therefore record a terminate timestamp a few hundred
 * nanoseconds *before* its spawn timestamp. That is a clock artifact, not data
 * corruption — and it matches the trace's own per-worker (not global)
 * monotonicity guarantee.
 *
 * Genuine corruption (decoder desync, wrong task association) inverts by the
 * gap between unrelated tasks — on the order of the whole trace (seconds) and
 * usually across many tasks at once. Such inversions exceed `toleranceNs` and
 * are returned in `gross`; sub-tolerance inversions are returned in `tolerated`.
 *
 * @param taskSpawnTimes  taskId -> spawn ts (ns)
 * @param taskTerminateTimes  taskId -> terminate ts (ns)
 * @param toleranceNs max skew treated as a clock artifact
 *   (1 ms — ~1000x larger than cross-core skew observed on real hardware, and
 *   ~1000x smaller than the seconds-scale inversions real corruption produces)
 */
export function findTaskLifecycleInversions(
  taskSpawnTimes: Map<number, number>,
  taskTerminateTimes: Map<number, number>,
  toleranceNs = 1_000_000,
): { tolerated: LifecycleInversion[]; gross: LifecycleInversion[] } {
  const tolerated: LifecycleInversion[] = [];
  const gross: LifecycleInversion[] = [];
  for (const [taskId, spawnTime] of taskSpawnTimes) {
    const termTime = taskTerminateTimes.get(taskId);
    if (termTime !== undefined && termTime < spawnTime) {
      const delta = spawnTime - termTime;
      if (delta > toleranceNs) gross.push({ taskId, delta });
      else tolerated.push({ taskId, delta });
    }
  }
  return { tolerated, gross };
}

// Pure derivations for the Task tab's flamegraph: which CPU samples belong to
// the selected task, and which belong to every task sharing its spawn location.
// No store, no DOM - inspector.ts owns the widget lifecycle and renders from
// these.
//
// The two scopes read DIFFERENT sources on purpose:
//
//   - "task" walks the selected task's own polls, whose `cpuSamples` the lane
//     reconstruction already attached. The Task tab holds those polls anyway,
//     so this costs nothing beyond a concat.
//
//   - "spawn-location" reads `sample.spawnLoc`, which `attachCpuSamples` stamps
//     on every sample from the poll it landed in. Walking the lanes instead
//     would materialize a poll flyweight per poll in the trace - millions on a
//     large one - to reach data the samples already carry.
//
// Everything here reads the trace's own task maps. Nothing reaches for
// taskIndexFor: that builds the whole task index (worker spans + per-task
// aggregates), and the lanes call into this file from a paint frame.

import { isFoldableCpuSample } from "./region-analysis-model.js";
import type { CpuSample, ParsedTrace, PollSpan } from "../../lib/trace/index.js";

/**
 * The pinned spawn location, or null for none. This is the WHOLE scope state:
 * a location, not a mode. Deriving the family from "a mode plus whatever task
 * is selected" made the subject swap silently when the selection moved, and
 * made the rail's list depend on a member of that same list. A pinned string
 * has neither problem - every surface answers "is this task in the family?"
 * with a string compare, and nothing has to be kept in sync.
 */
export type SpawnPin = string | null;

/**
 * Validate a pin arriving from a URL. Spawn locations are opaque strings from
 * the trace, so anything non-empty is structurally valid; a pin naming a
 * location the trace does not have simply resolves to an empty family.
 */
export function parseSpawnPin(value: string | null): SpawnPin {
  if (value === null) return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
}

/** What the Task tab renders for one (task, pin) pair. */
export interface TaskFlamegraphView {
  /** True when the view folds the whole pinned family rather than one task. */
  isFamily: boolean;
  /** Foldable on-CPU samples, in trace order. Empty means nothing to draw. */
  samples: readonly CpuSample[];
  /** How many tasks contributed - 1 for a single task, the pinned location's
   *  whole task count for a family. */
  taskCount: number;
  /** Title for the flamegraph's export/header. */
  title: string;
}

const EMPTY_SAMPLES: readonly CpuSample[] = [];

/** The selected task's own on-CPU samples, gathered from its polls. */
export function taskCpuSamples(polls: readonly PollSpan[]): CpuSample[] {
  const out: CpuSample[] = [];
  for (const poll of polls) {
    const samples = poll.cpuSamples;
    if (samples === undefined) continue;
    for (const s of samples) {
      if (isFoldableCpuSample(s)) out.push(s);
    }
  }
  return out;
}

/**
 * Every on-CPU sample taken inside a poll of a task spawned at `location`.
 *
 * `attachCpuSamples` writes each sample's `spawnLoc` from its enclosing poll's
 * RESOLVED location (trace_analysis.js: `sample.spawnLoc = poll.spawnLoc`), not
 * from an id - so this compares strings directly and stays correct whatever
 * `spawnLocations` keys its entries by. Samples outside any poll carry a null
 * `spawnLoc` and are skipped.
 */
export function spawnLocationCpuSamples(
  trace: ParsedTrace,
  location: string,
): CpuSample[] {
  const out: CpuSample[] = [];
  for (const s of trace.cpuSamples) {
    if (s.spawnLoc !== location) continue;
    if (isFoldableCpuSample(s)) out.push(s);
  }
  return out;
}

/** How many tasks in the trace were spawned at `location`. */
export function tasksAtSpawnLocation(trace: ParsedTrace, location: string): number {
  return taskIdsAtSpawnLocation(trace, location).size;
}

/**
 * The spawn location recorded for a task, or null when the trace has none.
 *
 * Reads the task maps rather than a poll's `spawnLocId`, so a caller that has
 * only a task id (the lanes) resolves the SAME string the Task tab shows. The
 * single seam for that resolve: wakerLabelFor and taskIndexFor call it too.
 */
export function spawnLocationOf(
  trace: Pick<ParsedTrace, "taskSpawnLocs" | "spawnLocations">,
  taskId: number,
): string | null {
  const locId = trace.taskSpawnLocs.get(taskId);
  if (locId == null) return null;
  return trace.spawnLocations.get(locId) ?? null;
}

const EMPTY_TASK_IDS: ReadonlySet<number> = new Set();
const scopeSetCache = new WeakMap<ParsedTrace, Map<string, ReadonlySet<number>>>();

/**
 * Every task in the trace spawned at `location`.
 *
 * Scans `taskSpawnLocs`, which the parser fills from BOTH TaskSpawn and the
 * first PollStart of a task - the same union taskIndexFor's rows cover - so this
 * is the whole sibling set without building that index. Memoized per (trace,
 * location): the lanes ask for this on every frame.
 */
export function taskIdsAtSpawnLocation(
  trace: ParsedTrace,
  location: string,
): ReadonlySet<number> {
  let byLocation = scopeSetCache.get(trace);
  if (byLocation === undefined) {
    byLocation = new Map();
    scopeSetCache.set(trace, byLocation);
  }
  const cached = byLocation.get(location);
  if (cached !== undefined) return cached;
  const ids = new Set<number>();
  for (const taskId of trace.taskSpawnLocs.keys()) {
    if (spawnLocationOf(trace, taskId) === location) ids.add(taskId);
  }
  byLocation.set(location, ids);
  return ids;
}

/**
 * The family the lanes tint: every task at the pinned location. Empty for no
 * pin, which must render as no tint rather than as an arbitrary group.
 *
 * Takes only the pin, never the selection: the tint is a property of what is
 * pinned, so moving the selection cannot silently re-target it.
 */
export function spawnScopeTaskIds(
  trace: ParsedTrace | null,
  pin: SpawnPin,
): ReadonlySet<number> {
  if (pin === null || trace === null) return EMPTY_TASK_IDS;
  return taskIdsAtSpawnLocation(trace, pin);
}

/** Whether `taskId` was spawned at the pinned location. */
export function isInPinnedFamily(
  trace: ParsedTrace | null,
  taskId: number | null,
  pin: SpawnPin,
): boolean {
  if (pin === null || trace === null || taskId === null) return false;
  return spawnLocationOf(trace, taskId) === pin;
}

/**
 * Build the view for one (task, pin) pair. `polls` are the selected task's own
 * polls (the Task tab's derivation).
 *
 * The family view is used only when the selected task is IN the pinned family.
 * Selecting a task from elsewhere while a pin is held leaves the tab describing
 * that task - the pin still governs the rail and the lane tint, but this panel
 * is titled with the selected task and must not show another group's numbers.
 */
export function buildTaskFlamegraphView(
  trace: ParsedTrace | null,
  taskId: number | null,
  polls: readonly PollSpan[],
  pin: SpawnPin,
): TaskFlamegraphView {
  if (trace === null || taskId === null) {
    return { isFamily: false, samples: EMPTY_SAMPLES, taskCount: 0, title: "" };
  }
  const hexId = `0x${taskId.toString(16)}`;
  if (isInPinnedFamily(trace, taskId, pin) && pin !== null) {
    return {
      isFamily: true,
      samples: spawnLocationCpuSamples(trace, pin),
      taskCount: tasksAtSpawnLocation(trace, pin),
      title: `CPU - all tasks from ${pin}`,
    };
  }
  return {
    isFamily: false,
    samples: taskCpuSamples(polls),
    taskCount: 1,
    title: `CPU - task ${hexId}`,
  };
}

/**
 * A signature that changes exactly when the folded tree would. The sample
 * arrays are rebuilt on every derive, so identity is useless here; the inputs
 * that determine the tree are.
 */
export function taskFlamegraphCacheSignature(args: {
  traceId: number;
  taskId: number | null;
  isFamily: boolean;
  pin: SpawnPin;
  sampleCount: number;
}): string {
  const subject = args.isFamily ? (args.pin ?? "-") : String(args.taskId);
  return `${args.traceId}|${args.isFamily ? "family" : "task"}|${subject}|${args.sampleCount}`;
}

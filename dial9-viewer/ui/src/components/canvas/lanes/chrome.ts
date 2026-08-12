// Lane "chrome": the per-runtime accent palette and the in-lane geometry both
// lane kinds draw against — worker lanes (render.ts) and the per-runtime summary
// lane (runtime-metrics-lane.ts).
//
// Its own module rather than one of theirs because both need it and
// render.ts already imports the summary lane's draw function: putting the shared
// constants in either would make the pair mutually importing.

/**
 * Per-runtime accent colours, assigned by group render order and wrapping past
 * the end. Every lane belonging to a runtime carries its accent as a left rail,
 * continuous with the same rail on its gutter row, so "which runtime is this
 * lane in" is answerable without scrolling back to the group header.
 *
 * Deliberately disjoint from the lanes' SEMANTIC colours (poll heat, park
 * #cc5533, sched #ff0000, wake #66bb6a, CPU #ce93d8, selection #ffeb3b, waker
 * #ff8a65, local queue rgba(255,200,50)): an accent must never read as a mark.
 */
export const RUNTIME_ACCENTS: readonly string[] = [
  "#5c9dff",
  "#c792ea",
  "#4db6ac",
  "#d98ba0",
  "#8fbc5a",
  "#a0a8d0",
];

/** Accent for the group at `index` in render order (wraps). */
export function runtimeAccent(index: number): string {
  return RUNTIME_ACCENTS[index % RUNTIME_ACCENTS.length]!;
}

/** Accent for a lane whose runtime has no derived accent (unit tests and the
 *  headerless single-group fallback both hit this). */
export const DEFAULT_ACCENT: string = RUNTIME_ACCENTS[0]!;

/**
 * Width of the runtime accent rail at a lane's left edge (CSS px). ALSO the left
 * inset every in-lane label uses, so a label is never hidden under the rail.
 */
export const RAIL_W = 3;

/** How a worker lane identifies itself in the DRAW AREA: its runtime's accent,
 *  painted as the lane's left rail. The lane's NAME lives in the label gutter
 *  (labels.ts), which is the sole producer of the "W<id>" string. */
export interface LaneIdentity {
  /** The owning runtime's accent colour (the lane's left rail). */
  accent: string;
}

/** The lane-identity maps for one trace's runtime groups, all keyed off group
 *  render order: per worker (accent), per group name (accent), and each worker's
 *  owning group name (so the gutter can find a worker row's accent). */
export interface LaneIdentities {
  byWorker: ReadonlyMap<number, LaneIdentity>;
  byRuntime: ReadonlyMap<string, string>;
  workerRuntime: ReadonlyMap<number, string>;
}

/**
 * Build the lane-identity maps from the runtime groups in render order: every
 * worker maps to its group's accent and its group's name, and every group name
 * maps to that accent. Derived once per trace (lanes/data.ts) — never per frame.
 */
export function buildLaneIdentities(
  groups: readonly { name: string; workerIds: readonly number[] }[],
): LaneIdentities {
  const byWorker = new Map<number, LaneIdentity>();
  const byRuntime = new Map<string, string>();
  const workerRuntime = new Map<number, string>();
  groups.forEach((g, i) => {
    const accent = runtimeAccent(i);
    byRuntime.set(g.name, accent);
    for (const w of g.workerIds) {
      byWorker.set(w, { accent });
      workerRuntime.set(w, g.name);
    }
  });
  return { byWorker, byRuntime, workerRuntime };
}

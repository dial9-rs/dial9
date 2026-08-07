// Load-path performance instrumentation: the User Timing marks/measures behind
// the `loadPerf` records this module's callers have long referenced but never
// had. Off by default and no-op when off, because every hook here sits on the
// parse critical path (loadTraceOnMainThread parses on the page's main thread,
// so an unguarded measure would itself be part of what it measures).
//
// Enable with `?perf=1` on the viewer URL, or persistently with
// `localStorage["dial9.viewer.perf"] = "1"`. Marks land in the DevTools
// Performance timeline alongside the browser's own; the console summary is a
// convenience for A/B runs where opening a profile is overkill.
//
// Phases, in order:
//   fetch   - network + gunzip (buffered mode only; streaming overlaps it)
//   parse   - decoder wall time, INCLUDING the paint-throttle yields
//   derive  - store update -> first painted frame over the new trace, i.e. the
//             post-parse derivation everything defers into one frame
//   total   - load start -> first painted frame

const PERF_STORAGE_KEY = "dial9.viewer.perf";
const PREFIX = "d9:load";

export type LoadPhase = "start" | "fetch-done" | "parse-done" | "store-updated" | "first-paint";

/** A single load's recorder. Every method is a no-op on a disabled recorder,
 *  so callers never branch at the call site. */
export interface LoadPerfRecorder {
  readonly enabled: boolean;
  mark(phase: LoadPhase): void;
  /** Close the run: emit the phase measures and, when enabled, log a summary.
   *  `stats` is the load's own tally (events/bytes/mode) for the log line. */
  finish(stats?: LoadPerfStats): void;
}

export interface LoadPerfStats {
  mode?: string;
  events?: number;
  bytes?: number;
  urlCount?: number;
}

/** Minimal slice of the User Timing API, injectable for tests. */
export interface PerfLike {
  mark(name: string): unknown;
  measure(name: string, start: string, end: string): unknown;
  clearMarks(name?: string): void;
  clearMeasures(name?: string): void;
}

export interface LoadPerfOptions {
  /** Force enable/disable, bypassing URL + localStorage detection. */
  enabled?: boolean;
  perf?: PerfLike | undefined;
  /** Where the summary goes; defaults to console.info. */
  log?: (message: string) => void;
}

const NOOP: LoadPerfRecorder = {
  enabled: false,
  mark: () => {},
  finish: () => {},
};

/**
 * Whether load instrumentation is on. Reads `?perf=1` first so a shared link
 * can turn it on for one session without touching storage. Both lookups are
 * wrapped: `localStorage` throws outright in some privacy modes, and there is
 * no `window` under Vitest/Node.
 */
export function isLoadPerfEnabled(search?: string): boolean {
  const query = search ?? (typeof location !== "undefined" ? location.search : "");
  if (query.length > 0) {
    const flag = new URLSearchParams(query).get("perf");
    if (flag === "1" || flag === "true") return true;
  }
  try {
    return globalThis.localStorage?.getItem(PERF_STORAGE_KEY) === "1";
  } catch {
    // Storage blocked (private mode / third-party context): treat as off.
    return false;
  }
}

/**
 * Time `fn` under a named `performance.measure` when load instrumentation is on,
 * else just call it. Used to break the single "derive" bar into attributable
 * sub-spans (reconstruction, detectors, span data) without threading a recorder
 * through the memoized derivations. The measure is logged to the console so an
 * A/B run reads without opening a profile.
 */
export function measureSpan<T>(label: string, fn: () => T): T {
  if (!isLoadPerfEnabled() || typeof performance === "undefined") return fn();
  const start = performance.now();
  try {
    return fn();
  } finally {
    const ms = performance.now() - start;
    performance.measure(`d9:derive ${label}`, { start, duration: ms });
    // eslint-disable-next-line no-console
    console.info(`[dial9 derive] ${label} ${ms.toFixed(0)}ms`);
  }
}

// Distinguishes concurrent or superseded loads, whose marks would otherwise
// collide on the same names and produce nonsense measures.
let runSeq = 0;

/**
 * Begin recording one load. Returns a no-op recorder unless instrumentation is
 * enabled, so the caller pays a dead method call and nothing else.
 */
export function startLoadPerf(opts: LoadPerfOptions = {}): LoadPerfRecorder {
  const enabled = opts.enabled ?? isLoadPerfEnabled();
  if (!enabled) return NOOP;

  const perf = opts.perf ?? (typeof performance !== "undefined" ? performance : undefined);
  if (perf === undefined) return NOOP;

  const log = opts.log ?? ((m: string) => console.info(m));
  const run = `${PREFIX}#${++runSeq}`;
  const seen = new Set<LoadPhase>();
  const name = (phase: LoadPhase): string => `${run}:${phase}`;

  return {
    enabled: true,
    mark(phase: LoadPhase): void {
      if (seen.has(phase)) return; // first-paint can be signalled more than once
      seen.add(phase);
      perf.mark(name(phase));
    },
    finish(stats: LoadPerfStats = {}): void {
      // Streaming mode never marks fetch-done (fetch and parse overlap), so
      // each span is emitted only when both of its endpoints exist.
      const span = (label: string, from: LoadPhase, to: LoadPhase): number | null => {
        if (!seen.has(from) || !seen.has(to)) return null;
        const entry = perf.measure(`${run} ${label}`, name(from), name(to)) as
          | { duration?: number }
          | undefined;
        return typeof entry?.duration === "number" ? entry.duration : null;
      };

      const parts: string[] = [];
      const push = (label: string, ms: number | null): void => {
        if (ms !== null) parts.push(`${label} ${ms.toFixed(0)}ms`);
      };
      push("fetch", span("fetch", "start", "fetch-done"));
      push("parse", span("parse", seen.has("fetch-done") ? "fetch-done" : "start", "parse-done"));
      push("derive", span("derive", "store-updated", "first-paint"));
      const total = span("total", "start", seen.has("first-paint") ? "first-paint" : "parse-done");
      push("total", total);

      const tally: string[] = [];
      if (stats.mode !== undefined) tally.push(stats.mode);
      if (stats.urlCount !== undefined) tally.push(`${stats.urlCount} file(s)`);
      if (stats.events !== undefined) tally.push(`${stats.events.toLocaleString()} events`);
      if (stats.bytes !== undefined) tally.push(`${(stats.bytes / 1e6).toFixed(1)} MB`);
      if (total !== null && stats.events !== undefined && total > 0) {
        tally.push(`${Math.round(stats.events / (total / 1000)).toLocaleString()} ev/s`);
      }

      log(`[dial9 loadPerf] ${parts.join("  ")}${tally.length > 0 ? `  (${tally.join(", ")})` : ""}`);

      for (const phase of seen) perf.clearMarks(name(phase));
      for (const label of ["fetch", "parse", "derive", "total"]) {
        perf.clearMeasures(`${run} ${label}`);
      }
    },
  };
}

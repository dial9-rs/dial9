// The points-of-interest model behind the issues rail: the pure, Node-testable
// data layer. The POIs come only from the frozen `filterPointsOfInterest`
// detector set - no new detector is built here. The same output feeds the
// minimap ticks independently.
//
// The detectors run over the RESIDENT `trace` slice. Whole-trace loads make
// the POI set complete; when segment windowing feeds a partial trace, the
// count is over the resident window only - consumers must not present it as
// whole-trace truth.

import {
  EVENT_TYPES,
  filterPointsOfInterest,
  formatHumanDuration,
} from "../../lib/trace/index.js";
import { sharedDetectorInputs } from "../../lib/trace/derived.js";
import type { LaneSource } from "../../lib/trace/columnar-worker-spans.js";
import type {
  ParsedTrace,
  ParkSpan,
  PointOfInterest,
  PointOfInterestType,
  PollSpan,
  SchedDelay,
} from "../../types/trace.js";
import type { PoiSlice, PoiSortKey, ViewportSlice } from "../../types/state.js";

/** The five detector filters, in `#poi-filter` option order. */
export const POI_FILTERS: readonly PointOfInterestType[] = [
  "sched",
  "long-poll",
  "cpu-sampled",
  "wake-delay",
  "uninstrumented",
];

/** Validate a filter arriving from the DOM (a `<select>` value is just a
 * string) before it reaches the store and the detectors. */
export function parsePoiFilter(value: string): PointOfInterestType | null {
  return (POI_FILTERS as readonly string[]).includes(value)
    ? (value as PointOfInterestType)
    : null;
}

/** The full-length option label for the filter dropdown. */
export function filterLabel(type: PointOfInterestType): string {
  switch (type) {
    case "sched":
      return "Kernel Scheduling Delays";
    case "long-poll":
      return "Long Polls (>1ms)";
    case "cpu-sampled":
      return "Polls with CPU Samples";
    case "wake-delay":
      return "Wake->Poll Delays (>100us)";
    case "uninstrumented":
      return "Uninstrumented Polls";
  }
}

/** The short "what" label shown in a rail row + the red-flags chip. */
export function kindLabel(type: PointOfInterestType): string {
  switch (type) {
    case "sched":
      return "sched delay";
    case "long-poll":
      return "long poll";
    case "cpu-sampled":
      return "cpu-sampled poll";
    case "wake-delay":
      return "wake delay";
    case "uninstrumented":
      return "uninstrumented poll";
  }
}

// ── Heavy source, memoized on trace identity ─────────────────────────────

/**
 * The frame-invariant inputs the detectors need, derived once per loaded
 * trace: reconstructed worker spans (CPU samples attached, so the
 * "cpu-sampled" filter sees them), the worker id set, the scheduling delays
 * (the "wake-delay" detector's input), and the two trace-level flags the
 * detector reads. This is the expensive part (buildWorkerSpans scans every
 * event), so it is memoized on the `ParsedTrace` identity - a pan/zoom/sort
 * never rebuilds it, only a genuine load/reparse (new trace object) does.
 */
export interface PoiSource {
  workerIds: number[];
  /** Which representation the detectors run against: the columnar store scans
   * raw columns, the fat path takes the frozen filterPointsOfInterest. */
  lanes: LaneSource;
  schedDelays: SchedDelay[];
  hasSchedWait: boolean;
  taskInstrumented: Map<number, boolean>;
  /** Lazy per-filter detector output cache (keyed by filter type). The list is
   *  capped at POI_DETECTOR_LIMIT; `matched` is the true pre-cap count. */
  readonly _byFilter: Map<PointOfInterestType, { list: PointOfInterest[]; matched: number }>;
}

/**
 * Ceiling on how many points a single detector materializes.
 *
 * "Uninstrumented Polls" matches EVERY poll of an uninstrumented task, which on
 * a lightly-instrumented 13M-event trace is millions - enough that building the
 * list (and then a formatted row per entry) exhausts the tab before anything
 * renders. Detectors run with `sortByWorst`, so the cap keeps the worst N,
 * which is the part anyone acts on. The true count is reported separately and
 * is what the rail displays.
 */
export const POI_DETECTOR_LIMIT = 50_000;

const sourceCache = new WeakMap<ParsedTrace, PoiSource>();

/** The worker id SET the detectors iterate: every worker seen on a non-queue,
 *  non-wake event, sorted ascending. Runtime-group reordering is order-only
 *  and never changes the count, so it is skipped here (the rail applies its
 *  own display sort). */

/** The memoized POI source for a trace (built once per loaded trace). */
export function poiSourceFor(trace: ParsedTrace): PoiSource {
  let source = sourceCache.get(trace);
  if (source !== undefined) return source;

  // Shared with the minimap ticks: same worker set, same lane source, same
  // scheduling delays, computed once per trace.
  const { workerIds, lanes, schedDelays } = sharedDetectorInputs(trace);

  source = {
    workerIds,
    lanes,
    schedDelays,
    hasSchedWait: trace.hasSchedWait,
    taskInstrumented: trace.taskInstrumented,
    _byFilter: new Map(),
  };
  sourceCache.set(trace, source);
  return source;
}

/**
 * The detector output for one filter, memoized per source. Calls
 * `filterPointsOfInterest` with `sortByWorst: true` so the base order is
 * worst-first; the rail applies its own display sort on top, which never
 * changes the COUNT.
 */
function detectorResult(
  source: PoiSource,
  filter: PointOfInterestType,
): { list: PointOfInterest[]; matched: number } {
  const cached = source._byFilter.get(filter);
  if (cached !== undefined) return cached;
  let matched = -1;
  const opts = {
    hasSchedWait: source.hasSchedWait,
    sortByWorst: true,
    taskInstrumented: source.taskInstrumented,
    limit: POI_DETECTOR_LIMIT,
    onTotal: (n: number) => {
      matched = n;
    },
  };
  const list = source.lanes.columnar
    ? source.lanes.store.pointsOfInterest(filter, source.workerIds, source.schedDelays, opts)
    : filterPointsOfInterest(
        filter, source.lanes.workerSpans, source.workerIds, source.schedDelays, opts,
      );
  // The frozen fat-path detector honours neither `limit` nor `onTotal`, so its
  // result is already complete and its length IS the true count.
  const result = { list, matched: matched >= 0 ? matched : list.length };
  source._byFilter.set(filter, result);
  return result;
}

export function poisForFilter(
  source: PoiSource,
  filter: PointOfInterestType,
): PointOfInterest[] {
  return detectorResult(source, filter).list;
}

/**
 * The TRUE number of points a detector matched, which is >= the length of
 * `poisForFilter` once POI_DETECTOR_LIMIT truncates. Counts shown to the user
 * come from here so a capped list never understates how many issues exist.
 */
export function poiMatchCount(
  source: PoiSource,
  filter: PointOfInterestType,
): number {
  return detectorResult(source, filter).matched;
}

/** Per-detector counts for the red-flags summary chip. Zero-count detectors
 *  are included; the chip filters them out. */
export function redFlagCounts(
  source: PoiSource,
): { type: PointOfInterestType; count: number }[] {
  return POI_FILTERS.map((type) => ({
    type,
    count: poiMatchCount(source, type),
  }));
}

// ── Display sort (the four sortable columns) ─────────────────────────────

/**
 * A stable client-side sort of a filtered POI list by the chosen column. The
 * list from `poisForFilter` is one detector type, so `value`s share units and
 * the "duration" column is comparable. Returns a NEW array (never mutates the
 * memoized detector list). Ties fall back to time ascending for determinism.
 */
export function sortPois(
  pois: readonly PointOfInterest[],
  sortKey: PoiSortKey,
  sortDir: "asc" | "desc",
): PointOfInterest[] {
  const dir = sortDir === "asc" ? 1 : -1;
  const keyed = pois.map((poi, i) => ({ poi, i }));
  keyed.sort((a, b) => {
    const cmp = compareBy(a.poi, b.poi, sortKey);
    if (cmp !== 0) return cmp * dir;
    // Stable tiebreak: original (worst-first) order, so equal rows never jitter.
    return a.i - b.i;
  });
  return keyed.map((k) => k.poi);
}

function compareBy(
  a: PointOfInterest,
  b: PointOfInterest,
  key: PoiSortKey,
): number {
  switch (key) {
    case "worker":
      return a.worker - b.worker;
    case "time":
      return a.time - b.time;
    case "duration":
      return a.value - b.value;
    case "kind":
      return kindLabel(a.type).localeCompare(kindLabel(b.type));
  }
}

// ── Row formatting (the rail's four columns + the severity dot) ─────────

/** `W{n}` worker chip label. */
export function workerLabel(worker: number): string {
  return `W${worker}`;
}

/** `+1.72s` relative-offset time label (the rail's `t` column). */
export function relTimeLabel(ns: number, minTs: number): string {
  return `+${((ns - minTs) / 1e9).toFixed(2)}s`;
}

/** The POI's severity value converted to nanoseconds (per detector units).
 *  `filterPointsOfInterest` stores `value` in different units per type
 *  (sched: ns schedWait; long-poll/cpu-sampled/uninstrumented: ms; wake-delay:
 *  us) - normalize to ns so `formatHumanDuration` reads them uniformly. */
export function valueNs(poi: PointOfInterest): number {
  switch (poi.type) {
    case "sched":
      return poi.value; // schedWait, already ns
    case "wake-delay":
      return poi.value * 1e3; // us -> ns
    case "long-poll":
    case "cpu-sampled":
    case "uninstrumented":
      return poi.value * 1e6; // ms -> ns
  }
}

/** The rail's `dur` column label (human duration of the severity value). */
export function durationLabel(poi: PointOfInterest): string {
  return formatHumanDuration(valueNs(poi));
}

/** Severity tier for the row dot: the value as a fraction of the list's peak.
 *  Stable regardless of display sort. */
export type PoiSeverity = "high" | "med" | "low";

export function severityOf(poi: PointOfInterest, maxValue: number): PoiSeverity {
  if (!(maxValue > 0)) return "low";
  const ratio = poi.value / maxValue;
  if (ratio >= 0.66) return "high";
  if (ratio >= 0.33) return "med";
  return "low";
}

/** The peak severity value in a list (for `severityOf`); 0 when empty. */
export function peakValue(pois: readonly PointOfInterest[]): number {
  let max = 0;
  for (const p of pois) if (p.value > max) max = p.value;
  return max;
}

// ── Jump semantics ───────────────────────────────────────────────────────

/** The viewport window + optional task selection a POI jump produces. */
export interface PoiJump {
  viewStart: number;
  viewEnd: number;
  /** Task to select; null when the POI resolves to none. */
  selectedTaskId: number | null;
}

/**
 * Center the viewport on a POI: show ~5x the span duration (min 1ms) with 30%
 * left padding, clamped to [minTs, maxTs]. Wake-delay POIs instead frame the
 * full wake->poll window (~3x, 20% pad) and select the delayed task. Other
 * POIs select the poll's task when it has one, so the inspector and lane
 * highlight follow the jump.
 */
export function poiJump(poi: PointOfInterest, vp: ViewportSlice): PoiJump {
  const { minTs, maxTs } = vp;
  const spanDur = poi.span.end - poi.span.start;
  const viewDur = Math.max(spanDur * 5, 1e6);
  let viewStart = Math.max(minTs, poi.time - viewDur * 0.3);
  let viewEnd = Math.min(maxTs, viewStart + viewDur);
  let selectedTaskId: number | null = pollTaskId(poi.span);

  if (poi.schedDelay) {
    const sd = poi.schedDelay;
    const totalDur = sd.poll.end - sd.wakeTime;
    const padded = Math.max(totalDur * 3, 1e6);
    viewStart = Math.max(minTs, sd.wakeTime - padded * 0.2);
    viewEnd = Math.min(maxTs, viewStart + padded);
    selectedTaskId = sd.taskId;
  }

  return { viewStart, viewEnd, selectedTaskId };
}

/** The task id of a POI's span when it is a poll (ParkSpans have none). */
function pollTaskId(span: PollSpan | ParkSpan): number | null {
  if ("taskId" in span && typeof span.taskId === "number") return span.taskId;
  return null;
}

// ── Stepping ─────────────────────────────────────────────────────────────

/**
 * The index the `n`/`p` keys / Prev/Next buttons move to. With nothing selected
 * (index -1) and a non-empty list, both directions land on 0; otherwise step
 * by `dir`, clamped to the list. Returns -1 for an empty list (nothing to
 * select).
 */
export function stepIndex(len: number, current: number, dir: 1 | -1): number {
  if (len === 0) return -1;
  if (current < 0) return 0;
  const next = current + dir;
  if (next < 0) return 0;
  if (next >= len) return len - 1;
  return next;
}

// ── The rail's per-render view model (derived from state) ────────────────

/** One rendered rail row. */
export interface PoiRow {
  poi: PointOfInterest;
  worker: string;
  kind: string;
  time: string;
  duration: string;
  severity: PoiSeverity;
}

/** The whole rail's derived view model for one render pass. */
export interface PoiViewModel {
  filter: PointOfInterestType;
  sortKey: PoiSortKey;
  sortDir: "asc" | "desc";
  index: number;
  /** The rendered slice, at most RAIL_WINDOW long - NOT the whole list. Row i
   *  here is absolute index `windowStart + i`. */
  rows: PoiRow[];
  /** Absolute index of `rows[0]`. */
  windowStart: number;
  /**
   * The full retained list in display order. `n`/`p` step across ALL of it, not
   * just the formatted window, so navigation needs the entries themselves;
   * bounded by POI_DETECTOR_LIMIT, so holding it is cheap.
   */
  sorted: readonly PointOfInterest[];
  /** Total count (the "N/total" position); the TRUE detector match count, which
   *  can exceed both `rows.length` and POI_DETECTOR_LIMIT. */
  total: number;
  /** How many points the detector actually retained. Below `total` when the
   *  detector cap truncated; the rail says so rather than silently showing
   *  fewer than it claims. */
  retained: number;
  /** Per-detector counts for the red-flags summary chip. */
  redFlags: { type: PointOfInterestType; count: number }[];
}

/**
 * How many rows the rail builds around the current index. Each row carries five
 * pre-formatted strings, so mapping a whole detector list is itself enough to
 * kill the tab - the window has to be applied BEFORE the row mapping, not at
 * render time.
 */
export const RAIL_WINDOW = 500;

/** The slice of `[0, total)` to materialize so `index` is inside it, biased to
 *  show context on both sides and clamped at either end of the list. */
export function railWindow(total: number, index: number): { start: number; end: number } {
  if (total <= RAIL_WINDOW) return { start: 0, end: total };
  const anchor = index < 0 ? 0 : index;
  const start = Math.max(0, Math.min(anchor - Math.floor(RAIL_WINDOW / 2), total - RAIL_WINDOW));
  return { start, end: start + RAIL_WINDOW };
}

/**
 * Build the rail's view model from a trace + the POI slice + the viewport's
 * `minTs` (for the relative time labels). The sorted list is what the rail
 * renders AND what `n`/`p` step through, so the current index refers to this
 * list. With no trace this is an empty model.
 */
export function derivePoiViewModel(
  trace: ParsedTrace | null,
  poi: PoiSlice,
  minTs: number,
): PoiViewModel {
  if (trace === null) {
    return {
      filter: poi.filter,
      sortKey: poi.sortKey,
      sortDir: poi.sortDir,
      index: -1,
      rows: [],
      windowStart: 0,
      sorted: [],
      total: 0,
      retained: 0,
      redFlags: [],
    };
  }
  const source = poiSourceFor(trace);
  const filtered = poisForFilter(source, poi.filter);
  const sorted = sortPois(filtered, poi.sortKey, poi.sortDir);
  const peak = peakValue(sorted);
  const index = poi.index < sorted.length ? poi.index : -1;
  // Format only the visible window. `peak` is computed over the whole retained
  // list, so severity shading stays consistent as the window moves.
  const { start, end } = railWindow(sorted.length, index);
  const rows: PoiRow[] = [];
  for (let i = start; i < end; i++) {
    const p = sorted[i]!;
    rows.push({
      poi: p,
      worker: workerLabel(p.worker),
      kind: kindLabel(p.type),
      time: relTimeLabel(p.time, minTs),
      duration: durationLabel(p),
      severity: severityOf(p, peak),
    });
  }
  return {
    filter: poi.filter,
    sortKey: poi.sortKey,
    sortDir: poi.sortDir,
    index,
    rows,
    windowStart: start,
    sorted,
    total: poiMatchCount(source, poi.filter),
    retained: sorted.length,
    redFlags: redFlagCounts(source).filter((r) => r.count > 0),
  };
}

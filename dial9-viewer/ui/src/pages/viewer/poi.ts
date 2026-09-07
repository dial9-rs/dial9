// The points-of-interest model behind the issues rail: the pure, Node-testable
// data layer. The POIs come only from the frozen `filterPointsOfInterest`
// detector set - no new detector is built here. The same output feeds the
// minimap ticks independently.
//
// The detectors RANK by severity rather than applying a fixed cutoff, so the
// rail shows the worst N of a kind (`poi.worstN`) and reports the true match
// count beside it. A cutoff failed in both directions: ">1ms" buried the real
// outliers under thousands of borderline rows on a busy trace, and showed an
// empty rail on a fast one whose worst poll was 800us.
//
// Ranking still excludes ZERO-severity points, which is not a cutoff in
// disguise: a park the kernel delayed for 0ns was not delayed, and 98% of parks
// on a healthy trace are exactly that. The predicate detectors keep theirs,
// since being sampled or uninstrumented is a fact about the poll, not a
// severity.
//
// The detectors run over the RESIDENT `trace` slice. Whole-trace loads make
// the POI set complete; when segment windowing feeds a partial trace, the
// count is over the resident window only - consumers must not present it as
// whole-trace truth.

import {
  DEFAULT_SPAWN_DELAY_THRESHOLD_US,
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

/** The detector filters, in issues-rail display order. */
export const POI_FILTERS: readonly PointOfInterestType[] = [
  "sched",
  "long-poll",
  "cpu-sampled",
  "wake-delay",
  "uninstrumented",
  "spawn-delay",
  "off-cpu-active",
];

export { DEFAULT_SPAWN_DELAY_THRESHOLD_US };

/** Threshold input bounds, in microseconds. 0 lists every positive delay. */
export const SPAWN_DELAY_THRESHOLD_MIN_US = 0;
export const SPAWN_DELAY_THRESHOLD_MAX_US = 60_000_000;

/** Clamp a DOM/URL threshold into range; null when unusable, so a half-typed
 *  field never resets the rail to the default. */
export function parseSpawnThresholdUs(value: string): number | null {
  // Number("") is 0, which would read a cleared field as "list every task".
  if (value.trim() === "") return null;
  const n = Number(value);
  if (!Number.isFinite(n)) return null;
  return Math.min(SPAWN_DELAY_THRESHOLD_MAX_US, Math.max(SPAWN_DELAY_THRESHOLD_MIN_US, n));
}

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
      return "Longest Polls";
    case "cpu-sampled":
      return "Polls with CPU Samples";
    case "wake-delay":
      return "Wake->Poll Delays";
    case "uninstrumented":
      return "Uninstrumented Polls";
    case "spawn-delay":
      // The rail renders this detector's optional floor in its own input.
      return "Spawn->First Poll Delays";
    case "off-cpu-active":
      return "Descheduled Worker Periods";
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
    case "spawn-delay":
      return "spawn delay";
    case "off-cpu-active":
      return "off-cpu active";
  }
}

// ── Heavy source, memoized on trace identity ─────────────────────────────

/**
 * The frame-invariant inputs the detectors need, derived once per loaded
 * trace: reconstructed worker spans (CPU samples attached, so the
 * "cpu-sampled" filter sees them), the worker id set, the scheduling delays
 * (the "wake-delay" detector's input), and the trace-level flags the detectors
 * read. This is the expensive part (buildWorkerSpans scans every
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
  taskSpawnTimes: Map<number, number>;
  /** Gates "off-cpu-active"; see DetectorInputs.hasWorkerCpuTime. */
  hasWorkerCpuTime: boolean;
  /** Lazy detector output cache, keyed by `detectorCacheKey`: the filter, the
   *  threshold for detectors that take one, and the requested length. `matched`
   *  is the true pre-cap count, so a capped list never understates the
   *  population it came from. */
  readonly _byFilter: Map<string, DetectorResult>;
}

interface DetectorResult {
  list: PointOfInterest[];
  matched: number;
}

function usesSpawnThreshold(filter: PointOfInterestType): boolean {
  return filter === "spawn-delay";
}

/** Only the threshold-sensitive detector folds the threshold into its key, so
 *  moving the input never invalidates the others. */
function detectorCacheKey(
  filter: PointOfInterestType,
  spawnThresholdUs: number,
  worstN: number,
): string {
  const base = usesSpawnThreshold(filter) ? `${filter}:${spawnThresholdUs}` : filter;
  return `${base}@${worstN}`;
}

/**
 * The "show everything" choice, and the ceiling that keeps the rail alive while
 * doing it.
 *
 * With no cutoff, a detector can match every poll in the trace - millions on a
 * 13M-event one, enough to exhaust the tab if each became a row. So "all" means
 * "as many as the rail can safely hold"; past that the list is capped and the
 * rail says so rather than pretending it showed everything.
 */
export const POI_WORST_N_ALL = 50_000;

/** The list-length choices the rail offers, smallest first. */
export const POI_WORST_N_CHOICES: readonly number[] = [10, 50, 200, POI_WORST_N_ALL];

/** How many rows the rail shows before the user picks otherwise. */
export const POI_WORST_N_DEFAULT = 50;

/** The `<option>` text for a list length. */
export function worstNLabel(n: number): string {
  return n === POI_WORST_N_ALL ? "all" : `worst ${n}`;
}

/** Clamp a DOM/URL list length onto an offered choice; null when unusable, so
 *  a malformed value never silently resizes the rail. */
export function parsePoiWorstN(value: string): number | null {
  const n = Number(value);
  return POI_WORST_N_CHOICES.includes(n) ? n : null;
}

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
  const { workerIds, lanes, schedDelays, hasWorkerCpuTime } = sharedDetectorInputs(trace);

  source = {
    workerIds,
    lanes,
    schedDelays,
    hasSchedWait: trace.hasSchedWait,
    taskInstrumented: trace.taskInstrumented,
    taskSpawnTimes: trace.taskSpawnTimes,
    hasWorkerCpuTime,
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
  spawnThresholdUs: number,
  worstN: number,
): DetectorResult {
  const cacheKey = detectorCacheKey(filter, spawnThresholdUs, worstN);
  const cached = source._byFilter.get(cacheKey);
  if (cached !== undefined) return cached;
  // One live entry per threshold-sensitive detector: the input is a spinner, so
  // keeping every value passed through would retain a capped list per keystroke.
  if (usesSpawnThreshold(filter)) {
    for (const key of source._byFilter.keys()) {
      if (key.startsWith(`${filter}:`)) source._byFilter.delete(key);
    }
  }
  let matched = -1;
  const opts = {
    hasSchedWait: source.hasSchedWait,
    sortByWorst: true,
    taskInstrumented: source.taskInstrumented,
    taskSpawnTimes: source.taskSpawnTimes,
    spawnDelayThresholdUs: spawnThresholdUs,
    hasWorkerCpuTime: source.hasWorkerCpuTime,
    limit: worstN,
    onTotal: (n: number) => {
      matched = n;
    },
  };
  const list = source.lanes.columnar
    ? source.lanes.store.pointsOfInterest(filter, source.workerIds, source.schedDelays, opts)
    : filterPointsOfInterest(
        filter, source.lanes.workerSpans, source.workerIds, source.schedDelays, opts,
      );
  // Both paths report through `onTotal`; the fallback covers a stubbed detector
  // in a test, whose result is complete and whose length IS the true count.
  const result: DetectorResult = {
    list,
    matched: matched >= 0 ? matched : list.length,
  };
  source._byFilter.set(cacheKey, result);
  return result;
}

/**
 * The worst `worstN` points of one kind, severity-ranked. Each length is its own
 * detector run, memoized: "all" retains up to POI_WORST_N_ALL, and making every
 * length a prefix of that would charge a 10-row view for a 50,000-row scan.
 */
export function poisForFilter(
  source: PoiSource,
  filter: PointOfInterestType,
  spawnThresholdUs: number = DEFAULT_SPAWN_DELAY_THRESHOLD_US,
  worstN: number = POI_WORST_N_DEFAULT,
): PointOfInterest[] {
  return detectorResult(source, filter, spawnThresholdUs, worstN).list;
}

/**
 * The TRUE number of points a detector matched, which is >= the length of
 * `poisForFilter` - always, now that the detectors rank rather than threshold.
 * Counts shown to the user come from here, so "worst 50" never reads as "found
 * 50".
 */
export function poiMatchCount(
  source: PoiSource,
  filter: PointOfInterestType,
  spawnThresholdUs: number = DEFAULT_SPAWN_DELAY_THRESHOLD_US,
  worstN: number = POI_WORST_N_DEFAULT,
): number {
  return detectorResult(source, filter, spawnThresholdUs, worstN).matched;
}

/** Per-detector counts for the red-flags summary chip. Zero-count detectors
 *  are included; the chip filters them out. */
export function redFlagCounts(
  source: PoiSource,
  spawnThresholdUs: number = DEFAULT_SPAWN_DELAY_THRESHOLD_US,
): { type: PointOfInterestType; count: number }[] {
  return POI_FILTERS.map((type) => ({
    type,
    count: poiMatchCount(source, type, spawnThresholdUs),
  }));
}

/**
 * Detectors whose COUNT is a fact about the trace rather than an artefact of
 * ranking: they select on a predicate (this poll carries samples; this task was
 * spawned uninstrumented), so "9,136 of them" means something.
 *
 * Every other detector now admits every candidate and ranks it, so its count is
 * just the population - "56,125 long polls" on a trace whose worst poll is
 * 40us. Those report their worst VALUE instead.
 */
const PREDICATE_FILTERS: ReadonlySet<PointOfInterestType> = new Set([
  "cpu-sampled",
  "uninstrumented",
]);

export function isPredicateFilter(type: PointOfInterestType): boolean {
  return PREDICATE_FILTERS.has(type);
}

/**
 * Whether the rail should print "of N" beside the list length. True when the
 * detector genuinely narrowed the population, or when the list was cut short at
 * the ceiling - the two cases where the number tells the reader something they
 * cannot infer.
 */
export function showsTotal(
  filter: PointOfInterestType,
  worstN: number,
  total: number,
): boolean {
  if (worstN === POI_WORST_N_ALL && total > POI_WORST_N_ALL) return true;
  return isPredicateFilter(filter) && total > worstN;
}

/** One detector's line in the red-flags chip. */
export interface RedFlag {
  type: PointOfInterestType;
  /** True when `count` stands on its own (a predicate detector). */
  counted: boolean;
  /** The true match count. Meaningful on its own only when `counted`. */
  count: number;
  /** Severity of the worst match, in nanoseconds; null when nothing matched. */
  worstNs: number | null;
}

/**
 * The red-flags summary: a count for the predicate detectors, the worst
 * severity for the ranked ones. Detectors with nothing to report are dropped
 * here rather than by the caller, since "nothing to report" now differs by kind.
 */
export function redFlagSummary(
  source: PoiSource,
  spawnThresholdUs: number = DEFAULT_SPAWN_DELAY_THRESHOLD_US,
): RedFlag[] {
  const out: RedFlag[] = [];
  for (const type of POI_FILTERS) {
    const count = poiMatchCount(source, type, spawnThresholdUs);
    if (count === 0) continue;
    const worst = poisForFilter(source, type, spawnThresholdUs, 1)[0];
    out.push({
      type,
      counted: isPredicateFilter(type),
      count,
      worstNs: worst === undefined ? null : valueNs(worst),
    });
  }
  return out;
}

/** The chip's text for one detector. */
export function redFlagLabel(flag: RedFlag): string {
  if (flag.counted) {
    return `${flag.count} ${kindLabel(flag.type)}${flag.count === 1 ? "" : "s"}`;
  }
  const worst = flag.worstNs === null ? "n/a" : formatHumanDuration(flag.worstNs);
  return `worst ${kindLabel(flag.type)} ${worst}`;
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

/** Resolve a poll-backed issue after detector output or sort order changes. */
export function poiIndexForPoll(
  pois: readonly PointOfInterest[],
  poll: Pick<PollSpan, "start" | "taskId">,
): number {
  return pois.findIndex(
    (poi) =>
      "taskId" in poi.span &&
      poi.span.start === poll.start &&
      poi.span.taskId === poll.taskId,
  );
}

export interface PoiAnchor {
  worker: number;
  time: number;
  spanStart: number;
  taskId?: number;
}

/** Stable identity for one issue, independent of its current sorted index. */
export function poiAnchor(poi: PointOfInterest): PoiAnchor {
  return {
    worker: poi.worker,
    time: poi.time,
    spanStart: poi.span.start,
    ...("taskId" in poi.span ? { taskId: poi.span.taskId } : {}),
  };
}

/** Resolve a stable issue identity against freshly computed detector output. */
export function poiIndexForAnchor(
  pois: readonly PointOfInterest[],
  anchor: PoiAnchor,
): number {
  return pois.findIndex((poi) => {
    const taskId = "taskId" in poi.span ? poi.span.taskId : undefined;
    return (
      poi.worker === anchor.worker &&
      poi.time === anchor.time &&
      poi.span.start === anchor.spanStart &&
      taskId === anchor.taskId
    );
  });
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

/**
 * `+1.72s` relative-offset time label (the rail's `t` column).
 *
 * The offset can be NEGATIVE: `minTs` comes from the lifecycle events, and a
 * spawn-delay POI is anchored on a spawn, which can precede the first of them.
 */
export function relTimeLabel(ns: number, minTs: number): string {
  // Round before taking the sign, so an offset rounding to zero reads "+0.00s".
  const rounded = Number(((ns - minTs) / 1e9).toFixed(2));
  return `${rounded < 0 ? "-" : "+"}${Math.abs(rounded).toFixed(2)}s`;
}

/** The POI's severity value converted to nanoseconds (per detector units).
 *  `filterPointsOfInterest` stores `value` in different units per type
 *  (sched: ns schedWait; off-cpu-active: ns off-CPU time;
 *  long-poll/cpu-sampled/uninstrumented: ms; wake-delay: us) -
 *  normalize to ns so `formatHumanDuration` reads them uniformly. */
export function valueNs(poi: PointOfInterest): number {
  switch (poi.type) {
    case "sched":
      return poi.value; // schedWait, already ns
    case "off-cpu-active":
      return poi.value; // off-CPU time, already ns
    case "wake-delay":
    case "spawn-delay":
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
 * full wake->poll window (~3x, 20% pad) and select the delayed task, and
 * spawn-delay POIs frame the equivalent spawn->first-poll window - for both,
 * the whole point is the gap, so a window sized off the poll alone would leave
 * the cause off-screen. Other POIs select the poll's task when it has one, so
 * the inspector and lane highlight follow the jump.
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
  } else if (poi.type === "spawn-delay") {
    // poi.time is the spawn; poi.span is the first poll it waited for.
    const totalDur = poi.span.end - poi.time;
    const padded = Math.max(totalDur * 3, 1e6);
    viewStart = Math.max(minTs, poi.time - padded * 0.2);
    viewEnd = Math.min(maxTs, viewStart + padded);
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
   * bounded by `worstN`, so holding it is cheap.
   */
  sorted: readonly PointOfInterest[];
  /** The TRUE detector match count ("worst 50 of 12,431"). Since the detectors
   *  rank rather than threshold, this is the population the worst N came from,
   *  not a count of problems. */
  total: number;
  /** How many points the rail actually holds: `min(worstN, total)`. */
  retained: number;
  /** The selected list length - one of POI_WORST_N_CHOICES. */
  worstN: number;
  /**
   * Whether `total` is worth showing next to the list length.
   *
   * For a detector that ranks the whole population, "worst 50 of 56,125" invites
   * reading 56,125 as a count of problems when it is just "how many polls
   * exist". The denominator earns its place only when the detector actually
   * selected a subset, or when the list hit POI_WORST_N_ALL and the user needs
   * to know it was cut short.
   */
  showTotal: boolean;
  /** True when "all" was asked for and the population exceeded the ceiling. */
  cappedAtCeiling: boolean;
  /** Per-detector counts for the red-flags summary chip. */
  redFlags: { type: PointOfInterestType; count: number }[];
  spawnThresholdUs: number;
  /** Whether the trace carries spawn timestamps at all, so the rail can say why
   *  the list is empty instead of showing a dead control. */
  hasSpawnTimes: boolean;
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
      worstN: poi.worstN,
      showTotal: false,
      cappedAtCeiling: false,
      redFlags: [],
      spawnThresholdUs: poi.spawnThresholdUs,
      hasSpawnTimes: false,
    };
  }
  const source = poiSourceFor(trace);
  const filtered = poisForFilter(source, poi.filter, poi.spawnThresholdUs, poi.worstN);
  const total = poiMatchCount(source, poi.filter, poi.spawnThresholdUs, poi.worstN);
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
    total,
    retained: sorted.length,
    worstN: poi.worstN,
    showTotal: showsTotal(poi.filter, poi.worstN, total),
    cappedAtCeiling: poi.worstN === POI_WORST_N_ALL && total > POI_WORST_N_ALL,
    redFlags: redFlagCounts(source, poi.spawnThresholdUs).filter((r) => r.count > 0),
    spawnThresholdUs: poi.spawnThresholdUs,
    hasSpawnTimes: source.taskSpawnTimes.size > 0,
  };
}

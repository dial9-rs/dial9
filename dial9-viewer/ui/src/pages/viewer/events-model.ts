// src/pages/viewer/events-model.ts - the custom-events track's PURE derivation
// + filtering + clustering + task resolution + dimming logic (T27; features/02
// section K; legacy `renderCustomEventsPanel` / `taskForEvent` / `pollForEvent`
// / the generic-custom-event extraction in viewer.html). No store, no DOM, no
// canvas: every function here is referentially transparent so the events-track
// component (events-track.ts) can cache the trace-invariant half in a store
// `derived()` (perf finding F5) and unit-test the window/cluster/task/dim halves
// directly (the DoD's Vitest checks).
//
// The split mirrors F5's two tiers:
//   1. computeEventTrackData(customEvents) - TRACE-INVARIANT: the generic
//      (non-span) custom events sorted by timestamp, plus the sorted unique
//      name list (legend chips K5). Recomputed ONLY when the trace slice
//      changes; the component wraps it in store.derived(["trace"], ...).
//   2. buildEventRenderModel(...) - VIEWPORT/FILTER-dependent: the visible
//      window (BINARY-SEARCHED both edges, F5/F6 - never the legacy O(N)
//      per-frame scan, which is the exact bug 03 F5 flags and 03 F6's
//      binary-search-helpers rule fixes), the name filter (K5), per-pixel
//      clustering + tick geometry (K1), and the panel-info counts (K2).
//      Produces final bucket geometry + a base alpha; the DRAW step applies the
//      selection-driven fade (S4/K1) so a selection change stays a cheap redraw
//      and never recomputes the layout.
//
// Task resolution (K7) is `resolveTaskForEvent` / `resolvePollForEvent`, ported
// verbatim from the legacy `taskForEvent` / `pollForEvent`: field task_id ->
// field worker_id + poll-at-timestamp -> unambiguous cross-worker scan. Pure
// over the worker lanes passed in; the component memoizes per event (WeakMap,
// legacy `_taskForEventCache`).
//
// SEAMS (scope fence): the hover guide line (I4) and the pinned-event marker
// (I5) are drawn by the T24 overlay (components/overlay/crosshair.ts) reading
// `transient.hoverEventTs` and `selection.pinnedEvent`. This track DISPATCHES
// those slices (events-track.ts); it never draws the overlay marks. The
// event-KV sidebar is T31's surface; this track dispatches `selection.
// pinnedEvent`, T31 renders it.

import { findSpanAt } from "../../lib/trace/index.js";
import type {
  CustomTraceEvent,
  PollSpan,
  WorkerLane,
} from "../../lib/trace/index.js";
import type { SelectionSlice } from "../../types/state.js";

// ── Generic-event extraction (legacy viewer.html:2100-2119) ──────────────

/** Span lifecycle events, excluded from the custom-events track (they drive
 *  the spans track T26, not the events markers). */
const SPAN_EVENT_PREFIXES: readonly string[] = ["SpanEnter:", "SpanExit:"];
const SPAN_EVENT_EXACT: ReadonlySet<string> = new Set([
  "SpanEnterEvent",
  "SpanExitEvent",
  "SpanCloseEvent",
]);

/** True when `name` is a span lifecycle event (excluded from this track). */
function isSpanEvent(name: string): boolean {
  if (SPAN_EVENT_EXACT.has(name)) return true;
  return SPAN_EVENT_PREFIXES.some((p) => name.startsWith(p));
}

// ── Trace-invariant event data (F5 tier 1) ───────────────────────────────

/**
 * Everything about a trace's generic custom events that does NOT depend on the
 * viewport, filter, or selection - computed once per trace load and cached in a
 * store `derived(["trace"], ...)` (F5). `events` is sorted by timestamp (the
 * window binary search relies on it), exactly the legacy `genericCustomEvents`.
 */
export interface EventTrackData {
  /** Generic (non-span) custom events, sorted ascending by timestamp. */
  events: readonly CustomTraceEvent[];
  /** Unique event names, sorted - the legend chip set (K5). */
  eventNames: readonly string[];
}

/** Empty resting data (no trace / no generic events) so callers never
 *  special-case null. */
export const EMPTY_EVENT_TRACK_DATA: EventTrackData = {
  events: [],
  eventNames: [],
};

/**
 * Derive the trace-invariant event data: the generic custom events (span
 * lifecycle events filtered out), sorted by timestamp, plus the sorted unique
 * name list. Pure. Returns EMPTY_EVENT_TRACK_DATA when there are none so the
 * track renders its resting state without branching (legacy
 * viewer.html:2100-2127).
 */
export function computeEventTrackData(
  customEvents: readonly CustomTraceEvent[] | null | undefined,
): EventTrackData {
  if (customEvents == null || customEvents.length === 0) {
    return EMPTY_EVENT_TRACK_DATA;
  }
  const events: CustomTraceEvent[] = [];
  const names = new Set<string>();
  for (const ev of customEvents) {
    if (isSpanEvent(ev.name)) continue;
    events.push(ev);
    names.add(ev.name);
  }
  if (events.length === 0) return EMPTY_EVENT_TRACK_DATA;
  events.sort((a, b) => a.timestamp - b.timestamp);
  return { events, eventNames: [...names].sort() };
}

// ── Filtering (K5 name chips) ────────────────────────────────────────────

/** True when `ev` passes the name-chip filter (empty set = no filter, legacy
 *  `selectedCENames.size > 0 && !selectedCENames.has(ev.name)`). */
export function eventMatchesFilter(
  ev: CustomTraceEvent,
  selectedNames: ReadonlySet<string>,
): boolean {
  return selectedNames.size === 0 || selectedNames.has(ev.name);
}

// ── Visibility window (BINARY SEARCH both edges - F5/F6) ──────────────────

/**
 * First index in `events` (sorted by timestamp) whose `timestamp` is >=
 * `viewStart` - the inclusive left bound of the visible window. Binary search:
 * events before this index end (are) before the view and cannot be visible.
 */
export function lowerBoundByTimestamp(
  events: readonly CustomTraceEvent[],
  viewStart: number,
): number {
  let lo = 0;
  let hi = events.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (events[mid]!.timestamp < viewStart) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/**
 * First index in `events` (sorted by timestamp) whose `timestamp` is strictly
 * greater than `viewEnd` - the exclusive right bound of the visible window.
 */
export function upperBoundByTimestamp(
  events: readonly CustomTraceEvent[],
  viewEnd: number,
): number {
  let lo = 0;
  let hi = events.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (events[mid]!.timestamp <= viewEnd) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/**
 * Events whose timestamp is in [viewStart, viewEnd] AND which pass the name
 * filter. UNLIKE spans (which have a duration, so only the right edge is
 * binary-bounded), custom events are point records, so BOTH edges are
 * binary-searched: the per-frame work is O(log N + windowSize), never the
 * legacy O(all generic events) scan (03 F5 -> F6). This IS the perf fix T27
 * owns.
 */
export function filterVisibleEvents(
  data: EventTrackData,
  viewStart: number,
  viewEnd: number,
  selectedNames: ReadonlySet<string>,
): CustomTraceEvent[] {
  const events = data.events;
  const lo = lowerBoundByTimestamp(events, viewStart);
  const hi = upperBoundByTimestamp(events, viewEnd);
  const out: CustomTraceEvent[] = [];
  for (let i = lo; i < hi; i++) {
    const ev = events[i]!;
    if (!eventMatchesFilter(ev, selectedNames)) continue;
    out.push(ev);
  }
  return out;
}

// ── Task / poll resolution (K7; legacy taskForEvent / pollForEvent) ───────

/**
 * The task a custom event belongs to (legacy `_resolveTaskForEvent`,
 * viewer.html:2645-2668): field `task_id` if present, else field `worker_id` +
 * the poll running at the event's timestamp on that worker, else an
 * unambiguous cross-worker scan (concurrent polls on different workers ->
 * null). Pure over the worker lanes; the component memoizes per event.
 */
export function resolveTaskForEvent(
  ev: CustomTraceEvent,
  workerSpans: Record<number, WorkerLane>,
  workerIds: readonly number[],
): number | null {
  const f = ev.fields || {};
  if (f["task_id"] != null) {
    const t = Number(f["task_id"]);
    return Number.isFinite(t) ? t : null;
  }
  if (f["worker_id"] != null) {
    const wid = Number(f["worker_id"]);
    const lane = workerSpans[wid];
    const poll = lane ? findSpanAt(lane.polls, ev.timestamp) : null;
    return poll && poll.taskId ? poll.taskId : null;
  }
  // No hint: search every worker; accept only an unambiguous single match.
  let found: number | null = null;
  for (const w of workerIds) {
    const lane = workerSpans[w];
    if (lane === undefined) continue;
    const poll = findSpanAt(lane.polls, ev.timestamp);
    if (poll && poll.taskId) {
      if (found != null && found !== poll.taskId) return null;
      found = poll.taskId;
    }
  }
  return found;
}

/**
 * The specific poll a custom event landed in - the single bar to highlight
 * (legacy `pollForEvent`, viewer.html:2675-2698), or null when it cannot be
 * pinned to one poll. `preferTask` pins the search to a known task (e.g. the
 * task a whole cluster resolved to), overriding the field-derived one. Returns
 * the actual PollSpan so T31's sidebar renders its full detail.
 */
export function resolvePollForEvent(
  ev: CustomTraceEvent,
  workerSpans: Record<number, WorkerLane>,
  workerIds: readonly number[],
  preferTask: number | null = null,
): PollSpan | null {
  const f = ev.fields || {};
  if (f["worker_id"] != null) {
    const wid = Number(f["worker_id"]);
    const lane = workerSpans[wid];
    const poll = lane ? findSpanAt(lane.polls, ev.timestamp) : null;
    return poll && poll.taskId && (preferTask == null || poll.taskId === preferTask)
      ? poll
      : null;
  }
  const wantTask =
    preferTask != null ? preferTask : f["task_id"] != null ? Number(f["task_id"]) : null;
  // Search every worker; accept only an unambiguous single match.
  let found: PollSpan | null = null;
  for (const w of workerIds) {
    const lane = workerSpans[w];
    if (lane === undefined) continue;
    const poll = findSpanAt(lane.polls, ev.timestamp);
    if (poll && poll.taskId && (wantTask == null || poll.taskId === wantTask)) {
      if (found != null && found.taskId !== poll.taskId) return null;
      found = poll;
    }
  }
  return found;
}

/**
 * The single task a whole cluster resolves to, or null (legacy
 * renderCustomEventsPanel:3600-3606): every event in the cluster must map to
 * the SAME single task; any null or any disagreement -> null (not clickable to
 * a task). `taskOf` is the memoized per-event resolver the component supplies.
 */
export function resolveClusterTask(
  events: readonly CustomTraceEvent[],
  taskOf: (ev: CustomTraceEvent) => number | null,
): number | null {
  let taskId: number | null = null;
  for (const ev of events) {
    const t = taskOf(ev);
    if (t == null) return null;
    if (taskId == null) taskId = t;
    else if (taskId !== t) return null;
  }
  return taskId;
}

// ── Selection-driven dimming (S4; same rule as T26) ──────────────────────

/**
 * The highlighted task the events track dims to (S4 / legacy K1
 * viewer.html:3611-3613): the selected task if any, else the pinned event's
 * resolved task. When non-null, clusters that did NOT run on it fade to 20%
 * (the DRAW step applies the fade, keeping a selection change a cheap redraw -
 * never a layout recompute, mirroring T26's spanHighlight). Reads only the
 * selection slice, so this is the same selection-driven dimming T26 uses.
 */
export function eventHighlightTask(
  selection: Pick<SelectionSlice, "selectedTaskId" | "pinnedEvent">,
): number | null {
  if (selection.selectedTaskId != null) return selection.selectedTaskId;
  return selection.pinnedEvent?.taskId ?? null;
}

/** How much a dimmed (unrelated) tick's alpha is scaled (legacy `*= 0.2`). */
export const EVENT_DIM_FACTOR = 0.2;

// ── Render model (viewport/filter tier - K1/K2) ──────────────────────────

/** Tick geometry constants (legacy renderCustomEventsPanel:3577,3595,3633). */
export const EVENT_TICK_MIN_W = 3;
export const EVENT_TICK_MAX_W = 10;
export const EVENT_HIT_MIN_W = 12;
/** Vertical padding above/below the tick within the canvas (legacy y=2, h=ph-4). */
export const EVENT_TICK_PAD = 2;

/**
 * One drawn event marker cluster: final geometry + colors + the pre-fade base
 * alpha, so the DRAW step needs only the highlight task for the S4/K1 fade.
 */
export interface EventDrawBucket {
  events: readonly CustomTraceEvent[];
  representative: CustomTraceEvent;
  /** Center x (draw-area-relative px, rounded - the cluster's pixel column). */
  px: number;
  /** Tick width (legacy `max(3, min(3 + log2(size)*2, 10))`). */
  w: number;
  /** Tick top y. */
  y: number;
  /** Tick height. */
  h: number;
  /** The cluster's resolved single task (K7), or null when not task-clickable. */
  taskId: number | null;
  /** Base alpha before dimming (legacy `min(0.4 + size*0.15, 1)`). */
  baseAlpha: number;
  /** Representative tick color (ceColor(rep.name)). */
  color: string;
  /** Secondary-color bottom stripe for mixed-name clusters, else null. */
  secondColor: string | null;
  /** Hit region left edge (draw-area px), padded to >= 12px wide (K1). */
  hitX: number;
  /** Hit region width (>= 12px). */
  hitW: number;
}

/** Why the render set is empty, for the canvas's resting message (legacy
 *  "No custom events" vs "No events in view"). */
export type EventEmptyReason = "no-events" | "no-visible" | null;

/** The full per-frame render input for the events canvas + info readout. */
export interface EventRenderModel {
  buckets: readonly EventDrawBucket[];
  /** K2 panel info: `N events · M markers`. */
  info: string;
  emptyReason: EventEmptyReason;
}

export interface EventRenderModelOpts {
  data: EventTrackData;
  viewStart: number;
  viewEnd: number;
  /** Draw-area width in CSS px (drawW from the shared layout). */
  drawW: number;
  /** Canvas height in CSS px (the events draw area). */
  canvasH: number;
  selectedNames: ReadonlySet<string>;
  /** Memoized per-event task resolver (K7). */
  taskOf: (ev: CustomTraceEvent) => number | null;
  /** Stable name -> color assigner (ceColor). */
  colorOf: (name: string) => string;
}

/** Timestamp (ns) -> draw-area-relative x (px), the shared A13 mapping (no
 *  LABEL_W: the track canvas already sits after the DOM label gutter). */
function nsToDrawX(ns: number, viewStart: number, viewEnd: number, drawW: number): number {
  const span = viewEnd - viewStart || 1;
  return ((ns - viewStart) / span) * drawW;
}

/**
 * Build the per-frame render model: binary-searched visible window -> name
 * filter -> per-pixel clustering -> tick geometry (K1) + info (K2). Pure and
 * cheap enough to memoize on its primitive inputs (the component keys the memo
 * on trace identity + viewport + drawW/canvasH + the name filter, NOT the
 * selection, so an S4 dim change reuses this cached model). Does NOT read the
 * highlight task - the DRAW step applies that fade.
 */
export function buildEventRenderModel(opts: EventRenderModelOpts): EventRenderModel {
  const { data, viewStart, viewEnd, drawW, canvasH, selectedNames, taskOf, colorOf } = opts;
  if (data.events.length === 0) {
    return { buckets: [], info: "", emptyReason: "no-events" };
  }
  if (drawW <= 0 || canvasH <= 0 || viewEnd <= viewStart) {
    return { buckets: [], info: "", emptyReason: "no-visible" };
  }

  const visible = filterVisibleEvents(data, viewStart, viewEnd, selectedNames);
  if (visible.length === 0) {
    return { buckets: [], info: "", emptyReason: "no-visible" };
  }

  // Cluster events that land on the same rounded pixel column (legacy
  // viewer.html:3576-3584). Insertion order per bucket is timestamp order
  // (visible is a window of the sorted array), so events[0] is the earliest.
  const clusters = new Map<number, CustomTraceEvent[]>();
  for (const ev of visible) {
    let px = Math.round(nsToDrawX(ev.timestamp, viewStart, viewEnd, drawW));
    if (px < 0) px = 0;
    else if (px > drawW) px = drawW;
    let bucket = clusters.get(px);
    if (bucket === undefined) {
      bucket = [];
      clusters.set(px, bucket);
    }
    bucket.push(ev);
  }

  const h = canvasH - EVENT_TICK_PAD * 2;
  const buckets: EventDrawBucket[] = [];
  for (const [px, events] of clusters) {
    const rep = events[0]!;
    const size = events.length;
    const w = Math.max(
      EVENT_TICK_MIN_W,
      Math.min(EVENT_TICK_MIN_W + Math.log2(size) * 2, EVENT_TICK_MAX_W),
    );
    const taskId = resolveClusterTask(events, taskOf);
    const baseAlpha = Math.min(0.4 + size * 0.15, 1.0);

    // Mixed-name cluster: a small second-color bottom stripe (legacy 3623-3631).
    let secondColor: string | null = null;
    if (size > 1) {
      const second = events.find((e) => e.name !== rep.name);
      if (second !== undefined) secondColor = colorOf(second.name);
    }

    const hitW = Math.max(w, EVENT_HIT_MIN_W);
    buckets.push({
      events,
      representative: rep,
      px,
      w,
      y: EVENT_TICK_PAD,
      h,
      taskId,
      baseAlpha,
      color: colorOf(rep.name),
      secondColor,
      hitX: px - hitW / 2,
      hitW,
    });
  }

  return {
    buckets,
    info: `${visible.length} events · ${clusters.size} markers`,
    emptyReason: null,
  };
}

// The custom-events track's PURE derivation + filtering + clustering + task
// resolution + dimming logic. No store, no DOM, no canvas: every function is
// referentially transparent so the component can cache the trace-invariant
// half in a store derived() and unit-test the halves directly.
//
// Two tiers:
//   1. computeEventTrackData(customEvents) - trace-invariant: the generic
//      (non-span) custom events sorted by timestamp, plus the sorted unique
//      name list (legend chips). Recomputed only when the trace slice changes.
//   2. buildEventRenderModel(...) - viewport/filter-dependent: the visible
//      window (binary-searched at BOTH edges - point events, no duration), the
//      name filter, per-pixel clustering + tick geometry, and the info counts.
//      Produces bucket geometry + a base alpha; the DRAW step applies the
//      selection-driven fade, so a selection change stays a cheap redraw and
//      never recomputes the layout.
//
// Task resolution is resolveTaskForEvent / resolvePollForEvent: field task_id
// -> field worker_id + poll-at-timestamp -> unambiguous cross-worker scan.

import { findSpanAt } from "../../lib/trace/index.js";
import type {
  CustomTraceEvent,
  PollSpan,
  WorkerLane,
} from "../../lib/trace/index.js";
import type { PinnedCustomEvent, SelectionSlice } from "../../types/state.js";

// ── Generic-event extraction ─────────────────────────────────────────────

/** Span lifecycle events, excluded from the custom-events track (they drive
 *  the spans track, not the events markers). */
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

// ── Trace-invariant event data ───────────────────────────────────────────

/**
 * Everything about a trace's generic custom events that does NOT depend on the
 * viewport, filter, or selection - computed once per trace load and cached in a
 * store `derived(["trace"], ...)`. `events` is sorted by timestamp (the window
 * binary search relies on it).
 */
export interface EventTrackData {
  /** Generic (non-span) custom events, sorted ascending by timestamp. */
  events: readonly CustomTraceEvent[];
  /** Unique event names, sorted - the legend chip set. */
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
 * track renders its resting state without branching.
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

// ── Filtering (name chips) ───────────────────────────────────────────────

/** True when `ev` passes the name-chip filter (empty set = no filter). */
export function eventMatchesFilter(
  ev: CustomTraceEvent,
  selectedNames: ReadonlySet<string>,
): boolean {
  return selectedNames.size === 0 || selectedNames.has(ev.name);
}

// ── Visibility window (binary-searched at both edges) ────────────────────

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
 * filter. Custom events are point records, so BOTH edges are binary-searched:
 * the per-frame work is O(log N + windowSize), never an O(all events) scan.
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

// ── Task / poll resolution ───────────────────────────────────────────────

/**
 * The task a custom event belongs to: field `task_id` if present, else field
 * `worker_id` + the poll running at the event's timestamp on that worker, else
 * an unambiguous cross-worker scan (concurrent polls on different workers ->
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
 * The specific poll a custom event landed in - the single bar to highlight, or
 * null when it cannot be pinned to one poll. `preferTask` pins the search to a
 * known task (e.g. the task a whole cluster resolved to), overriding the
 * field-derived one. Returns the actual PollSpan so the sidebar renders its
 * full detail.
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
 * The single task a whole cluster resolves to, or null: every event in the
 * cluster must map to the SAME single task; any null or any disagreement ->
 * null (not clickable to a task). `taskOf` is the memoized per-event resolver
 * the component supplies.
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

// ── Selection-driven dimming ─────────────────────────────────────────────

/**
 * The highlighted task the events track dims to: the selected task if any,
 * else the pinned event's resolved task. When non-null, clusters that did NOT
 * run on it fade to 20% (the DRAW step applies the fade, keeping a selection
 * change a cheap redraw - never a layout recompute). Reads only the selection
 * slice.
 */
export function eventHighlightTask(
  selection: Pick<SelectionSlice, "selectedTaskId" | "pinnedEvent">,
): number | null {
  if (selection.selectedTaskId != null) return selection.selectedTaskId;
  return selection.pinnedEvent?.taskId ?? null;
}

/** How much a dimmed (unrelated) tick's alpha is scaled. */
export const EVENT_DIM_FACTOR = 0.2;

// ── Render model (viewport/filter tier) ──────────────────────────────────

/** Tick geometry constants. */
export const EVENT_TICK_MIN_W = 3;
export const EVENT_TICK_MAX_W = 10;
export const EVENT_HIT_MIN_W = 12;
/** Vertical padding above/below the tick within the canvas. */
export const EVENT_TICK_PAD = 2;

/**
 * One drawn event marker cluster: final geometry + colors + the pre-fade base
 * alpha, so the DRAW step needs only the highlight task for the fade.
 */
export interface EventDrawBucket {
  events: readonly CustomTraceEvent[];
  representative: CustomTraceEvent;
  /** Center x (draw-area-relative px, rounded - the cluster's pixel column). */
  px: number;
  /** Tick width. */
  w: number;
  /** Tick top y. */
  y: number;
  /** Tick height. */
  h: number;
  /** The cluster's resolved single task, or null when not task-clickable. */
  taskId: number | null;
  /** Base alpha before dimming. */
  baseAlpha: number;
  /** Representative tick color (ceColor(rep.name)). */
  color: string;
  /** Secondary-color bottom stripe for mixed-name clusters, else null. */
  secondColor: string | null;
  /** Hit region left edge (draw-area px), padded to >= 12px wide. */
  hitX: number;
  /** Hit region width (>= 12px). */
  hitW: number;
}

/** Why the render set is empty, for the canvas's resting message. */
export type EventEmptyReason = "no-events" | "no-visible" | null;

/** The full per-frame render input for the events canvas + info readout. */
export interface EventRenderModel {
  buckets: readonly EventDrawBucket[];
  /** Panel info: `N events · M markers`. */
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
  /** Memoized per-event task resolver. */
  taskOf: (ev: CustomTraceEvent) => number | null;
  /** Stable name -> color assigner (ceColor). */
  colorOf: (name: string) => string;
}

/** Timestamp (ns) -> draw-area-relative x (px), the shared mapping (no
 *  LABEL_W: the track canvas already sits after the DOM label gutter). */
function nsToDrawX(ns: number, viewStart: number, viewEnd: number, drawW: number): number {
  const span = viewEnd - viewStart || 1;
  return ((ns - viewStart) / span) * drawW;
}

/**
 * Build the per-frame render model: binary-searched visible window -> name
 * filter -> per-pixel clustering -> tick geometry + info. Pure and cheap
 * enough to memoize on its primitive inputs (the component keys the memo on
 * trace identity + viewport + drawW/canvasH + the name filter, NOT the
 * selection, so a dim change reuses this cached model). Does NOT read the
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

  // Cluster events that land on the same rounded pixel column. Insertion order
  // per bucket is timestamp order (visible is a window of the sorted array),
  // so events[0] is the earliest.
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

    // Mixed-name cluster: a small second-color bottom stripe.
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

// ── Click-to-pin: the PinnedCustomEvent contract ──────────────────────────

/**
 * True when `bucket` is the cluster currently pinned (a repeat click toggles
 * the pin off). Identity of the representative event object uniquely names the
 * cluster - the same underlying CustomTraceEvent survives re-renders, and no
 * two clusters share an event - so this needs no pixel column (which the
 * PinnedCustomEvent contract does not carry).
 */
export function isSameCluster(
  pinned: PinnedCustomEvent | null,
  bucket: EventDrawBucket,
): boolean {
  return (
    pinned !== null &&
    pinned.events.length === bucket.events.length &&
    pinned.events[0] === bucket.representative
  );
}

/**
 * Build the PinnedCustomEvent for a clicked cluster: the events, the cluster
 * timestamp, its resolved task, a display name (single event name or `N
 * events`), the specific poll it landed in (for the Poll tab + the lane
 * highlight), and the single detail event (null for a cluster - the Related
 * tab is single-event only). Pure over the worker lanes.
 */
export function buildPinnedEvent(
  bucket: EventDrawBucket,
  workerSpans: Record<number, WorkerLane>,
  workerIds: readonly number[],
): PinnedCustomEvent {
  const events = bucket.events;
  const single = events.length === 1;
  return {
    events: [...events],
    timestamp: bucket.representative.timestamp,
    taskId: bucket.taskId,
    name: single ? bucket.representative.name : `${events.length} events`,
    poll: resolvePollForEvent(
      bucket.representative,
      workerSpans,
      workerIds,
      bucket.taskId,
    ),
    detailEvent: single ? bucket.representative : null,
  };
}

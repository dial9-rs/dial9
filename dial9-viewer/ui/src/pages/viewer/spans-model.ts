// The spans track's pure derivation + filtering + focus + dimming logic. No
// store, no DOM, no canvas - every function is referentially transparent.
//
// Two tiers:
//   1. computeSpanTrackData(customEvents) - trace-invariant: buildSpanData plus
//      the per-name duration index (percentile filter) and the sorted name list
//      (legend chips). Recomputed only when the trace slice changes; the
//      component wraps it in store.derived(["trace"], ...).
//   2. buildSpanRenderModel(...) - viewport/filter/focus-dependent: the visible
//      window (binary-searched, never an O(allSpans) scan), the filter,
//      selectSpanRenderSet, computeSpanLayout, and focus repositioning. Produces
//      final bucket geometry so the draw step needs only the highlight set for
//      alpha (dimming stays a cheap redraw, never a layout recompute).
//
// Incomplete spans are never silently dropped: buildSpanData's `unmatchedSpans`
// (enter without exit - trace ended mid-span or a segment rotated) is carried
// through so the component can surface the warning.

import {
  buildSpanData,
  fatLanes,
  collectDescendants,
  computeSpanLayout,
  selectSpanRenderSet,
  formatFieldValue,
  formatHumanDuration,
} from "../../lib/trace/index.js";
import type {
  CustomTraceEvent,
  DecodedFieldValue,
  SpanData,
  SpanLayoutBucket,
  TracingSpan,
  UnmatchedSpan,
  LaneSpans,
} from "../../lib/trace/index.js";
import type { ColumnarSpans } from "../../lib/trace/columnar-spans.js";
import type { SelectionSlice } from "../../types/state.js";

// ── Trace-invariant span data ────────────────────────────────────────────

/** Per-span-name metadata carried out of buildSpanData. */
export interface SpanNameMeta {
  spanName: string;
  fields: Record<string, unknown>;
  parentSpanId: string | null;
}

/**
 * Everything about a trace's tracing spans that does NOT depend on the viewport,
 * filter, or selection - computed once per trace load and cached in a store
 * `derived(["trace"], ...)`. `allSpans` is sorted by start (buildSpanData's
 * contract), which the visibility binary search relies on.
 */
export interface SpanTrackData {
  allSpans: readonly TracingSpan[];
  /** Columnar span store (main-thread path); scan/window fns dispatch on it.
   * When set, `allSpans` is empty. */
  columnarSpans?: ColumnarSpans | undefined;
  spanMeta: SpanData["spanMeta"];
  childrenByParent: SpanData["childrenByParent"];
  /** Spans with an enter but no exit - the truncation/incompleteness surface,
   *  surfaced, never dropped. */
  unmatchedSpans: readonly UnmatchedSpan[];
  maxDepth: number;
  /** Unique span names, sorted - the legend chip set. */
  spanNames: readonly string[];
  /**
   * Per-name duration lists (ns), each sorted ascending, for the percentile
   * filter and percentile-rank readouts.
   */
  durationsByName: ReadonlyMap<string, readonly number[]>;
}

/** Empty resting data (no trace / no spans) so callers never special-case null. */
export const EMPTY_SPAN_TRACK_DATA: SpanTrackData = {
  allSpans: [],
  spanMeta: new Map(),
  childrenByParent: new Map(),
  unmatchedSpans: [],
  maxDepth: 0,
  spanNames: [],
  durationsByName: new Map(),
};

/**
 * Derive the trace-invariant span data from a trace's custom events. Pure:
 * buildSpanData plus the per-name duration index and the sorted name list.
 * Returns EMPTY_SPAN_TRACK_DATA when there are no spans so the track renders its
 * resting state without branching.
 */
export function computeSpanTrackData(
  customEvents: readonly CustomTraceEvent[] | null | undefined,
  workerSpans?: Record<number, LaneSpans>,
  precomputedSpanData?: SpanData,
): SpanTrackData {
  // The columnar path routes SPAN events OUT of `customEvents` into a separate
  // span-event store, so `customEvents` can be empty even when the trace is
  // ALL spans (e.g. a shale trace of only SpanEnter/SpanExit). In that case the
  // authoritative span data lives in `precomputedSpanData`, so only bail early
  // when there is NEITHER a precomputed result NOR any events to build one from.
  // (The fat/legacy path keeps span events in `customEvents`, so its guard trips
  // only for a genuinely span-free trace.)
  if (
    precomputedSpanData === undefined &&
    (customEvents == null || customEvents.length === 0)
  ) {
    return EMPTY_SPAN_TRACK_DATA;
  }
  // With workerSpans, each span's active segments are reconstructed from its
  // owning task's polls (idle gaps materialize, activeNs is on-CPU time) and
  // its taskId is resolved; without it, the coarse on-wire segments are kept.
  // The viewer passes `precomputedSpanData` (shared with the lanes derivation)
  // so buildSpanData runs once per trace instead of once per span consumer.
  const data =
    precomputedSpanData ??
    buildSpanData(customEvents!, workerSpans && fatLanes(workerSpans));
  const cs = data.columnarSpans;
  const spanCount = cs ? cs.length : data.allSpans.length;
  if (spanCount === 0 && data.unmatchedSpans.length === 0) {
    return EMPTY_SPAN_TRACK_DATA;
  }

  // Name set + per-name duration lists. On the columnar path scan the columns
  // (spanName id + start/end) - no span materialization.
  const names = new Set<string>();
  const durationsByName = new Map<string, number[]>();
  const addDur = (spanName: string, dur: number): void => {
    names.add(spanName);
    let durs = durationsByName.get(spanName);
    if (durs === undefined) { durs = []; durationsByName.set(spanName, durs); }
    durs.push(dur);
  };
  if (cs) {
    for (let r = 0; r < cs.length; r++) addDur(cs.spanNameAt(r), cs.end[r]! - cs.start[r]!);
  } else {
    for (const s of data.allSpans) addDur(s.spanName, s.end - s.start);
  }
  for (const durs of durationsByName.values()) durs.sort((a, b) => a - b);

  return {
    allSpans: data.allSpans,
    columnarSpans: cs,
    spanMeta: data.spanMeta,
    childrenByParent: data.childrenByParent,
    unmatchedSpans: data.unmatchedSpans,
    maxDepth: data.maxDepth,
    spanNames: [...names].sort(),
    durationsByName,
  };
}

// ── Filtering (text, percentile, chips) ──────────────────────────────────

/** The AND-combined span filter state (uiPrefs.spanFilter/spanPctFilter/
 *  selectedSpanNames). */
export interface SpanFilterState {
  /** Case-insensitive substring over name + field key/value. */
  text: string;
  /** 0 = All, else 50/90/95/99 - the percentile floor. */
  pctFloor: number;
  /** Selected legend-chip names; empty = no name filter. */
  selectedNames: ReadonlySet<string>;
}

/** True when no filter is active (all three empty). */
export function isFilterActive(filter: SpanFilterState): boolean {
  return (
    filter.text.length > 0 ||
    filter.pctFloor > 0 ||
    filter.selectedNames.size > 0
  );
}

/**
 * Whether `span` passes the AND-combined filter: name-chip membership, then the
 * percentile floor of its name's duration distribution, then the
 * case-insensitive substring over the span name and every field key/value.
 */
export function spanMatchesFilter(
  span: TracingSpan,
  filter: SpanFilterState,
  durationsByName: ReadonlyMap<string, readonly number[]>,
): boolean {
  if (filter.selectedNames.size > 0 && !filter.selectedNames.has(span.spanName)) {
    return false;
  }
  if (filter.pctFloor > 0) {
    const durs = durationsByName.get(span.spanName);
    if (durs === undefined) return false;
    const thresholdIdx = Math.floor((durs.length * filter.pctFloor) / 100);
    const threshold = durs[thresholdIdx] ?? 0;
    if (span.end - span.start < threshold) return false;
  }
  if (filter.text.length === 0) return true;
  const f = filter.text.toLowerCase();
  if (span.spanName.toLowerCase().includes(f)) return true;
  for (const [k, v] of Object.entries(span.fields)) {
    if (k.toLowerCase().includes(f) || String(v).toLowerCase().includes(f)) {
      return true;
    }
  }
  return false;
}

/**
 * Percentile rank (0..100) of `span` within its name's duration distribution,
 * or null when the name has no recorded durations.
 */
export function spanPercentileRank(
  span: TracingSpan,
  durationsByName: ReadonlyMap<string, readonly number[]>,
): number | null {
  const durs = durationsByName.get(span.spanName);
  if (durs === undefined || durs.length === 0) return null;
  const dur = span.end - span.start;
  let rank = 0;
  for (const d of durs) if (d <= dur) rank++;
  return Math.round((rank / durs.length) * 100);
}

// ── Visibility window (binary search) ────────────────────────────────────

/**
 * First index in `allSpans` (sorted by start) whose `start` is strictly greater
 * than `viewEnd` - the exclusive right bound of the candidate window. Binary
 * search: spans at/after this index start after the view and cannot be visible,
 * so the per-frame scan never walks the whole array's tail.
 */
export function upperBoundByStart(
  allSpans: readonly TracingSpan[],
  viewEnd: number,
): number {
  let lo = 0;
  let hi = allSpans.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (allSpans[mid]!.start <= viewEnd) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/**
 * Spans that overlap [viewStart, viewEnd] AND pass the filter. On the columnar
 * path BOTH edges are bounded: `ColumnarSpans.windowRows` binary-searches the
 * right edge (start <= viewEnd) and the left edge via the maxSpanDur interval
 * bound (start >= viewStart - maxSpanDur), so panning right no longer walks the
 * whole prefix from row 0 each frame. Rows in the band still test `end >=
 * viewStart` (a maxSpanDur-wide band admits some non-overlappers). Output is
 * identical to the old full-prefix scan.
 */
export function filterVisibleSpans(
  data: SpanTrackData,
  viewStart: number,
  viewEnd: number,
  filter: SpanFilterState,
): TracingSpan[] {
  const cs = data.columnarSpans;
  const out: TracingSpan[] = [];
  if (cs) {
    const { lo, hi } = cs.windowRows(viewStart, viewEnd);
    for (let i = lo; i < hi; i++) {
      if (cs.end[i]! < viewStart) continue;
      const s = cs.at(i);
      if (!spanMatchesFilter(s, filter, data.durationsByName)) continue;
      out.push(s);
    }
    return out;
  }
  const hi = upperBoundByStart(data.allSpans, viewEnd);
  for (let i = 0; i < hi; i++) {
    const s = data.allSpans[i]!;
    if (s.end < viewStart) continue;
    if (!spanMatchesFilter(s, filter, data.durationsByName)) continue;
    out.push(s);
  }
  return out;
}

// ── Render model (viewport/filter/focus tier) ────────────────────────────

/** Cluster x-grid + bar height. */
export const CLUSTER_X_PX = 6;
export const BASE_BAR_H = 4;

/** A drawn span cluster: final geometry (post focus reposition + height
 *  scaling), so the draw step needs only the highlight set for alpha. */
export interface SpanDrawBucket {
  spans: readonly TracingSpan[];
  representative: TracingSpan;
  /** Draw-area-relative x (0..drawW), clamped (computeSpanLayout's own x). */
  x1: number;
  x2: number;
  y: number;
  h: number;
}

/** Why the render set is empty, for the canvas's resting message. */
export type SpanEmptyReason = "no-visible" | "no-roots" | null;

/** The full per-frame render input for the spans canvas + info readout. */
export interface SpanRenderModel {
  buckets: readonly SpanDrawBucket[];
  /** Info readout: focused -> percentile line; else "N spans · M clusters". */
  info: string;
  /** Log-scale y-axis duration ticks bounds. */
  minDur: number;
  maxDur: number;
  /** Spans in the render set (roots or focused+descendants). */
  renderCount: number;
  emptyReason: SpanEmptyReason;
}

export interface SpanRenderModelOpts {
  data: SpanTrackData;
  viewStart: number;
  viewEnd: number;
  /** Draw-area width in CSS px (drawW from the shared layout). */
  drawW: number;
  /** Canvas height in CSS px (the span-panel draw area). */
  canvasH: number;
  /** The subtree-focused span, or null (selection.focusedSpanId). */
  focusedSpanId: string | null;
  filter: SpanFilterState;
}

/**
 * Height of one drawn bucket: the focused span is pinned tall, its children are
 * enlarged, and multi-span clusters grow by log2(size). Depends only on focus +
 * cluster size (NOT the selection highlight), so it is computed here and the
 * draw step never recomputes layout on a selection change.
 */
function bucketHeight(
  clusterSize: number,
  isFocused: boolean,
  hasFocus: boolean,
  canvasH: number,
): number {
  let h = BASE_BAR_H;
  if (isFocused) {
    h = Math.min(h * 6, canvasH / 3);
  } else if (hasFocus) {
    h = Math.max(h * 3, 12);
    if (clusterSize > 1) h = Math.min(h + Math.log2(clusterSize) * 3, canvasH / 4);
  } else if (clusterSize > 1) {
    h = Math.min(h + Math.log2(clusterSize) * 3, canvasH / 4);
  }
  return h;
}

/** Rich focus readout: `name: dur (P% of N) · P50=.. P99=..`. */
export function focusInfoLine(
  span: TracingSpan,
  durationsByName: ReadonlyMap<string, readonly number[]>,
): string {
  const dur = span.end - span.start;
  const durs = durationsByName.get(span.spanName) ?? [];
  const n = durs.length;
  const pct = n > 0 ? spanPercentileRank(span, durationsByName) : null;
  const pctStr = pct != null ? `P${pct} of ${n}` : `${n}`;
  const p50 = n > 0 ? durs[Math.floor(n * 0.5)] ?? dur : dur;
  const p99 = n > 0 ? durs[Math.floor(n * 0.99)] ?? dur : dur;
  return (
    `${span.spanName}: ${formatHumanDuration(dur)} (${pctStr}) · ` +
    `P50=${formatHumanDuration(p50)} P99=${formatHumanDuration(p99)}`
  );
}

/**
 * Build the per-frame render model: visibility window -> filter -> render set
 * (roots-only or focused+descendants) -> computeSpanLayout -> focus
 * repositioning + height scaling. Pure and cheap enough to memoize on its
 * primitive inputs; the DRAW step consumes `buckets` plus the selection
 * highlight set (spanHighlight) and never recomputes this on a dim change.
 */
export function buildSpanRenderModel(opts: SpanRenderModelOpts): SpanRenderModel {
  const { data, viewStart, viewEnd, drawW, canvasH, focusedSpanId, filter } = opts;
  if (drawW <= 0 || viewEnd <= viewStart) {
    return { buckets: [], info: "", minDur: 0, maxDur: 0, renderCount: 0, emptyReason: "no-visible" };
  }

  const visible = filterVisibleSpans(data, viewStart, viewEnd, filter);
  if (visible.length === 0) {
    return { buckets: [], info: "", minDur: 0, maxDur: 0, renderCount: 0, emptyReason: "no-visible" };
  }

  const renderSet = selectSpanRenderSet({
    // selectSpanRenderSet's typed signature takes a mutable array.
    allSpans: visible,
    focusedSpanId,
    childrenByParent: data.childrenByParent,
  });
  if (renderSet.length === 0) {
    return {
      buckets: [],
      info: "0 spans · 0 clusters",
      minDur: 0,
      maxDur: 0,
      renderCount: 0,
      emptyReason: "no-roots",
    };
  }

  const layout = computeSpanLayout({
    spans: renderSet,
    viewStart,
    viewEnd,
    drawW,
    panelH: canvasH,
    clusterXPx: CLUSTER_X_PX,
    barH: BASE_BAR_H,
  });

  const hasFocus = focusedSpanId != null;
  // Focus repositioning: pin the focused cluster at the top, pack the rest just
  // below it.
  const PARENT_Y = 4;
  const CHILD_Y = 34;
  const focusBucketExists =
    hasFocus && layout.buckets.some((b) => b.representative.spanId === focusedSpanId);

  const buckets: SpanDrawBucket[] = layout.buckets.map((b: SpanLayoutBucket<TracingSpan>) => {
    const isFocused = b.representative.spanId === focusedSpanId;
    const h = bucketHeight(b.spans.length, isFocused, hasFocus, canvasH);
    let y = b.y;
    if (focusBucketExists) y = isFocused ? PARENT_Y : CHILD_Y;
    return {
      spans: b.spans,
      representative: b.representative,
      x1: b.x1,
      // Hit-test / draw minimum width.
      x2: Math.max(b.x2, b.x1 + 4),
      y,
      h,
    };
  });

  let info: string;
  if (hasFocus) {
    const cs = data.columnarSpans;
    let focused: TracingSpan | undefined;
    if (cs) { const r = cs.spanIdToRow.get(focusedSpanId!); focused = r === undefined ? undefined : cs.at(r); }
    else focused = data.allSpans.find((s) => s.spanId === focusedSpanId);
    info = focused
      ? focusInfoLine(focused, data.durationsByName)
      : `${renderSet.length} spans · ${layout.buckets.length} clusters (focused)`;
  } else {
    info = `${renderSet.length} spans · ${layout.buckets.length} clusters`;
  }

  return {
    buckets,
    info,
    minDur: layout.minDur,
    maxDur: layout.maxDur,
    renderCount: renderSet.length,
    emptyReason: null,
  };
}

// ── Focus chain + selection-driven dimming ───────────────────────────────

/**
 * The focus highlight chain for a clicked span: the span itself plus its
 * ancestor chain walked through spanMeta parents. Cycle-safe: stops when a
 * parent is already in the chain. This is the set written to
 * selection.spanFocus.chain and the set the lanes highlight; here it also drives
 * the spans-track dimming.
 */
export function spanFocusChain(
  spanId: string,
  data: SpanTrackData,
): Set<string> {
  const chain = new Set<string>([spanId]);
  const cs = data.columnarSpans;
  const parentOf = cs
    ? (id: string): string | null => { const r = cs.spanIdToRow.get(id); return r === undefined ? null : cs.parentSpanIdAt(r); }
    : (id: string): string | null => data.spanMeta.get(id)?.parentSpanId ?? null;
  let parentId = parentOf(spanId);
  while (parentId != null && !chain.has(parentId)) {
    chain.add(parentId);
    parentId = parentOf(parentId);
  }
  return chain;
}

/**
 * The dim/highlight decision for the spans track, derived from the selection
 * slice. When a span is focused (`selection.spanFocus`), the focused span + its
 * ancestor chain are the highlight set and every other cluster dims to the
 * background.
 *
 * The dimming is driven by the SPAN highlight set, not by a bare task selection:
 * clicking a poll sets `selectedTaskId` but does NOT dim the spans panel. Only
 * selecting a span - the selection this track owns - dims the non-selected spans.
 */
export interface SpanHighlight {
  /** Span ids drawn bright; everything else dims when `active`. */
  set: ReadonlySet<string>;
  /** True when a highlight is active (non-highlighted clusters recede). */
  active: boolean;
}

export function spanHighlight(selection: Pick<SelectionSlice, "spanFocus">): SpanHighlight {
  const focus = selection.spanFocus;
  if (focus === null || focus.chain.size === 0) {
    return { set: EMPTY_HIGHLIGHT, active: false };
  }
  return { set: focus.chain, active: true };
}

const EMPTY_HIGHLIGHT: ReadonlySet<string> = new Set<string>();

// ── Legend chips + metadata rows declarative models ──────────────────────

/** One legend chip's declarative model: keyed template input. */
export interface SpanChipModel {
  name: string;
  color: string;
  active: boolean;
}

/**
 * The legend chip models for the current selection - a keyed template input
 * (declarative keyed chips, not innerHTML rebuild + relisten).
 */
export function spanChipModels(
  data: SpanTrackData,
  selectedNames: ReadonlySet<string>,
  colorOf: (name: string) => string,
): SpanChipModel[] {
  return data.spanNames.map((name) => ({
    name,
    color: colorOf(name),
    active: selectedNames.has(name),
  }));
}

/** One metadata k/v row for the focused span's label area. */
export interface SpanFieldRow {
  key: string;
  /** Display value (unit-aware, formatFieldValue). */
  display: string;
  /** Raw value copied by the copy button. */
  copy: string;
}

/**
 * The focused span's label content: its name plus a unit-aware k/v row per field
 * (each row gets a copy button). Returns null when nothing is focused (the label
 * shows the plain "Spans" title). Prefers the live span's fields, falling back
 * to spanMeta.
 */
export interface SpanLabelModel {
  name: string;
  rows: SpanFieldRow[];
}

export function spanLabelModel(
  focusedSpanId: string | null,
  data: SpanTrackData,
): SpanLabelModel | null {
  if (focusedSpanId == null) return null;
  const cs = data.columnarSpans;
  let fields: Record<string, DecodedFieldValue>;
  let name: string;
  if (cs) {
    const r = cs.spanIdToRow.get(focusedSpanId);
    fields = r === undefined ? {} : cs.fieldsAt(r);
    name = r === undefined ? "Span" : cs.spanNameAt(r);
  } else {
    const meta = data.spanMeta.get(focusedSpanId);
    const span = data.allSpans.find((s) => s.spanId === focusedSpanId);
    fields = span?.fields ?? meta?.fields ?? {};
    name = meta?.spanName ?? span?.spanName ?? "Span";
  }
  const rows: SpanFieldRow[] = Object.entries(fields).map(([key, value]) => ({
    key,
    display: formatFieldValue(value),
    copy: String(value),
  }));
  return { name, rows };
}

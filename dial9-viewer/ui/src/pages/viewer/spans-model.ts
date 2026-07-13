// src/pages/viewer/spans-model.ts - the spans track's PURE derivation +
// filtering + focus + dimming logic (T26; features/02 section J; legacy
// renderSpanPanel / spanMatchesFilter / selectSpanRenderSet / computeSpanLayout
// in viewer.html). No store, no DOM, no canvas: every function here is
// referentially transparent so the spans-track component (spans-track.ts) can
// cache the trace-invariant half in a store `derived()` (perf finding F5) and
// unit-test the filter/focus/dim halves directly (the DoD's Vitest checks).
//
// The split mirrors F5's two tiers:
//   1. computeSpanTrackData(customEvents) - TRACE-INVARIANT: buildSpanData plus
//      the per-name duration index (percentile filter J8) and the sorted name
//      list (legend chips J9). Recomputed ONLY when the trace slice changes;
//      the component wraps it in store.derived(["trace"], ...).
//   2. buildSpanRenderModel(...) - VIEWPORT/FILTER/FOCUS-dependent: the visible
//      window (binary-searched, F5/F6 - never an O(allSpans) scan), the
//      name/text/percentile filter (J7-J9), selectSpanRenderSet (roots-only or
//      focused+descendants), computeSpanLayout, and the focus repositioning
//      (J2). Produces final bucket geometry so the DRAW step only needs the
//      selection-derived highlight set for alpha (S4 dimming stays a cheap
//      redraw, never a layout recompute).
//
// T17 obligation (audit notes 6+7): incomplete spans are NEVER silently
// dropped. buildSpanData's `unmatchedSpans` (enter without exit - trace ended
// mid-span OR a segment rotated) is surfaced as the J13 warning by the
// component; this file carries the count through so the consumer exists.

import {
  buildSpanData,
  collectDescendants,
  computeSpanLayout,
  selectSpanRenderSet,
  formatFieldValue,
  formatHumanDuration,
} from "../../lib/trace/index.js";
import type {
  CustomTraceEvent,
  SpanData,
  SpanLayoutBucket,
  TracingSpan,
  UnmatchedSpan,
} from "../../lib/trace/index.js";
import type { SelectionSlice } from "../../types/state.js";

// ── Trace-invariant span data (F5 tier 1) ───────────────────────────────

/** Per-span-name metadata carried out of buildSpanData for J3/J4. */
export interface SpanNameMeta {
  spanName: string;
  fields: Record<string, unknown>;
  parentSpanId: string | null;
}

/**
 * Everything about a trace's tracing spans that does NOT depend on the
 * viewport, filter, or selection - computed once per trace load and cached
 * in a store `derived(["trace"], ...)` (F5). `allSpans` is sorted by start
 * (buildSpanData's contract), which the visibility binary search relies on.
 */
export interface SpanTrackData {
  allSpans: readonly TracingSpan[];
  spanMeta: SpanData["spanMeta"];
  childrenByParent: SpanData["childrenByParent"];
  /** Spans with an enter but no exit (J13; the truncation/incompleteness
   *  surface - T17 audit notes 6+7: surfaced, never dropped). */
  unmatchedSpans: readonly UnmatchedSpan[];
  maxDepth: number;
  /** Unique span names, sorted - the legend chip set (J9). */
  spanNames: readonly string[];
  /**
   * Per-name duration lists (ns), each sorted ascending, for the percentile
   * filter (J8) and percentile-rank readouts (J5/J6). Legacy getSpanDurations.
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
 * buildSpanData (frozen core) plus the per-name duration index and the sorted
 * name list. Returns EMPTY_SPAN_TRACK_DATA when there are no spans so the
 * track renders its resting state without branching.
 */
export function computeSpanTrackData(
  customEvents: readonly CustomTraceEvent[] | null | undefined,
): SpanTrackData {
  if (customEvents == null || customEvents.length === 0) {
    return EMPTY_SPAN_TRACK_DATA;
  }
  const data = buildSpanData(customEvents);
  if (data.allSpans.length === 0 && data.unmatchedSpans.length === 0) {
    return EMPTY_SPAN_TRACK_DATA;
  }

  const names = new Set<string>();
  const durationsByName = new Map<string, number[]>();
  for (const s of data.allSpans) {
    names.add(s.spanName);
    let durs = durationsByName.get(s.spanName);
    if (durs === undefined) {
      durs = [];
      durationsByName.set(s.spanName, durs);
    }
    durs.push(s.end - s.start);
  }
  for (const durs of durationsByName.values()) durs.sort((a, b) => a - b);

  return {
    allSpans: data.allSpans,
    spanMeta: data.spanMeta,
    childrenByParent: data.childrenByParent,
    unmatchedSpans: data.unmatchedSpans,
    maxDepth: data.maxDepth,
    spanNames: [...names].sort(),
    durationsByName,
  };
}

// ── Filtering (J7 text, J8 percentile, J9 chips) ─────────────────────────

/** The AND-combined span filter state (uiPrefs.spanFilter/spanPctFilter/
 *  selectedSpanNames). */
export interface SpanFilterState {
  /** Case-insensitive substring over name + field key/value (J7). */
  text: string;
  /** 0 = All, else 50/90/95/99 - the percentile floor (J8). */
  pctFloor: number;
  /** Selected legend-chip names; empty = no name filter (J9). */
  selectedNames: ReadonlySet<string>;
}

/** True when no filter is active (all three empty) - legacy `hasFilter`. */
export function isFilterActive(filter: SpanFilterState): boolean {
  return (
    filter.text.length > 0 ||
    filter.pctFloor > 0 ||
    filter.selectedNames.size > 0
  );
}

/**
 * Whether `span` passes the AND-combined filter (legacy spanMatchesFilter,
 * viewer.html:1113-1131): name-chip membership, then the percentile floor of
 * its name's duration distribution, then the case-insensitive substring over
 * the span name and every field key/value.
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
 * or null when the name has no recorded durations (legacy spanPercentileRank).
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

// ── Visibility window (binary search - F5/F6) ────────────────────────────

/**
 * First index in `allSpans` (sorted by start) whose `start` is strictly
 * greater than `viewEnd` - the exclusive right bound of the candidate window.
 * Binary search: spans at/after this index start after the view and cannot be
 * visible, so the per-frame scan never walks the whole array's tail (F5/F6:
 * no O(allSpans) visibility scan per frame).
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
 * Spans that overlap [viewStart, viewEnd] AND pass the filter. The right edge
 * is binary-searched (upperBoundByStart); the candidate prefix is then scanned
 * for `end >= viewStart` (a long-running span with a small start survives).
 * The prefix scan's left edge is not itself binary-bounded (that needs a
 * max-end interval index); it is a strict improvement over the legacy
 * whole-array scan and is where the T17 windowing would later clip.
 */
export function filterVisibleSpans(
  data: SpanTrackData,
  viewStart: number,
  viewEnd: number,
  filter: SpanFilterState,
): TracingSpan[] {
  const hi = upperBoundByStart(data.allSpans, viewEnd);
  const out: TracingSpan[] = [];
  for (let i = 0; i < hi; i++) {
    const s = data.allSpans[i]!;
    if (s.end < viewStart) continue;
    if (!spanMatchesFilter(s, filter, data.durationsByName)) continue;
    out.push(s);
  }
  return out;
}

// ── Render model (viewport/filter/focus tier) ────────────────────────────

/** Cluster x-grid + bar height (legacy CLUSTER_X / BAR_H constants). */
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
  /** J5 info readout: focused -> percentile line; else "N spans · M clusters". */
  info: string;
  /** Log-scale y-axis duration ticks bounds (legacy minDur/maxDur). */
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
  /** Canvas height in CSS px (the span-panel draw area, legacy ph=120). */
  canvasH: number;
  /** The subtree-focused span, or null (selection.focusedSpanId, J2). */
  focusedSpanId: string | null;
  filter: SpanFilterState;
}

/**
 * Height of one drawn bucket (legacy renderSpanPanel bar-height scaling):
 * the focused span is pinned tall, its children are enlarged, and multi-span
 * clusters grow by log2(size). Depends only on focus + cluster size (NOT the
 * selection highlight), so it is computed here and the draw step never
 * recomputes layout on a selection change (S4).
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

/** Rich J5 focus readout: `name: dur (P% of N) · P50=.. P99=..`. */
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
  // Focus repositioning (J2): pin the focused cluster at the top, pack the
  // rest just below it. Matches the legacy PARENT_Y / CHILD_Y placement.
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
      // Hit-test / draw minimum width (legacy x2 = max(x2, x1+4)).
      x2: Math.max(b.x2, b.x1 + 4),
      y,
      h,
    };
  });

  let info: string;
  if (hasFocus) {
    const focused = data.allSpans.find((s) => s.spanId === focusedSpanId);
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

// ── Focus chain (J2) + selection-driven dimming (S4) ─────────────────────

/**
 * The focus highlight chain for a clicked span: the span itself plus its
 * ancestor chain walked through spanMeta parents (legacy click handler
 * viewer.html:3861-3868). Cycle-safe: stops when a parent is already in the
 * chain. This is the set written to selection.spanFocus.chain and the set the
 * lanes highlight; here it also drives the spans-track dimming.
 */
export function spanFocusChain(
  spanId: string,
  data: SpanTrackData,
): Set<string> {
  const chain = new Set<string>([spanId]);
  const first = data.spanMeta.get(spanId);
  let parentId = first?.parentSpanId ?? null;
  while (parentId != null && !chain.has(parentId)) {
    chain.add(parentId);
    parentId = data.spanMeta.get(parentId)?.parentSpanId ?? null;
  }
  return chain;
}

/**
 * The dim/highlight decision for the spans track, derived from the selection
 * slice (S4). When a span is focused (`selection.spanFocus`), the focused span
 * + its ancestor chain are the highlight set and every other cluster dims to
 * the background (legacy `selectedSpanIds`-driven alpha in renderSpanPanel).
 *
 * Faithful to the legacy: the dimming is driven by the SPAN highlight set, not
 * by a bare task selection (clicking a poll set `selectedTaskId` but did NOT
 * dim the spans panel). Selecting a span - the selection this track owns - is
 * a genuine selection-slice change that dims the non-selected spans, which is
 * exactly the S4 (partial) reaction the DoD asserts.
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

// ── Legend chips (J9) + metadata rows (J3) declarative models ────────────

/** One legend chip's declarative model (J9): keyed template input. */
export interface SpanChipModel {
  name: string;
  color: string;
  active: boolean;
}

/**
 * The legend chip models for the current selection - a keyed template input
 * (perf finding F7: declarative keyed chips, not innerHTML rebuild + relisten).
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

/** One metadata k/v row for the focused span's label area (J3/J4). */
export interface SpanFieldRow {
  key: string;
  /** Display value (unit-aware, formatFieldValue). */
  display: string;
  /** Raw value copied by the J4 copy button. */
  copy: string;
}

/**
 * The focused span's label content (J3): its name plus a unit-aware k/v row
 * per field (each row gets a J4 copy button). Returns null when nothing is
 * focused (the label shows the plain "Spans" title). Prefers the live span's
 * fields, falling back to spanMeta (matching the legacy updateSpanPanelLabel).
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
  const meta = data.spanMeta.get(focusedSpanId);
  const span = data.allSpans.find((s) => s.spanId === focusedSpanId);
  const fields = span?.fields ?? meta?.fields ?? {};
  const name = meta?.spanName ?? span?.spanName ?? "Span";
  const rows: SpanFieldRow[] = Object.entries(fields).map(([key, value]) => ({
    key,
    display: formatFieldValue(value),
    copy: String(value),
  }));
  return { name, rows };
}

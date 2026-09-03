// The worker-lanes canvas draw pass: a pure render(ctx, input, layout) function
// under these render rules:
//
//   - Pixel-bounded FILLS via pixelDownsampleSpans + makeBarCoalescer
//     (lib/canvas/downsample) - one representative per pixel column, coalesced
//     runs.
//   - Pixel-bounded, style-BATCHED strokes via the stroke batcher
//     (lib/canvas/stroke): the local-queue step line and every dashed marker
//     are downsampled to per-column vertices and drawn one path per style, NOT
//     one setLineDash + stroke() per item (stroke() was 76% of pan-storm CPU).
//   - No per-frame re-derivation: the worker spans, queue series and span index
//     are derived once (data.ts, store-cached) and handed in; this function
//     only reads per-frame inputs (viewport, selection).
//
// The unified-column layout gives the lanes ONE canvas. N worker rows are
// stacked into it; each row's internal band geometry is a 60px reference scaled
// by `sf = laneH / LANE_REF_H`, so the poll band, queue step area, ticks and
// triangles keep their proportions at any row height.

import type {
  ActiveSpan,
  BlockInPlaceGap,
  PollSpan,
  TracingSpan,
  WorkerWake,
} from "../../../types/trace.js";
import type { TimePanelLayout } from "../../../types/state.js";
import {
  makeBarCoalescer,
  pixelDownsampleSpans,
} from "../../../lib/canvas/downsample.js";
import {
  drawStrokeBatches,
  makeStrokeBatcher,
  type StrokePathContext,
  type StrokeStyleSpec,
} from "../../../lib/canvas/stroke.js";
import { pollColor } from "../../../lib/canvas/palette.js";
import { laneRowLayout } from "../../../lib/canvas/layout.js";
import type { LaneRowLayout } from "../../../lib/canvas/layout.js";
import type { RuntimeMetrics } from "../../../lib/trace/runtime-metrics-model.js";
import { drawRuntimeMetricsLane } from "./runtime-metrics-lane.js";
import { DEFAULT_ACCENT, RAIL_W, type LaneIdentity } from "./chrome.js";
import { findSpanAt, type SpanList } from "../../../lib/trace/query.js";
import {
  isColumnarLane,
  type LaneSpans,
  type ParkLike,
} from "../../../lib/trace/columnar-worker-spans.js";
import type { SpanByIdMulti } from "../../../lib/trace/columnar-spans.js";

/** The per-lane reference height. Band offsets below are expressed against
 *  this and scaled to the real row height. */
export const LANE_REF_H = 60;

/** The FIXED per-worker row height. Rows no longer flex to fit the track: N
 *  workers stack at LANE_ROW_H each and the lanes box scrolls, so 64+ workers
 *  stay legible + clickable instead of collapsing to slivers. Equal to
 *  LANE_REF_H so the band scale factor is 1. */
export const LANE_ROW_H = 60;

/** Runtime-group header band height (drawn only when >1 runtime group). */
export const RUNTIME_HEADER_H = 24;

/** Divider between adjacent worker lanes, so row boundaries are unambiguous. */
const LANE_DIVIDER = "#0b0e18";

/** The minimal 2D-context surface renderLanes needs (Node-testable). */
export interface LaneDrawContext extends StrokePathContext {
  fillStyle: string | CanvasGradient | CanvasPattern;
  font: string;
  textAlign: CanvasTextAlign;
  clearRect(x: number, y: number, w: number, h: number): void;
  fillRect(x: number, y: number, w: number, h: number): void;
  fillText(text: string, x: number, y: number): void;
  fill(): void;
  closePath(): void;
}

/** Per-frame render inputs (all derived/selection state, no live store). */
export interface LanesRenderInput {
  /** Worker ids in render order (top to bottom). */
  workerIds: readonly number[];
  /** Reconstructed spans per worker (buildWorkerSpans + attachCpuSamples). */
  workerSpans: Readonly<Record<number, LaneSpans>>;
  /** Per-runtime scheduler metrics, drawn into each runtime's summary lane.
   *  Empty for pre-RuntimeMetrics traces (no summary lanes emitted). */
  runtimeMetrics: RuntimeMetrics;
  /** Runtime group name -> sorted task-spawn timestamps. */
  runtimeTaskSpawns: ReadonlyMap<string, readonly number[]>;
  /** worker id -> its owning runtime's accent (the lane's left rail). A worker
   *  absent from this map falls back to the default accent, so a caller that has
   *  not built identities still gets a rail. */
  laneIdentity: ReadonlyMap<number, LaneIdentity>;
  /** Runtime-group name -> its accent, so a group header carries the same colour
   *  as its lanes' rails. Absent name -> the default accent. */
  runtimeAccents: ReadonlyMap<string, string>;
  /** Per-worker local-queue samples, sorted by t. */
  workerQueueSamples: Readonly<Record<number, readonly { t: number; local: number }[]>>;
  /** Wake events indexed by target worker. */
  wakesByWorker: Readonly<Record<number, readonly WorkerWake[]>>;
  /** span id -> every span instance sharing it (highlight lookup). */
  spansById: SpanByIdMulti;
  /** Block-in-place handoff gaps (hatched overlay). */
  blockInPlaceGaps: readonly BlockInPlaceGap[];
  hasCpuTime: boolean;
  hasSchedWait: boolean;
  /** False when `workerQueueSamples` carries sentinel zeros, not measurements. */
  hasLocalQueueDepth: boolean;
  viewStart: number;
  viewEnd: number;
  /** Selected task -> yellow polls + wake markers. */
  selectedTaskId: number | null;
  /** Focused span + ancestor chain -> yellow poll outlines. */
  selectedSpanIds: ReadonlySet<string>;
  /** Hovered waker task -> orange polls. */
  hoveredWakerTaskId: number | null;
  /** Pinned custom-event poll -> single yellow bar (the in-lane mark). */
  pinnedPoll: PollSpan | null;
  /** Shared visible local-queue max across all workers (scale). */
  sharedMaxQ: number;
  /** Cached channel-multiply dimmer for non-selected polls. */
  dimmer: (color: string) => string;
}

/** Where and how big the lanes canvas is this frame. */
export interface LanesLayout {
  /** Shared ns<->x mapping (labelW-shifted; we subtract labelW for local x). */
  time: TimePanelLayout;
  /** Visible canvas height in CSS px (the resizeable lanes box; the canvas is
   *  sized to this, NOT to the full stacked content - the content scrolls). */
  height: number;
  /**
   * The runtime-aware vertical stack (fixed-height worker rows + optional
   * headers), lanes-local before scroll. Omit for a headerless fixed-height
   * stack over `input.workerIds` (used by unit tests + the single-runtime
   * fallback).
   */
  rowLayout?: LaneRowLayout;
  /** Scroll offset of the lanes box (px). Rows draw at `row.y - scrollTop` and
   *  the canvas bounds clip anything scrolled past its edges. Default 0. */
  scrollTop?: number;
}

const spanDur = (s: { start: number; end: number }): number => s.end - s.start;

/** First index whose span could be visible (end >= viewStart); binary search
 *  over a start-sorted, non-overlapping span list. Reads via `.at(i)` so a
 *  columnar lane view backs it (arrays' `.at` returns the same element `[i]`
 *  would). */
function firstVisible(
  spans: { readonly length: number; at(i: number): { start: number; end: number } | undefined },
  viewStart: number,
): number {
  let lo = 0;
  let hi = spans.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (spans.at(mid)!.end < viewStart) lo = mid + 1;
    else hi = mid - 1;
  }
  return lo;
}

/** Largest index i with arr[i].t <= val (or -1); binary search. */
function lowerBoundT(arr: readonly { t: number }[], val: number): number {
  let lo = 0;
  let hi = arr.length - 1;
  let ans = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (arr[mid]!.t <= val) {
      ans = mid;
      lo = mid + 1;
    } else hi = mid - 1;
  }
  return ans;
}

/** Smallest index i with arr[i].t >= val (or arr.length); binary search. */
function upperBoundT(arr: readonly { t: number }[], val: number): number {
  let lo = 0;
  let hi = arr.length - 1;
  let ans = arr.length;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (arr[mid]!.t >= val) {
      ans = mid;
      hi = mid - 1;
    } else lo = mid + 1;
  }
  return ans;
}

/** Style keys are namespaced per row ("queue:w0") so ticks dedupe within a
 *  lane, not across lanes; styleOf reads the kind prefix. */
function strokeStyleOf(styleKey: string): StrokeStyleSpec {
  const kind = styleKey.slice(0, styleKey.indexOf(":"));
  switch (kind) {
    case "queue":
      return { strokeStyle: "rgba(255,200,50,0.8)", lineWidth: 1.5 };
    case "openpoll":
      return { strokeStyle: "#ff9800", lineWidth: 2, lineDash: [3, 2] };
    case "gap":
      return { strokeStyle: "#ff9800", lineWidth: 1, lineDash: [4, 4] };
    case "sep":
      return { strokeStyle: "#333", lineWidth: 0.5 };
    default:
      return { strokeStyle: "#888", lineWidth: 1 };
  }
}

/**
 * Compute the max visible local-queue depth across all workers (a SHARED scale
 * so lanes are comparable). Bounded to the visible window by binary search;
 * never scans the whole series. Minimum 1.
 */
export function sharedVisibleMaxQueue(
  workerIds: readonly number[],
  workerQueueSamples: Readonly<Record<number, readonly { t: number; local: number }[]>>,
  viewStart: number,
  viewEnd: number,
): number {
  let max = 1;
  for (const w of workerIds) {
    const samples = workerQueueSamples[w];
    if (!samples || samples.length === 0) continue;
    const start = Math.max(0, lowerBoundT(samples, viewStart));
    const end = Math.min(samples.length - 1, upperBoundT(samples, viewEnd));
    for (let i = start; i <= end; i++) {
      const local = samples[i]!.local;
      if (local > max) max = local;
    }
  }
  return max;
}

/**
 * Draw every worker lane into `ctx`, plus the selection/hover highlights. Pure
 * over its inputs; the only mutable state is the canvas. The context transform
 * is assumed already DPR-scaled by the caller (lib/canvas/dpr sizer), so all
 * coordinates are CSS px.
 */
export function renderLanes(
  ctx: LaneDrawContext,
  input: LanesRenderInput,
  layout: LanesLayout,
): void {
  const drawW = layout.time.drawW;
  const viewportH = layout.height;
  const n = input.workerIds.length;
  if (drawW <= 0 || viewportH <= 0 || n === 0) return;

  const scrollTop = layout.scrollTop ?? 0;
  // Fixed row height (no divide-by-N collapse); sf = 1 since LANE_ROW_H ==
  // LANE_REF_H, but keep it derived so the reference can change independently.
  const laneH = LANE_ROW_H;
  const sf = laneH / LANE_REF_H;
  // Rows: the runtime-aware stack, or a single headerless group over the flat
  // worker list (unit tests / single-runtime fallback).
  const rowLayout =
    layout.rowLayout ??
    laneRowLayout(
      [{ name: "", inferred: true, workerIds: [...input.workerIds] }],
      laneH,
      RUNTIME_HEADER_H,
    );

  const labelW = layout.time.labelW;
  const nsToX = (ns: number): number => layout.time.nsToPanelX(ns) - labelW;
  const { viewStart, viewEnd } = input;

  // One batcher for the whole canvas: strokes accumulate per (kind,row) style
  // key and stroke ONCE each at the end.
  const batcher = makeStrokeBatcher();

  ctx.clearRect(0, 0, drawW, viewportH);

  const viewBot = scrollTop + viewportH;
  for (const r of rowLayout.rows) {
    // Cull rows fully outside the visible scroll window; partially-visible rows
    // draw at a negative/overflowing top and the canvas bounds clip them.
    if (r.y + r.height <= scrollTop || r.y >= viewBot) continue;
    const top = r.y - scrollTop;

    if (r.kind === "header") {
      drawRuntimeHeader(
        ctx,
        r,
        input.runtimeAccents.get(r.name) ?? DEFAULT_ACCENT,
        drawW,
        top,
      );
      continue;
    }

    if (r.kind === "runtime-metrics") {
      // The inferred default group is labelled "main" but its metrics are keyed
      // under the empty runtime name (the unnamed default runtime on the wire).
      const series = input.runtimeMetrics.byRuntime.get(r.inferred ? "" : r.name);
      drawRuntimeMetricsLane(
        ctx,
        series,
        input.runtimeTaskSpawns.get(r.name) ?? [],
        r,
        input.runtimeAccents.get(r.name) ?? DEFAULT_ACCENT,
        viewStart,
        viewEnd,
        drawW,
        top,
      );
      continue;
    }

    const workerId = r.workerId;
    const spans = input.workerSpans[workerId];
    if (!spans) continue;
    // Per-row batcher key: the worker's flat index keeps style keys unique +
    // stable across frames so ticks dedupe within a lane, not across lanes.
    const row = r.index;
    // Band geometry scaled from the 60px reference.
    const bandTop = top + 10 * sf;
    const bandH = 20 * sf;
    const sepY = top + 33 * sf;
    const qTop = top + 34 * sf;
    const qH = laneH - 38 * sf;

    drawLaneBackground(ctx, spans, input, nsToX, drawW, top, laneH);
    drawBlockInPlaceGaps(ctx, batcher, row, input, workerId, nsToX, drawW, top, laneH, sf);
    drawParks(ctx, spans, input, nsToX, drawW, top, laneH, sf);
    drawPolls(ctx, batcher, row, spans, input, nsToX, drawW, bandTop, bandH);
    drawCpuTicks(ctx, spans.cpuSampleTimes, viewStart, viewEnd, nsToX, drawW, bandTop, bandH);
    drawSchedTriangles(ctx, spans.polls, input, nsToX, bandTop, bandH);
    drawWakerHighlight(ctx, spans.polls, input, nsToX, drawW, bandTop, bandH);
    drawSelectedSpanOutlines(ctx, spans.polls, workerId, input, nsToX, drawW, bandTop, bandH);
    drawWakeMarkers(ctx, workerId, input, nsToX, top, sf);
    drawQueueStepLine(ctx, batcher, row, workerId, input, nsToX, drawW, qTop, qH, sf);

    // Separator between poll band and queue area (batched solid stroke).
    batcher.polyline(`sep:w${row}`, [
      { x: 0, y: sepY },
      { x: drawW, y: sepY },
    ]);

    // Identity LAST so the rail and divider sit above the lane's marks rather
    // than being overpainted by a park/poll bar at x=0.
    drawLaneIdentity(ctx, input.laneIdentity.get(workerId), drawW, top, laneH);
  }

  // One stroke() per style: the whole canvas's queue lines, dashed markers,
  // gap outlines and separators drain here.
  drawStrokeBatches(ctx, batcher.batches(), strokeStyleOf);
}

/**
 * Draw a worker lane's runtime accent rail and its bottom divider.
 *
 * The lane's IDENTITY (its "W<id>" and the runtime it belongs to) is spelled out
 * in the label gutter (labels.ts), which costs no data pixels; this rail is the
 * link between the two — it carries the runtime's colour continuously from the
 * gutter row across the lane, so a lane's group is readable without tracking back
 * to the gutter, and no poll bar is hidden to say so.
 */
function drawLaneIdentity(
  ctx: LaneDrawContext,
  identity: LaneIdentity | undefined,
  drawW: number,
  top: number,
  laneH: number,
): void {
  // Left rail: full-height accent stripe.
  ctx.fillStyle = identity?.accent ?? DEFAULT_ACCENT;
  ctx.fillRect(0, top, RAIL_W, laneH);
  // Bottom divider, so adjacent rows never blur into one band.
  ctx.fillStyle = LANE_DIVIDER;
  ctx.fillRect(0, top + laneH - 1, drawW, 1);
}

/**
 * Draw a runtime-group header band across the draw area: the runtime's accent
 * rail, a caret (folded -> right, open -> down), the runtime name, and its worker
 * count. Clicking the band folds/unfolds the runtime (lane-interaction). Only
 * reached when a trace has more than one runtime group (e.g. two traces opened
 * together).
 *
 * The name is drawn here AND in the label gutter: the gutter names every row
 * uniformly, while the band is the click target, and a header with an empty band
 * reads as a gap rather than a control.
 */
function drawRuntimeHeader(
  ctx: LaneDrawContext,
  row: { name: string; inferred: boolean; workerCount: number; collapsed: boolean; height: number },
  accent: string,
  drawW: number,
  top: number,
): void {
  ctx.fillStyle = "#1b2036";
  ctx.fillRect(0, top, drawW, row.height);
  // The group's accent, so the header and its lanes' rails read as one unit.
  ctx.fillStyle = accent;
  ctx.fillRect(0, top, RAIL_W, row.height);
  drawCaret(ctx, 14, top + row.height / 2, row.collapsed);
  const label = row.inferred ? `${row.name} runtime` : `runtime: ${row.name}`;
  const count = row.collapsed
    ? `${row.workerCount} worker${row.workerCount === 1 ? "" : "s"} (folded)`
    : `${row.workerCount} worker${row.workerCount === 1 ? "" : "s"}`;
  ctx.fillStyle = "#aeb6e0";
  ctx.font = "bold 11px sans-serif";
  ctx.textAlign = "left";
  ctx.fillText(label, 26, top + row.height - 8);
  ctx.fillStyle = "#6b73a0";
  ctx.font = "11px sans-serif";
  ctx.fillText(count, drawW - 110, top + row.height - 8);
}

/** A small disclosure caret centred at (cx, cy): right-pointing when collapsed,
 *  down-pointing when expanded. */
function drawCaret(ctx: LaneDrawContext, cx: number, cy: number, collapsed: boolean): void {
  ctx.fillStyle = "#8b93c0";
  ctx.beginPath();
  if (collapsed) {
    ctx.moveTo(cx - 3, cy - 4);
    ctx.lineTo(cx + 3, cy);
    ctx.lineTo(cx - 3, cy + 4);
  } else {
    ctx.moveTo(cx - 4, cy - 3);
    ctx.lineTo(cx + 4, cy - 3);
    ctx.lineTo(cx, cy + 3);
  }
  ctx.closePath();
  ctx.fill();
}

function drawLaneBackground(
  ctx: LaneDrawContext,
  spans: LaneSpans,
  input: LanesRenderInput,
  nsToX: (ns: number) => number,
  drawW: number,
  top: number,
  laneH: number,
): void {
  // Base: entire lane "active" (dark).
  ctx.fillStyle = "#1a1e2a";
  ctx.fillRect(0, top, drawW, laneH);

  // CPU scheduling tint: one representative active per pixel column, longest
  // wins. Only when the trace carries CPU time.
  if (!input.hasCpuTime) return;
  let reps: readonly ActiveSpan[];
  if (isColumnarLane(spans)) {
    const startIdx = spans.store.firstVisibleActive(spans.w, input.viewStart);
    reps = spans.store.downsampleActives(
      spans.w, startIdx, input.viewStart, input.viewEnd, drawW,
    );
  } else {
    const startIdx = firstVisible(spans.actives, input.viewStart);
    reps = pixelDownsampleSpans(
      spans.actives, startIdx, input.viewStart, input.viewEnd, drawW, spanDur,
    );
  }
  for (const s of reps) {
    const x1 = Math.max(0, nsToX(s.start));
    const x2 = Math.min(drawW, nsToX(s.end));
    const w = Math.max(x2 - x1, 1);
    ctx.fillStyle =
      s.ratio >= 0.95 ? "#1a2a1a" : s.ratio >= 0.5 ? "#2a2a1a" : "#2a1a1a";
    ctx.fillRect(x1, top, w, laneH);
  }
}

function drawBlockInPlaceGaps(
  ctx: LaneDrawContext,
  batcher: ReturnType<typeof makeStrokeBatcher>,
  row: number,
  input: LanesRenderInput,
  workerId: number,
  nsToX: (ns: number) => number,
  drawW: number,
  top: number,
  laneH: number,
  _sf: number,
): void {
  const gaps = input.blockInPlaceGaps;
  if (gaps.length === 0) return;
  for (const g of gaps) {
    if (g.workerId !== workerId) continue;
    if (g.endNs < input.viewStart || g.startNs > input.viewEnd) continue;
    const x1 = Math.max(0, nsToX(g.startNs));
    const x2 = Math.min(drawW, nsToX(g.endNs));
    const w = Math.max(x2 - x1, 1);
    ctx.fillStyle = "#3a2a00";
    ctx.fillRect(x1, top, w, laneH);
    // Dashed outline via the batcher.
    const l = x1 + 0.5;
    const r = x1 + w - 0.5;
    const t = top + 0.5;
    const b = top + laneH - 0.5;
    batcher.polyline(`gap:w${row}`, [
      { x: l, y: t },
      { x: r, y: t },
      { x: r, y: b },
      { x: l, y: b },
      { x: l, y: t },
    ]);
  }
}

function drawParks(
  ctx: LaneDrawContext,
  spans: LaneSpans,
  input: LanesRenderInput,
  nsToX: (ns: number) => number,
  drawW: number,
  top: number,
  laneH: number,
  sf: number,
): void {
  let reps: readonly ParkLike[];
  if (isColumnarLane(spans)) {
    const startIdx = spans.store.firstVisiblePark(spans.w, input.viewStart);
    reps = spans.store.downsampleParks(
      spans.w, startIdx, input.viewStart, input.viewEnd, drawW,
    );
  } else {
    const startIdx = firstVisible(spans.parks, input.viewStart);
    reps = pixelDownsampleSpans(
      spans.parks, startIdx, input.viewStart, input.viewEnd, drawW, spanDur,
    );
  }
  const accentH = 4 * sf;
  for (const s of reps) {
    const x1 = Math.max(0, nsToX(s.start));
    const x2 = Math.min(drawW, nsToX(s.end));
    const w = Math.max(x2 - x1, 1);
    const schedWait = s.schedWait;
    if (input.hasSchedWait && schedWait != null && schedWait > 0) {
      const wakeupShouldBe = s.end - schedWait;
      if (wakeupShouldBe > s.start) {
        const xSplit = Math.min(drawW, nsToX(wakeupShouldBe));
        const normalW = Math.max(xSplit - x1, 0);
        if (normalW > 0) {
          ctx.fillStyle = "#2a1520";
          ctx.fillRect(x1, top, normalW, laneH);
          ctx.fillStyle = "#cc5533";
          ctx.fillRect(x1, top, normalW, accentH);
        }
        const delayW = Math.max(x2 - xSplit, 0);
        if (delayW > 0) {
          ctx.fillStyle = "#ff0000";
          ctx.fillRect(xSplit, top, delayW, laneH);
        }
      } else {
        ctx.fillStyle = "#ff0000";
        ctx.fillRect(x1, top, w, laneH);
      }
    } else {
      ctx.fillStyle = "#2a1520";
      ctx.fillRect(x1, top, w, laneH);
      ctx.fillStyle = "#cc5533";
      ctx.fillRect(x1, top, w, accentH);
    }
  }
}

function drawPolls(
  ctx: LaneDrawContext,
  batcher: ReturnType<typeof makeStrokeBatcher>,
  row: number,
  spans: LaneSpans,
  input: LanesRenderInput,
  nsToX: (ns: number) => number,
  drawW: number,
  bandTop: number,
  bandH: number,
): void {
  const { viewStart, viewEnd, selectedTaskId } = input;
  let pollStart: number;
  let reps: readonly PollSpan[];
  if (isColumnarLane(spans)) {
    // Columnar path: the store binary-searches + LOD-downsamples the poll
    // columns for the viewport, materializing only the <= drawW representatives.
    pollStart = spans.store.firstVisiblePoll(spans.w, viewStart);
    reps = spans.store.downsamplePolls(
      spans.w, pollStart, viewStart, viewEnd, drawW, selectedTaskId ?? null,
    );
  } else {
    pollStart = firstVisible(spans.polls, viewStart);
    // Representative weighting: latest starter wins a column (an unbiased colour
    // sample, not the worst-case red); a selected task's polls force-win so a
    // short selected poll can't vanish.
    const weight = selectedTaskId
      ? (s: PollSpan): number => (s.taskId === selectedTaskId ? Infinity : s.start)
      : (s: PollSpan): number => s.start;
    reps = pixelDownsampleSpans(
      spans.polls, pollStart, viewStart, viewEnd, drawW, weight,
    );
  }

  const drawCoalesced = (color: (s: PollSpan) => string | null): void => {
    const merge = makeBarCoalescer((x, w, fill) => {
      ctx.fillStyle = fill;
      ctx.fillRect(x, bandTop, w, bandH);
    });
    for (const s of reps) {
      const fill = color(s);
      if (fill == null) continue;
      const x1 = Math.max(0, nsToX(s.start));
      const x2 = Math.min(drawW, nsToX(s.end));
      merge.push(x1, x2, fill);
    }
    merge.flush();
  };

  if (selectedTaskId) {
    // Dim pass (non-selected) then the selected task's polls solid yellow.
    drawCoalesced((s) =>
      s.taskId === selectedTaskId ? null : input.dimmer(pollColor(s.start, s.end)),
    );
    drawCoalesced((s) => (s.taskId === selectedTaskId ? "#ffeb3b" : null));
  } else {
    drawCoalesced((s) => pollColor(s.start, s.end));
  }

  // Open-ended poll markers - dashed right edge, batched (bounded: breaks once
  // start passes viewEnd). On the columnar path read the raw columns directly
  // rather than materializing a PollView (+ CSR sample slices) per poll every
  // frame; the fat path keeps the .at(i) flyweight.
  const nPolls = spans.polls.length;
  if (isColumnarLane(spans)) {
    const { store, w } = spans;
    for (let i = pollStart; i < nPolls; i++) {
      if (store.pollStartAt(w, i) > viewEnd) break;
      if (!store.pollOpenEndedAt(w, i)) continue;
      const x2 = Math.min(drawW, nsToX(store.pollEndAt(w, i)));
      batcher.tick(`openpoll:w${row}`, x2, bandTop, bandTop + bandH);
    }
  } else {
    for (let i = pollStart; i < nPolls; i++) {
      const s = spans.polls.at(i)!;
      if (s.start > viewEnd) break;
      if (!s.openEnded) continue;
      const x2 = Math.min(drawW, nsToX(s.end));
      batcher.tick(`openpoll:w${row}`, x2, bandTop, bandTop + bandH);
    }
  }

  // Pinned custom-event poll highlight (in-lane mark): resolve the worker by
  // value-matching the poll in this lane, then a single yellow bar.
  const pinned = input.pinnedPoll;
  if (pinned && pinned.end >= viewStart && pinned.start <= viewEnd) {
    const hit = findSpanAt(spans.polls, pinned.start);
    if (
      hit &&
      hit.start === pinned.start &&
      hit.end === pinned.end &&
      hit.taskId === pinned.taskId
    ) {
      const x1 = Math.max(0, nsToX(hit.start));
      const x2 = Math.min(drawW, nsToX(hit.end));
      const w = Math.max(x2 - x1, 2);
      ctx.fillStyle = "#ffeb3b";
      ctx.fillRect(x1, bandTop, w, bandH);
      ctx.strokeStyle = "#fff";
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(x1 - 0.5, bandTop - 0.5);
      ctx.lineTo(x1 + w + 0.5, bandTop - 0.5);
      ctx.lineTo(x1 + w + 0.5, bandTop + bandH + 0.5);
      ctx.lineTo(x1 - 0.5, bandTop + bandH + 0.5);
      ctx.lineTo(x1 - 0.5, bandTop - 0.5);
      ctx.stroke();
    }
  }
}

function drawCpuTicks(
  ctx: LaneDrawContext,
  times: readonly number[],
  viewStart: number,
  viewEnd: number,
  nsToX: (ns: number) => number,
  _drawW: number,
  bandTop: number,
  bandH: number,
): void {
  if (times.length === 0) return;
  ctx.fillStyle = "#ce93d8";
  // Binary search for the first visible sample time.
  let lo = 0;
  let hi = times.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (times[mid]! < viewStart) lo = mid + 1;
    else hi = mid - 1;
  }
  const y = bandTop + bandH;
  for (let i = lo; i < times.length; i++) {
    if (times[i]! > viewEnd) break;
    ctx.fillRect(nsToX(times[i]!), y, 3, 4);
  }
}

function drawSchedTriangles(
  ctx: LaneDrawContext,
  polls: SpanList<PollSpan>,
  input: LanesRenderInput,
  nsToX: (ns: number) => number,
  bandTop: number,
  bandH: number,
): void {
  const { viewStart, viewEnd } = input;
  const pollStart = firstVisible(polls, viewStart);
  ctx.fillStyle = "#ff2222";
  const yTop = bandTop + bandH + 1;
  const yBot = bandTop + bandH + 7;
  for (let i = pollStart; i < polls.length; i++) {
    const s = polls.at(i)!;
    if (s.start > viewEnd) break;
    if (!s.schedSamples) continue;
    for (const sample of s.schedSamples) {
      if (sample.timestamp < viewStart || sample.timestamp > viewEnd) continue;
      const x = nsToX(sample.timestamp);
      ctx.beginPath();
      ctx.moveTo(x, yTop);
      ctx.lineTo(x - 3, yBot);
      ctx.lineTo(x + 3, yBot);
      ctx.closePath();
      ctx.fill();
    }
  }
}

function drawWakerHighlight(
  ctx: LaneDrawContext,
  polls: SpanList<PollSpan>,
  input: LanesRenderInput,
  nsToX: (ns: number) => number,
  drawW: number,
  bandTop: number,
  bandH: number,
): void {
  const waker = input.hoveredWakerTaskId;
  if (!waker) return;
  const { viewStart, viewEnd } = input;
  const pollStart = firstVisible(polls, viewStart);
  ctx.fillStyle = "#ff8a65";
  for (let i = pollStart; i < polls.length; i++) {
    const s = polls.at(i)!;
    if (s.start > viewEnd) break;
    if (s.taskId !== waker) continue;
    const x1 = Math.max(0, nsToX(s.start));
    const x2 = Math.min(drawW, nsToX(s.end));
    ctx.fillRect(x1, bandTop, Math.max(x2 - x1, 2), bandH);
  }
}

function drawSelectedSpanOutlines(
  ctx: LaneDrawContext,
  polls: SpanList<PollSpan>,
  workerId: number,
  input: LanesRenderInput,
  nsToX: (ns: number) => number,
  drawW: number,
  bandTop: number,
  bandH: number,
): void {
  if (input.selectedSpanIds.size === 0) return;
  ctx.strokeStyle = "#ffeb3b";
  ctx.lineWidth = 2;
  // O(workers x selectedSpanIds) via the span index, NOT an allSpans scan:
  // each selected span id maps to its instances; each instance's segments on
  // this worker map to a poll via binary search.
  for (const spanId of input.selectedSpanIds) {
    const matches = input.spansById.get(spanId);
    if (!matches) continue;
    for (const s of matches) {
      for (const seg of s.segments) {
        if (seg.workerId !== workerId) continue;
        const poll = findSpanAt(polls, seg.start);
        if (!poll) continue;
        const x1 = Math.max(0, nsToX(poll.start));
        const x2 = Math.min(drawW, nsToX(poll.end));
        const w = Math.max(x2 - x1, 4);
        ctx.beginPath();
        ctx.moveTo(x1, bandTop);
        ctx.lineTo(x1 + w, bandTop);
        ctx.lineTo(x1 + w, bandTop + bandH);
        ctx.lineTo(x1, bandTop + bandH);
        ctx.lineTo(x1, bandTop);
        ctx.stroke();
      }
    }
  }
}

function drawWakeMarkers(
  ctx: LaneDrawContext,
  workerId: number,
  input: LanesRenderInput,
  nsToX: (ns: number) => number,
  top: number,
  sf: number,
): void {
  const selected = input.selectedTaskId;
  if (!selected) return;
  const wakes = input.wakesByWorker[workerId];
  if (!wakes || wakes.length === 0) return;
  const { viewStart, viewEnd } = input;
  ctx.fillStyle = "#66bb6a";
  const yTop = top + 2 * sf;
  const yBot = top + 8 * sf;
  for (const w of wakes) {
    if (w.timestamp < viewStart || w.timestamp > viewEnd) continue;
    if (w.wokenTaskId !== selected) continue;
    const x = nsToX(w.timestamp);
    ctx.beginPath();
    ctx.moveTo(x, yTop);
    ctx.lineTo(x - 3, yBot);
    ctx.lineTo(x + 3, yBot);
    ctx.closePath();
    ctx.fill();
  }
}

function drawQueueStepLine(
  ctx: LaneDrawContext,
  batcher: ReturnType<typeof makeStrokeBatcher>,
  row: number,
  workerId: number,
  input: LanesRenderInput,
  nsToX: (ns: number) => number,
  drawW: number,
  qTop: number,
  qH: number,
  sf: number,
): void {
  // The recorded 0 is a sentinel; a step line would read as "the local queues
  // are empty" rather than "unmeasured".
  if (!input.hasLocalQueueDepth) return;
  const samples = input.workerQueueSamples[workerId];
  if (!samples || samples.length === 0) return;
  const { viewStart, viewEnd } = input;
  const iStart = Math.max(0, lowerBoundT(samples, viewStart));
  const iEnd = Math.min(samples.length - 1, upperBoundT(samples, viewEnd));
  if (iEnd < iStart) return;
  const maxQ = input.sharedMaxQ || 1;

  // q:N scale label (cheap text, drawn directly - not a stroke primitive). Inset
  // past the runtime accent rail so the label is never hidden under it.
  ctx.fillStyle = "#8a90a8";
  ctx.font = "8px monospace";
  ctx.textAlign = "left";
  ctx.fillText(`q:${maxQ}`, RAIL_W + 2, qTop + 8 * sf);

  // Downsample the visible slice to one vertex per pixel column (spikes win),
  // expand into a step polyline, and extend the last level to the right edge
  // - one batched stroke per lane instead of a per-sample path.
  const yOf = (s: { local: number }): number => qTop + qH - (s.local / maxQ) * qH;
  // One slice of the visible window (was two: an intermediate copy per worker
  // per frame). `points` is pushed onto below; nothing else reads the slice.
  const points: { t: number; local: number }[] = samples.slice(iStart, iEnd + 1);
  const last = points[points.length - 1]!;
  // Trailing extension so the final level runs to the panel edge - but ONLY
  // when the last visible sample sits left of the edge. iEnd may include one
  // sample past viewEnd (so the step line exits at the correct height);
  // appending a viewEnd point after such a sample would be out of ascending-x
  // order and trip the sorted-input contract in downsampleSeriesToColumns. When
  // the last sample already reaches/passes the edge, it covers the trailing
  // level itself.
  if (viewEnd > last.t) points.push({ t: viewEnd, local: last.local });

  batcher.stepSeries(`queue:w${row}`, points, {
    xOf: (s) => nsToX(s.t),
    yOf,
    weightOf: (s) => s.local,
    drawW,
  });
}

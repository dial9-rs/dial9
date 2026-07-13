// src/pages/viewer/axis.ts - the timeline header / time axis track (T25;
// docs/ui-inventory/features/02-viewer-html.md section F; legacy
// `renderTimeline`/`fmtTs`/`fmtWallClock` in viewer.html).
//
// The time axis is the top track of the unified column (track-layout.ts
// TRACKS[0] "timeline"). It draws tick marks + labels sharing the ONE
// ns<->x mapping every track uses (lib/canvas/layout, the A13 alignment
// invariant), so a tick lines up pixel-exact with the poll/span/CPU marks
// below it. Non-interactive; redrawn each frame the viewport, trace, or
// clock mode changes.
//
// LAYOUT SEAM vs legacy (features/02 F2): the legacy `#timeline-canvas`
// spanned the FULL panel width and offset ticks by LABEL_W inline
// (`x = LABEL_W + nsToX(t)`). In the new shell every track's canvas is
// `drawW` wide and sits AFTER a LABEL_W DOM label gutter (tracks.ts), so
// the axis draws in DRAW-AREA-relative coordinates - `nsToDrawX` = the
// legacy `nsToX(ns, drawW)` with NO LABEL_W added. Because the DOM gutter
// supplies the same offset for lanes and every panel, drawing at
// `nsToDrawX` keeps the axis aligned with them by construction (the shared
// gutter is the invariant; see tracks.ts).
//
// CLOCK MODES (features/02 E1/E2, consumed here): `uiPrefs.timeMode`
// ("rel"|"abs") and `uiPrefs.tz` ("utc"|"local") come from the store - the
// axis READS them; the toolbar toggle BUTTONS that drive them are T33's.
// Formatting mirrors the legacy `fmtTs` exactly: relative offsets from the
// trace start (`fmtDuration`), or wall-clock via the nearest clock-sync
// anchor (`fmtWallClock`), falling back to relative when the trace has no
// anchors.
//
// DATE-QUALIFICATION AMENDMENT (T25; ledger row, no 04 finding id - S2/#137
// legibility family): when the visible span's start and end fall on
// DIFFERENT calendar days (evaluated in the active tz mode), absolute-mode
// tick labels gain a `MM-DD ` date prefix so ticks across a day boundary
// are unambiguous. Same rule as T15's heatmap-axis amendment (features/01
// F10), narrower prefix (`MM-DD` vs the heatmap's `YYYY-MM-DD`) per this
// ticket. Relative mode has no calendar day, so it is never qualified.

import type {
  PanelGeometry,
  StoreState,
  TimeMode,
  TimeZoneMode,
} from "../../types/state.js";
import type { ClockSyncAnchor } from "../../types/trace.js";

/**
 * The clock/format state the axis needs to label a tick, lifted out of the
 * store so the pure formatters stay Node-testable (no store, no DOM).
 * `deriveAxisInputs` builds it from a store snapshot.
 */
export interface AxisInputs {
  /** Trace start (ns): the baseline for relative offsets (viewport.minTs). */
  minTs: number;
  /** Clock display mode (uiPrefs.timeMode). */
  timeMode: TimeMode;
  /** Timezone for absolute timestamps (uiPrefs.tz). */
  tz: TimeZoneMode;
  /** Clock-sync anchors, sorted by monotonicNs (ParsedTrace). Empty if none. */
  clockSyncAnchors: readonly ClockSyncAnchor[];
  /** Single monotonic->wall-clock offset; null when the trace has no anchor. */
  clockOffsetNs: number | null;
}

/** Lift the axis's clock/format inputs from a store snapshot. */
export function deriveAxisInputs(state: StoreState): AxisInputs {
  const trace = state.trace.trace;
  return {
    minTs: state.viewport.minTs,
    timeMode: state.uiPrefs.timeMode,
    tz: state.uiPrefs.tz,
    clockSyncAnchors: trace?.clockSyncAnchors ?? [],
    clockOffsetNs: trace?.clockOffsetNs ?? null,
  };
}

// ── Tick geometry (features/02 F1) ──────────────────────────────────────

/**
 * The "nice" tick intervals in ns (legacy `renderTimeline`): 1µs..10s in a
 * 1/5 progression. The auto-picker snaps the raw interval up to the first
 * of these >= it, so ticks land on round durations.
 */
const NICE_INTERVALS: readonly number[] = [
  1e3, 5e3, 1e4, 5e4, 1e5, 5e5, 1e6, 5e6, 1e7, 5e7, 1e8, 5e8, 1e9, 5e9, 1e10,
];

/**
 * Auto-pick the tick interval (ns) for a visible span over `drawW` px,
 * targeting ~4-16 ticks (`max(4, floor(drawW/100))`) - the legacy
 * `renderTimeline` rule verbatim. Returns the raw interval when the span is
 * wider than the largest nice value.
 */
export function pickTickInterval(
  viewStart: number,
  viewEnd: number,
  drawW: number,
): number {
  const viewDur = viewEnd - viewStart;
  const targetTicks = Math.max(4, Math.floor(drawW / 100));
  const rawInterval = viewDur / targetTicks;
  return NICE_INTERVALS.find((i) => i >= rawInterval) ?? rawInterval;
}

/**
 * The tick timestamps (ns) for a visible span at a given interval: every
 * multiple of `interval` in `[ceil(viewStart/interval)*interval, viewEnd]`
 * (legacy `firstTick` loop). Guards against a non-positive interval so a
 * degenerate viewport can never spin.
 */
export function tickTimestamps(
  viewStart: number,
  viewEnd: number,
  interval: number,
): number[] {
  const out: number[] = [];
  if (!(interval > 0) || viewEnd < viewStart) return out;
  const firstTick = Math.ceil(viewStart / interval) * interval;
  for (let t = firstTick; t <= viewEnd; t += interval) out.push(t);
  return out;
}

/**
 * Timestamp (ns) -> draw-area-relative x (px), the F2 `nsToX(ns, drawW)`.
 * NO LABEL_W is added: the track canvas already sits after the DOM label
 * gutter, so this is the same expression the lanes/panels use for their
 * canvas-local x (the alignment invariant - see the file header).
 */
export function nsToDrawX(
  ns: number,
  viewStart: number,
  viewEnd: number,
  drawW: number,
): number {
  const span = viewEnd - viewStart || 1;
  return ((ns - viewStart) / span) * drawW;
}

// ── Wall-clock resolution (legacy clockOffsetForTimestamp/fmtWallClock) ──

/**
 * The monotonic->wall-clock offset (ns) to apply at timestamp `ns`, ported
 * verbatim from the legacy `clockOffsetForTimestamp`: no anchors -> the
 * whole-trace `clockOffsetNs` (may be null); one anchor -> its fixed
 * offset; many -> the nearest anchor by monotonic time (binary search).
 */
export function clockOffsetForTimestamp(
  anchors: readonly ClockSyncAnchor[],
  clockOffsetNs: number | null,
  ns: number,
): number | null {
  if (anchors.length === 0) return clockOffsetNs;
  if (anchors.length === 1) {
    const a = anchors[0]!;
    return a.realtimeNs - a.monotonicNs;
  }
  let lo = 0;
  let hi = anchors.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (anchors[mid]!.monotonicNs < ns) lo = mid + 1;
    else hi = mid;
  }
  if (lo === 0) {
    const a = anchors[0]!;
    return a.realtimeNs - a.monotonicNs;
  }
  if (lo >= anchors.length) {
    const a = anchors[anchors.length - 1]!;
    return a.realtimeNs - a.monotonicNs;
  }
  const prev = anchors[lo - 1]!;
  const next = anchors[lo]!;
  const pick =
    Math.abs(ns - prev.monotonicNs) <= Math.abs(next.monotonicNs - ns)
      ? prev
      : next;
  return pick.realtimeNs - pick.monotonicNs;
}

/**
 * Wall-clock timestamp (ns) for a monotonic `ns`, or null when the trace
 * carries no clock-sync anchor (caller falls back to relative time, exactly
 * as the legacy `fmtWallClock` does).
 */
export function wallClockNs(inputs: AxisInputs, ns: number): number | null {
  const off = clockOffsetForTimestamp(
    inputs.clockSyncAnchors,
    inputs.clockOffsetNs,
    ns,
  );
  return off == null ? null : ns + off;
}

// ── Date-qualification amendment (T25) ──────────────────────────────────

/**
 * Whether two wall-clock timestamps (ns) fall on different calendar days in
 * the active tz - the date-prefix trigger. Mirrors T15's
 * `crossesDayBoundary` (features/01 F10) but on ns wall-clock values.
 */
export function crossesDayBoundaryNs(
  startWallNs: number,
  endWallNs: number,
  localTz: boolean,
): boolean {
  const a = new Date(startWallNs / 1e6);
  const b = new Date(endWallNs / 1e6);
  if (localTz) {
    return (
      a.getFullYear() !== b.getFullYear() ||
      a.getMonth() !== b.getMonth() ||
      a.getDate() !== b.getDate()
    );
  }
  return (
    a.getUTCFullYear() !== b.getUTCFullYear() ||
    a.getUTCMonth() !== b.getUTCMonth() ||
    a.getUTCDate() !== b.getUTCDate()
  );
}

/**
 * Whether the axis should date-qualify its labels for the current view: the
 * amendment applies ONLY in absolute mode with resolvable wall-clock (no
 * anchors -> relative fallback, no calendar day) and only when the visible
 * span crosses a day boundary in the active tz. Computed once per render.
 */
export function isDateQualified(
  inputs: AxisInputs,
  viewStart: number,
  viewEnd: number,
): boolean {
  if (inputs.timeMode !== "abs") return false;
  const startWall = wallClockNs(inputs, viewStart);
  const endWall = wallClockNs(inputs, viewEnd);
  if (startWall == null || endWall == null) return false;
  return crossesDayBoundaryNs(startWall, endWall, inputs.tz === "local");
}

// ── Label formatting (legacy fmtDuration/fmtWallClock/fmtTs) ─────────────

function pad2(n: number): string {
  return String(n).padStart(2, "0");
}

/**
 * Relative offset label with a `+` prefix (legacy `fmtDuration`):
 * s / ms / µs / ns by magnitude. `ns` here is already the offset from the
 * trace start.
 */
export function fmtDuration(ns: number): string {
  const v = ns;
  if (v >= 1e9) return "+" + (v / 1e9).toFixed(2) + "s";
  if (v >= 1e6) return "+" + (v / 1e6).toFixed(2) + "ms";
  if (v >= 1e3) return "+" + (v / 1e3).toFixed(1) + "µs";
  return "+" + v.toFixed(0) + "ns";
}

/**
 * Wall-clock label for a wall-clock timestamp (ns): "HH:MM:SS", or
 * "MM-DD HH:MM:SS" when `withDate` (the date-qualification amendment), in
 * UTC or the local zone. Mirrors the legacy `fmtWallClock` HH:MM:SS output
 * with the amendment's date prefix bolted on the front.
 */
export function fmtWallClockLabel(
  wallNs: number,
  localTz: boolean,
  withDate: boolean,
): string {
  const d = new Date(wallNs / 1e6);
  const mo = localTz ? d.getMonth() + 1 : d.getUTCMonth() + 1;
  const day = localTz ? d.getDate() : d.getUTCDate();
  const hh = localTz ? d.getHours() : d.getUTCHours();
  const mm = localTz ? d.getMinutes() : d.getUTCMinutes();
  const ss = localTz ? d.getSeconds() : d.getUTCSeconds();
  const time = pad2(hh) + ":" + pad2(mm) + ":" + pad2(ss);
  return withDate ? pad2(mo) + "-" + pad2(day) + " " + time : time;
}

/**
 * Format one tick's label (legacy `fmtTs` + the amendment). Absolute mode
 * renders wall-clock (date-qualified per `withDate`); with no resolvable
 * anchor it falls back to relative, exactly as the legacy `fmtWallClock`
 * did. `withDate` is precomputed once per render (`isDateQualified`).
 */
export function fmtAxisTick(
  inputs: AxisInputs,
  ns: number,
  withDate: boolean,
): string {
  if (inputs.timeMode === "abs") {
    const wall = wallClockNs(inputs, ns);
    if (wall != null) return fmtWallClockLabel(wall, inputs.tz === "local", withDate);
    // No anchor: legacy fmtWallClock falls back to a relative offset.
  }
  return fmtDuration(ns - inputs.minTs);
}

// ── Canvas render (features/02 F1) ──────────────────────────────────────

// Legacy `renderTimeline` colours (viewer.html): axis fill, tick strokes,
// label text. Kept identical so the migrated axis reads the same.
const AXIS_BG = "#16213e";
const TICK_STROKE = "#333";
const LABEL_FILL = "#888";

/**
 * Render the time axis into `ctx` (already DPR-scaled and sized to
 * `geometry.time.drawW` x `geometry.height`). Draws the F1 ruler: a filled
 * background, a short vertical tick at each auto-picked timestamp, and the
 * `fmtAxisTick` label centred above it. Draw-area-relative x (`nsToDrawX`);
 * the DOM gutter provides the LABEL_W offset (see the file header).
 *
 * Called from tracks.ts `sizeTracks` for the "timeline" track, inside the
 * store's frame tick (the one place renders run + layout reads batch, F3).
 * A blank axis is painted before the trace loads / when the panel is too
 * narrow so the slot still reads as a track.
 */
export function renderTimeAxis(
  ctx: CanvasRenderingContext2D,
  geometry: PanelGeometry,
  viewStart: number,
  viewEnd: number,
  inputs: AxisInputs,
  hasTrace: boolean,
): void {
  const drawW = geometry.time.drawW;
  const height = geometry.height;
  ctx.clearRect(0, 0, drawW, height);
  ctx.fillStyle = AXIS_BG;
  ctx.fillRect(0, 0, drawW, height);
  if (!hasTrace || drawW <= 0 || viewEnd <= viewStart) return;

  const interval = pickTickInterval(viewStart, viewEnd, drawW);
  const ticks = tickTimestamps(viewStart, viewEnd, interval);
  const withDate = isDateQualified(inputs, viewStart, viewEnd);

  ctx.fillStyle = LABEL_FILL;
  ctx.font = "10px monospace";
  ctx.textAlign = "center";
  ctx.strokeStyle = TICK_STROKE;
  // Tick marks span the bottom 10px; label baseline sits above them
  // (legacy: 20->30 tick, label at y=16 in a 30px canvas).
  const tickTop = height - 10;
  const labelY = height - 14;
  for (const t of ticks) {
    const x = nsToDrawX(t, viewStart, viewEnd, drawW);
    ctx.beginPath();
    ctx.moveTo(x, tickTop);
    ctx.lineTo(x, height);
    ctx.stroke();
    ctx.fillText(fmtAxisTick(inputs, t, withDate), x, labelY);
  }
}

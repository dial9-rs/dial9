// The timeline header / time axis track: the top track of the unified column.
// Draws tick marks + labels in draw-area-relative x (nsToDrawX, no LABEL_W
// added) so ticks line up pixel-exact with the poll/span/CPU marks below.
// Non-interactive; redrawn when the viewport, trace, or clock mode changes.
//
// The ruler is WINDOW-RELATIVE. Ticks sit at round offsets from the left edge
// of the visible window (0, 100µs, 200µs, ...) and carry those offsets as their
// labels, through the viewer's one duration format. So the units follow the
// zoom (s -> ms -> µs -> ns), labels stay short at any depth, and the tick step
// itself reads as a duration. Two chips frame them:
//
//   Time | +2.000700s   0   100µs   200µs   300µs   <-> 500µs (step 100µs)
//          ^ anchor: where the window starts        ^ width + step
//
// The anchor is the only absolute reading on the ruler; the at-cursor readout
// (components/overlay) is the other place absolute time is available. Its
// precision follows the tick step, so it always resolves finer than one tick.
//
// Absolute-mode anchors gain a `MM-DD ` date prefix when the visible span
// crosses a calendar-day boundary, so a window straddling midnight reads
// unambiguously. Relative mode has no calendar day, so it is never qualified.

import { nsToDrawX } from "../../lib/canvas/index.js";
import { formatHumanDuration } from "../../lib/trace/index.js";
export { nsToDrawX };

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

// ── Tick geometry ────────────────────────────────────────────────────────

/**
 * The "nice" tick intervals in ns: 1ns to 1h. A 1/5 progression up to 10s,
 * then clock-friendly 30s / 1m / 5m / 10m / 30m / 1h so a long absolute span
 * still lands on round steps. The auto-picker snaps the raw interval up to the
 * first of these >= it, so ticks land on round durations at EVERY zoom level -
 * including below a microsecond, where the ruler used to run out of values.
 */
const NICE_INTERVALS: readonly number[] = [
  1, 5, 10, 50, 100, 500, 1e3, 5e3, 1e4, 5e4, 1e5, 5e5, 1e6, 5e6, 1e7, 5e7,
  1e8, 5e8, 1e9, 5e9, 1e10, 3e10, 6e10, 3e11, 6e11, 1.8e12, 3.6e12,
];

/**
 * Auto-pick the tick interval (ns) for a visible span over `drawW` px,
 * targeting ~4-16 ticks (`max(4, floor(drawW/100))`). Returns the raw interval
 * when the span is wider than the largest nice value.
 *
 * Snapping UP can overshoot the window - a 4.5s view on a narrow panel snaps
 * 1.1s to 5s and leaves a ruler with one tick - so an interval that cannot fit
 * twice steps back down to the largest nice value that does.
 */
export function pickTickInterval(
  viewStart: number,
  viewEnd: number,
  drawW: number,
): number {
  const viewDur = viewEnd - viewStart;
  const targetTicks = Math.max(4, Math.floor(drawW / 100));
  const rawInterval = viewDur / targetTicks;
  const snapped = NICE_INTERVALS.find((i) => i >= rawInterval) ?? rawInterval;
  if (snapped <= viewDur / 2) return snapped;
  for (let i = NICE_INTERVALS.length - 1; i >= 0; i--) {
    const candidate = NICE_INTERVALS[i]!;
    if (candidate <= viewDur / 2) return candidate;
  }
  return snapped;
}

/**
 * The tick offsets (ns from the window's left edge) at a given interval:
 * `0, interval, 2*interval, ...` up to the window duration. Guards against a
 * non-positive interval or duration so a degenerate viewport can never spin.
 *
 * Offsets, not absolute timestamps: that is what keeps a tick label short at
 * deep zoom, and it means the ticks stay glued to the left edge while panning.
 */
export function tickOffsets(viewDur: number, interval: number): number[] {
  const out: number[] = [];
  if (!(interval > 0) || !(viewDur >= 0)) return out;
  for (let o = 0; o <= viewDur; o += interval) out.push(o);
  return out;
}

/**
 * The label precision (ns) for a point-in-time readout at the current zoom -
 * the at-cursor readout and other single-timestamp labels, which have no tick
 * step of their own. Targets ~1 screen pixel (span/1000) snapped to a nice
 * value, so the readout resolves finer than the ruler beneath it.
 */
export function cursorPrecisionNs(viewStart: number, viewEnd: number): number {
  const span = viewEnd - viewStart;
  if (!(span > 0)) return 1;
  const raw = span / 1000;
  return NICE_INTERVALS.find((i) => i >= raw) ?? raw;
}

/**
 * Timestamp (ns) -> draw-area-relative x (px). NO LABEL_W is added: the track
 * canvas already sits after the DOM label gutter, so this is the same
 * expression the lanes/panels use for their canvas-local x (the alignment
 * invariant - see the file header).
 */

// ── Wall-clock resolution ────────────────────────────────────────────────

/**
 * The monotonic->wall-clock offset (ns) to apply at timestamp `ns`: no anchors
 * -> the whole-trace `clockOffsetNs` (may be null); one anchor -> its fixed
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
 * carries no clock-sync anchor (caller falls back to relative time).
 */
export function wallClockNs(inputs: AxisInputs, ns: number): number | null {
  const off = clockOffsetForTimestamp(
    inputs.clockSyncAnchors,
    inputs.clockOffsetNs,
    ns,
  );
  return off == null ? null : ns + off;
}

// ── Date qualification ───────────────────────────────────────────────────

/**
 * Whether two wall-clock timestamps (ns) fall on different calendar days in
 * the active tz - the date-prefix trigger.
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
 * Whether the axis should date-qualify its anchor for the current view: only
 * in absolute mode with resolvable wall-clock (no anchors -> relative
 * fallback, no calendar day) and only when the visible span crosses a day
 * boundary in the active tz. Computed once per render.
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

// ── Label formatting ─────────────────────────────────────────────────────

function pad2(n: number): string {
  return String(n).padStart(2, "0");
}

/**
 * Decimals needed to render a step of `stepNs` exactly in units of `unitNs`
 * (1e9 for s, 1e6 for ms, ...), capped at 9. Zero once the step is a whole
 * unit. The 1e-9 slack absorbs log10 rounding on exact decades.
 */
export function decimalsForStep(stepNs: number, unitNs: number): number {
  if (!(stepNs > 0)) return 0;
  const step = stepNs / unitNs;
  if (step >= 1) return 0;
  return Math.min(9, Math.ceil(-Math.log10(step) - 1e-9));
}

/** Unit divisor / suffix / default decimals, by offset magnitude. */
function durationUnit(v: number): { div: number; suffix: string; fixed: number } {
  if (v >= 1e9) return { div: 1e9, suffix: "s", fixed: 2 };
  if (v >= 1e6) return { div: 1e6, suffix: "ms", fixed: 2 };
  if (v >= 1e3) return { div: 1e3, suffix: "µs", fixed: 1 };
  return { div: 1, suffix: "ns", fixed: 0 };
}

/**
 * Relative POSITION label with a `+` prefix: the offset from the trace start,
 * in s / ms / µs / ns by magnitude. Unlike a duration this must stay
 * comparable digit-by-digit as you pan, so the unit follows the magnitude and
 * the precision follows the zoom.
 *
 * `precisionNs` is the step the label must resolve (the tick interval, or
 * `cursorPrecisionNs` for a point readout): decimals are derived from it so
 * two nearby positions never collapse to the same text. Omitted, the label
 * keeps its magnitude-default decimals.
 *
 * `unitBasisNs` overrides which magnitude picks the unit, so the two ends of a
 * range can share one unit ("+0.000s - +4.574s", not "+0ns - +4.574s").
 */
export function fmtDuration(
  ns: number,
  precisionNs?: number,
  unitBasisNs: number = ns,
): string {
  const { div, suffix, fixed } = durationUnit(unitBasisNs);
  const decimals =
    precisionNs === undefined ? fixed : decimalsForStep(precisionNs, div);
  return "+" + (ns / div).toFixed(decimals) + suffix;
}

/**
 * Sub-second digits for a wall-clock label at a step of `precisionNs`: none,
 * ms (3) or µs (6), in 3-digit groups.
 *
 * Capped at µs on purpose: wall-clock ns since the epoch is past 2^53, so a
 * float64 timestamp is already quantised to a few hundred ns - printing 9
 * digits would invent precision the value does not carry. Relative mode has no
 * such limit and goes to single ns.
 */
export function wallClockFracDigits(precisionNs: number | undefined): number {
  if (precisionNs === undefined || precisionNs >= 1e9) return 0;
  return precisionNs >= 1e6 ? 3 : 6;
}

/**
 * Wall-clock label for a wall-clock timestamp (ns): "HH:MM:SS", or
 * "MM-DD HH:MM:SS" when `withDate` (date-qualified), in UTC or the local zone.
 * `fracDigits` (see `wallClockFracDigits`) appends that many sub-second
 * digits, e.g. "HH:MM:SS.001500" at 6.
 */
export function fmtWallClockLabel(
  wallNs: number,
  localTz: boolean,
  withDate: boolean,
  fracDigits = 0,
): string {
  const d = new Date(wallNs / 1e6);
  const mo = localTz ? d.getMonth() + 1 : d.getUTCMonth() + 1;
  const day = localTz ? d.getDate() : d.getUTCDate();
  const hh = localTz ? d.getHours() : d.getUTCHours();
  const mm = localTz ? d.getMinutes() : d.getUTCMinutes();
  const ss = localTz ? d.getSeconds() : d.getUTCSeconds();
  let time = pad2(hh) + ":" + pad2(mm) + ":" + pad2(ss);
  if (fracDigits > 0) {
    // Euclidean remainder: a pre-epoch (negative) wall clock would otherwise
    // pad its minus sign into the fraction ("0-1500000" -> ".0-1").
    const subSecNs = ((Math.floor(wallNs) % 1e9) + 1e9) % 1e9;
    time += "." + String(subSecNs).padStart(9, "0").slice(0, fracDigits);
  }
  return withDate ? pad2(mo) + "-" + pad2(day) + " " + time : time;
}

/**
 * Format one absolute POSITION (the ruler anchor, the at-cursor readout, an
 * inspector timestamp). Absolute mode renders wall-clock (date-qualified per
 * `withDate`); with no resolvable anchor it falls back to relative.
 * `withDate` is precomputed once per render (`isDateQualified`).
 *
 * `precisionNs` is the step the label must resolve - the ruler passes its tick
 * interval, point readouts pass `cursorPrecisionNs`. Omitted, labels keep the
 * coarse magnitude defaults. `unitBasisNs` pins the relative-mode unit (see
 * `fmtDuration`); it does not apply to wall-clock labels.
 */
export function fmtAxisTick(
  inputs: AxisInputs,
  ns: number,
  withDate: boolean,
  precisionNs?: number,
  unitBasisNs?: number,
): string {
  if (inputs.timeMode === "abs") {
    const wall = wallClockNs(inputs, ns);
    if (wall != null)
      return fmtWallClockLabel(
        wall,
        inputs.tz === "local",
        withDate,
        wallClockFracDigits(precisionNs),
      );
    // No anchor: fall back to a relative offset.
  }
  return fmtDuration(
    ns - inputs.minTs,
    precisionNs,
    unitBasisNs ?? ns - inputs.minTs,
  );
}

/**
 * A tick's own label: its offset from the window's left edge as a duration, so
 * the unit tracks the zoom. The zero tick is bare - the anchor beside it
 * already names that position.
 */
export function fmtTickOffset(offsetNs: number): string {
  return offsetNs === 0 ? "0" : formatHumanDuration(offsetNs);
}

// ── Ruler layout ─────────────────────────────────────────────────────────

// Label width estimate for the collision rules: 10px monospace is ~6px/glyph,
// plus a gap so neighbours never touch. Estimated rather than measured so the
// whole layout stays pure and Node-testable.
const LABEL_CHAR_W = 6;
const LABEL_GAP = 8;

/**
 * Estimated pixel width of ruler text. The one place this estimate lives: the
 * selection measuring bar sits in this same row and sizes itself through it,
 * so a font change moves both together.
 */
export function estimateLabelWidth(text: string): number {
  return text.length * LABEL_CHAR_W;
}

/** Half the estimated pixel width of a label. */
export function labelHalfWidth(text: string): number {
  return estimateLabelWidth(text) / 2;
}

/**
 * Draw widths below which the ruler sheds furniture rather than crowding it:
 * no step in the chip, then no chip at all, then no anchor either (a sliver of
 * a panel keeps bare ticks).
 */
const CHIP_WITH_STEP_MIN_W = 520;
const CHIP_MIN_W = 320;
const ANCHOR_MIN_W = 180;

/** One painted tick: its x in draw-area px, and the label if one survived. */
export interface RulerTick {
  offsetNs: number;
  x: number;
  /** null when the label was dropped for a collision (the mark still draws). */
  text: string | null;
}

/** Everything the ruler paints, resolved from the viewport. Pure. */
export interface RulerLayout {
  /** The picked tick step (ns). */
  interval: number;
  ticks: RulerTick[];
  /** Absolute position of the window's left edge; null on a sliver panel. */
  anchor: string | null;
  /** Visible width (and step, when it fits); null when there is no room. */
  chip: string | null;
}

/**
 * Resolve the ruler for a viewport: tick step, tick positions, which tick
 * labels survive, and the two framing chips.
 *
 * Label collisions are resolved left to right: a label is dropped when its
 * estimated box would overlap the anchor, the chip, or the last label kept.
 * The tick mark is always painted, so the grid stays complete even where the
 * numbers thin out.
 */
export function rulerLayout(
  viewStart: number,
  viewEnd: number,
  drawW: number,
  inputs: AxisInputs,
): RulerLayout {
  const interval = pickTickInterval(viewStart, viewEnd, drawW);
  const viewDur = viewEnd - viewStart;
  const withDate = isDateQualified(inputs, viewStart, viewEnd);

  // The anchor's unit comes from the window's far end, not from viewStart: at
  // the trace start it would otherwise read "+0ns" beside second-scale ticks.
  const anchor =
    drawW >= ANCHOR_MIN_W
      ? fmtAxisTick(inputs, viewStart, withDate, interval, viewEnd - inputs.minTs)
      : null;
  const chip =
    drawW >= CHIP_WITH_STEP_MIN_W
      ? `<-> ${formatHumanDuration(viewDur)} (step ${formatHumanDuration(interval)})`
      : drawW >= CHIP_MIN_W
        ? `<-> ${formatHumanDuration(viewDur)}`
        : null;

  // Reserved bands: a tick label may not paint over either chip, and never
  // past a canvas edge - a centred label at x=0 or x=drawW would be half
  // clipped, which is what happens on a panel too narrow to carry a chip.
  let lastRight =
    anchor === null ? 0 : estimateLabelWidth(anchor) + LABEL_GAP;
  const rightLimit =
    chip === null ? drawW : drawW - estimateLabelWidth(chip) - LABEL_GAP;

  const ticks: RulerTick[] = [];
  for (const offsetNs of tickOffsets(viewDur, interval)) {
    const x = nsToDrawX(viewStart + offsetNs, viewStart, viewEnd, drawW);
    const text = fmtTickOffset(offsetNs);
    const half = labelHalfWidth(text);
    const fits = x - half >= lastRight && x + half <= rightLimit;
    if (fits) lastRight = x + half + LABEL_GAP;
    ticks.push({ offsetNs, x, text: fits ? text : null });
  }
  return { interval, ticks, anchor, chip };
}

// ── Canvas render ────────────────────────────────────────────────────────

// Axis colours: fill, tick strokes, tick-label text, and the brighter chip
// text that sets the two framing readings apart from the tick offsets.
const AXIS_BG = "#16213e";
const TICK_STROKE = "#333";
const LABEL_FILL = "#888";
const CHIP_FILL = "#bbb";

/**
 * Render the time axis into `ctx` (already DPR-scaled and sized to
 * `geometry.time.drawW` x `geometry.height`): a filled background, a short
 * vertical tick at each offset, the surviving tick labels, and the anchor /
 * width chips at the two ends. Draw-area-relative x (`nsToDrawX`); the DOM
 * gutter provides the LABEL_W offset (see the file header).
 *
 * Called from tracks.ts `sizeTracks` for the "timeline" track, inside the
 * store's frame tick. A blank axis is painted before the trace loads / when
 * the panel is too narrow so the slot still reads as a track.
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

  const layout = rulerLayout(viewStart, viewEnd, drawW, inputs);

  ctx.font = "10px monospace";
  ctx.strokeStyle = TICK_STROKE;
  // Tick marks span the bottom 10px; every label baseline sits above them.
  const tickTop = height - 10;
  const labelY = height - 14;

  ctx.fillStyle = LABEL_FILL;
  ctx.textAlign = "center";
  for (const tick of layout.ticks) {
    ctx.beginPath();
    ctx.moveTo(tick.x, tickTop);
    ctx.lineTo(tick.x, height);
    ctx.stroke();
    if (tick.text !== null) ctx.fillText(tick.text, tick.x, labelY);
  }

  ctx.fillStyle = CHIP_FILL;
  if (layout.anchor !== null) {
    ctx.textAlign = "left";
    ctx.fillText(layout.anchor, 0, labelY);
  }
  if (layout.chip !== null) {
    ctx.textAlign = "right";
    ctx.fillText(layout.chip, drawW, labelY);
  }
}

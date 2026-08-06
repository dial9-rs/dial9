// The lanes track's LABEL GUTTER canvas: one row label per lane, drawn against
// the same `LaneRowLayout` + `scrollTop` the lanes canvas draws from.
//
// Why a canvas in the gutter rather than DOM labels: the lanes canvas is
// virtualized (sticky, sized to the visible box, repainted from `scrollTop` on
// every scroll frame), so DOM row labels would have to be positioned and
// recycled per frame to stay aligned. Painting the gutter from the SAME row
// layout in the same frame makes drift structurally impossible.
//
// Why the gutter rather than in-lane text: the draw area is the data. Every
// pixel of label over a lane hides a poll bar, which is why the id previously
// had to be a 30x9 chip crammed into the strip above the poll band. The gutter
// is ~100px of otherwise-empty chrome, so the label can be legible AND say more
// than an id — the runtime a worker belongs to, and what a summary row totals.
//
// The gutter is NOT part of the time axis (drawW starts after it), so nothing
// here can disturb the cross-track alignment invariant.

import type { LaneRow, LaneRowLayout } from "../../../lib/canvas/layout.js";
import { DEFAULT_ACCENT, RAIL_W } from "./chrome.js";
import type { LaneDrawContext } from "./render.js";

const GUTTER_BG = "#131834";
const WORKER_ID = "#c8cee8";
const RUNTIME_NAME = "#aeb6e0";
const METRICS_TITLE = "#aeb6e0";
const SUBTLE = "#6b73a0";
const BORDER = "#2b3358";

/** Left inset for gutter text, past the accent rail. */
const TEXT_X = RAIL_W + 6;

/** Per-frame inputs for the gutter paint (all derived; no store, no DOM). */
export interface LaneLabelsInput {
  /** The SAME row stack the lanes canvas drew, so labels cannot drift. */
  rowLayout: LaneRowLayout;
  /** The SAME scroll offset the lanes canvas drew at. */
  scrollTop: number;
  /** Gutter width in CSS px (LABEL_W). */
  labelW: number;
  /** Visible gutter height in CSS px (the lanes box height). */
  height: number;
  /** Runtime-group name -> accent, so a row's rail matches its lane's. */
  runtimeAccents: ReadonlyMap<string, string>;
  /** worker id -> owning runtime-group name, for the worker rows' accent. */
  workerRuntime: ReadonlyMap<number, string>;
}

/**
 * Paint the label gutter: an accent rail plus a label for every visible row —
 * "W<id>" for a worker, the runtime name for a group header, and
 * "<runtime> runtime metrics" for a summary row. Rows outside the scroll window
 * are culled exactly as the lanes canvas culls them.
 *
 * Deliberately NO disclosure carets. The gutter is not a click target (the click
 * hit-test resolves fold gestures inside the lanes box, from x >= LABEL_W), and
 * the band beside each foldable row already carries a working caret. Drawing one
 * here too doubled every triangle on screen and pointed at dead pixels.
 */
export function renderLaneLabels(ctx: LaneDrawContext, input: LaneLabelsInput): void {
  const { labelW, height, scrollTop } = input;
  if (labelW <= 0 || height <= 0) return;

  ctx.clearRect(0, 0, labelW, height);
  ctx.fillStyle = GUTTER_BG;
  ctx.fillRect(0, 0, labelW, height);

  const viewBot = scrollTop + height;
  for (const r of input.rowLayout.rows) {
    if (r.y + r.height <= scrollTop || r.y >= viewBot) continue;
    drawRowLabel(ctx, r, r.y - scrollTop, labelW, accentFor(r, input));
  }

  // Right edge, matching the DOM gutter's border so the seam is unbroken.
  ctx.fillStyle = BORDER;
  ctx.fillRect(labelW - 1, 0, 1, height);
}

/** The accent of the runtime a row belongs to (default when unknown). */
function accentFor(r: LaneRow, input: LaneLabelsInput): string {
  const name = r.kind === "worker" ? input.workerRuntime.get(r.workerId) : r.name;
  if (name === undefined) return DEFAULT_ACCENT;
  return input.runtimeAccents.get(name) ?? DEFAULT_ACCENT;
}

function drawRowLabel(
  ctx: LaneDrawContext,
  r: LaneRow,
  top: number,
  labelW: number,
  accent: string,
): void {
  // The row's accent rail, continuous with the same rail on its lane.
  ctx.fillStyle = accent;
  ctx.fillRect(0, top, RAIL_W, r.height);

  if (r.kind === "header") {
    // The bare runtime name: the band beside it already reads "<name> runtime" /
    // "runtime: <name>", and LABEL_W has no room to repeat the qualifier.
    ctx.fillStyle = RUNTIME_NAME;
    ctx.font = "bold 11px sans-serif";
    ctx.textAlign = "left";
    ctx.fillText(r.name, TEXT_X, top + r.height - 8);
    return;
  }

  if (r.kind === "runtime-metrics") {
    // "<name> runtime metrics" wrapped onto two lines: the phrase does not fit
    // LABEL_W on one (~112px at this font vs ~75px of room). Folded to the strip
    // there is only room for one line, so the name shortens rather than clipping
    // and the band's own title spells it out in full.
    if (r.collapsed) {
      ctx.fillStyle = METRICS_TITLE;
      ctx.font = "bold 10px sans-serif";
      ctx.textAlign = "left";
      ctx.fillText(`${r.name} metrics`, TEXT_X, top + r.height / 2 + 4);
      return;
    }
    // Two lines, centred as a block in the row.
    const firstBaseline = top + r.height / 2 - 2;
    ctx.fillStyle = METRICS_TITLE;
    ctx.font = "bold 10px sans-serif";
    ctx.textAlign = "left";
    ctx.fillText(`${r.name} runtime`, TEXT_X, firstBaseline);
    ctx.fillStyle = SUBTLE;
    ctx.font = "10px sans-serif";
    ctx.fillText("metrics", TEXT_X, firstBaseline + 12);
    return;
  }

  // A worker row: its id, centred in the row so it reads as belonging to the
  // whole lane rather than to the poll band at its top.
  ctx.fillStyle = WORKER_ID;
  ctx.font = "bold 12px monospace";
  ctx.textAlign = "left";
  ctx.fillText(`W${r.workerId}`, TEXT_X, top + r.height / 2 + 4);
  // Row separator, matching the lane's own bottom divider.
  ctx.fillStyle = BORDER;
  ctx.fillRect(RAIL_W, top + r.height - 1, labelW - RAIL_W, 1);
}

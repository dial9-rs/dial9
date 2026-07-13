// src/pages/viewer/tracks.ts - the unified time-aligned track column
// (T21; concept-1 layout, features/02 sections F/G/J/K/L/M/N as tracks).
//
// T21 renders the SLOTS: each track is a row = [ LABEL_W label gutter |
// draw canvas ], the canvas sized to the shared drawW from lib/canvas/
// layout so every track's time axis lines up vertically (the A13
// invariant). Track CONTENT is out of scope - each canvas is painted an
// empty placeholder and a `render(ctx,state,layout)` component fills it
// later (owner recorded per track in track-layout.ts). Because every track
// uses ONE DOM label gutter of LABEL_W and a canvas of exactly drawW, the
// tracks are axis-aligned by construction; a downstream track that draws
// its own internal gutter would break that, so the shell keeps the gutter
// in the DOM (matching the lanes' DOM-flex label, A13).
//
// Declarative: the row structure is a lit-html template; canvas sizing is a
// post-render side effect (measure the column once per frame, size every
// backing store - F3: geometry-change-only resizes via createCanvasSizer).

import { html, type TemplateResult } from "lit-html";
import { createCanvasSizer } from "../../lib/canvas/dpr.js";
import type { CanvasSizer } from "../../lib/canvas/dpr.js";
import { LABEL_W, TRACKS, trackGeometry } from "./track-layout.js";
import type { TrackSpec } from "./track-layout.js";
import { renderTimeAxis, type AxisInputs } from "./axis.js";

export interface TracksViewModel {
  /** True once a trace is loaded (tracks render empty until then). */
  hasTrace: boolean;
  /** True while a task is selected (reveals the task-detail track). */
  taskSelected: boolean;
  viewStart: number;
  viewEnd: number;
  /**
   * Clock/format state the time-axis track (T25) reads to label its ticks.
   * The shell lifts it from the store via `deriveAxisInputs`; other tracks
   * ignore it (they render placeholders until their own tickets land).
   */
  axis: AxisInputs;
}

/** The tracks visible for a view model (task-detail only when selected). */
export function visibleTracks(vm: TracksViewModel): TrackSpec[] {
  return TRACKS.filter((t) => !t.selectionOnly || vm.taskSelected);
}

/**
 * The track column template. One `.d9-track` per visible track: a label
 * gutter (LABEL_W wide) plus a canvas host. The canvas carries data-*
 * attributes the parity row-walker reads to assert the placeholder
 * contract (label present, canvas sized by layout).
 */
export function tracksTemplate(vm: TracksViewModel): TemplateResult {
  const tracks = visibleTracks(vm);
  return html`
    <div
      class="d9-tracks"
      role="group"
      aria-label="Timeline tracks"
      style="--d9-label-w:${LABEL_W}px"
    >
      ${tracks.map(
        (t) => html`
          <div class="d9-track" data-track-id=${t.id} style="height:${t.height}px">
            <div class="d9-track-label" id="d9-track-label-${t.id}">
              <span class="d9-track-name">${t.label}</span>
              <span class="d9-track-owner" aria-hidden="true">${t.ownedBy}</span>
            </div>
            <div class="d9-track-canvas-wrap">
              <canvas
                class="d9-track-canvas"
                data-track-canvas=${t.id}
                aria-labelledby="d9-track-label-${t.id}"
                role="img"
              ></canvas>
            </div>
          </div>
        `,
      )}
    </div>
  `;
}

/** Per-track sizing result (returned for tests / the row-walker evidence). */
export interface TrackSizing {
  id: string;
  drawW: number;
  height: number;
}

// One sizer per live canvas element; keyed by the element so lit-html node
// reuse keeps the same sizer (and its geometry memo) across frames.
const sizers = new WeakMap<HTMLCanvasElement, CanvasSizer<CanvasRenderingContext2D>>();

/**
 * Measure the track column and size every track canvas to the shared
 * drawW (lib/canvas/layout). Paints each canvas an empty placeholder so a
 * correctly-sized, visibly-empty canvas is on screen (the DoD's
 * "placeholder" definition). Returns per-track sizing for assertions.
 *
 * Call after the template has rendered into `columnEl`, inside the store's
 * frame tick (the one place layout reads are batched, F3).
 */
export function sizeTracks(
  columnEl: HTMLElement,
  vm: TracksViewModel,
): TrackSizing[] {
  const dpr = (typeof devicePixelRatio === "number" ? devicePixelRatio : 1) || 1;
  // Full column width and the scrollbar gutter (so the draw area's right
  // edge matches the lanes' scrollable region, A12). offsetWidth includes
  // the scrollbar; clientWidth excludes it.
  const pw = columnEl.clientWidth;
  const scrollbarW = Math.max(0, columnEl.offsetWidth - columnEl.clientWidth);
  const out: TrackSizing[] = [];
  for (const track of visibleTracks(vm)) {
    const canvas = columnEl.querySelector<HTMLCanvasElement>(
      `canvas[data-track-canvas="${track.id}"]`,
    );
    if (!canvas) continue;
    const geometry = trackGeometry(track, {
      pw,
      scrollbarW,
      viewStart: vm.viewStart,
      viewEnd: vm.viewEnd,
      dpr,
    });
    const drawW = geometry.time.drawW;
    // Narrow-panel contract (lib/canvas/layout): drawW can be <= 0 on a
    // collapsed column; render nothing but keep the slot.
    if (drawW <= 0) {
      out.push({ id: track.id, drawW: 0, height: track.height });
      continue;
    }
    let sizer = sizers.get(canvas);
    if (!sizer) {
      sizer = createCanvasSizer<CanvasRenderingContext2D>(canvas);
      sizers.set(canvas, sizer);
    }
    const ctx = sizer.ensure(drawW, track.height, dpr);
    // The time-axis track (T25) is the first slot with real content: it
    // draws the F-row ruler instead of a placeholder. Every other track
    // stays an empty placeholder until its own ticket (T22-T30) fills it.
    if (track.id === "timeline") {
      renderTimeAxis(ctx, geometry, vm.viewStart, vm.viewEnd, vm.axis, vm.hasTrace);
    } else {
      paintPlaceholder(ctx, drawW, track.height, vm.hasTrace);
    }
    canvas.dataset["drawW"] = String(Math.round(drawW));
    out.push({ id: track.id, drawW, height: track.height });
  }
  return out;
}

/**
 * Paint an empty, correctly-sized placeholder: the track background plus a
 * baseline rule, so an empty-but-present canvas reads as "a track will draw
 * here" rather than a rendering bug. Deliberately minimal - real content is
 * each track's own ticket.
 */
function paintPlaceholder(
  ctx: CanvasRenderingContext2D,
  drawW: number,
  height: number,
  hasTrace: boolean,
): void {
  ctx.clearRect(0, 0, drawW, height);
  ctx.fillStyle = hasTrace ? "#12172a" : "#0f1424";
  ctx.fillRect(0, 0, drawW, height);
  // Faint bottom rule so stacked empty tracks are individually legible.
  ctx.strokeStyle = "rgba(255,255,255,0.06)";
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, height - 0.5);
  ctx.lineTo(drawW, height - 0.5);
  ctx.stroke();
}

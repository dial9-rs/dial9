// The unified time-aligned track column.
//
// Renders the slots: each track is a row = [ LABEL_W label gutter | draw
// canvas ], the canvas sized to the shared drawW from lib/canvas/layout so every
// track's time axis lines up vertically. Because every track uses ONE DOM label
// gutter of LABEL_W and a canvas of exactly drawW, the tracks are axis-aligned
// by construction; a track that drew its own internal gutter would break that,
// so the shell keeps the gutter in the DOM (matching the lanes' DOM-flex label).
//
// Declarative: the row structure is a lit-html template; canvas sizing is a
// post-render side effect (measure the column once per frame, size every backing
// store - geometry-change-only resizes via createCanvasSizer).

import { html, type TemplateResult } from "lit-html";
import { repeat } from "lit-html/directives/repeat.js";
import { createCanvasSizer } from "../../lib/canvas/dpr.js";
import type { CanvasSizer } from "../../lib/canvas/dpr.js";
import { LABEL_W, lanesScrollbarWidth, trackGeometry } from "../../lib/canvas/track-layout.js";
import {
  isFieldChartTrackId,
  type TrackId,
  type TrackSpec,
} from "../../lib/canvas/track-layout.js";
import type { FieldChartSpec } from "../../types/state.js";
import {
  COLLAPSED_TRACK_H,
  isCollapsed,
  isManageableTrack,
  orderedTracks,
  type TrackManageActions,
} from "./track-management.js";
import { renderTimeAxis, type AxisInputs } from "./axis.js";
import { isTrackClaimed } from "../../lib/canvas/track-renderers.js";
import { renderCpuTrack, type CpuInputs } from "./cpu.js";
import type { SpansTrackController } from "./spans-track.js";
import type { QueueTrackController } from "./queue-track.js";
import type { TaskDetailTrackController } from "./task-detail-track.js";
import type { EventsTrackController } from "./events-track.js";
import { fieldChartTrackSpecs } from "./field-chart-model.js";
import type { FieldChartTrackController } from "./field-chart-track.js";

export interface TracksViewModel {
  /** True once a trace is loaded (tracks render empty until then). */
  hasTrace: boolean;
  /** True while a task is selected (reveals the task-detail track). */
  taskSelected: boolean;
  viewStart: number;
  viewEnd: number;
  /**
   * Clock/format state the time-axis track reads to label its ticks. The shell
   * lifts it from the store via `deriveAxisInputs`; other tracks ignore it.
   */
  axis: AxisInputs;
  /**
   * CPU series + capacity + window state the CPU track renders. The shell lifts
   * it from the store via `deriveCpuInputs`; other tracks ignore it.
   */
  cpu: CpuInputs;
  /** URL-defined numeric-field charts appended to the manageable catalogue. */
  fieldCharts: readonly FieldChartSpec[];
  /**
   * Track management, lifted from uiPrefs by the shell. `trackOrder` reorders the
   * manageable analysis tracks; `collapsed` overrides a track's height to
   * label-only. A collapsed track stays in the visible list (row present, canvas
   * hidden) so re-expanding re-paints it from CURRENT windowed state.
   */
  trackOrder: readonly string[];
  collapsed: Readonly<Record<string, boolean>>;
  /**
   * Tracks the loaded trace carries no data for. These are dropped from the
   * column entirely, unlike a collapsed track (which keeps its row so it can be
   * re-expanded): a track that can never paint anything is not something the
   * user should have to fold away, and at full height it pushes real surfaces
   * past the scroll fold.
   *
   * Only consulted once a trace is loaded - before that every track is
   * trivially empty, and the empty column is the drop-zone's backdrop.
   */
  emptyTracks: ReadonlySet<TrackId>;
  /** Height (CSS px) of the worker-lanes scroll box. The lanes row sizes its
   *  viewport to this; the user drag-resizes it via the lanes bottom gutter. */
  lanesViewportHeight: number;
}

/**
 * The tracks visible for a view model, in the user's order: apply `trackOrder`
 * (manageable tracks permuted, structural tracks pinned), then drop the
 * selection-only task-detail track unless a task is selected, and drop tracks
 * the loaded trace has no data for. Collapsed tracks REMAIN visible
 * (label-only) - collapse is a height override, not a hide.
 */
export function visibleTracks(vm: TracksViewModel): TrackSpec[] {
  return orderedTracks(
    vm.trackOrder,
    fieldChartTrackSpecs(vm.fieldCharts),
  ).filter((t) => {
    if (t.selectionOnly && !vm.taskSelected) return false;
    if (vm.hasTrace && vm.emptyTracks.has(t.id)) return false;
    return true;
  });
}

/**
 * The TrackSpec a row renders at: the catalogue spec, or a label-only-height
 * clone when the track is collapsed. The controllers read `track.height`, so
 * passing the collapsed clone shrinks their row DOM; CSS
 * (`.d9-track-manage.is-collapsed`) hides the drawing body.
 */
function effectiveTrack(
  t: TrackSpec,
  collapsed: Readonly<Record<string, boolean>>,
): TrackSpec {
  return isCollapsed(collapsed, t.id) ? { ...t, height: COLLAPSED_TRACK_H } : t;
}

/**
 * The track column template. One `.d9-track` per visible track: a label gutter
 * (LABEL_W wide) plus a canvas host.
 *
 * Content tracks that need richer per-row DOM than a label + canvas (the spans
 * track's legend/filter controls + focused-span metadata; the events track's
 * name-chip legend) register a controller and render their OWN row template
 * here; every other track uses the uniform placeholder row. The delegation is
 * keyed by track id, mirroring the axis delegation in `sizeTracks`.
 */
export function tracksTemplate(
  vm: TracksViewModel,
  actions: TrackManageActions,
  spansTrack?: SpansTrackController,
  taskDetailTrack?: TaskDetailTrackController,
  eventsTrack?: EventsTrackController,
  queueTrack?: QueueTrackController,
): TemplateResult {
  const tracks = visibleTracks(vm);
  return html`
    <div
      class="d9-tracks"
      role="group"
      aria-label="Timeline tracks"
      style="--d9-label-w:${LABEL_W}px"
    >
      ${repeat(
        tracks,
        // Key by track id so lit-html MOVES a track's DOM (and its canvas
        // backing store) on reorder instead of repainting nodes in place -
        // otherwise a reordered canvas would show the previous track's pixels
        // until its next paint.
        (t) => t.id,
        (t) => {
          const eff = effectiveTrack(t, vm.collapsed);
          const inner = innerRow(
            eff,
            vm,
            spansTrack,
            taskDetailTrack,
            eventsTrack,
            queueTrack,
          );
          // Manageable tracks (the foldable analysis surfaces) gain the shell-
          // owned collapse caret + reorder grip; structural/task-detail tracks
          // render bare (they are pinned). The wrapper is outside each track's
          // own row renderer, so the delegation is unchanged.
          return isManageableTrack(t.id)
            ? manageWrapper(t, vm, actions, inner)
            : inner;
        },
      )}
    </div>
  `;
}

/**
 * Delegate to a track's own content renderer via the id-keyed branches, or fall
 * back to the uniform placeholder row. `t` is the EFFECTIVE spec: a collapsed
 * track carries the label-only height.
 */
function innerRow(
  t: TrackSpec,
  vm: TracksViewModel,
  spansTrack?: SpansTrackController,
  taskDetailTrack?: TaskDetailTrackController,
  eventsTrack?: EventsTrackController,
  queueTrack?: QueueTrackController,
): TemplateResult {
  if (t.id === "lanes") return lanesTrackRow(t, vm.lanesViewportHeight);
  if (t.id === "spans" && spansTrack !== undefined) return spansTrack.rowTemplate(t);
  if (t.id === "queue" && queueTrack !== undefined) return queueTrack.rowTemplate(t);
  if (t.id === "task-detail" && taskDetailTrack !== undefined) {
    return taskDetailTrack.rowTemplate(t);
  }
  if (t.id === "events" && eventsTrack !== undefined) return eventsTrack.rowTemplate(t);
  return defaultTrackRow(t);
}

/**
 * The worker-lanes row: a fixed-height scroll box the user can drag-resize. The
 * "Workers" label sits in the LABEL_W gutter; the box (`.d9-lanes-viewport`)
 * holds the sticky lanes canvas over a `.d9-lanes-spacer` whose height (set by
 * the lanes mount) makes the box scroll when the stacked rows exceed the box.
 * The `.d9-track-canvas-wrap` wrapper is the legend's overlay anchor (kept so
 * the legend pins to the box bottom, not the scrolling content), and the bottom
 * `.d9-lanes-resize` gutter is the drag target the lanes mount wires.
 */
function lanesTrackRow(t: TrackSpec, viewportHeight: number): TemplateResult {
  return html`
    <div class="d9-track d9-track--lanes" data-track-id=${t.id}>
      <div class="d9-lanes-head">
        <div class="d9-track-label" id="d9-track-label-lanes">
          <span class="d9-track-name">${t.label}</span>
        </div>
        <div class="d9-track-canvas-wrap">
          <div class="d9-lanes-viewport" style="height:${viewportHeight}px">
            <canvas
              class="d9-track-canvas d9-lanes-canvas"
              data-track-canvas=${t.id}
              aria-labelledby="d9-track-label-lanes"
              role="img"
            ></canvas>
            <div class="d9-lanes-spacer" aria-hidden="true"></div>
          </div>
        </div>
      </div>
      <div
        class="d9-lanes-resize"
        role="separator"
        aria-orientation="horizontal"
        aria-label="Resize the workers lane"
        title="Drag to resize the workers lane"
      ></div>
    </div>
  `;
}

// ── Track management overlay: collapse, reorder, dynamic close ─────────────
//
// The affordances are shell-owned and sit in a reserved strip over the LEFT
// edge of the label gutter (CSS `.d9-track-manage-strip`), so the per-track
// labels (the spans/queue/events controllers render their own) are never
// modified. The strip itself is pointer-events:none, while its controls accept
// pointer input, so the rest of the label (e.g. spans copy buttons) stays
// interactive.

// Id of the track whose grip is being dragged; module-level so `drop` can
// read it without relying on DataTransfer (jsdom/older browsers vary). Set on
// dragstart, cleared on dragend/drop. Transient, like the `sizers` memo below.
let dragSourceId: TrackId | null = null;

function onGripDragStart(e: DragEvent, id: TrackId): void {
  dragSourceId = id;
  if (e.dataTransfer !== null) {
    e.dataTransfer.effectAllowed = "move";
    // Best-effort payload for native DnD; the module var is the source of truth.
    try {
      e.dataTransfer.setData("text/plain", id);
    } catch {
      /* setData can throw in restricted contexts; the module var covers us. */
    }
  }
}

function onRowDragOver(e: DragEvent, id: TrackId): void {
  // Only a manageable target accepts a drop; preventDefault enables it.
  if (dragSourceId !== null && dragSourceId !== id && isManageableTrack(id)) {
    e.preventDefault();
    if (e.dataTransfer !== null) e.dataTransfer.dropEffect = "move";
  }
}

function onRowDrop(e: DragEvent, targetId: TrackId, actions: TrackManageActions): void {
  e.preventDefault();
  const src = dragSourceId;
  dragSourceId = null;
  if (src === null || src === targetId || !isManageableTrack(targetId)) return;
  actions.reorder(src, targetId);
}

function onGripDragEnd(): void {
  dragSourceId = null;
}

/**
 * Wrap a manageable track's row with the collapse caret + reorder grip and
 * the drop target. The wrapper is `position:relative` (the strip's containing
 * block) and carries `is-collapsed` so CSS can hide the drawing body.
 */
function manageWrapper(
  t: TrackSpec,
  vm: TracksViewModel,
  actions: TrackManageActions,
  inner: TemplateResult,
): TemplateResult {
  const collapsed = isCollapsed(vm.collapsed, t.id);
  return html`
    <div
      class="d9-track-manage ${collapsed ? "is-collapsed" : ""}
        ${isFieldChartTrackId(t.id) ? "is-dynamic" : ""}"
      data-track-manage=${t.id}
      @dragover=${(e: DragEvent) => onRowDragOver(e, t.id)}
      @drop=${(e: DragEvent) => onRowDrop(e, t.id, actions)}
    >
      <div class="d9-track-manage-strip" aria-hidden="false">
        <button
          type="button"
          class="d9-track-caret"
          aria-expanded=${collapsed ? "false" : "true"}
          aria-label=${collapsed
            ? `Expand ${t.label} track`
            : `Collapse ${t.label} track`}
          title=${collapsed ? "Expand track" : "Collapse track"}
          @click=${() => actions.toggleCollapse(t.id)}
        ></button>
        <span class="d9-track-strip-name" aria-hidden="true">${t.label}</span>
        <span
          class="d9-track-grip"
          draggable="true"
          role="button"
          tabindex="-1"
          aria-label=${`Drag to reorder ${t.label} track`}
          title="Drag to reorder"
          @dragstart=${(e: DragEvent) => onGripDragStart(e, t.id)}
          @dragend=${onGripDragEnd}
        ></span>
        ${isFieldChartTrackId(t.id)
          ? html`
              <button
                type="button"
                class="d9-track-close"
                aria-label=${`Close ${t.label} track`}
                title="Close chart"
                @click=${() => actions.close(t.id)}
              >
                &times;
              </button>
            `
          : null}
      </div>
      ${inner}
    </div>
  `;
}

/** The uniform placeholder row: label gutter + canvas host. */
function defaultTrackRow(t: TrackSpec): TemplateResult {
  return html`
    <div class="d9-track" data-track-id=${t.id} style="height:${t.height}px">
      <div class="d9-track-label" id="d9-track-label-${t.id}">
        <span class="d9-track-name">${t.label}</span>
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
  `;
}

/** Per-track sizing result (returned for tests). */
export interface TrackSizing {
  id: string;
  drawW: number;
  height: number;
}

// One sizer per live canvas element; keyed by the element so lit-html node
// reuse keeps the same sizer (and its geometry memo) across frames.
const sizers = new WeakMap<HTMLCanvasElement, CanvasSizer<CanvasRenderingContext2D>>();

/**
 * Measure the track column and size every track canvas to the shared drawW
 * (lib/canvas/layout). Paints each canvas an empty placeholder so a
 * correctly-sized, visibly-empty canvas is on screen. Returns per-track sizing
 * for assertions.
 *
 * Call after the template has rendered into `columnEl`, inside the store's frame
 * tick (the one place layout reads are batched).
 */
export function sizeTracks(
  columnEl: HTMLElement,
  vm: TracksViewModel,
  spansTrack?: SpansTrackController,
  taskDetailTrack?: TaskDetailTrackController,
  eventsTrack?: EventsTrackController,
  queueTrack?: QueueTrackController,
  fieldChartTrack?: FieldChartTrackController,
): TrackSizing[] {
  const dpr = (typeof devicePixelRatio === "number" ? devicePixelRatio : 1) || 1;
  // Full column width and the LANES-BOX scrollbar gutter, so every track's draw
  // area right edge lines up with the worker-lanes draw area (the box owns the
  // scrollbar, not the column).
  const pw = columnEl.clientWidth;
  const scrollbarW = lanesScrollbarWidth(columnEl);
  const out: TrackSizing[] = [];
  for (const track of visibleTracks(vm)) {
    // A collapsed track is label-only: its drawing body is hidden by CSS
    // (`.d9-track-manage.is-collapsed`) and its canvas is not painted this
    // frame - saving the work; the stale backing store stays hidden. Re-
    // expanding flips this off and a normal render+size pass re-paints it from
    // CURRENT windowed state, so a collapsed track still respects windowing on
    // re-expand.
    if (isCollapsed(vm.collapsed, track.id)) {
      out.push({ id: track.id, drawW: 0, height: COLLAPSED_TRACK_H });
      continue;
    }
    // A track whose content is owned by a mounted renderer sizes AND draws its
    // own canvas on its own store subscription. The shell leaves it alone - no
    // placeholder paint, no backing-store resize that would clear the renderer's
    // last draw.
    if (isTrackClaimed(track.id)) {
      out.push({ id: track.id, drawW: 0, height: track.height });
      continue;
    }
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
    // The spans track owns its own canvas sizing + draw: it reserves a controls
    // strip above the canvas, so its draw area is shorter than the full track
    // height. Delegate and skip the uniform placeholder path.
    if (track.id === "spans" && spansTrack !== undefined) {
      spansTrack.paint(canvas, drawW, track.height, dpr, vm.viewStart, vm.viewEnd);
      canvas.dataset["drawW"] = String(Math.round(drawW));
      out.push({ id: track.id, drawW, height: track.height });
      continue;
    }
    // The queue track owns its own canvas sizing + draw: it reserves a legend
    // strip above the canvas, so its draw area is shorter than the full track
    // height (like the spans track). Delegate and skip the placeholder.
    if (track.id === "queue" && queueTrack !== undefined) {
      queueTrack.paint(canvas, drawW, track.height, dpr, vm.viewStart, vm.viewEnd);
      canvas.dataset["drawW"] = String(Math.round(drawW));
      out.push({ id: track.id, drawW, height: track.height });
      continue;
    }
    // The task-detail track likewise owns its own canvas sizing + draw (it hosts
    // a status readout + interaction). Its canvas fills the full track height
    // (no controls strip). Only reached while a task is selected. Delegate and
    // skip the placeholder path.
    if (track.id === "task-detail" && taskDetailTrack !== undefined) {
      taskDetailTrack.paint(canvas, drawW, track.height, dpr, vm.viewStart, vm.viewEnd);
      canvas.dataset["drawW"] = String(Math.round(drawW));
      out.push({ id: track.id, drawW, height: track.height });
      continue;
    }
    // The custom-events track likewise reserves a legend strip above its canvas,
    // so it owns its own sizing + draw. Same delegation shape.
    if (track.id === "events" && eventsTrack !== undefined) {
      eventsTrack.paint(canvas, drawW, track.height, dpr, vm.viewStart, vm.viewEnd);
      canvas.dataset["drawW"] = String(Math.round(drawW));
      out.push({ id: track.id, drawW, height: track.height });
      continue;
    }
    if (isFieldChartTrackId(track.id) && fieldChartTrack !== undefined) {
      const spec = vm.fieldCharts.find((chart) => chart.id === track.id);
      if (spec !== undefined) {
        fieldChartTrack.paint(
          canvas,
          geometry,
          spec,
          vm.viewStart,
          vm.viewEnd,
        );
        canvas.dataset["drawW"] = String(Math.round(drawW));
        out.push({ id: track.id, drawW, height: track.height });
        continue;
      }
    }
    let sizer = sizers.get(canvas);
    if (!sizer) {
      sizer = createCanvasSizer<CanvasRenderingContext2D>(canvas);
      sizers.set(canvas, sizer);
    }
    const ctx = sizer.ensure(drawW, track.height, dpr);
    // Tracks with landed content render it; the rest stay empty placeholders.
    //  - timeline: the time-axis ruler.
    //  - cpu: the avg-cores bar chart; its render returns the info readout,
    //    mirrored into a DOM attribute for tests.
    if (track.id === "timeline") {
      renderTimeAxis(ctx, geometry, vm.viewStart, vm.viewEnd, vm.axis, vm.hasTrace);
    } else if (track.id === "cpu") {
      const readout = renderCpuTrack(
        ctx,
        geometry,
        vm.viewStart,
        vm.viewEnd,
        vm.cpu,
        vm.hasTrace,
      );
      canvas.dataset["cpuReadout"] = readout;
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
 * here" rather than a rendering bug. Deliberately minimal.
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

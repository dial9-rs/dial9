// Store-integration + render tests for the queue track. The browser-driven
// halves (row-walker, zero-global visual check) are covered elsewhere; what
// lives at the store/render-input boundary is asserted here:
//   1. render: with an all-zero global series, drawQueueCanvas plots the global
//      line on the VISIBLE zero baseline (strictly above the axis), not fused to
//      the canvas bottom - the render-side complement to the queueScaleY unit
//      test (queue-model.test.ts).
//   2. dispatch: commitRange writes the drag-selected range to
//      selection.spawnedTasksRange, idempotently.

import { describe, it, expect } from "vitest";
import { createViewerStore } from "./store.js";
import { createQueueTrack, drawQueueCanvas } from "./queue-track.js";
import { ZERO_BASELINE_PX, queueBaselineY, type QueueRenderModel, type QueueWindow } from "./queue-model.js";
import type { ParsedTrace } from "../../types/trace.js";

const COMPLETE: QueueWindow = { truncatedAt: null, oversized: false };

/** A recording 2D context: captures path vertices, fills, strokes, labels. */
interface Rec {
  pathYs: number[];
  fillRects: { x: number; y: number; w: number; h: number }[];
  strokes: number;
  labels: { text: string; x: number; y: number }[];
}
function recordingCtx(): { ctx: CanvasRenderingContext2D; rec: Rec } {
  const rec: Rec = { pathYs: [], fillRects: [], strokes: 0, labels: [] };
  const ctx = {
    fillStyle: "",
    strokeStyle: "",
    lineWidth: 1,
    font: "",
    textAlign: "" as CanvasTextAlign,
    clearRect() {},
    fillRect(x: number, y: number, w: number, h: number) {
      rec.fillRects.push({ x, y, w, h });
    },
    strokeRect() {},
    beginPath() {},
    closePath() {},
    moveTo(_x: number, y: number) {
      rec.pathYs.push(y);
    },
    lineTo(_x: number, y: number) {
      rec.pathYs.push(y);
    },
    fill() {},
    stroke() {
      rec.strokes++;
    },
    setLineDash() {},
    fillText(text: string, x: number, y: number) {
      rec.labels.push({ text, x, y });
    },
  } as unknown as CanvasRenderingContext2D;
  return { ctx, rec };
}

const zeros = (n: number): number[] => new Array<number>(n).fill(0);

describe("drawQueueCanvas (S6: zero-global renders a visible baseline)", () => {
  it("plots an all-zero global series on the baseline, above the axis", () => {
    const canvasH = 56;
    // A model with samples present (hasData) but every value 0 - the exact
    // split-brain #282 case: a global queue that stays at 0 across the window.
    const model: QueueRenderModel = {
      numBuckets: 4,
      global: zeros(4),
      local: zeros(4),
      maxQ: 1,
      activeTask: null,
      hasData: true,
    };
    const { ctx, rec } = recordingCtx();
    drawQueueCanvas(ctx, model, COMPLETE, 4, canvasH);

    // CHART_TOP=12, CHART_BOTTOM=6 (queue-track.ts); the visible baseline is
    // ZERO_BASELINE_PX above the chart bottom.
    const chartTop = 12;
    const chartH = canvasH - 12 - 6;
    const baselineY = queueBaselineY(chartTop, chartH);

    expect(rec.pathYs.length).toBeGreaterThan(0);
    // No vertex sits on the axis (the invisible-at-zero bug); every plotted
    // point is at least ZERO_BASELINE_PX above the chart bottom.
    const axisBottom = chartTop + chartH;
    for (const y of rec.pathYs) {
      expect(y).toBeLessThanOrEqual(axisBottom - ZERO_BASELINE_PX);
    }
    // The zero series is a flat, visible line ON the baseline.
    expect(rec.pathYs.some((y) => y === baselineY)).toBe(true);
    expect(Math.max(...rec.pathYs)).toBe(baselineY);
    expect(baselineY).toBeLessThan(canvasH);
    expect(rec.strokes).toBeGreaterThan(0);
  });

  it("renders the max + 0 axis labels and, when present, the tasks label", () => {
    const model: QueueRenderModel = {
      numBuckets: 3,
      global: [1, 2, 2],
      local: [0, 1, 1],
      maxQ: 2,
      activeTask: { points: [{ x: 10, count: 4 }], startCount: 1, maxTasks: 4 },
      hasData: true,
    };
    const { ctx, rec } = recordingCtx();
    drawQueueCanvas(ctx, model, COMPLETE, 3, 56);
    const texts = rec.labels.map((l) => l.text);
    expect(texts).toContain("2"); // maxQ label
    expect(texts).toContain("0"); // zero-baseline label
    expect(texts).toContain("tasks:4"); // active-task right-axis label
  });

  it("shows the empty message and no strokes when there is no data", () => {
    const model: QueueRenderModel = {
      numBuckets: 4,
      global: zeros(4),
      local: zeros(4),
      maxQ: 1,
      activeTask: null,
      hasData: false,
    };
    const { ctx, rec } = recordingCtx();
    drawQueueCanvas(ctx, model, COMPLETE, 4, 56);
    expect(rec.labels.map((l) => l.text)).toContain("No queue samples in view");
    expect(rec.strokes).toBe(0);
  });

  it("surfaces an oversized window with a partial-window badge (T17)", () => {
    const model: QueueRenderModel = {
      numBuckets: 2,
      global: [0, 0],
      local: [0, 0],
      maxQ: 1,
      activeTask: null,
      hasData: true,
    };
    const { ctx, rec } = recordingCtx();
    drawQueueCanvas(ctx, model, { truncatedAt: null, oversized: true }, 2, 56);
    expect(rec.labels.map((l) => l.text)).toContain("partial window");
  });
});

describe("createQueueTrack (M7 dispatch through the selection slice)", () => {
  function newStore() {
    const pending: (() => void)[] = [];
    const store = createViewerStore({ scheduler: (cb) => pending.push(cb) });
    return { store, pending };
  }

  it("commitRange writes the drag range to selection.spawnedTasksRange", () => {
    const { store } = newStore();
    const track = createQueueTrack(store);
    expect(store.getState().selection.spawnedTasksRange).toBeNull();

    track.commitRange({ startNs: 100, endNs: 500 });
    expect(store.getState().selection.spawnedTasksRange).toEqual({
      startNs: 100,
      endNs: 500,
    });
    track.dispose();
  });

  it("does not re-dispatch when the range is unchanged (no store thrash)", () => {
    const { store, pending } = newStore();
    const track = createQueueTrack(store);

    track.commitRange({ startNs: 100, endNs: 500 });
    expect(pending.length).toBe(1); // one frame scheduled for the write
    pending.splice(0).forEach((cb) => cb());

    // An identical range commits nothing - no new frame is scheduled.
    track.commitRange({ startNs: 100, endNs: 500 });
    expect(pending.length).toBe(0);
    track.dispose();
  });

  it("commitRange(null) clears the range", () => {
    const { store } = newStore();
    const track = createQueueTrack(store);
    track.commitRange({ startNs: 1, endNs: 2 });
    track.commitRange(null);
    expect(store.getState().selection.spawnedTasksRange).toBeNull();
    track.dispose();
  });

  it("spawnedTasks delegates to the derivation over the current trace data", () => {
    const { store } = newStore();
    const track = createQueueTrack(store);
    // A trace with no first-polled tasks -> no spawned tasks in any range.
    const trace = {
      events: [],
      maxTs: 1000,
      blockInPlaceGaps: [],
      runtimeWorkers: new Map(),
      taskSpawnTimes: new Map(),
      taskTerminateTimes: new Map(),
      taskSpawnLocs: new Map(),
      spawnLocations: new Map(),
      hasTaskTracking: false,
    } as unknown as ParsedTrace;
    store.update("trace", { trace });
    expect(track.spawnedTasks({ startNs: 0, endNs: 1000 })).toBeNull();
    track.dispose();
  });
});

// Store-integration tests for the task-detail track. The checks that live at
// the store/render-input boundary (the browser-driven halves - row-walker,
// visual - are covered elsewhere):
//   1. derived-cache invalidation: the selection-keyed collection recomputes
//      when the SELECTED TASK changes, a PAN does not recompute it, and a
//      waker-hover selection write reuses it (no O(all-polls) re-collect).
//   2. waker-hover dispatch: hovering a waker label writes
//      selection.hoveredWakerTaskId - the store field the lanes consume.
//   3. canvas click dispatch: waker labels select their task, while dumped idle
//      spans store their captures for the inspector Stack flamegraph.
//   4. drawTaskDetailCanvas render input: the hovered waker's label bolds, and
//      the window markers surface a truncated/oversized window.

import { describe, it, expect } from "vitest";
import { EVENT_TYPES, type ParsedTrace } from "../../lib/trace/index.js";
import { createViewerStore } from "./store.js";
import {
  createTaskDetailDerivation,
  createTaskDetailTrack,
  drawTaskDetailCanvas,
} from "./task-detail-track.js";
import {
  BAND_TOP,
  buildTaskDetailRenderModel,
  COMPLETE_TASK_DETAIL_WINDOW,
  type TaskDetailWindow,
} from "./task-detail-model.js";

// ── Minimal real trace (buildWorkerSpans over synthetic poll events) ──────

function pollEvents(
  workerId: number,
  taskId: number,
  start: number,
  end: number,
  spawnLocId = "L",
): unknown[] {
  return [
    { eventType: EVENT_TYPES.PollStart, workerId, timestamp: start, taskId, spawnLocId, spawnLoc: null, localQueue: 0 },
    { eventType: EVENT_TYPES.PollEnd, workerId, timestamp: end, localQueue: 0 },
  ];
}

function wakeEvent(timestamp: number, wakerTaskId: number, wokenTaskId: number): unknown {
  return { eventType: EVENT_TYPES.WakeEvent, timestamp, wakerTaskId, wokenTaskId, targetWorker: 0 };
}

/** A ParsedTrace stub carrying only what buildTaskPollSource + the collection
 *  read: events, bounds, gaps/runtime, and the task maps. */
function traceWith(events: unknown[]): ParsedTrace {
  return {
    events,
    minTs: 0,
    maxTs: 2000,
    blockInPlaceGaps: [],
    runtimeWorkers: new Map(),
    spawnLocations: new Map([["L", "src/app/task.rs:7"]]),
    taskSpawnLocs: new Map(),
    taskSpawnTimes: new Map([[42, 100]]),
    taskTerminateTimes: new Map([[42, 1500]]),
    taskInstrumented: new Map(),
    taskDumps: new Map(),
  } as unknown as ParsedTrace;
}

// Two tasks (42 and 7) each polled once, plus a wake for task 42 (waker 500).
function demoTrace(): ParsedTrace {
  return traceWith([
    ...pollEvents(0, 42, 1000, 1100),
    ...pollEvents(0, 7, 1200, 1300),
    wakeEvent(500, 500, 42),
  ]);
}

// ── 1. Derived-cache invalidation ─────────────────────────────────────────

describe("task-detail derived cache", () => {
  it("pan does not recompute; task change invalidates; hover reuses it", () => {
    const store = createViewerStore({ scheduler: () => {} });
    const derivation = createTaskDetailDerivation(store);
    store.update("trace", { trace: demoTrace() });
    store.update("selection", { selectedTaskId: 42 });

    const first = derivation();
    expect(first.taskId).toBe(42);
    expect(first.pollCount).toBe(1);
    expect(first.polls[0]!.start).toBe(1000);

    // A pan (viewport change) is NOT a dep of the collection - same reference.
    store.update("viewport", { viewStart: 5, viewEnd: 1999 });
    expect(derivation()).toBe(first);

    // A waker-hover selection write changes the selection slice but NOT the
    // task, so the inner (source, taskId) guard reuses the collection (no
    // O(all-polls) re-collect on every hover frame).
    store.update("selection", { hoveredWakerTaskId: 500 });
    expect(derivation()).toBe(first);

    // Selecting a DIFFERENT task invalidates it (a genuine re-collect).
    store.update("selection", { selectedTaskId: 7 });
    const second = derivation();
    expect(second).not.toBe(first);
    expect(second.taskId).toBe(7);
    expect(second.polls[0]!.start).toBe(1200);
  });

  it("resolves the numbers from the trace maps for the selected task", () => {
    const store = createViewerStore({ scheduler: () => {} });
    const derivation = createTaskDetailDerivation(store);
    store.update("trace", { trace: demoTrace() });
    store.update("selection", { selectedTaskId: 42 });
    const d = derivation();
    expect(d.spawnLocation).toBe("src/app/task.rs:7");
    expect(d.spawnTs).toBe(100);
    expect(d.terminateTs).toBe(1500);
    expect(d.lifetimeNs).toBe(1400);
    expect(d.wakeCount).toBe(1);
  });
});

// ── 2. Waker-hover dispatch ───────────────────────────────────────────────

describe("waker-hover dispatch (the lanes contract)", () => {
  function setup() {
    const store = createViewerStore({ scheduler: () => {} });
    const track = createTaskDetailTrack(store);
    store.update("trace", { trace: demoTrace() });
    store.update("selection", { selectedTaskId: 42 });
    // A wide wake band: poll@1000 with a wake@500 over a 0..2000 / 2000px view
    // is 500px wide (> 40), so a waker region exists.
    const model = buildTaskDetailRenderModel({
      data: track.taskDetail(),
      viewStart: 0,
      viewEnd: 2000,
      drawW: 2000,
    });
    return { store, track, model };
  }

  it("has a waker region for the wide wake band", () => {
    const { model } = setup();
    expect(model.wakeRegions.map((r) => r.wakerTaskId)).toEqual([500]);
  });

  it("hovering a waker label dispatches selection.hoveredWakerTaskId", () => {
    const { store, track, model } = setup();
    const region = model.wakeRegions[0]!;
    expect(store.getState().selection.hoveredWakerTaskId).toBeNull();
    track.hoverWaker(model, (region.x1 + region.x2) / 2, (region.y1 + region.y2) / 2);
    expect(store.getState().selection.hoveredWakerTaskId).toBe(500);
  });

  it("hovering off any waker label clears the highlight", () => {
    const { store, track, model } = setup();
    store.update("selection", { hoveredWakerTaskId: 500 });
    // A point above the label strip is over no waker region.
    track.hoverWaker(model, 10, BAND_TOP + 1);
    expect(store.getState().selection.hoveredWakerTaskId).toBeNull();
  });

  it("clearHover resets the highlight (mouseleave)", () => {
    const { store, track } = setup();
    store.update("selection", { hoveredWakerTaskId: 500 });
    track.clearHover();
    expect(store.getState().selection.hoveredWakerTaskId).toBeNull();
  });

  it("clicking a waker label selects that waker task", () => {
    const { store, track, model } = setup();
    const region = model.wakeRegions[0]!;
    track.clickAt(model, (region.x1 + region.x2) / 2, (region.y1 + region.y2) / 2);
    expect(store.getState().selection.selectedTaskId).toBe(500);
    expect(store.getState().selection.hoveredWakerTaskId).toBeNull();
  });
});

// ── 3. Task-dump click dispatch ───────────────────────────────────────────

describe("task-dump click dispatch", () => {
  function setup() {
    const store = createViewerStore({ scheduler: () => {} });
    const track = createTaskDetailTrack(store);
    const dumps = [{ timestamp: 200, callchain: ["leaf", "root"] }];
    const model = buildTaskDetailRenderModel({
      data: {
        taskId: 42,
        polls: [
          { start: 100, end: 150, taskId: 42, spawnLocId: "L", spawnLoc: null },
          { start: 300, end: 350, taskId: 42, spawnLocId: "L", spawnLoc: null },
          { start: 600, end: 650, taskId: 42, spawnLocId: "L", spawnLoc: null },
        ],
        wakes: [],
        pollWakes: [null, null, null],
        pollCount: 3,
        wakeCount: 0,
        spawnLocation: null,
        isInstrumented: true,
        spawnTs: null,
        terminateTs: null,
        hasTerminate: false,
        lifetimeNs: null,
        taskDumps: dumps,
        workerIdCount: 1,
        hasPolls: true,
      },
      viewStart: 0,
      viewEnd: 1000,
      drawW: 1000,
    });
    const hit = model.hitRegions.find((region) => region.dumps !== null)!;
    return { store, track, model, hit };
  }

  it("stores a semantic anchor for the clicked dump-bearing idle span", () => {
    const { store, track, model, hit } = setup();
    store.update("selection", { selectedTaskId: 42 });

    track.clickAt(model, (hit.x1 + hit.x2) / 2, BAND_TOP + 1);

    expect(store.getState().selection.taskDump).toEqual({
      taskId: 42,
      timestamps: [200],
    });
  });

  it("ignores a click when the painted model belongs to the previously selected task", () => {
    const { store, track, model, hit } = setup();
    store.update("selection", { selectedTaskId: 7 });

    track.clickAt(model, (hit.x1 + hit.x2) / 2, BAND_TOP + 1);

    expect(store.getState().selection.taskDump).toBeNull();
  });
});

// ── 4. drawTaskDetailCanvas render input (bolding + markers) ───────────────

interface DrawnText {
  text: string;
  font: string;
  fillStyle: string;
}
interface DrawnRect {
  x: number;
  w: number;
  fillStyle: string;
}

/** A recording 2d context capturing the fills/texts drawTaskDetailCanvas emits. */
function recordingCtx(): {
  ctx: CanvasRenderingContext2D;
  texts: DrawnText[];
  rects: DrawnRect[];
} {
  const texts: DrawnText[] = [];
  const rects: DrawnRect[] = [];
  const ctx = {
    globalAlpha: 1,
    fillStyle: "",
    strokeStyle: "",
    font: "",
    textAlign: "left",
    lineWidth: 1,
    clearRect() {},
    fillRect(x: number, _y: number, w: number, _h: number) {
      const self = this as CanvasRenderingContext2D;
      rects.push({ x, w, fillStyle: String(self.fillStyle) });
    },
    fillText(text: string, _x: number, _y: number) {
      const self = this as CanvasRenderingContext2D;
      texts.push({ text, font: self.font, fillStyle: String(self.fillStyle) });
    },
    strokeRect() {},
    beginPath() {},
    moveTo() {},
    lineTo() {},
    stroke() {},
    fill() {},
    closePath() {},
    rect() {},
    clip() {},
    save() {},
    restore() {},
    setLineDash() {},
    setTransform() {},
    scale() {},
  } as unknown as CanvasRenderingContext2D;
  return { ctx, texts, rects };
}

function wakeBandModel() {
  // One poll@1000 with a wake@500 (waker 500) over 0..2000 / 2000px: a 500px
  // band whose waker label is drawn (w > 40).
  const data = {
    taskId: 42,
    polls: [{ start: 1000, end: 1100, taskId: 42, spawnLocId: "L", spawnLoc: null }],
    wakes: [],
    pollWakes: [{ wake: { timestamp: 500, wakerTaskId: 500, targetWorker: 0 }, effectiveWake: 500, wakerLabel: "task 0x1f4" }],
    pollCount: 1,
    wakeCount: 0,
    spawnLocation: null,
    isInstrumented: true,
    spawnTs: null,
    terminateTs: null,
    hasTerminate: false,
    lifetimeNs: null,
    taskDumps: [],
    workerIdCount: 1,
    hasPolls: true,
  } as const;
  return buildTaskDetailRenderModel({ data, viewStart: 0, viewEnd: 2000, drawW: 2000 });
}

describe("drawTaskDetailCanvas render input", () => {
  it("draws a high spawn-to-first-poll delay in scheduling red", () => {
    const model = buildTaskDetailRenderModel({
      data: {
        taskId: 42,
        polls: [
          {
            start: 8_135_941,
            end: 8_235_941,
            taskId: 42,
            spawnLocId: "L",
            spawnLoc: null,
          },
        ],
        wakes: [],
        pollWakes: [null],
        pollCount: 1,
        wakeCount: 0,
        spawnLocation: null,
        isInstrumented: true,
        spawnTs: 1_000_000,
        terminateTs: null,
        hasTerminate: false,
        lifetimeNs: null,
        taskDumps: [],
        workerIdCount: 1,
        hasPolls: true,
      },
      viewStart: 0,
      viewEnd: 10_000_000,
      drawW: 1000,
    });
    const { ctx, rects, texts } = recordingCtx();

    drawTaskDetailCanvas(
      ctx,
      model,
      null,
      1000,
      160,
      COMPLETE_TASK_DETAIL_WINDOW,
    );

    expect(
      rects.some(
        (r) =>
          r.x === 100 &&
          r.w === 713.5941 &&
          r.fillStyle === "rgba(255,50,50,0.3)",
      ),
    ).toBe(true);
    expect(texts.some((t) => t.text.startsWith("⬆"))).toBe(false);
  });

  it("bolds the hovered waker's label", () => {
    const model = wakeBandModel();
    const hovered = recordingCtx();
    drawTaskDetailCanvas(hovered.ctx, model, 500, 2000, 160, COMPLETE_TASK_DETAIL_WINDOW);
    const bold = hovered.texts.find((t) => t.text.startsWith("⬆"));
    expect(bold?.font).toBe("bold 8px monospace");
    expect(bold?.fillStyle).toBe("#fff");

    const plain = recordingCtx();
    drawTaskDetailCanvas(plain.ctx, model, null, 2000, 160, COMPLETE_TASK_DETAIL_WINDOW);
    const notBold = plain.texts.find((t) => t.text.startsWith("⬆"));
    expect(notBold?.font).toBe("8px monospace");
    expect(notBold?.fillStyle).toBe("#66bb6a");
  });

  it("draws the legend", () => {
    const model = wakeBandModel();
    const { ctx, texts } = recordingCtx();
    drawTaskDetailCanvas(ctx, model, null, 2000, 160, COMPLETE_TASK_DETAIL_WINDOW);
    expect(texts.some((t) => t.text === "Task")).toBe(true);
    expect(texts.some((t) => t.text === "▲ = wake")).toBe(true);
  });

  it("surfaces a truncated/oversized window rather than a clean edge", () => {
    const model = wakeBandModel();
    const window: TaskDetailWindow = { truncatedAt: "both", oversized: true };
    const { ctx, texts, rects } = recordingCtx();
    drawTaskDetailCanvas(ctx, model, null, 2000, 160, window);
    // "partial window" badge for the oversized segment.
    expect(texts.some((t) => t.text === "partial window")).toBe(true);
    // A hatch band at each truncated edge (left x=0 and right x=drawW-6).
    const trunc = rects.filter((r) => r.fillStyle === "rgba(255,120,120,0.28)");
    expect(trunc.some((r) => r.x === 0)).toBe(true);
    expect(trunc.some((r) => r.x === 2000 - 6)).toBe(true);
  });

  it("no-ops the window markers for a complete window", () => {
    const model = wakeBandModel();
    const { ctx, texts } = recordingCtx();
    drawTaskDetailCanvas(ctx, model, null, 2000, 160, COMPLETE_TASK_DETAIL_WINDOW);
    expect(texts.some((t) => t.text === "partial window")).toBe(false);
  });
});

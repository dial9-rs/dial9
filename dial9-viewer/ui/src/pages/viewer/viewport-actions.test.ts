// Viewport actions: the pure zoom/pan/fit/region math and the store-bound
// actions with the zoom-history stack + the coalescing contract (many zoom
// calls in one frame -> one render).

import { describe, it, expect, vi } from "vitest";
import {
  zoomWindow,
  fitWindow,
  panWindow,
  regionWindow,
  createViewportActions,
  MIN_VIEW_NS,
  type ViewWindow,
} from "./viewport-actions.js";
import { createViewerStore } from "./store.js";
import type { FrameScheduler } from "../../store/store.js";
import type { ViewportSlice } from "../../types/state.js";

const VP: ViewportSlice = { viewStart: 0, viewEnd: 1_000, minTs: 0, maxTs: 10_000 };

// ── pure math ─────────────────────────────────────────────────────────────

describe("zoomWindow (viewer.html:5090)", () => {
  it("zoom in about the centre halves the visible duration", () => {
    expect(zoomWindow(VP, 0.5)).toEqual({ viewStart: 250, viewEnd: 750 });
  });

  it("zoom out about the centre doubles it, clamped to bounds", () => {
    // dur 1000 -> 2000, centre 500 -> [-500,1500] clamped -> [0,1500]
    expect(zoomWindow(VP, 2)).toEqual({ viewStart: 0, viewEnd: 1_500 });
  });

  it("clamps the duration to the 100ns floor", () => {
    const tiny: ViewportSlice = { viewStart: 0, viewEnd: 100, minTs: 0, maxTs: 10_000 };
    // dur 100 * 0.5 = 50, floored to 100 -> stays [0,100]
    expect(zoomWindow(tiny, 0.5)).toEqual({ viewStart: 0, viewEnd: MIN_VIEW_NS });
  });

  it("honours a non-centre fraction (cursor-anchored wheel zoom)", () => {
    // centre = 0 + 1000*0.25 = 250; newDur 500; start 250-125=125; end 250+375=625
    expect(zoomWindow(VP, 0.5, 0.25)).toEqual({ viewStart: 125, viewEnd: 625 });
  });
});

describe("fitWindow / panWindow / regionWindow", () => {
  it("fit returns the full navigable extent", () => {
    expect(fitWindow(VP)).toEqual({ viewStart: 0, viewEnd: 10_000 });
  });

  it("pan right by 20% preserves duration", () => {
    const mid: ViewportSlice = { viewStart: 1_000, viewEnd: 2_000, minTs: 0, maxTs: 10_000 };
    expect(panWindow(mid, 1)).toEqual({ viewStart: 1_200, viewEnd: 2_200 });
  });

  it("pan left clamps at minTs, keeping the width", () => {
    const near: ViewportSlice = { viewStart: 100, viewEnd: 1_100, minTs: 0, maxTs: 10_000 };
    // shift 200 left -> [-100,900] -> clamp start 0, end 1000
    expect(panWindow(near, -1)).toEqual({ viewStart: 0, viewEnd: 1_000 });
  });

  it("region zoom clamps [start,end] to bounds and orders the pair", () => {
    expect(regionWindow(VP, 800, 300)).toEqual({ viewStart: 300, viewEnd: 800 });
    expect(regionWindow(VP, -50, 99_999)).toEqual({ viewStart: 0, viewEnd: 10_000 });
  });
});

// ── store-bound actions + history + coalescing ───────────────────────────

function manualStore(): { store: ReturnType<typeof createViewerStore>; frame: () => void } {
  const queue: (() => void)[] = [];
  const scheduler: FrameScheduler = (cb) => {
    queue.push(cb);
  };
  const store = createViewerStore({ scheduler });
  store.update("viewport", { viewStart: 0, viewEnd: 1_000, minTs: 0, maxTs: 10_000 });
  // flush the seed so it is not counted in the coalescing assertions
  for (const cb of queue.splice(0)) cb();
  return { store, frame: () => { for (const cb of queue.splice(0)) cb(); } };
}

describe("createViewportActions - store dispatch", () => {
  it("zoom/fit/pan write the viewport slice", () => {
    const { store, frame } = manualStore();
    const actions = createViewportActions(store);
    actions.zoom(0.5);
    frame();
    expect(store.getState().viewport).toMatchObject({ viewStart: 250, viewEnd: 750 });
    actions.fit();
    frame();
    expect(store.getState().viewport).toMatchObject({ viewStart: 0, viewEnd: 10_000 });
  });

  it("applyPan updates the viewport WITHOUT recording history", () => {
    const { store, frame } = manualStore();
    const actions = createViewportActions(store);
    actions.recordCurrent(); // baseline [0,1000]
    actions.applyPan({ viewStart: 500, viewEnd: 1_500 });
    frame();
    expect(store.getState().viewport).toMatchObject({ viewStart: 500, viewEnd: 1_500 });
    expect(actions.undoDepth()).toBe(0); // pan frames do not push history
  });
});

describe("createViewportActions - zoom history", () => {
  it("undo steps back to the previous committed view", () => {
    const { store, frame } = manualStore();
    const actions = createViewportActions(store);
    actions.recordCurrent(); // baseline [0,1000]
    actions.zoom(0.5); // -> [250,750]
    frame();
    expect(actions.undoDepth()).toBe(1);
    expect(actions.undo()).toBe(true);
    frame();
    expect(store.getState().viewport).toMatchObject({ viewStart: 0, viewEnd: 1_000 });
  });

  it("undo with an empty stack returns false and no-ops", () => {
    const { store } = manualStore();
    const actions = createViewportActions(store);
    expect(actions.undo()).toBe(false);
  });

  it("resetHistory drops the stack (new trace)", () => {
    const { store } = manualStore();
    const actions = createViewportActions(store);
    actions.recordCurrent();
    actions.zoom(2);
    expect(actions.undoDepth()).toBe(1);
    actions.resetHistory();
    expect(actions.undoDepth()).toBe(0);
  });
});

describe("coalescing: many zooms in one frame => one render", () => {
  it("6 wheel-style zooms flush a single subscriber notification", () => {
    const { store, frame } = manualStore();
    const actions = createViewportActions(store);
    const renders = vi.fn();
    store.subscribe(["viewport"], renders);
    // Six rapid cursor-anchored zooms, as a wheel storm would emit.
    for (let i = 0; i < 6; i++) actions.zoom(1 / 1.3, 0.5);
    expect(renders).not.toHaveBeenCalled(); // nothing until the frame flushes
    frame();
    expect(renders).toHaveBeenCalledTimes(1); // coalesced to ONE render
    // The final viewport reflects all six zooms applied in sequence.
    const finalDur = 1_000 * Math.pow(1 / 1.3, 6);
    const vp = store.getState().viewport;
    expect(vp.viewEnd - vp.viewStart).toBeCloseTo(finalDur, 3);
  });
});

// Type-only guard: ViewWindow is the shared shape zoom/pan/region return.
const _sample: ViewWindow = { viewStart: 0, viewEnd: 1 };
void _sample;

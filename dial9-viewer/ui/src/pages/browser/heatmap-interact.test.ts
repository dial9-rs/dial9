import { afterEach, describe, expect, it, vi } from "vitest";
import type { BrowserActions } from "./actions.js";
import type { BrowserEls } from "./dom.js";
import { mountHeatmapInteraction } from "./heatmap-interact.js";
import { createBrowserStore, type HeatmapSelection } from "./state.js";

type Listener = (event: Record<string, unknown>) => void;

class FakeEventTarget {
  private readonly listeners = new Map<string, Listener[]>();

  addEventListener(type: string, listener: Listener): void {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  dispatch(type: string, event: Record<string, unknown> = {}): void {
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }
}

class FakeElement extends FakeEventTarget {
  private readonly ancestors: readonly string[];

  constructor(ancestors: readonly string[] = []) {
    super();
    this.ancestors = ancestors;
  }

  closest(selector: string): FakeElement | null {
    return this.ancestors.includes(selector) ? this : null;
  }

  getBoundingClientRect(): DOMRect {
    return { left: 0, top: 0, width: 100 } as DOMRect;
  }
}

const SELECTION: HeatmapSelection = {
  keys: ["trace.bin"],
  bytes: 1,
  t0: 10,
  t1: 50,
  rows: [0, 0],
};

/**
 * Mount the interaction against fake window/document/plot targets. The
 * returned handles dispatch raw events; `setHeatmapSelection` is a spy so a
 * test can tell "never asked to clear" from "cleared and re-set".
 */
function setup() {
  const win = new FakeEventTarget();
  const doc = new FakeEventTarget();
  vi.stubGlobal("Element", FakeElement);
  vi.stubGlobal("window", win);
  vi.stubGlobal("document", doc);

  const plot = new FakeElement(["#heatmap-view"]);
  const canvas = new FakeElement() as FakeElement & { clientWidth: number };
  canvas.clientWidth = 100;
  const els = {
    heatmapPlot: plot,
    heatmapCanvas: canvas,
    heatmapResetZoom: new FakeElement(),
  } as unknown as BrowserEls;

  const store = createBrowserStore();
  store.update("browse", {
    rows: [
      {
        service: "service",
        host: "host",
        label: "service / host",
        segments: [],
        totalBytes: 0,
        tiled: [],
        gaps: [],
      },
    ],
    selection: null,
  });

  const setHeatmapSelection = vi.fn((next: HeatmapSelection | null) => {
    store.update("browse", { selection: next });
  });
  const actions = {
    finalizeSelection: vi.fn(() => {
      store.update("browse", { selection: SELECTION });
    }),
    selectSegmentAt: vi.fn(),
    zoomToX: vi.fn(),
    resetHeatmapZoom: vi.fn(),
    setHeatmapSelection,
  } as unknown as BrowserActions;

  mountHeatmapInteraction({ store, els, actions });
  return { win, doc, plot, store, actions, setHeatmapSelection };
}

/** Drag across the plot and release, committing a region selection. */
function dragSelect(plot: FakeElement, win: FakeEventTarget): void {
  plot.dispatch("mousedown", {
    clientX: 10,
    clientY: 5,
    altKey: false,
    preventDefault: vi.fn(),
  });
  win.dispatch("mouseup", { clientX: 50, clientY: 40 });
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("heatmap pointer interaction", () => {
  it("keeps a selection when its drag ends outside the pane", () => {
    const { win, doc, plot, store, setHeatmapSelection } = setup();

    dragSelect(plot, win);
    doc.dispatch("click", { target: new FakeElement() });

    expect(store.getState().browse.selection).toBe(SELECTION);
    expect(setHeatmapSelection).not.toHaveBeenCalled();

    // The suppression is one-shot: the next genuine click-away still clears.
    doc.dispatch("click", { target: new FakeElement() });
    expect(store.getState().browse.selection).toBeNull();
  });

  // Regression (#645): the TZ toggle and the AWS-credentials button live in
  // the page <header>, which is neither the timeline nor the actions bar. The
  // clear-on-click-away handler used to fire for them, wiping the selection -
  // which defeated the in-place TZ redraw. Header chrome is a control
  // surface, so a click there must preserve the selection.
  it("keeps the selection when a header control (TZ toggle) is clicked", () => {
    const { win, doc, plot, store, setHeatmapSelection } = setup();

    dragSelect(plot, win);
    // Consume the synthetic click that trails the drag, so the header click
    // below is judged on its own target rather than by drag suppression.
    doc.dispatch("click", { target: new FakeElement() });
    expect(store.getState().browse.selection).toBe(SELECTION);

    doc.dispatch("click", { target: new FakeElement(["header"]) });

    expect(store.getState().browse.selection).toBe(SELECTION);
    expect(setHeatmapSelection).not.toHaveBeenCalled();
  });

  // The actions bar is the other control surface: profiling a selection must
  // not destroy the selection being profiled.
  it("keeps the selection when the actions bar is clicked", () => {
    const { win, doc, plot, store, setHeatmapSelection } = setup();

    dragSelect(plot, win);
    doc.dispatch("click", { target: new FakeElement() });

    doc.dispatch("click", { target: new FakeElement(["#actions-bar"]) });

    expect(store.getState().browse.selection).toBe(SELECTION);
    expect(setHeatmapSelection).not.toHaveBeenCalled();
  });
});

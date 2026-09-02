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
  constructor(private readonly ancestors: readonly string[] = []) {
    super();
  }

  closest(selector: string): FakeElement | null {
    return this.ancestors.includes(selector) ? this : null;
  }

  getBoundingClientRect(): DOMRect {
    return { left: 0, top: 0, width: 100 } as DOMRect;
  }
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("heatmap pointer interaction", () => {
  it("keeps a selection when its drag ends outside the pane", () => {
    const win = new FakeEventTarget();
    const doc = new FakeEventTarget();
    vi.stubGlobal("Element", FakeElement);
    vi.stubGlobal("window", win);
    vi.stubGlobal("document", doc);

    const plot = new FakeElement(["#heatmap-view"]);
    const canvas = new FakeElement() as FakeElement & { clientWidth: number };
    canvas.clientWidth = 100;
    const reset = new FakeElement();
    const els = {
      heatmapPlot: plot,
      heatmapCanvas: canvas,
      heatmapResetZoom: reset,
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
    const selection: HeatmapSelection = {
      keys: ["trace.bin"],
      bytes: 1,
      t0: 10,
      t1: 50,
      rows: [0, 0],
    };
    const setHeatmapSelection = vi.fn((next: HeatmapSelection | null) => {
      store.update("browse", { selection: next });
    });
    const actions = {
      finalizeSelection: vi.fn(() => {
        store.update("browse", { selection });
      }),
      selectSegmentAt: vi.fn(),
      zoomToX: vi.fn(),
      resetHeatmapZoom: vi.fn(),
      setHeatmapSelection,
    } as unknown as BrowserActions;

    mountHeatmapInteraction({ store, els, actions });

    plot.dispatch("mousedown", {
      clientX: 10,
      clientY: 5,
      altKey: false,
      preventDefault: vi.fn(),
    });
    win.dispatch("mouseup", { clientX: 50, clientY: 40 });
    doc.dispatch("click", { target: new FakeElement() });

    expect(store.getState().browse.selection).toBe(selection);
    expect(setHeatmapSelection).not.toHaveBeenCalled();

    // The suppression is one-shot: the next genuine click-away still clears.
    doc.dispatch("click", { target: new FakeElement() });
    expect(store.getState().browse.selection).toBeNull();
  });
});

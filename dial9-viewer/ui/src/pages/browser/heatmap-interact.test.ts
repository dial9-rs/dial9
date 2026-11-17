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
  /** Only the properties the components write; enough to assert placement. */
  readonly style: Record<string, string> = {};
  textContent = "";
  /** Measured label width, stubbed: there is no layout in this harness. */
  offsetWidth = 120;
  rectTop = 0;

  constructor(ancestors: readonly string[] = []) {
    super();
    this.ancestors = ancestors;
  }

  closest(selector: string): FakeElement | null {
    return this.ancestors.includes(selector) ? this : null;
  }

  getBoundingClientRect(): DOMRect {
    return { left: 0, top: this.rectTop, width: 100 } as DOMRect;
  }
}

/** 2026-01-15 10:00:00Z + 10 minutes, in epoch seconds. */
const DOMAIN = {
  tMin: Date.UTC(2026, 0, 15, 10, 0, 0) / 1000,
  tMax: Date.UTC(2026, 0, 15, 10, 10, 0) / 1000,
};

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
  const cursor = new FakeElement();
  const cursorLabel = new FakeElement();
  const body = Object.assign(new FakeElement(["#heatmap-view"]), { scrollTop: 0 });
  const labels = new FakeElement(["#heatmap-view"]);
  const els = {
    heatmapPlot: plot,
    heatmapCanvas: canvas,
    heatmapCursor: cursor,
    heatmapCursorLabel: cursorLabel,
    heatmapResetZoom: new FakeElement(),
    heatmapBody: body,
    heatmapLabels: labels,
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
    domain: DOMAIN,
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
  return {
    win, doc, plot, body, labels, cursor, cursorLabel, store, actions, setHeatmapSelection,
  };
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
  it("selects the original host row after scrolling the sticky canvas", () => {
    const { win, plot, body, store, actions } = setup();
    const row = store.getState().browse.rows[0]!;
    store.update("browse", { rows: Array.from({ length: 200 }, () => row) });
    body.scrollTop = 100 * 26;
    plot.rectTop = -body.scrollTop;
    plot.dispatch("mousedown", {
      clientX: 50, clientY: 5, altKey: false, preventDefault: vi.fn(),
    });
    win.dispatch("mouseup", { clientX: 50, clientY: 5 });
    expect(actions.selectSegmentAt).toHaveBeenCalledWith(50, 2605);
  });

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
  // Regression (#1126): the timeline body fills the pane, so the blank area
  // below the last host row is inside #heatmap-view. Clicking that blank
  // area is a click-away and must clear.
  it.each(["body", "labels"] as const)(
    "clears the selection when the blank %s area below the rows is clicked",
    (which) => {
      const handles = setup();
      const { win, doc, plot, store } = handles;
      dragSelect(plot, win);
      doc.dispatch("click", { target: new FakeElement() });
      expect(store.getState().browse.selection).toBe(SELECTION);

      doc.dispatch("mousedown", { target: handles[which] });
      doc.dispatch("click", { target: handles[which] });

      expect(store.getState().browse.selection).toBeNull();
    },
  );

  it("clears the selection on a double-click in the blank body area", () => {
    const { win, doc, plot, body, store } = setup();
    dragSelect(plot, win);
    doc.dispatch("click", { target: new FakeElement() });

    for (let i = 0; i < 2; i++) {
      doc.dispatch("mousedown", { target: body });
      doc.dispatch("click", { target: body });
    }

    expect(store.getState().browse.selection).toBeNull();
  });

  // A press released over a different child of the timeline dispatches its
  // click on the shared ancestor (the body or the label column). Such a click
  // did not start on the blank area, so it must keep the selection.
  it.each([
    ["the plot", "body"],
    ["the label resizer", "body"],
    ["a host label", "labels"],
  ] as const)("keeps the selection when a press on %s is released over the %s", (_, which) => {
    const handles = setup();
    const { win, doc, plot, store, setHeatmapSelection } = handles;
    dragSelect(plot, win);
    doc.dispatch("click", { target: new FakeElement() });

    doc.dispatch("mousedown", { target: new FakeElement(["#heatmap-view"]) });
    doc.dispatch("click", { target: handles[which] });

    expect(store.getState().browse.selection).toBe(SELECTION);
    expect(setHeatmapSelection).not.toHaveBeenCalled();
  });

  // The full plot path: a click-select whose release slips onto the blank
  // body dispatches its click on the body; the new selection must survive.
  it("keeps a plot click-selection whose release lands on the blank body", () => {
    const { win, doc, plot, body, store, actions, setHeatmapSelection } = setup();
    vi.mocked(actions.selectSegmentAt).mockImplementation(() => {
      store.update("browse", { selection: SELECTION });
    });
    const canvasTarget = new FakeElement(["#heatmap-view"]);
    doc.dispatch("mousedown", { target: canvasTarget });
    plot.dispatch("mousedown", {
      clientX: 50, clientY: 24, altKey: false, preventDefault: vi.fn(),
    });
    win.dispatch("mouseup", { clientX: 50, clientY: 27 });
    doc.dispatch("click", { target: body });

    expect(actions.selectSegmentAt).toHaveBeenCalled();
    expect(store.getState().browse.selection).toBe(SELECTION);
    expect(setHeatmapSelection).not.toHaveBeenCalled();
  });

  it("keeps the selection when the host filter controls are clicked", () => {
    const { win, doc, plot, store } = setup();
    dragSelect(plot, win);
    doc.dispatch("click", { target: new FakeElement() });

    doc.dispatch("click", { target: new FakeElement(["#heatmap-view"]) });

    expect(store.getState().browse.selection).toBe(SELECTION);
  });
});

// The hover readout (#631): pointing at the timeline answers "what time is
// this?" without dragging out a selection to read its bounds.
describe("heatmap hover time readout", () => {
  it("keeps the time label in the viewport and hides it when scrolling", () => {
    const { plot, body, cursor, cursorLabel } = setup();
    body.scrollTop = 2600;
    plot.dispatch("mousemove", { clientX: 50, clientY: 5 });
    expect(cursorLabel.style["top"]).toBe("2602px");
    body.dispatch("scroll");
    expect(cursor.style["display"]).toBe("none");
    expect(cursorLabel.style["display"]).toBe("none");
  });

  it("follows the pointer with the timestamp beneath it", () => {
    const { plot, cursor, cursorLabel } = setup();

    // The canvas is 100px wide over a 10-minute window, so the midpoint is
    // 5 minutes in.
    plot.dispatch("mousemove", { clientX: 50, clientY: 5 });

    expect(cursorLabel.textContent).toBe("2026/01/15 10:05:00 UTC");
    expect(cursor.style["display"]).toBe("block");
    expect(cursorLabel.style["display"]).toBe("block");
    expect(cursor.style["left"]).toBe("50px");
  });

  it("clamps the label inside the plot at the edges", () => {
    const { plot, cursorLabel } = setup();

    // A 120px-wide label is wider than this 100px plot, so it centers.
    plot.dispatch("mousemove", { clientX: 0, clientY: 5 });
    expect(cursorLabel.style["left"]).toBe("50px");

    cursorLabel.offsetWidth = 40;
    plot.dispatch("mousemove", { clientX: 0, clientY: 5 });
    expect(cursorLabel.style["left"]).toBe("20px");
    plot.dispatch("mousemove", { clientX: 100, clientY: 5 });
    expect(cursorLabel.style["left"]).toBe("80px");
  });

  it("hides on mouseleave", () => {
    const { plot, cursor, cursorLabel } = setup();

    plot.dispatch("mousemove", { clientX: 50, clientY: 5 });
    plot.dispatch("mouseleave", {});

    expect(cursor.style["display"]).toBe("none");
    expect(cursorLabel.style["display"]).toBe("none");
  });

  it("measures a hidden label after showing it on the first hover", () => {
    const { plot, cursorLabel } = setup();
    cursorLabel.style["display"] = "none";
    Object.defineProperty(cursorLabel, "offsetWidth", {
      get: () => cursorLabel.style["display"] === "none" ? 0 : 40,
    });

    plot.dispatch("mousemove", { clientX: 100, clientY: 5 });
    expect(cursorLabel.style["left"]).toBe("80px");
  });

  it("hides an existing readout as soon as a drag starts", () => {
    const { plot, cursor, cursorLabel } = setup();
    plot.dispatch("mousemove", { clientX: 50, clientY: 5 });
    plot.dispatch("mousedown", {
      clientX: 50, clientY: 5, altKey: false, preventDefault: vi.fn(),
    });
    expect(cursor.style["display"]).toBe("none");
    expect(cursorLabel.style["display"]).toBe("none");
  });

  // Mid-drag the rubber band already shows the span; a second floating time
  // label on top of it is just clutter.
  it("stays hidden while dragging", () => {
    const { plot, cursor } = setup();

    plot.dispatch("mousedown", {
      clientX: 10,
      clientY: 5,
      altKey: false,
      preventDefault: vi.fn(),
    });
    plot.dispatch("mousemove", { clientX: 50, clientY: 40 });

    expect(cursor.style["display"]).toBe("none");
  });

  it("stays hidden when there is no data to point at", () => {
    const { plot, cursor, store } = setup();
    store.update("browse", { rows: [], domain: null });

    plot.dispatch("mousemove", { clientX: 50, clientY: 5 });

    expect(cursor.style["display"]).toBe("none");
  });
});

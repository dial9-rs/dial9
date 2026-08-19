// Selection overlay: the pure "which box to show" precedence and the box
// placement math (blue region / teal zoom). The DOM mount is thin; the logic
// under test is these two pure functions.

import { describe, it, expect } from "vitest";
import { activeSelectionRegion, selectionBox } from "./selection-overlay.js";
import { timePanelLayout, LABEL_W } from "../../lib/canvas/layout.js";
import type { SelectionSlice, TransientSlice } from "../../types/state.js";

function transient(over: Partial<TransientSlice> = {}): TransientSlice {
  return {
    mouseNs: null,
    hoverEventTs: null,
    drag: null,
    keyboardSelection: null,
    atCursor: null,
    ...over,
  };
}

function selection(over: Partial<SelectionSlice> = {}): SelectionSlice {
  return {
    selectedTaskId: null,
    spanFocus: null,
    focusedSpanId: null,
    pollDetail: null,
    pinnedEvent: null,
    taskDump: null,
    sidebarRange: null,
    hoveredWakerTaskId: null,
    spawnedTasksRange: null,
    ...over,
  };
}

/** The trace extent the retained-box rule compares against. */
const EXTENT = { minTs: 0, maxTs: 10_000 };

describe("activeSelectionRegion - precedence", () => {
  it("a live keyboard selection wins over everything", () => {
    const region = activeSelectionRegion(
      transient({ keyboardSelection: { kind: "zoom-select", startNs: 800, cursorNs: 200 } }),
      selection({ sidebarRange: { startNs: 0, endNs: 9_999 } }),
      EXTENT,
    );
    expect(region).toEqual({ startNs: 200, endNs: 800, mode: "zoom" });
  });

  it("a moved region/zoom drag draws the live box", () => {
    const region = activeSelectionRegion(
      transient({ drag: { kind: "region-select", startX: 0, startNs: 400, curNs: 100, moved: true } }),
      selection(),
      EXTENT,
    );
    expect(region).toEqual({ startNs: 100, endNs: 400, mode: "region" });
  });

  it("a PAN drag draws no box (moves the viewport, not a selection)", () => {
    const region = activeSelectionRegion(
      transient({ drag: { kind: "pan", startX: 0, startNs: 400, curNs: 100, moved: true } }),
      selection(),
      EXTENT,
    );
    expect(region).toBeNull();
  });

  it("an UNMOVED select drag draws no live box (falls through)", () => {
    const region = activeSelectionRegion(
      transient({ drag: { kind: "region-select", startX: 0, startNs: 400, curNs: 400, moved: false } }),
      selection(),
      EXTENT,
    );
    expect(region).toBeNull();
  });

  it("a retained sidebar range persists as a blue box (no live gesture)", () => {
    const region = activeSelectionRegion(
      transient(),
      selection({ sidebarRange: { startNs: 1_000, endNs: 2_000 } }),
      EXTENT,
    );
    expect(region).toEqual({ startNs: 1_000, endNs: 2_000, mode: "region" });
  });

  it("a retained range covering the whole trace draws NO box (issue #796)", () => {
    // The toolbar Flamegraph / Blocking Calls / Heap buttons open a whole-trace
    // analysis by retaining [minTs, maxTs]; boxing everything just tints the
    // page blue without distinguishing any scope.
    const region = activeSelectionRegion(
      transient(),
      selection({ sidebarRange: { startNs: EXTENT.minTs, endNs: EXTENT.maxTs } }),
      EXTENT,
    );
    expect(region).toBeNull();
  });

  it("a retained range one ns short of the extent still draws its box", () => {
    const region = activeSelectionRegion(
      transient(),
      selection({ sidebarRange: { startNs: EXTENT.minTs + 1, endNs: EXTENT.maxTs } }),
      EXTENT,
    );
    expect(region).toEqual({
      startNs: EXTENT.minTs + 1,
      endNs: EXTENT.maxTs,
      mode: "region",
    });
  });

  it("nothing selected => null (box hidden)", () => {
    expect(activeSelectionRegion(transient(), selection(), EXTENT)).toBeNull();
  });
});

describe("selectionBox - placement (shared ns<->x mapping)", () => {
  const layout = timePanelLayout({ pw: 1_000, scrollbarW: 0, viewStart: 0, viewEnd: 1_000 });

  it("maps [start,end] through the same layout the lanes use", () => {
    // drawW = 1000 - LABEL_W; x(ns) = LABEL_W + ns/1000 * drawW.
    const box = selectionBox({ startNs: 250, endNs: 750, mode: "region" }, layout);
    const drawW = 1_000 - LABEL_W;
    expect(box.left).toBeCloseTo(LABEL_W + 0.25 * drawW, 6);
    expect(box.width).toBeCloseTo(0.5 * drawW, 6);
  });

  it("clamps edges outside the view into the draw area", () => {
    // A keyboard cursor panned past the right edge still renders in-bounds.
    const box = selectionBox({ startNs: 500, endNs: 5_000, mode: "zoom" }, layout);
    expect(box.left + box.width).toBeCloseTo(1_000, 6); // right edge = LABEL_W + drawW = pw
  });

  it("guarantees at least 1px width for a degenerate region", () => {
    expect(selectionBox({ startNs: 500, endNs: 500, mode: "region" }, layout).width).toBe(1);
  });
});

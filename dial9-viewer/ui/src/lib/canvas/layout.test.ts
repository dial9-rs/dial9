// DoD (T08): layout invariant - label gutter + drawW + scrollbar math -
// against known cases. The frozen core's own math is covered by
// tests/core/panel_layout.test.ts; this suite tests the src/ wrapper:
// that it pins labelW to the canonical LABEL_W=100 and forwards the
// geometry unchanged, so every panel built through it lines up.

import { describe, it, expect } from "vitest";
import {
  LABEL_W,
  timePanelLayout,
  panelGeometry,
  laneStackGeometry,
} from "./layout.js";

describe("timePanelLayout", () => {
  it("known case: pw=1200, sb=17 -> drawW=1083, axis spans [100, 1183]", () => {
    const l = timePanelLayout({
      pw: 1200,
      scrollbarW: 17,
      viewStart: 1_000_000,
      viewEnd: 5_000_000,
    });
    expect(l.pw).toBe(1200);
    expect(l.labelW).toBe(LABEL_W);
    expect(l.drawW).toBe(1200 - LABEL_W - 17); // 1083
    // viewStart lands exactly at the gutter edge...
    expect(l.nsToPanelX(1_000_000)).toBe(LABEL_W);
    // ...and viewEnd at pw - scrollbarW (the lane right edge).
    expect(l.nsToPanelX(5_000_000)).toBe(1200 - 17);
  });

  it("known case: no scrollbar -> drawW = pw - LABEL_W", () => {
    const l = timePanelLayout({ pw: 1000, viewStart: 0, viewEnd: 1000 });
    expect(l.drawW).toBe(900);
    expect(l.nsToPanelX(0)).toBe(LABEL_W);
    expect(l.nsToPanelX(1000)).toBe(1000);
    // Midpoint is linear.
    expect(l.nsToPanelX(500)).toBe(LABEL_W + 450);
  });

  it("INVARIANT: the gutter is always LABEL_W - never caller-defined", () => {
    // The wrapper takes no labelW parameter at all; a panel CANNOT rebuild
    // the historical 200px-gutter bug through this API. Every layout built
    // here agrees on where viewStart lands.
    const panels = [
      timePanelLayout({ pw: 1200, scrollbarW: 17, viewStart: 5, viewEnd: 10 }),
      timePanelLayout({ pw: 1200, scrollbarW: 17, viewStart: 5, viewEnd: 10 }),
      timePanelLayout({ pw: 1200, scrollbarW: 17, viewStart: 5, viewEnd: 10 }),
    ];
    for (const p of panels) {
      expect(p.labelW).toBe(LABEL_W);
      expect(p.nsToPanelX(5)).toBe(LABEL_W);
    }
  });

  it("scrollbarW shrinks drawW but does not move the gutter edge", () => {
    const noSb = timePanelLayout({ pw: 1000, viewStart: 0, viewEnd: 1 });
    const withSb = timePanelLayout({
      pw: 1000,
      scrollbarW: 17,
      viewStart: 0,
      viewEnd: 1,
    });
    expect(withSb.drawW).toBe(noSb.drawW - 17);
    expect(noSb.nsToPanelX(0)).toBe(LABEL_W);
    expect(withSb.nsToPanelX(0)).toBe(LABEL_W);
  });

  it("round-trips x <-> ns like the legacy wrapper", () => {
    const l = timePanelLayout({
      pw: 1234,
      scrollbarW: 17,
      viewStart: 1_000_000,
      viewEnd: 2_000_000,
    });
    for (const ns of [1_000_000, 1_250_000, 1_999_999]) {
      expect(Math.abs(l.panelXToNs(l.nsToPanelX(ns)) - ns)).toBeLessThan(1e-6);
    }
  });

  it("narrow panel: drawW <= 0 is reported, not masked", () => {
    // Caller contract: early-return on drawW <= 0. The wrapper must
    // surface the value rather than clamping it to something plausible.
    const l = timePanelLayout({
      pw: 80,
      scrollbarW: 17,
      viewStart: 0,
      viewEnd: 1000,
    });
    expect(l.drawW).toBe(80 - LABEL_W - 17);
    expect(l.drawW).toBeLessThan(0);
  });
});

describe("panelGeometry", () => {
  it("wraps the time mapping with the panel box", () => {
    const g = panelGeometry({
      kind: "queue",
      pw: 1200,
      scrollbarW: 17,
      viewStart: 0,
      viewEnd: 1000,
      height: 80,
      dpr: 2,
    });
    expect(g.kind).toBe("queue");
    expect(g.height).toBe(80);
    expect(g.dpr).toBe(2);
    expect(g.time.drawW).toBe(1083);
    expect(g.time.nsToPanelX(0)).toBe(LABEL_W);
  });
});

describe("laneStackGeometry", () => {
  it("stacks lanes in workerIds order at y = index * laneHeight", () => {
    const lanes = laneStackGeometry([3, 0, 7], 60);
    expect(lanes).toEqual([
      { workerId: 3, index: 0, y: 0, height: 60 },
      { workerId: 0, index: 1, y: 60, height: 60 },
      { workerId: 7, index: 2, y: 120, height: 60 },
    ]);
  });

  it("empty worker set -> empty stack", () => {
    expect(laneStackGeometry([], 60)).toEqual([]);
  });
});

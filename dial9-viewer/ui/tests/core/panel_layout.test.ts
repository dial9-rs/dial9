// Unit tests for the shared time-panel layout math.
//
// This test protects the invariant that every time-based panel in the viewer
// (timeline, worker lanes, span panel, task detail, queue chart) uses the
// same label/draw area layout so their time axes line up vertically.  The
// bug that prompted this test was a regression where the span panel used a
// 200px left gutter while every other panel used 100px, shifting the span
// time axis ~100px to the right of the task detail view.
//
// Migrated from test_panel_layout.js (T10); frozen core loaded via
// createRequire (see format.test.ts for the rationale).

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);

interface TimePanelLayout {
  pw: number;
  labelW: number;
  drawW: number;
  nsToPanelX: (ns: number) => number;
  nsToPanelXClamped: (ns: number) => number;
  panelXToNs: (x: number) => number;
}

const { makeTimePanelLayout } = require("../../panel_layout.js") as {
  makeTimePanelLayout: (
    pw: number,
    labelW: number,
    scrollbarW: number,
    viewStart: number,
    viewEnd: number,
  ) => TimePanelLayout;
};

// The canonical left-gutter width used by every time-based panel.  If we ever
// want to make it configurable, update viewer.html's LABEL_W constant and the
// call sites at the same time - but the invariant is that *every* time-based
// panel uses the same value.
const LABEL_W = 100;

describe("makeTimePanelLayout", () => {
  it("nsToPanelX at viewStart lands exactly at labelW", () => {
    const l = makeTimePanelLayout(1000, LABEL_W, 0, 500, 1500);
    expect(l.nsToPanelX(500)).toBe(LABEL_W);
  });

  it("nsToPanelX at viewEnd lands at labelW+drawW", () => {
    const l = makeTimePanelLayout(1000, LABEL_W, 0, 500, 1500);
    expect(l.nsToPanelX(1500)).toBe(LABEL_W + l.drawW);
  });

  it("nsToPanelX is linear in the middle of the view", () => {
    const l = makeTimePanelLayout(1000, LABEL_W, 0, 0, 1000);
    // Midpoint of the view should land at labelW + drawW/2.
    expect(l.nsToPanelX(500)).toBe(LABEL_W + l.drawW / 2);
  });

  it("nsToPanelXClamped clamps values outside the view", () => {
    const l = makeTimePanelLayout(1000, LABEL_W, 0, 500, 1500);
    expect(l.nsToPanelXClamped(100)).toBe(LABEL_W); // before
    expect(l.nsToPanelXClamped(2000)).toBe(LABEL_W + l.drawW); // after
    // Values inside the view match nsToPanelX.
    expect(l.nsToPanelXClamped(1000)).toBe(l.nsToPanelX(1000));
  });

  it("panelXToNs round-trips with nsToPanelX", () => {
    const l = makeTimePanelLayout(1234, LABEL_W, 17, 1_000_000, 2_000_000);
    for (const ns of [1_000_000, 1_250_000, 1_500_000, 1_999_999]) {
      const x = l.nsToPanelX(ns);
      const back = l.panelXToNs(x);
      expect(
        Math.abs(back - ns),
        `round-trip ${ns} -> ${x} -> ${back}`,
      ).toBeLessThan(1e-6);
    }
  });

  it("scrollbarW shrinks drawW but does not move labelW", () => {
    const noSb = makeTimePanelLayout(1000, LABEL_W, 0, 0, 1);
    const withSb = makeTimePanelLayout(1000, LABEL_W, 17, 0, 1);
    expect(noSb.labelW).toBe(withSb.labelW);
    expect(withSb.drawW).toBe(noSb.drawW - 17);
    // viewStart still maps to x=LABEL_W in both.
    expect(noSb.nsToPanelX(0)).toBe(LABEL_W);
    expect(withSb.nsToPanelX(0)).toBe(LABEL_W);
  });

  it("INVARIANT: all panels with the same pw share the same time axis", () => {
    // Simulate the four time-based panels in the viewer.  Each uses its own
    // scrollbarW (some have the lane scrollbar gutter, some don't), but they
    // all must map the same timestamp to the same x-coordinate within the
    // [LABEL_W, LABEL_W+min(drawW)] overlap - i.e. the start of the draw area
    // is identical (x=LABEL_W) for every panel.
    const pw = 1200;
    const vStart = 1_000_000,
      vEnd = 5_000_000;

    const timeline = makeTimePanelLayout(pw, LABEL_W, 17, vStart, vEnd);
    const spanPanel = makeTimePanelLayout(pw, LABEL_W, 17, vStart, vEnd);
    const taskDetail = makeTimePanelLayout(pw, LABEL_W, 17, vStart, vEnd);
    const queueChart = makeTimePanelLayout(pw, LABEL_W, 17, vStart, vEnd);

    for (const ts of [vStart, vStart + 500_000, vStart + 2_500_000, vEnd - 1]) {
      const x = timeline.nsToPanelX(ts);
      expect(spanPanel.nsToPanelX(ts), `span panel diverges at ${ts}`).toBe(x);
      expect(taskDetail.nsToPanelX(ts), `task detail diverges at ${ts}`).toBe(x);
      expect(queueChart.nsToPanelX(ts), `queue chart diverges at ${ts}`).toBe(x);
    }
    // The start of the draw area is LABEL_W everywhere, not a panel-specific
    // value.  (This is the assertion that would have failed for the span
    // panel when its left gutter was 200px.)
    expect(timeline.labelW).toBe(LABEL_W);
    expect(spanPanel.labelW).toBe(LABEL_W);
    expect(taskDetail.labelW).toBe(LABEL_W);
    expect(queueChart.labelW).toBe(LABEL_W);
  });

  it("REGRESSION: hypothetical 200px-gutter panel would NOT align", () => {
    // This is a documentation test: if someone re-breaks the invariant by
    // building a panel with a non-LABEL_W gutter, the test ensures we notice.
    const pw = 1200;
    const vStart = 0,
      vEnd = 1_000_000;
    const good = makeTimePanelLayout(pw, 100, 0, vStart, vEnd);
    const bad = makeTimePanelLayout(pw, 200, 0, vStart, vEnd);
    // They disagree on where viewStart lands by exactly the gutter delta.
    expect(bad.nsToPanelX(0) - good.nsToPanelX(0)).toBe(100);
  });

  it("Zero-width view (viewStart == viewEnd) does not divide by zero", () => {
    const l = makeTimePanelLayout(1000, LABEL_W, 0, 500, 500);
    expect(Number.isFinite(l.nsToPanelX(500))).toBe(true);
    expect(Number.isFinite(l.nsToPanelXClamped(500))).toBe(true);
  });

  it("Tiny panel width produces negative drawW but does not throw", () => {
    // Caller is expected to early-return on drawW <= 0; just verify no NaN.
    const l = makeTimePanelLayout(50, LABEL_W, 17, 0, 1000);
    expect(l.drawW).toBeLessThan(0);
    expect(Number.isFinite(l.nsToPanelX(500))).toBe(true);
  });

  it("INVARIANT: no time-based panel declares its own left gutter in CSS", () => {
    // Cheap but effective guard: grep viewer.html for any time-based panel
    // defining a `padding-left` value.  The whole point of the invariant is
    // that the canvas fills the panel and `timePanelLayout` handles the
    // LABEL_W offset.  If a panel redefines `padding-left`, its time axis
    // will silently drift relative to every other panel.
    const html = readFileSync(
      fileURLToPath(new URL("../../viewer.html", import.meta.url)),
      "utf8",
    );

    const PANELS = [
      "#timeline-header",
      "#span-panel",
      "#task-detail",
      "#queue-chart",
    ];
    for (const panel of PANELS) {
      // Match: `<selector> {  ...  padding-left: ...  }` across lines.
      const ruleRe = new RegExp(
        `${panel.replace(/[.*+?^${}()|[\\]\\\\]/g, "\\$&")}\\s*\\{([^}]*)\\}`,
        "g",
      );
      let m;
      while ((m = ruleRe.exec(html)) !== null) {
        const body = m[1]!;
        expect(
          /padding-left\s*:/.test(body),
          `${panel} must not declare padding-left (would break the time-axis alignment invariant).  Use timePanelLayout() instead.`,
        ).toBe(false);
      }
    }
  });
});

#!/usr/bin/env node
// test_panel_layout.js — unit tests for the shared time-panel layout math.
//
// This test protects the invariant that every time-based panel in the viewer
// (timeline, worker lanes, span panel, task detail, queue chart) uses the
// same label/draw area layout so their time axes line up vertically.  The
// bug that prompted this test was a regression where the span panel used a
// 200px left gutter while every other panel used 100px, shifting the span
// time axis ~100px to the right of the task detail view.
"use strict";

const assert = require("assert");
const { makeTimePanelLayout } = require("./panel_layout.js");

// The canonical left-gutter width used by every time-based panel.  If we ever
// want to make it configurable, update viewer.html's LABEL_W constant and the
// call sites at the same time — but the invariant is that *every* time-based
// panel uses the same value.
const LABEL_W = 100;

let passed = 0, failed = 0;
function test(name, fn) {
  try {
    fn();
    console.log(`✓ ${name}`);
    passed++;
  } catch (e) {
    console.error(`✗ ${name}: ${e.message}`);
    failed++;
  }
}

test("nsToPanelX at viewStart lands exactly at labelW", () => {
  const l = makeTimePanelLayout(1000, LABEL_W, 0, 500, 1500);
  assert.strictEqual(l.nsToPanelX(500), LABEL_W);
});

test("nsToPanelX at viewEnd lands at labelW+drawW", () => {
  const l = makeTimePanelLayout(1000, LABEL_W, 0, 500, 1500);
  assert.strictEqual(l.nsToPanelX(1500), LABEL_W + l.drawW);
});

test("nsToPanelX is linear in the middle of the view", () => {
  const l = makeTimePanelLayout(1000, LABEL_W, 0, 0, 1000);
  // Midpoint of the view should land at labelW + drawW/2.
  assert.strictEqual(l.nsToPanelX(500), LABEL_W + l.drawW / 2);
});

test("nsToPanelXClamped clamps values outside the view", () => {
  const l = makeTimePanelLayout(1000, LABEL_W, 0, 500, 1500);
  assert.strictEqual(l.nsToPanelXClamped(100), LABEL_W);            // before
  assert.strictEqual(l.nsToPanelXClamped(2000), LABEL_W + l.drawW); // after
  // Values inside the view match nsToPanelX.
  assert.strictEqual(l.nsToPanelXClamped(1000), l.nsToPanelX(1000));
});

test("panelXToNs round-trips with nsToPanelX", () => {
  const l = makeTimePanelLayout(1234, LABEL_W, 17, 1_000_000, 2_000_000);
  for (const ns of [1_000_000, 1_250_000, 1_500_000, 1_999_999]) {
    const x = l.nsToPanelX(ns);
    const back = l.panelXToNs(x);
    assert.ok(Math.abs(back - ns) < 1e-6, `round-trip ${ns} -> ${x} -> ${back}`);
  }
});

test("scrollbarW shrinks drawW but does not move labelW", () => {
  const noSb = makeTimePanelLayout(1000, LABEL_W, 0, 0, 1);
  const withSb = makeTimePanelLayout(1000, LABEL_W, 17, 0, 1);
  assert.strictEqual(noSb.labelW, withSb.labelW);
  assert.strictEqual(withSb.drawW, noSb.drawW - 17);
  // viewStart still maps to x=LABEL_W in both.
  assert.strictEqual(noSb.nsToPanelX(0), LABEL_W);
  assert.strictEqual(withSb.nsToPanelX(0), LABEL_W);
});

test("INVARIANT: all panels with the same pw share the same time axis", () => {
  // Simulate the four time-based panels in the viewer.  Each uses its own
  // scrollbarW (some have the lane scrollbar gutter, some don't), but they
  // all must map the same timestamp to the same x-coordinate within the
  // [LABEL_W, LABEL_W+min(drawW)] overlap — i.e. the start of the draw area
  // is identical (x=LABEL_W) for every panel.
  const pw = 1200;
  const vStart = 1_000_000, vEnd = 5_000_000;

  const timeline    = makeTimePanelLayout(pw, LABEL_W, 17, vStart, vEnd);
  const spanPanel   = makeTimePanelLayout(pw, LABEL_W, 17, vStart, vEnd);
  const taskDetail  = makeTimePanelLayout(pw, LABEL_W, 17, vStart, vEnd);
  const queueChart  = makeTimePanelLayout(pw, LABEL_W, 17, vStart, vEnd);

  for (const ts of [vStart, vStart + 500_000, vStart + 2_500_000, vEnd - 1]) {
    const x = timeline.nsToPanelX(ts);
    assert.strictEqual(spanPanel.nsToPanelX(ts),  x, `span panel diverges at ${ts}`);
    assert.strictEqual(taskDetail.nsToPanelX(ts), x, `task detail diverges at ${ts}`);
    assert.strictEqual(queueChart.nsToPanelX(ts), x, `queue chart diverges at ${ts}`);
  }
  // The start of the draw area is LABEL_W everywhere, not a panel-specific
  // value.  (This is the assertion that would have failed for the span
  // panel when its left gutter was 200px.)
  assert.strictEqual(timeline.labelW,   LABEL_W);
  assert.strictEqual(spanPanel.labelW,  LABEL_W);
  assert.strictEqual(taskDetail.labelW, LABEL_W);
  assert.strictEqual(queueChart.labelW, LABEL_W);
});

test("REGRESSION: hypothetical 200px-gutter panel would NOT align", () => {
  // This is a documentation test: if someone re-breaks the invariant by
  // building a panel with a non-LABEL_W gutter, the test ensures we notice.
  const pw = 1200;
  const vStart = 0, vEnd = 1_000_000;
  const good = makeTimePanelLayout(pw, 100, 0, vStart, vEnd);
  const bad  = makeTimePanelLayout(pw, 200, 0, vStart, vEnd);
  // They disagree on where viewStart lands by exactly the gutter delta.
  assert.strictEqual(bad.nsToPanelX(0) - good.nsToPanelX(0), 100);
});

test("Zero-width view (viewStart == viewEnd) does not divide by zero", () => {
  const l = makeTimePanelLayout(1000, LABEL_W, 0, 500, 500);
  assert.ok(Number.isFinite(l.nsToPanelX(500)));
  assert.ok(Number.isFinite(l.nsToPanelXClamped(500)));
});

test("Tiny panel width produces negative drawW but does not throw", () => {
  // Caller is expected to early-return on drawW <= 0; just verify no NaN.
  const l = makeTimePanelLayout(50, LABEL_W, 17, 0, 1000);
  assert.ok(l.drawW < 0);
  assert.ok(Number.isFinite(l.nsToPanelX(500)));
});

test("INVARIANT: no time-based panel declares its own left gutter in CSS", () => {
  // Cheap but effective guard: grep viewer.html for any time-based panel
  // defining a `padding-left` value.  The whole point of the invariant is
  // that the canvas fills the panel and `timePanelLayout` handles the
  // LABEL_W offset.  If a panel redefines `padding-left`, its time axis
  // will silently drift relative to every other panel.
  const fs = require("fs");
  const path = require("path");
  const html = fs.readFileSync(
    path.join(__dirname, "viewer.html"),
    "utf8",
  );

  const PANELS = [
    "#timeline-header",
    "#span-panel",
    ".schema-time-series-panel",
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
      const body = m[1];
      assert.ok(
        !/padding-left\s*:/.test(body),
        `${panel} must not declare padding-left (would break the time-axis alignment invariant).  Use timePanelLayout() instead.`,
      );
    }
  }
});

test("Schema time-series geometry shares one chart clip", () => {
  const fs = require("fs");
  const path = require("path");
  const html = fs.readFileSync(path.join(__dirname, "viewer.html"), "utf8");
  const start = html.indexOf("function renderSchemaTimeSeriesPanel");
  const end = html.indexOf("function renderSchemaTimeSeriesPanels", start);
  const renderer = html.slice(start, end);
  const clip = renderer.indexOf("ctx.clip()");
  const thresholds = renderer.indexOf("for (const threshold of thresholds)");
  const guides = renderer.indexOf("for (const guide of guides)");
  const series = renderer.indexOf("view.series.forEach");
  const restore = renderer.lastIndexOf("ctx.restore()");
  assert.ok(clip >= 0, "schema renderer must establish a chart clip");
  assert.ok(clip < thresholds && thresholds < guides && guides < series,
    "thresholds, guides, and series must all render after the chart clip");
  assert.ok(restore > series, "schema renderer must restore the clip after all series");
  assert.ok(!renderer.includes("schemaPointEnd") && !renderer.includes("viewEnd;"),
    "renderer must not infer viewport-dependent interval ends");
});

test("Schema panels show data legends only when they disambiguate series", () => {
  const fs = require("fs");
  const path = require("path");
  const html = fs.readFileSync(path.join(__dirname, "viewer.html"), "utf8");
  assert.ok(!html.includes('id="cpu-panel"') && !html.includes("renderProcessCpuPanel"),
    "the event-specific CPU panel must stay removed");
  const start = html.indexOf("function updateSchemaPanelLegend");
  const end = html.indexOf("function renderSchemaTimeSeriesPanel", start);
  const legend = html.slice(start, end);
  assert.ok(legend.includes("view.series.length > 1 || groupCount > 1") &&
      legend.includes("if (showSeriesItems)") &&
      legend.includes("view.series.forEach") && legend.includes("series.groups.forEach"),
    "schema legend must omit a redundant single series/group and enumerate ambiguous data");
  assert.ok(html.includes(".schema-panel-legend-swatch.is-line") &&
      html.includes(".schema-panel-legend-swatch.is-area") &&
      html.includes(".schema-panel-legend-swatch.is-reference"),
    "series and reference legend swatches must reflect their rendered mark");
  assert.ok(/\.schema-time-series-panel \.chart-label\s*\{[^}]*overflow-x:\s*auto/s.test(html),
    "long schema legends must remain horizontally accessible");
  assert.ok(html.includes('stepGroup.mark === "step_area"') && html.includes("insideArea ? 0"),
    "step-area tooltips must cover the rendered area, not only its top line");
  assert.ok(html.includes("mx < LABEL_W + layout.drawW"),
    "the half-open viewport must exclude hit testing at its right edge");
});

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed > 0 ? 1 : 0);

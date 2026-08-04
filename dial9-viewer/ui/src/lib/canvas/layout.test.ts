// Layout invariant - label gutter + drawW + scrollbar math - against known
// cases. Tests the src/ wrapper: it pins labelW to the canonical LABEL_W=100
// and forwards the geometry unchanged, so every panel built through it lines up.

import { describe, it, expect } from "vitest";
import {
  LABEL_W,
  timePanelLayout,
  panelGeometry,
  laneRowLayout,
  workerAtLaneY,
  headerAtLaneY,
} from "./layout.js";
import type { RuntimeGroup } from "../../types/trace.js";

const group = (name: string, workerIds: number[], inferred = false): RuntimeGroup => ({
  name,
  workerIds,
  inferred,
});

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

describe("laneRowLayout", () => {
  it("single runtime group -> no header, fixed-height worker rows", () => {
    const { rows, contentHeight } = laneRowLayout([group("main", [3, 0, 7], true)], 60, 24);
    expect(rows).toEqual([
      { kind: "worker", workerId: 3, index: 0, y: 0, height: 60 },
      { kind: "worker", workerId: 0, index: 1, y: 60, height: 60 },
      { kind: "worker", workerId: 7, index: 2, y: 120, height: 60 },
    ]);
    expect(contentHeight).toBe(180);
  });

  it("multiple groups -> a header before each group, cumulative y", () => {
    const { rows, contentHeight } = laneRowLayout(
      [group("a", [0, 1]), group("b", [2])],
      60,
      24,
    );
    expect(rows).toEqual([
      { kind: "header", name: "a", inferred: false, workerCount: 2, collapsed: false, y: 0, height: 24 },
      { kind: "worker", workerId: 0, index: 0, y: 24, height: 60 },
      { kind: "worker", workerId: 1, index: 1, y: 84, height: 60 },
      { kind: "header", name: "b", inferred: false, workerCount: 1, collapsed: false, y: 144, height: 24 },
      { kind: "worker", workerId: 2, index: 2, y: 168, height: 60 },
    ]);
    // 2 headers (24) + 3 workers (60) = 228.
    expect(contentHeight).toBe(228);
  });

  it("emits a pinned runtime-metrics lane as each metric runtime's first lane", () => {
    const { rows, contentHeight } = laneRowLayout(
      [group("a", [0, 1]), group("b", [2])],
      60,
      24,
      {},
      new Set(["a"]), // only runtime "a" has a metric series
    );
    expect(rows).toEqual([
      { kind: "header", name: "a", inferred: false, workerCount: 2, collapsed: false, y: 0, height: 24 },
      // "a" gets its summary lane above its workers.
      { kind: "runtime-metrics", name: "a", inferred: false, y: 24, height: 60 },
      { kind: "worker", workerId: 0, index: 0, y: 84, height: 60 },
      { kind: "worker", workerId: 1, index: 1, y: 144, height: 60 },
      // "b" has no metric series -> no summary lane.
      { kind: "header", name: "b", inferred: false, workerCount: 1, collapsed: false, y: 204, height: 24 },
      { kind: "worker", workerId: 2, index: 2, y: 228, height: 60 },
    ]);
    // 2 headers (24) + 1 metrics lane (60) + 3 workers (60) = 228 + 60 = 288.
    expect(contentHeight).toBe(288);
  });

  it("folds the runtime-metrics lane away with its collapsed runtime", () => {
    const { rows } = laneRowLayout(
      [group("a", [0, 1]), group("b", [2])],
      60,
      24,
      { a: true },
      new Set(["a", "b"]),
    );
    // "a" is collapsed: neither its metrics lane nor its workers emit. "b" is
    // open, so its metrics lane leads its worker.
    expect(rows).toEqual([
      { kind: "header", name: "a", inferred: false, workerCount: 2, collapsed: true, y: 0, height: 24 },
      { kind: "header", name: "b", inferred: false, workerCount: 1, collapsed: false, y: 24, height: 24 },
      { kind: "runtime-metrics", name: "b", inferred: false, y: 48, height: 60 },
      { kind: "worker", workerId: 2, index: 0, y: 108, height: 60 },
    ]);
  });

  it("a collapsed group keeps its header but drops its worker rows", () => {
    const { rows, contentHeight } = laneRowLayout(
      [group("a", [0, 1]), group("b", [2])],
      60,
      24,
      { a: true },
    );
    expect(rows).toEqual([
      { kind: "header", name: "a", inferred: false, workerCount: 2, collapsed: true, y: 0, height: 24 },
      { kind: "header", name: "b", inferred: false, workerCount: 1, collapsed: false, y: 24, height: 24 },
      // b's worker keeps a dense index (0): folded a's workers never emit.
      { kind: "worker", workerId: 2, index: 0, y: 48, height: 60 },
    ]);
    // header a (24) + header b (24) + 1 worker (60) = 108.
    expect(contentHeight).toBe(108);
  });

  it("ignores the collapse map for a single-runtime (headerless) trace", () => {
    const { rows } = laneRowLayout([group("main", [3, 0], true)], 60, 24, { main: true });
    expect(rows).toEqual([
      { kind: "worker", workerId: 3, index: 0, y: 0, height: 60 },
      { kind: "worker", workerId: 0, index: 1, y: 60, height: 60 },
    ]);
  });

  it("no groups -> empty stack", () => {
    expect(laneRowLayout([], 60, 24)).toEqual({ rows: [], contentHeight: 0 });
  });
});

describe("workerAtLaneY", () => {
  const single = laneRowLayout([group("main", [5, 6, 7], true)], 60, 24);
  const grouped = laneRowLayout([group("a", [0, 1]), group("b", [2])], 60, 24);

  it("resolves the worker whose fixed row contains the lanes-local y", () => {
    expect(workerAtLaneY(single, 0)).toBe(5); // top of row 0
    expect(workerAtLaneY(single, 59)).toBe(5); // bottom of row 0
    expect(workerAtLaneY(single, 60)).toBe(6); // top of row 1
    expect(workerAtLaneY(single, 130)).toBe(7); // inside row 2
  });

  it("accounts for a scroll offset already folded into localY", () => {
    // localY = clientY - viewportTop + scrollTop. Scrolled 100px: the point 10px
    // below the box top lands at lanes-local 110 -> worker 6 (row [60,120)).
    expect(workerAtLaneY(single, 110)).toBe(6);
  });

  it("returns null over a runtime header band and past the content", () => {
    expect(workerAtLaneY(grouped, 10)).toBeNull(); // header "a" band [0,24)
    expect(workerAtLaneY(grouped, 24)).toBe(0); // first worker after header
    expect(workerAtLaneY(grouped, 150)).toBeNull(); // header "b" band [144,168)
    expect(workerAtLaneY(grouped, 168)).toBe(2); // worker under header "b"
    expect(workerAtLaneY(grouped, 10_000)).toBeNull(); // past the content
    expect(workerAtLaneY(single, -1)).toBeNull(); // above the content
  });
});

describe("headerAtLaneY", () => {
  const grouped = laneRowLayout([group("a", [0, 1]), group("b", [2])], 60, 24);
  const folded = laneRowLayout([group("a", [0, 1]), group("b", [2])], 60, 24, { a: true });

  it("resolves the runtime name whose header band contains the y, else null", () => {
    expect(headerAtLaneY(grouped, 10)).toBe("a"); // header "a" band [0,24)
    expect(headerAtLaneY(grouped, 24)).toBeNull(); // first worker row
    expect(headerAtLaneY(grouped, 150)).toBe("b"); // header "b" band [144,168)
    expect(headerAtLaneY(grouped, 10_000)).toBeNull(); // past the content
  });

  it("a folded runtime's header + the next header are adjacent hit targets", () => {
    expect(headerAtLaneY(folded, 10)).toBe("a"); // folded header a [0,24)
    expect(headerAtLaneY(folded, 30)).toBe("b"); // header b now at [24,48)
    expect(workerAtLaneY(folded, 60)).toBe(2); // b's only worker at [48,108)
  });
});

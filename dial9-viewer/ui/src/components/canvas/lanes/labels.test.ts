// Label-gutter tests over a recording 2D-context stub: no DOM, no trace parse.
//
// The gutter's whole job is to name the rows the lanes canvas drew, from the SAME
// row layout, so these assert the naming AND that the gutter culls to the same
// scroll window the lanes canvas culls to. A gutter that scrolled independently
// would label rows that are not on screen — the failure mode a DOM-label
// implementation would have had.

import { describe, it, expect } from "vitest";
import { renderLaneLabels } from "./labels.js";
import { buildLaneIdentities } from "./chrome.js";
import { LANE_ROW_H, RUNTIME_HEADER_H, type LaneDrawContext } from "./render.js";
import { laneRowLayout, METRICS_LANE_COLLAPSED_H } from "../../../lib/canvas/layout.js";
import type { RuntimeGroup } from "../../../types/trace.js";

function recordingCtx(): {
  ctx: LaneDrawContext;
  fillTexts: string[];
  /** Path fills — a caret is the only thing here that would draw one. */
  pathFills: number;
} {
  const fillTexts: string[] = [];
  const counts = { pathFills: 0 };
  const ctx = {
    fillStyle: "#000",
    strokeStyle: "#000",
    lineWidth: 1,
    lineJoin: "miter" as CanvasLineJoin,
    font: "",
    textAlign: "left" as CanvasTextAlign,
    clearRect: () => {},
    fillRect: () => {},
    fillText: (t: string) => {
      fillTexts.push(t);
    },
    beginPath: () => {},
    moveTo: () => {},
    lineTo: () => {},
    stroke: () => {},
    fill: () => {
      counts.pathFills++;
    },
    closePath: () => {},
    setLineDash: () => {},
  } as unknown as LaneDrawContext;
  return {
    ctx,
    fillTexts,
    get pathFills() {
      return counts.pathFills;
    },
  };
}

const group = (name: string, workerIds: number[], inferred = false): RuntimeGroup => ({
  name,
  workerIds,
  inferred,
});

/** Paint the gutter for `groups` and return the recorder. */
function paintRec(
  groups: RuntimeGroup[],
  opts: {
    height?: number;
    scrollTop?: number;
    metrics?: { runtimes: Set<string>; collapsed?: Record<string, boolean> };
    collapsedRuntimes?: Record<string, boolean>;
  } = {},
): ReturnType<typeof recordingCtx> {
  const rowLayout = laneRowLayout(
    groups,
    LANE_ROW_H,
    RUNTIME_HEADER_H,
    opts.collapsedRuntimes ?? {},
    opts.metrics ?? { runtimes: new Set() },
  );
  const identities = buildLaneIdentities(groups);
  const rec = recordingCtx();
  renderLaneLabels(rec.ctx, {
    rowLayout,
    scrollTop: opts.scrollTop ?? 0,
    labelW: 100,
    height: opts.height ?? 600,
    runtimeAccents: identities.byRuntime,
    workerRuntime: identities.workerRuntime,
  });
  return rec;
}

/** Paint the gutter and return just the strings it drew. */
function paint(
  groups: RuntimeGroup[],
  opts: Parameters<typeof paintRec>[1] = {},
): string[] {
  return paintRec(groups, opts).fillTexts;
}

describe("renderLaneLabels", () => {
  it("names every worker row by id", () => {
    const texts = paint([group("main", [0, 7, 31], true)]);
    expect(texts).toContain("W0");
    expect(texts).toContain("W7");
    expect(texts).toContain("W31");
  });

  it("names each runtime group's header", () => {
    const texts = paint([group("main", [0], true), group("io", [1])]);
    expect(texts).toContain("main");
    expect(texts).toContain("io");
  });

  it('names a summary row "<runtime> runtime metrics" across two lines', () => {
    // LABEL_W does not fit the phrase on one line, so it wraps; both halves must
    // be present, and together they must read as the band's title.
    const texts = paint([group("main", [0], true)], {
      metrics: { runtimes: new Set(["main"]) },
    });
    expect(texts).toContain("main runtime");
    expect(texts).toContain("metrics");
  });

  it("a folded summary row keeps a one-line name", () => {
    const texts = paint([group("io", [0])], {
      metrics: { runtimes: new Set(["io"]), collapsed: { io: true } },
    });
    // Only the strip's height is available, so the gutter shortens the phrase
    // rather than clipping it; the band spells the rest out.
    expect(texts).toContain("io metrics");
    expect(texts).not.toContain("metrics");
  });

  it("culls to the visible scroll window, exactly as the lanes canvas does", () => {
    // Four 60px rows in a 120px gutter: only two rows fit.
    const texts = paint([group("main", [0, 1, 2, 3], true)], { height: 120 });
    expect(texts).toEqual(["W0", "W1"]);
  });

  it("shifts the labelled window by scrollTop", () => {
    const texts = paint([group("main", [0, 1, 2, 3], true)], {
      height: 120,
      scrollTop: 120,
    });
    expect(texts).toEqual(["W2", "W3"]);
  });

  it("omits a folded runtime's workers and its summary row", () => {
    const texts = paint([group("a", [0, 1]), group("b", [2])], {
      collapsedRuntimes: { a: true },
      metrics: { runtimes: new Set(["a", "b"]) },
    });
    // "a" is folded: its header remains, its workers and summary row do not.
    expect(texts).toContain("a");
    expect(texts).not.toContain("W0");
    expect(texts).not.toContain("W1");
    // "b" is open, so its worker and summary row are both labelled.
    expect(texts).toContain("W2");
    expect(texts).toContain("b runtime");
  });

  it("degenerate geometry draws nothing", () => {
    const rowLayout = laneRowLayout([group("main", [0], true)], LANE_ROW_H, RUNTIME_HEADER_H);
    const identities = buildLaneIdentities([group("main", [0], true)]);
    for (const [labelW, height] of [
      [0, 600],
      [100, 0],
    ] as const) {
      const rec = recordingCtx();
      renderLaneLabels(rec.ctx, {
        rowLayout,
        scrollTop: 0,
        labelW,
        height,
        runtimeAccents: identities.byRuntime,
        workerRuntime: identities.workerRuntime,
      });
      expect(rec.fillTexts).toEqual([]);
    }
  });

  it("draws NO disclosure carets: the gutter is not a click target", () => {
    // Regression: the gutter used to draw a caret on every foldable row, doubling
    // the triangles on screen and pointing at dead pixels — the fold hit-test
    // resolves inside the lanes box (x >= LABEL_W), so a gutter caret is inert.
    // The band beside each foldable row carries the real one.
    const rec = paintRec([group("a", [0, 1]), group("b", [2])], {
      metrics: { runtimes: new Set(["a", "b"]), collapsed: { b: true } },
    });
    // Two headers + two summary rows = four rows that previously drew a caret.
    expect(rec.pathFills).toBe(0);
  });

  it("a collapsed summary row occupies only the strip height", () => {
    // Pins the gutter's dependence on the SHARED row heights: the gutter reads
    // heights off the row layout, so it cannot disagree with the lanes canvas.
    const { rows } = laneRowLayout([group("main", [0], true)], LANE_ROW_H, RUNTIME_HEADER_H, {}, {
      runtimes: new Set(["main"]),
      collapsed: { main: true },
    });
    const metricsRow = rows.find((r) => r.kind === "runtime-metrics");
    expect(metricsRow?.height).toBe(METRICS_LANE_COLLAPSED_H);
  });
});

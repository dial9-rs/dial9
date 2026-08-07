// Unit tests for the S3 browser density-timeline helpers in heatmap.js.
//
// Migrated from test_heatmap.js (T10); frozen core loaded via createRequire
// (see format.test.ts for the rationale). Compound ok(a && b) checks from the
// original are split into individual expects so failures pinpoint the exact
// leg; no original condition was dropped.

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

interface Segment {
  key: string;
  size: number;
  start: number;
  end: number;
  service: string;
  host: string;
  bootId: string;
}

const {
  MAX_OPEN_BYTES,
  groupByHost,
  bootTransitions,
  tileSegments,
  segmentGaps,
  accumulateDensity,
  segmentsOverlapping,
  totalBytes,
  densityColor,
  niceTimeTicks,
  shouldClearSelectionOnClick,
} = require("../../heatmap.js") as {
  MAX_OPEN_BYTES: number;
  groupByHost: (
    segs: Segment[],
  ) => { host: string; segments: Segment[]; totalBytes: number }[];
  bootTransitions: (
    segs: Segment[],
  ) => { time: number; fromBoot: string; toBoot: string }[];
  tileSegments: (segs: Segment[]) => (Segment & { realEnd?: number })[];
  segmentGaps: (segs: Segment[]) => { start: number; end: number }[];
  accumulateDensity: (
    segs: Segment[],
    t0: number,
    t1: number,
    width: number,
  ) => number[];
  segmentsOverlapping: (segs: Segment[], t0: number, t1: number) => Segment[];
  totalBytes: (segs: Segment[]) => number;
  densityColor: (v: number) => string;
  niceTimeTicks: (tMin: number, tMax: number, targetCount: number) => number[];
  shouldClearSelectionOnClick: (o: {
    isBrowseTab: boolean;
    hasSelection: boolean;
    wasDrag: boolean;
    targetInHeatmap: boolean;
    targetInActions: boolean;
    targetInHeader?: boolean;
  }) => boolean;
};

function seg(o: Partial<Segment>): Segment {
  return {
    key: "k",
    size: 0,
    start: 0,
    end: 0,
    service: "svc",
    host: "h",
    bootId: "",
    ...o,
  };
}

// Matches the original `approx(a, b, eps)` helper: |a - b| <= eps.
function expectApprox(a: number, b: number, eps: number, label: string): void {
  expect(Math.abs(a - b), `${label} (got ${a}, want ~${b})`).toBeLessThanOrEqual(
    eps,
  );
}

describe("groupByHost", () => {
  const rows = groupByHost([
    seg({ service: "api", host: "h2", size: 10, start: 5 }),
    seg({ service: "api", host: "h1", size: 20, start: 1 }),
    seg({ service: "api", host: "h1", size: 30, start: 3, bootId: "bbbb" }),
  ]);

  it("two host rows", () => {
    expect(rows.length).toBe(2);
  });
  it("sorted by label", () => {
    expect(rows[0]!.host).toBe("h1");
    expect(rows[1]!.host).toBe("h2");
  });
  it("h1 has both segments (boot not split)", () => {
    expect(rows[0]!.segments.length).toBe(2);
  });
  it("totalBytes summed", () => {
    expect(rows[0]!.totalBytes).toBe(50);
  });
  it("segments sorted by start", () => {
    expect(rows[0]!.segments[0]!.start).toBeLessThanOrEqual(
      rows[0]!.segments[1]!.start,
    );
  });
});

describe("bootTransitions", () => {
  const t = bootTransitions([
    seg({ start: 1, bootId: "aaaa" }),
    seg({ start: 2, bootId: "aaaa" }),
    seg({ start: 3, bootId: "bbbb" }),
    seg({ start: 4, bootId: "bbbb" }),
    seg({ start: 5, bootId: "cccc" }),
  ]);

  it("two transitions", () => {
    expect(t.length).toBe(2);
  });
  it("first", () => {
    expect(t[0]!.time).toBe(3);
    expect(t[0]!.fromBoot).toBe("aaaa");
    expect(t[0]!.toBoot).toBe("bbbb");
  });
  it("second", () => {
    expect(t[1]!.time).toBe(5);
    expect(t[1]!.toBoot).toBe("cccc");
  });
  it("none without boot ids (legacy)", () => {
    expect(bootTransitions([seg({ start: 1 }), seg({ start: 2 })]).length).toBe(
      0,
    );
  });
});

describe("accumulateDensity", () => {
  it("uniform segment spreads evenly and conserves bytes", () => {
    // One segment spanning the whole range, 400 bytes, width 4 -> 100 per col.
    const cols = accumulateDensity(
      [seg({ size: 400, start: 0, end: 4 })],
      0,
      4,
      4,
    );
    expect(cols.length, "width respected").toBe(4);
    expectApprox(cols[0]!, 100, 1e-6, "uniform col 0");
    expectApprox(cols[3]!, 100, 1e-6, "uniform col 3");
    const sum = cols.reduce((a, b) => a + b, 0);
    expectApprox(sum, 400, 1e-6, "total bytes conserved");
  });

  it("spike dominates the baseline in its own column", () => {
    // Baseline 1MB/min across 10 cols + a 50MB spike in one minute. The spike
    // column must dominate and be ~50x the baseline columns.
    const segs: Segment[] = [];
    for (let i = 0; i < 10; i++) {
      segs.push(
        seg({ size: 1e6, start: i * 60, end: (i + 1) * 60, key: "b" + i }),
      );
    }
    // spike: 50MB in minute index 4
    segs[4] = seg({ size: 50e6, start: 4 * 60, end: 5 * 60, key: "spike" });
    const cols = accumulateDensity(segs, 0, 600, 10);
    let maxIdx = 0;
    for (let i = 1; i < cols.length; i++) if (cols[i]! > cols[maxIdx]!) maxIdx = i;
    expect(maxIdx, "spike lands in the right column").toBe(4);
    expectApprox(cols[4]! / cols[0]!, 50, 0.001, "spike is ~50x baseline");
  });

  it("out-of-range / degenerate inputs are safe", () => {
    expect(
      accumulateDensity([], 0, 10, 5).every((v) => v === 0),
      "empty -> zeros",
    ).toBe(true);
    expect(
      accumulateDensity([seg({ size: 100, start: 0, end: 1 })], 5, 5, 4).length,
      "zero-width range -> zeros length kept",
    ).toBe(4);
  });

  it("zero-duration segment still contributes its bytes, localized", () => {
    // A segment with end <= start is given MIN_SEGMENT_SECONDS so its bytes
    // still land as a localized spike rather than vanishing.
    const cols = accumulateDensity(
      [seg({ size: 500, start: 10, end: 10 })],
      0,
      20,
      20,
    );
    const sum = cols.reduce((a, b) => a + b, 0);
    expectApprox(sum, 500, 1e-6, "zero-duration segment contributes its bytes");
    expect(cols[10]!, "spike present at its start").toBeGreaterThan(0);
    expect(cols[15]!, "spike is localized (no bleed)").toBe(0);
  });

  it("partial overlap attributes only the in-range bytes", () => {
    // Only the in-range portion of a segment's bytes is attributed when it
    // partially overlaps [t0,t1]: half of a uniformly-spread segment.
    const cols = accumulateDensity(
      [seg({ size: 800, start: 0, end: 80 })],
      40,
      80,
      4,
    );
    const sum = cols.reduce((a, b) => a + b, 0);
    expectApprox(sum, 400, 1e-6, "only the in-range bytes attributed");
  });
});

describe("tileSegments & segmentGaps", () => {
  // Upload-lag overlap: each segment's end (last_modified) runs past the next
  // segment's start. tileSegments clamps the end to the next start; the raw
  // overlap would double-count density at the seam.
  const segs = [
    seg({ key: "a", size: 100, start: 0, end: 17 }), // ends 2s into b
    seg({ key: "b", size: 100, start: 15, end: 32 }), // ends 2s into c
    seg({ key: "c", size: 100, start: 30, end: 47 }),
  ];
  const tiled = tileSegments(segs);

  it("ends clamped to the next start", () => {
    expect(tiled[0]!.end).toBe(15);
    expect(tiled[1]!.end).toBe(30);
  });
  it("last segment keeps its end", () => {
    expect(tiled[2]!.end).toBe(47);
  });
  it("real end preserved for selection/gaps", () => {
    expect(tiled[0]!.realEnd).toBe(17);
    expect(tiled[1]!.realEnd).toBe(32);
  });
  it("overlapping rotation has no gap", () => {
    expect(segmentGaps(segs).length).toBe(0);
  });

  it("seam no longer spikes once tiled", () => {
    // The seam column is no longer brighter than a body column once tiled.
    const cols = accumulateDensity(tiled, 0, 47, 47);
    const seam = cols[15]!; // the a/b boundary second
    const body = cols[5]!;
    expect(
      seam,
      `seam no longer spikes (seam ${seam.toFixed(1)} vs body ${body.toFixed(1)})`,
    ).toBeLessThanOrEqual(body * 1.2);
  });

  // A real coverage hole (next starts after this one's real end) is a gap.
  const withHole = [
    seg({ key: "a", size: 100, start: 0, end: 17 }),
    seg({ key: "b", size: 100, start: 60, end: 77 }), // 43s hole after a
  ];

  it("real coverage hole detected with [end, nextStart]", () => {
    const gaps = segmentGaps(withHole);
    expect(gaps.length).toBe(1);
    expect(gaps[0]!.start).toBe(17);
    expect(gaps[0]!.end).toBe(60);
  });
  it("no clamp across a real gap (next start is past end)", () => {
    // Tiling must not invent coverage across the hole.
    expect(tileSegments(withHole)[0]!.end).toBe(17);
  });

  it("sorts before clamping", () => {
    // Inputs need not be pre-sorted.
    const unsorted = [
      seg({ start: 30, end: 47 }),
      seg({ start: 0, end: 17 }),
      seg({ start: 15, end: 32 }),
    ];
    const ts = tileSegments(unsorted);
    expect(ts[0]!.start).toBe(0);
    expect(ts[1]!.start).toBe(15);
    expect(ts[2]!.start).toBe(30);
  });
});

describe("segmentsOverlapping & totalBytes", () => {
  const segs = [
    seg({ key: "a", size: 10, start: 0, end: 60 }),
    seg({ key: "b", size: 20, start: 60, end: 120 }),
    seg({ key: "c", size: 40, start: 120, end: 180 }),
  ];

  it("picks overlapping segments", () => {
    const sel = segmentsOverlapping(segs, 30, 90);
    expect(sel.length).toBe(2);
    expect(sel[0]!.key).toBe("a");
    expect(sel[1]!.key).toBe("b");
  });
  it("sums selected sizes", () => {
    const sel = segmentsOverlapping(segs, 30, 90);
    expect(totalBytes(sel)).toBe(30);
  });
  it("none outside range", () => {
    expect(segmentsOverlapping(segs, 200, 300).length).toBe(0);
  });

  // A point query (single click) exactly on a segment's start must select it
  // rather than fall through the crack between adjacent segments.
  it("click on a segment's start selects that segment", () => {
    const atStart = segmentsOverlapping(segs, 60, 60);
    expect(atStart.length).toBe(1);
    expect(atStart[0]!.key).toBe("b");
  });
  it("click on the very first start still selects", () => {
    const atZero = segmentsOverlapping(segs, 0, 0);
    expect(atZero.length).toBe(1);
    expect(atZero[0]!.key).toBe("a");
  });
});

describe("densityColor", () => {
  const lum = (c: string): number => {
    const [r, g, b] = c.match(/\d+/g)!.map(Number) as [number, number, number];
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
  };

  it("0 -> background", () => {
    expect(densityColor(0)).toBe("rgb(26,26,46)");
  });
  it("negative -> background", () => {
    expect(densityColor(-1)).toBe("rgb(26,26,46)");
  });
  it("small positive is visible (not background)", () => {
    expect(densityColor(0.05)).not.toBe("rgb(26,26,46)");
  });
  it("brighter with higher density", () => {
    const low = densityColor(0.05);
    const mid = densityColor(0.4);
    const hi = densityColor(1);
    expect(lum(low)).toBeLessThan(lum(mid));
    expect(lum(mid)).toBeLessThan(lum(hi));
  });
  it("1 -> hot yellow", () => {
    expect(densityColor(1)).toBe("rgb(255,217,61)");
  });
});

describe("constants", () => {
  it("MAX_OPEN_BYTES is 200 MiB", () => {
    expect(MAX_OPEN_BYTES).toBe(200 * 1024 * 1024);
  });
});

describe("shouldClearSelectionOnClick", () => {
  // The regression: a selection drag that ends outside the pane fires a
  // synthetic click on an ancestor above #heatmap-view. That click must NOT
  // clear the just-created selection.
  it("drag ending outside pane keeps selection", () => {
    expect(
      shouldClearSelectionOnClick({
        isBrowseTab: true,
        hasSelection: true,
        wasDrag: true,
        targetInHeatmap: false,
        targetInActions: false,
      }),
    ).toBe(false);
  });

  it("genuine outside click clears", () => {
    expect(
      shouldClearSelectionOnClick({
        isBrowseTab: true,
        hasSelection: true,
        wasDrag: false,
        targetInHeatmap: false,
        targetInActions: false,
      }),
    ).toBe(true);
  });

  it("click inside heatmap keeps selection", () => {
    expect(
      shouldClearSelectionOnClick({
        isBrowseTab: true,
        hasSelection: true,
        wasDrag: false,
        targetInHeatmap: true,
        targetInActions: false,
      }),
    ).toBe(false);
  });

  it("click on actions bar keeps selection", () => {
    expect(
      shouldClearSelectionOnClick({
        isBrowseTab: true,
        hasSelection: true,
        wasDrag: false,
        targetInHeatmap: false,
        targetInActions: true,
      }),
    ).toBe(false);
  });

  // Regression (#645/#644 interaction): the TZ toggle and credentials button
  // live in the page <header>, which is neither the timeline nor the actions
  // bar. Clicking header chrome must NOT clear the selection - toggling TZ only
  // relabels the axis.
  it("click in header (TZ toggle) keeps selection", () => {
    expect(
      shouldClearSelectionOnClick({
        isBrowseTab: true,
        hasSelection: true,
        wasDrag: false,
        targetInHeatmap: false,
        targetInActions: false,
        targetInHeader: true,
      }),
    ).toBe(false);
  });

  it("not browse tab -> no clear", () => {
    expect(
      shouldClearSelectionOnClick({
        isBrowseTab: false,
        hasSelection: true,
        wasDrag: false,
        targetInHeatmap: false,
        targetInActions: false,
      }),
    ).toBe(false);
  });

  it("no selection -> no clear", () => {
    expect(
      shouldClearSelectionOnClick({
        isBrowseTab: true,
        hasSelection: false,
        wasDrag: false,
        targetInHeatmap: false,
        targetInActions: false,
      }),
    ).toBe(false);
  });
});

describe("niceTimeTicks", () => {
  it("ticks ascending, in range, snapped to a round step", () => {
    const ticks = niceTimeTicks(1000, 1600, 6);
    expect(ticks.length, "returns at least one tick").toBeGreaterThan(0);
    expect(
      ticks.every((t, i) => i === 0 || t > ticks[i - 1]!),
      "strictly ascending",
    ).toBe(true);
    expect(
      ticks.every((t) => t >= 1000 && t <= 1600),
      "all ticks within range",
    ).toBe(true);

    // A ~600s span with target 6 must snap to a round step (a multiple of a
    // human interval like 120s), not an arbitrary 100s division.
    const step = ticks[1]! - ticks[0]!;
    expect([60, 120], `snaps to a round step (got ${step})`).toContain(step);
    expect(step % 60 === 0 || step % 30 === 0, "step is a round interval").toBe(true);

    // Count stays within the target.
    expect(ticks.length, "count <= target").toBeLessThanOrEqual(6);

    // Ticks are aligned to multiples of the step so they land on round times.
    expect(
      ticks.every((t) => t % step === 0),
      "ticks aligned to the step",
    ).toBe(true);
  });

  it("wide span still respects the target", () => {
    const wide = niceTimeTicks(0, 86400, 8);
    expect(wide.length, "wide span respects target").toBeLessThanOrEqual(8);
    expect(
      wide.every((t, i) => i === 0 || t > wide[i - 1]!),
      "wide span ascending",
    ).toBe(true);
  });

  it("degenerate range returns a single tick without throwing", () => {
    expect(niceTimeTicks(500, 500, 6)).toStrictEqual([500]);
    expect(niceTimeTicks(900, 500, 6).length, "inverted bounds -> single tick").toBe(1);
  });
});

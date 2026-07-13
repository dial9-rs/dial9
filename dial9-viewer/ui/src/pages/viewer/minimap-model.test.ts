// Unit tests for the overview-minimap density/coverage/geometry model. The
// headline behavior: a multi-segment trace whose tail is UNFETCHED (tier-1-only)
// must render a navigable overview whose tail is NOT presented as empty/complete,
// and a click/drag on that tail must move the viewport into it.

import { describe, it, expect } from "vitest";
import {
  buildMinimapModel,
  computeDensityBins,
  deriveMinimapRange,
  grabOffsetFor,
  insideViewport,
  minimapClickWindow,
  minimapDragWindow,
  nsToFrac,
  overallCoverage,
  segmentBinCoverage,
  viewportBox,
  type AggregateDensity,
} from "./minimap-model.js";
import { coverageSignal } from "../../lib/trace/index.js";
import type { SegmentEntry, SegmentLifecycle } from "../../types/state.js";

/** A minimal segment entry carrying only what the density model reads. */
function seg(
  startNs: number,
  endNs: number,
  state: SegmentLifecycle,
  sizeBytes = 1_000,
): SegmentEntry {
  return { state, extent: { startNs, endNs }, sizeBytes };
}

/**
 * The fixture shape: a multi-segment set (10 segments, each 1s wide, tiled)
 * where fetching is suppressed beyond segment N - the head is "parsed" (fetched
 * tier-2), the tail is "listed" (tier-1-only).
 */
function multiSegmentSet(
  count: number,
  fetchedThrough: number,
  segNs = 1_000_000_000,
): Map<string, SegmentEntry> {
  const m = new Map<string, SegmentEntry>();
  for (let i = 0; i < count; i++) {
    const start = i * segNs;
    const state: SegmentLifecycle = i <= fetchedThrough ? "parsed" : "listed";
    m.set(`seg-${i}.bin`, seg(start, start + segNs, state));
  }
  return m;
}

describe("deriveMinimapRange", () => {
  it("returns null when nothing is loaded", () => {
    expect(deriveMinimapRange({ minTs: 0, maxTs: 0 }, new Map())).toBeNull();
  });

  it("uses the viewport bounds when there are no segments", () => {
    expect(deriveMinimapRange({ minTs: 100, maxTs: 500 }, new Map())).toEqual({
      startNs: 100,
      endNs: 500,
    });
  });

  it("extends the range to cover segment extents beyond the resident bounds", () => {
    // Resident trace bounds only reach 3s, but the listing extends to 10s: the
    // tail must be inside the range so it is navigable (headline behavior).
    const segments = multiSegmentSet(10, 2);
    const range = deriveMinimapRange({ minTs: 0, maxTs: 3_000_000_000 }, segments);
    expect(range).toEqual({ startNs: 0, endNs: 10_000_000_000 });
  });
});

describe("segmentBinCoverage - residency mapping (matches readout.coverageAt)", () => {
  it("maps residency states to coverage", () => {
    expect(segmentBinCoverage("parsed")).toBe("complete");
    expect(segmentBinCoverage("listed")).toBe("truncated");
    expect(segmentBinCoverage("fetching")).toBe("truncated");
    expect(segmentBinCoverage("evicted")).toBe("truncated");
    expect(segmentBinCoverage("oversized")).toBe("oversized");
  });
});

describe("computeDensityBins - multi-segment tail UNFETCHED (headline DoD)", () => {
  const segments = multiSegmentSet(10, 4); // segments 0..4 fetched, 5..9 listed
  const range = deriveMinimapRange({ minTs: 0, maxTs: 3_000_000_000 }, segments)!;
  const bins = computeDensityBins({ range, segments, binCount: 10 });

  it("produces one bin per segment across the full extent", () => {
    expect(bins).toHaveLength(10);
    // Every bin carries listing-derived density (>0): the tail is NOT empty.
    for (const b of bins) expect(b.density).toBeGreaterThan(0);
  });

  it("marks the fetched head complete and the unfetched tail truncated", () => {
    expect(bins.slice(0, 5).map((b) => b.coverage)).toEqual(
      new Array(5).fill("complete"),
    );
    expect(bins.slice(5).map((b) => b.coverage)).toEqual(
      new Array(5).fill("truncated"),
    );
  });

  it("does NOT present the unfetched tail as complete (T17-audit notes 6-7)", () => {
    const { hasTier1Only, signal } = overallCoverage(bins, null);
    expect(hasTier1Only).toBe(true);
    expect(signal).toBe("partial");
  });

  it("keeps the tail navigable: a click on the tail moves the viewport there", () => {
    // View is over the head (0..1s). Click on segment 8 (~8.5s).
    const viewWidth = 1_000_000_000;
    const target = nsToFracRoundTrip(range, 8_500_000_000);
    const win = minimapClickWindow(range, viewWidth, target);
    expect(win.viewStart).toBeGreaterThan(7_000_000_000);
    expect(win.viewEnd).toBeLessThanOrEqual(range.endNs);
    expect(win.viewEnd - win.viewStart).toBeCloseTo(viewWidth, -3);
  });
});

/** Round-trip a ns through fraction space (mirrors the pointer path). */
function nsToFracRoundTrip(
  range: { startNs: number; endNs: number },
  ns: number,
): number {
  const frac = nsToFrac(range, ns);
  return range.startNs + frac * (range.endNs - range.startNs);
}

describe("computeDensityBins - source precedence", () => {
  const range = { startNs: 0, endNs: 1000 };

  it("trusts aggregate density when coverage is full", () => {
    const aggregate: AggregateDensity = { coverage: "full", bins: [0, 10, 0, 10] };
    const bins = computeDensityBins({ range, segments: new Map(), aggregate, binCount: 4 });
    expect(bins.map((b) => b.coverage)).toEqual(new Array(4).fill("complete"));
    expect(bins[1]!.density).toBe(1); // normalized peak
    expect(bins[0]!.density).toBe(0);
  });

  it("falls back to listing density on partial aggregate (never presents it complete)", () => {
    const segments = multiSegmentSet(4, 1);
    const aggregate: AggregateDensity = { coverage: "partial", bins: [5, 5, 5, 5] };
    const bins = computeDensityBins({ range: deriveMinimapRange({ minTs: 0, maxTs: 4e9 }, segments)!, segments, aggregate, binCount: 4 });
    // Coverage reflects segment residency, not the (ignored) aggregate.
    expect(bins.map((b) => b.coverage)).toEqual([
      "complete",
      "complete",
      "truncated",
      "truncated",
    ]);
  });

  it("falls back to listing density on coverage none", () => {
    const segments = multiSegmentSet(2, 0);
    const aggregate: AggregateDensity = { coverage: "none", bins: [] };
    const r = deriveMinimapRange({ minTs: 0, maxTs: 2e9 }, segments)!;
    const bins = computeDensityBins({ range: r, segments, aggregate, binCount: 2 });
    expect(bins.map((b) => b.coverage)).toEqual(["complete", "truncated"]);
  });

  it("falls back on a T18 coverageSignal-classified PARTIAL fold (real classifier)", () => {
    // A demand-driven aggregate mid-fold: files_folded < files_matched -> the
    // client classifies this "partial", so the minimap must NOT trust its
    // density and must fall back to listing residency.
    const signal = coverageSignal({
      files_matched: 10,
      files_folded: 4,
      samples_folded: 123,
      total_bytes: 1_000_000,
      hosts_matched: 3,
      hosts_folded: 2,
    });
    expect(signal).toBe("partial");
    const segments = multiSegmentSet(4, 1);
    const aggregate: AggregateDensity = { coverage: signal, bins: [9, 9, 9, 9] };
    const bins = computeDensityBins({
      range: deriveMinimapRange({ minTs: 0, maxTs: 4e9 }, segments)!,
      segments,
      aggregate,
      binCount: 4,
    });
    expect(bins.map((b) => b.coverage)).toEqual([
      "complete",
      "complete",
      "truncated",
      "truncated",
    ]);
    expect(overallCoverage(bins, aggregate).signal).toBe("partial");
  });

  it("uses the whole-trace histogram when there are no segments", () => {
    const bins = computeDensityBins({
      range,
      segments: new Map(),
      traceDensity: [0, 4, 8, 4],
      binCount: 4,
    });
    expect(bins.map((b) => b.coverage)).toEqual(new Array(4).fill("complete"));
    expect(bins[2]!.density).toBe(1);
  });

  it("renders a flat complete band when only a resident trace exists", () => {
    const bins = computeDensityBins({ range, segments: new Map(), binCount: 3 });
    expect(bins).toHaveLength(3);
    for (const b of bins) expect(b.coverage).toBe("complete");
  });
});

describe("computeDensityBins - oversized escalates a bin", () => {
  it("marks a bin oversized when a covering segment can never be resident", () => {
    const segments = new Map<string, SegmentEntry>([
      ["a", seg(0, 1000, "parsed")],
      ["b", seg(1000, 2000, "oversized")],
    ]);
    const range = deriveMinimapRange({ minTs: 0, maxTs: 2000 }, segments)!;
    const bins = computeDensityBins({ range, segments, binCount: 2 });
    expect(bins[0]!.coverage).toBe("complete");
    expect(bins[1]!.coverage).toBe("oversized");
    expect(overallCoverage(bins, null).hasTier1Only).toBe(true);
  });
});

describe("viewportBox + geometry", () => {
  const range = { startNs: 0, endNs: 1000 };
  it("maps the view window to left/width fractions", () => {
    expect(viewportBox(range, 250, 500)).toEqual({ leftFrac: 0.25, widthFrac: 0.25 });
  });
  it("clamps a view wider/earlier than the range", () => {
    const box = viewportBox(range, -100, 2000);
    expect(box.leftFrac).toBe(0);
    expect(box.widthFrac).toBe(1);
  });
});

describe("pointer navigation - click + drag (behavioral)", () => {
  const range = { startNs: 0, endNs: 10_000 };
  const viewWidth = 1_000;

  it("click centers the current view on the target, preserving width", () => {
    const win = minimapClickWindow(range, viewWidth, 5_000);
    expect(win).toEqual({ viewStart: 4_500, viewEnd: 5_500 });
  });

  it("click near the end clamps without shrinking the view", () => {
    const win = minimapClickWindow(range, viewWidth, 9_900);
    expect(win).toEqual({ viewStart: 9_000, viewEnd: 10_000 });
  });

  it("drag keeps the grabbed point under the pointer", () => {
    // Box at [2000, 3000]; grab at 2200 (offset 200), drag pointer to 6200.
    const win0: { viewStart: number; viewEnd: number } = { viewStart: 2_000, viewEnd: 3_000 };
    const grab = grabOffsetFor(win0, viewWidth, 2_200);
    expect(grab).toBe(200);
    const win = minimapDragWindow(range, viewWidth, grab, 6_200);
    expect(win).toEqual({ viewStart: 6_000, viewEnd: 7_000 });
  });

  it("a grab OUTSIDE the box centers on the cursor (offset = half width)", () => {
    const win0 = { viewStart: 0, viewEnd: 1_000 };
    expect(insideViewport(win0, 8_000)).toBe(false);
    const grab = grabOffsetFor(win0, viewWidth, 8_000);
    expect(grab).toBe(500);
    const win = minimapDragWindow(range, viewWidth, grab, 8_000);
    expect(win).toEqual({ viewStart: 7_500, viewEnd: 8_500 });
  });

  it("drag clamps at the range edges", () => {
    const win = minimapDragWindow(range, viewWidth, 500, 200);
    expect(win).toEqual({ viewStart: 0, viewEnd: 1_000 });
  });
});

describe("buildMinimapModel", () => {
  it("assembles range, bins, coverage badge, and the viewport box", () => {
    const segments = multiSegmentSet(4, 1);
    const range = deriveMinimapRange({ minTs: 0, maxTs: 4e9 }, segments)!;
    const model = buildMinimapModel(
      range,
      { viewStart: 0, viewEnd: 1e9 },
      { segments, binCount: 4 },
    );
    expect(model.bins).toHaveLength(4);
    expect(model.coverage).toBe("partial");
    expect(model.hasTier1Only).toBe(true);
    expect(model.viewportBox.leftFrac).toBe(0);
    expect(model.viewportBox.widthFrac).toBeCloseTo(0.25, 5);
  });
});

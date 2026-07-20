// Tests for the Process CPU usage track. Two checks are asserted mechanically
// here: (1) the avg/max readout figures match the legacy
// `visibleCpuStats`/`fmtCpu*`/info-label output to the digit (the
// behavioral-diff target), and (2) the render obeys the render discipline -
// bars via the run-length COALESCER (contiguous equal bars fold), grid +
// capacity via the BATCHED-STROKE path (one stroke() per style, not one per
// line). The carried obligation (surface a truncated/oversized window, never
// paint it as complete) is exercised directly on the renderer.

import { describe, it, expect } from "vitest";
import {
  visibleCpuStats,
  fmtCpuCores,
  fmtCpuPercent,
  cpuReadoutText,
  cpuIntervalAt,
  cpuIntervalTooltip,
  cpuBarColor,
  renderCpuTrack,
  deriveCpuInputs,
  cpuSeriesFor,
  COMPLETE_WINDOW,
  type CpuInputs,
  type CpuWindow,
} from "./cpu.js";
import { nsToDrawX as axisNsToDrawX } from "./axis.js";
import { nsToDrawX as sharedNsToDrawX } from "../../lib/canvas/index.js";
import { TRACKS, trackGeometry } from "./track-layout.js";
import type { TrackSpec } from "./track-layout.js";
import type { PanelGeometry, StoreState } from "../../types/state.js";
import type { ParsedTrace } from "../../types/trace.js";
import type { ProcessCpuUsageInterval } from "../../lib/trace/index.js";

const trackById = (id: string): TrackSpec =>
  TRACKS.find((t) => t.id === id) as TrackSpec;

/** A minimal CPU interval; only the fields the track reads are populated. */
function iv(
  start: number,
  end: number,
  cores: number,
  extra: Partial<ProcessCpuUsageInterval> = {},
): ProcessCpuUsageInterval {
  return {
    start,
    end,
    t: end,
    wallDeltaNs: end - start,
    userDeltaNs: 0,
    systemDeltaNs: 0,
    cpuDeltaNs: cores * (end - start),
    startCpuTimeNs: 0,
    endCpuTimeNs: 0,
    cores,
    totalPercent: null,
    // startSample/endSample are unused by the track; cast the stub.
    ...extra,
  } as ProcessCpuUsageInterval;
}

// ── visibleCpuStats: the exact behavioral-diff core ──────────────────────

describe("visibleCpuStats (legacy visibleCpuStats, verbatim)", () => {
  it("is the overlap-weighted mean and the peak over the visible window", () => {
    // Two equal-length intervals fully in view: mean of 2 and 4 = 3, max 4.
    const intervals = [iv(0, 100, 2), iv(100, 200, 4)];
    const stats = visibleCpuStats(intervals, 0, 200);
    expect(stats.avgCores).toBeCloseTo(3, 10);
    expect(stats.maxCores).toBe(4);
  });

  it("weights by the CLIPPED overlap, not the whole interval", () => {
    // View [50,150] clips 50ns of each interval: equal weights -> mean 3.
    const intervals = [iv(0, 100, 2), iv(100, 200, 4)];
    const stats = visibleCpuStats(intervals, 50, 150);
    expect(stats.avgCores).toBeCloseTo(3, 10);
    // Uneven clip: [0,90] gives 90ns of cores=2, 10ns of cores=6 -> 2.4.
    const uneven = visibleCpuStats([iv(0, 90, 2), iv(90, 200, 6)], 0, 100);
    expect(uneven.avgCores).toBeCloseTo((90 * 2 + 10 * 6) / 100, 10);
    expect(uneven.maxCores).toBe(6);
  });

  it("returns 0/0 for a window with no overlapping intervals", () => {
    const stats = visibleCpuStats([iv(0, 100, 2)], 500, 600);
    expect(stats.avgCores).toBe(0);
    expect(stats.maxCores).toBe(0);
  });

  it("skips before the left edge and breaks past the right edge", () => {
    const intervals = [iv(0, 100, 9), iv(100, 200, 2), iv(200, 300, 9)];
    // Only the middle interval overlaps -> avg 2, max 2 (the 9-core
    // neighbours must not leak in).
    const stats = visibleCpuStats(intervals, 110, 190);
    expect(stats.avgCores).toBe(2);
    expect(stats.maxCores).toBe(2);
  });

  it("binary-searches the left edge (many intervals before a far-right view)", () => {
    // 1000 contiguous 100ns intervals; the view is the very last one. The old
    // code skipped all 999 before it each frame; the result must be unchanged.
    const intervals = Array.from({ length: 1000 }, (_, i) => iv(i * 100, i * 100 + 100, i % 7));
    const stats = visibleCpuStats(intervals, 99_900, 100_000);
    expect(stats.maxCores).toBe(999 % 7);
    expect(stats.avgCores).toBeCloseTo(999 % 7, 10);
  });
});

// ── Formatting (fmtCpuCores / fmtCpuPercent) ─────────────────────────────

describe("fmtCpuCores / fmtCpuPercent (verbatim ports)", () => {
  it("fmtCpuCores: 2 decimals under 10, 1 at/above, trailing zeros stripped", () => {
    expect(fmtCpuCores(0)).toBe("0");
    expect(fmtCpuCores(0.5)).toBe("0.5");
    expect(fmtCpuCores(1.25)).toBe("1.25");
    expect(fmtCpuCores(2)).toBe("2");
    expect(fmtCpuCores(10)).toBe("10");
    expect(fmtCpuCores(12.34)).toBe("12.3");
    expect(fmtCpuCores(15.5)).toBe("15.5");
  });

  it("fmtCpuCores: non-finite renders a dash", () => {
    expect(fmtCpuCores(NaN)).toBe("-");
    expect(fmtCpuCores(Infinity)).toBe("-");
  });

  it("fmtCpuPercent: one decimal + %, dash when non-finite", () => {
    expect(fmtCpuPercent(37.5)).toBe("37.5%");
    expect(fmtCpuPercent(100)).toBe("100.0%");
    expect(fmtCpuPercent(NaN)).toBe("-");
  });
});

describe("cpuReadoutText (legacy #cpu-panel-info string, exact)", () => {
  const DOT = "·"; // middle dot the legacy label joins with

  it("includes the percent clause only when capacity is known", () => {
    const withCap = cpuReadoutText({ avgCores: 1.5, maxCores: 3 }, 4);
    expect(withCap).toBe(`avg 1.5 cores ${DOT} avg 37.5% ${DOT} max 3`);

    const noCap = cpuReadoutText({ avgCores: 1.5, maxCores: 3 }, null);
    expect(noCap).toBe(`avg 1.5 cores ${DOT} max 3`);
  });

  it("clamps the percent at 100 (legacy Math.min)", () => {
    // avg 10 cores on a 4-core box = 250% -> clamped to 100.0%.
    const s = cpuReadoutText({ avgCores: 10, maxCores: 12 }, 4);
    expect(s).toBe(`avg 10 cores ${DOT} avg 100.0% ${DOT} max 12`);
  });
});

// ── Hover content ────────────────────────────────────────────────────────

describe("cpuIntervalAt (legacy findProcessCpuIntervalAt binary search)", () => {
  const intervals = [iv(0, 100, 1), iv(100, 200, 2), iv(200, 300, 3)];

  it("finds the interval spanning a timestamp (edges inclusive)", () => {
    expect(cpuIntervalAt(intervals, 150)?.cores).toBe(2);
    expect(cpuIntervalAt(intervals, 0)?.cores).toBe(1);
    expect(cpuIntervalAt(intervals, 300)?.cores).toBe(3);
  });

  it("returns null outside every interval", () => {
    expect(cpuIntervalAt(intervals, -1)).toBeNull();
    expect(cpuIntervalAt(intervals, 999)).toBeNull();
    expect(cpuIntervalAt([], 10)).toBeNull();
  });
});

describe("cpuIntervalTooltip (legacy cpuIntervalTooltipHtml, structured)", () => {
  it("adds the Total CPU row only when capacity is known", () => {
    const interval = iv(0, 100, 2, { totalPercent: 50 });
    const withCap = cpuIntervalTooltip(interval, 4);
    expect(withCap.map((r) => r.label)).toEqual([
      "Window",
      "CPU time",
      "Cores",
      "Total CPU",
    ]);
    expect(withCap.find((r) => r.label === "Total CPU")?.value).toBe("50.0%");
    expect(withCap.find((r) => r.label === "Cores")?.value).toBe("2");

    const noCap = cpuIntervalTooltip(interval, null);
    expect(noCap.map((r) => r.label)).toEqual(["Window", "CPU time", "Cores"]);
  });
});

// ── Bar colour ramp ──────────────────────────────────────────────────────

describe("cpuBarColor (legacy load ramp, verbatim)", () => {
  it("ramps blue at zero load to pink/red at capacity", () => {
    expect(cpuBarColor(0, 4, 4)).toBe("rgba(79, 195, 247, 0.42)");
    expect(cpuBarColor(4, 4, 4)).toBe("rgba(255, 115, 97, 0.42)");
  });

  it("saturates at load 1 (cores beyond capacity share the top colour)", () => {
    expect(cpuBarColor(8, 4, 8)).toBe(cpuBarColor(4, 4, 8));
  });

  it("falls back to cores/scaleMax when capacity is unknown", () => {
    // load = 2/4 = 0.5 -> r=79+88=167, g=195-40=155, b=247-75=172.
    expect(cpuBarColor(2, null, 4)).toBe("rgba(167, 155, 172, 0.42)");
  });
});

// ── Draw-area alignment (same mapping as the axis) ───────────────────────

describe("nsToDrawX (alignment invariant)", () => {
  // Every time-based track shares ONE mapping (lib/canvas/layout). Reintroducing
  // a per-track copy is the regression this guards: it would still agree here by
  // value, so assert function IDENTITY, not just equal output.
  it("the axis track uses the shared mapping, not a copy", () => {
    expect(axisNsToDrawX).toBe(sharedNsToDrawX);
    for (const ns of [0, 250, 500, 1000]) {
      expect(sharedNsToDrawX(ns, 0, 1000, 1100)).toBe(
        axisNsToDrawX(ns, 0, 1000, 1100),
      );
    }
  });

  it("the cpu track's drawW matches the timeline track's at the same width", () => {
    for (const pw of [420, 1024, 2560]) {
      const opts = { pw, scrollbarW: 15, viewStart: 0, viewEnd: 1e9, dpr: 1 };
      const cpu = trackGeometry(trackById("cpu"), opts).time.drawW;
      const timeline = trackGeometry(trackById("timeline"), opts).time.drawW;
      expect(cpu).toBe(timeline);
    }
  });
});

// A recording 2D context capturing the ops the CPU render makes (node has no
// canvas). Enough to assert the stroke batching + coalesced fills.
interface RectCall {
  x: number;
  y: number;
  w: number;
  h: number;
}
interface TextCall {
  text: string;
  x: number;
  y: number;
}
interface Recording {
  fillRects: RectCall[];
  strokes: number;
  beginPaths: number;
  labels: TextCall[];
  dashes: number[][];
}
function recordingCtx(): { ctx: CanvasRenderingContext2D; rec: Recording } {
  const rec: Recording = {
    fillRects: [],
    strokes: 0,
    beginPaths: 0,
    labels: [],
    dashes: [],
  };
  const ctx = {
    fillStyle: "",
    strokeStyle: "",
    lineWidth: 1,
    font: "",
    textAlign: "" as CanvasTextAlign,
    clearRect() {},
    fillRect(x: number, y: number, w: number, h: number) {
      rec.fillRects.push({ x, y, w, h });
    },
    beginPath() {
      rec.beginPaths++;
    },
    moveTo() {},
    lineTo() {},
    stroke() {
      rec.strokes++;
    },
    setLineDash(segments: number[]) {
      rec.dashes.push([...segments]);
    },
    fillText(text: string, x: number, y: number) {
      rec.labels.push({ text, x, y });
    },
  } as unknown as CanvasRenderingContext2D;
  return { ctx, rec };
}

const geo = (pw = 1200): PanelGeometry =>
  trackGeometry(trackById("cpu"), {
    pw,
    scrollbarW: 0,
    viewStart: 0,
    viewEnd: 1000,
    dpr: 1,
  });

function inputs(
  intervals: ProcessCpuUsageInterval[],
  capacity: number | null,
  window: CpuWindow = COMPLETE_WINDOW,
): CpuInputs {
  return { intervals, capacity, window };
}

describe("renderCpuTrack", () => {
  it("paints only a background and returns the empty readout before a trace loads", () => {
    const { ctx, rec } = recordingCtx();
    const readout = renderCpuTrack(ctx, geo(), 0, 1000, inputs([], null), false);
    expect(rec.fillRects.length).toBe(1); // background only
    expect(rec.strokes).toBe(0);
    expect(readout).toBe("avg 0 cores · max 0");
  });

  it("batches the grid + capacity strokes: one stroke() per STYLE", () => {
    const { ctx, rec } = recordingCtx();
    // 3 grid lines + 1 capacity line = 4 primitives, but 2 styles.
    renderCpuTrack(ctx, geo(), 0, 1000, inputs([iv(0, 1000, 2)], 4), true);
    expect(rec.strokes).toBe(2); // NOT 4 - the whole point of the batcher
    // The capacity line is dashed exactly once, then reset to solid.
    expect(rec.dashes.some((d) => d.length > 0)).toBe(true);
    expect(rec.dashes[rec.dashes.length - 1]).toEqual([]);
  });

  it("draws only the grid stroke when capacity is unknown (no capacity line)", () => {
    const { ctx, rec } = recordingCtx();
    renderCpuTrack(ctx, geo(), 0, 1000, inputs([iv(0, 1000, 2)], null), true);
    expect(rec.strokes).toBe(1); // grid only
  });

  it("folds contiguous equal bars via the coalescer but keeps distinct ones", () => {
    const barFills = (rec: Recording) =>
      // bars have a top inside the chart (y > 0); the bg + edge bands sit at y=0.
      rec.fillRects.filter((r) => r.y > 0);

    // Four contiguous IDENTICAL bars -> one coalesced fillRect.
    const same = recordingCtx();
    renderCpuTrack(
      same.ctx,
      geo(),
      0,
      1000,
      inputs(
        [iv(0, 250, 2), iv(250, 500, 2), iv(500, 750, 2), iv(750, 1000, 2)],
        4,
      ),
      true,
    );
    expect(barFills(same.rec).length).toBe(1);

    // Four contiguous DISTINCT bars (different heights) -> four fillRects.
    const diff = recordingCtx();
    renderCpuTrack(
      diff.ctx,
      geo(),
      0,
      1000,
      inputs(
        [iv(0, 250, 1), iv(250, 500, 2), iv(500, 750, 3), iv(750, 1000, 4)],
        4,
      ),
      true,
    );
    expect(barFills(diff.rec).length).toBe(4);
  });

  it("returns the readout it painted (mirror source)", () => {
    const { ctx } = recordingCtx();
    const intervals = [iv(0, 500, 2), iv(500, 1000, 4)];
    const readout = renderCpuTrack(ctx, geo(), 0, 1000, inputs(intervals, 4), true);
    const stats = visibleCpuStats(intervals, 0, 1000);
    expect(readout).toBe(cpuReadoutText(stats, 4));
    expect(readout).toBe("avg 3 cores · avg 75.0% · max 4");
  });
});

// ── Surface a windowed/oversized view ────────────────────────────────────

describe("renderCpuTrack surfaces a windowed view (windowing obligation)", () => {
  const trace = [iv(0, 500, 2), iv(500, 1000, 4)];

  it("draws no window markers for a complete whole-trace window", () => {
    const { ctx, rec } = recordingCtx();
    renderCpuTrack(ctx, geo(), 0, 1000, inputs(trace, 4, COMPLETE_WINDOW), true);
    expect(rec.labels.some((l) => l.text === "partial window")).toBe(false);
    // Only bg fill sits at y=0 (no edge truncation bands).
    expect(rec.fillRects.filter((r) => r.y === 0).length).toBe(1);
  });

  it("hatches both edges when truncated at both, never a clean edge", () => {
    const { ctx, rec } = recordingCtx();
    const win: CpuWindow = { truncatedAt: "both", oversized: false };
    renderCpuTrack(ctx, geo(), 0, 1000, inputs(trace, 4, win), true);
    // bg + two edge bands = three y=0 fillRects.
    expect(rec.fillRects.filter((r) => r.y === 0).length).toBe(3);
  });

  it("badges an oversized window as partial", () => {
    const { ctx, rec } = recordingCtx();
    const win: CpuWindow = { truncatedAt: null, oversized: true };
    renderCpuTrack(ctx, geo(), 0, 1000, inputs(trace, 4, win), true);
    expect(rec.labels.some((l) => l.text === "partial window")).toBe(true);
  });
});

// ── Behavioral diff: readout EXACT vs legacy ─────────────────────────────
//
// The demo trace carries no ProcessResourceUsageEvent data (the legacy CPU
// panel is hidden for it), and no other repo fixture does either, so a live
// real-trace diff is not available. Instead this diffs the ported readout
// against the LEGACY functions copied VERBATIM from viewer.html (fmtCpuCores,
// fmtCpuPercent, visibleCpuStats, the info string) over a realistic interval
// series at many viewports. The readout is a pure function of (intervals,
// viewport, capacity), so this is complete behavioral coverage.

// --- verbatim legacy copies (viewer.html) ---
function legacyFmtCpuCores(value: number): string {
  if (!Number.isFinite(value)) return "-";
  return value
    .toFixed(value >= 10 ? 1 : 2)
    .replace(/(\.\d*?)0+$/, "$1")
    .replace(/\.$/, "");
}
function legacyFmtCpuPercent(value: number): string {
  if (!Number.isFinite(value)) return "-";
  return `${value.toFixed(1)}%`;
}
function legacyVisibleCpuStats(
  intervals: readonly ProcessCpuUsageInterval[],
  viewStart: number,
  viewEnd: number,
): { avgCores: number; maxCores: number } {
  let maxCores = 0;
  let totalOverlapNs = 0;
  let weightedCoresNs = 0;
  for (const interval of intervals) {
    if (interval.end < viewStart) continue;
    if (interval.start > viewEnd) break;
    const overlap =
      Math.min(interval.end, viewEnd) - Math.max(interval.start, viewStart);
    if (!(overlap > 0)) continue;
    if (interval.cores > maxCores) maxCores = interval.cores;
    totalOverlapNs += overlap;
    weightedCoresNs += interval.cores * overlap;
  }
  const avgCores = totalOverlapNs > 0 ? weightedCoresNs / totalOverlapNs : 0;
  return { avgCores, maxCores };
}
function legacyReadout(
  stats: { avgCores: number; maxCores: number },
  capacity: number | null,
): string {
  // Copied byte-for-byte from viewer.html (the middle dot is the literal
  // U+00B7 that ships in the legacy label).
  const avgPct =
    capacity != null ? Math.min(100, (stats.avgCores / capacity) * 100) : null;
  const pctText = avgPct != null ? ` · avg ${legacyFmtCpuPercent(avgPct)}` : "";
  return `avg ${legacyFmtCpuCores(stats.avgCores)} cores${pctText} · max ${legacyFmtCpuCores(stats.maxCores)}`;
}

describe("readout behavioral diff: ported == legacy, to the digit", () => {
  // A realistic, contiguous avg-cores series (varying loads incl. >capacity).
  const series: ProcessCpuUsageInterval[] = [];
  for (let i = 0; i < 60; i++) {
    const cores = 0.5 + ((i * 37) % 90) / 10; // 0.5 .. 9.4, non-round
    series.push(iv(i * 1000, (i + 1) * 1000, cores));
  }
  const viewports: Array<[number, number]> = [
    [0, 60000], // whole
    [0, 30000], // left half
    [30000, 60000], // right half
    [12345, 41000], // arbitrary offset
    [5000, 5000], // degenerate (zero-span)
    [59000, 60000], // last interval only
    [-1000, 1000], // spilling off the left
    [500, 2500], // straddling three intervals
  ];
  const capacities: Array<number | null> = [null, 4, 8, 0.5];

  for (const cap of capacities) {
    for (const [vs, ve] of viewports) {
      it(`cap=${cap} view=[${vs},${ve}] matches legacy`, () => {
        const mine = cpuReadoutText(visibleCpuStats(series, vs, ve), cap);
        const legacy = legacyReadout(legacyVisibleCpuStats(series, vs, ve), cap);
        expect(mine).toBe(legacy);
      });
    }
  }

  it("also matches for an empty series (panel-hidden analog)", () => {
    expect(cpuReadoutText(visibleCpuStats([], 0, 1000), 4)).toBe(
      legacyReadout(legacyVisibleCpuStats([], 0, 1000), 4),
    );
  });
});

// ── Store lifting + memoization ──────────────────────────────────────────

/** A ProcessResourceUsageEvent custom event (the CPU series source). */
function cpuEvent(t: number, userNs: number, systemNs: number): unknown {
  return {
    name: "ProcessResourceUsageEvent",
    timestamp: t,
    fields: { user_cpu_ns: userNs, system_cpu_ns: systemNs },
  };
}

/** A minimal ParsedTrace exposing only what the CPU track reads. */
function traceWith(
  events: unknown[],
  parallelism: string | null,
): ParsedTrace {
  const segmentMetadata = new Map<string, string>();
  if (parallelism !== null) {
    segmentMetadata.set("process.available_parallelism", parallelism);
  }
  return { customEvents: events, segmentMetadata } as unknown as ParsedTrace;
}

describe("deriveCpuInputs / cpuSeriesFor", () => {
  it("builds intervals from resource-usage events and reads capacity", () => {
    // Two 1000ns steps: cpuDelta 2000 over 1000 -> cores 2, then 4000 -> 4.
    const trace = traceWith(
      [
        cpuEvent(0, 0, 0),
        cpuEvent(1000, 1000, 1000),
        cpuEvent(2000, 3000, 3000),
      ],
      "4",
    );
    const series = cpuSeriesFor(trace);
    expect(series.availableParallelism).toBe(4);
    expect(series.intervals.map((i) => i.cores)).toEqual([2, 4]);
  });

  it("memoizes on the trace identity (built once per loaded trace)", () => {
    const trace = traceWith([cpuEvent(0, 0, 0), cpuEvent(1000, 500, 500)], "8");
    expect(cpuSeriesFor(trace)).toBe(cpuSeriesFor(trace)); // same object
  });

  it("resolves to an empty complete window with no trace", () => {
    const state = {
      trace: { trace: null },
      segments: { segments: new Map() },
    } as unknown as StoreState;
    const got = deriveCpuInputs(state);
    expect(got.intervals).toEqual([]);
    expect(got.capacity).toBeNull();
    expect(got.window).toEqual(COMPLETE_WINDOW);
  });

  it("surfaces the oversized segment state into the window descriptor", () => {
    const state = {
      trace: {
        trace: traceWith([cpuEvent(0, 0, 0), cpuEvent(1000, 500, 500)], "4"),
      },
      segments: {
        segments: new Map([
          ["seg-0", { state: "oversized" }],
          ["seg-1", { state: "parsed" }],
        ]),
      },
    } as unknown as StoreState;
    expect(deriveCpuInputs(state).window.oversized).toBe(true);
  });
});

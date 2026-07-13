// Tests for the time-axis track (T25; features/02 section F + the
// date-qualification amendment). Two load-bearing properties are asserted
// mechanically here (the DoD's primary checks, the live visual walk being a
// plus): (1) axis ticks align PIXEL-EXACT with every other track's ns<->x
// mapping at three column widths (the A13 invariant), and (2) the
// date-qualification amendment - a day-crossing absolute span gains a
// `MM-DD ` prefix, a same-day span does not.

import { describe, it, expect } from "vitest";
import {
  pickTickInterval,
  tickTimestamps,
  nsToDrawX,
  clockOffsetForTimestamp,
  wallClockNs,
  crossesDayBoundaryNs,
  isDateQualified,
  fmtDuration,
  fmtWallClockLabel,
  fmtAxisTick,
  renderTimeAxis,
  type AxisInputs,
} from "./axis.js";
import { TRACKS, LABEL_W, trackGeometry } from "./track-layout.js";
import type { TrackSpec } from "./track-layout.js";
import type { PanelGeometry } from "../../types/state.js";

const trackById = (id: string): TrackSpec =>
  TRACKS.find((t) => t.id === id) as TrackSpec;

/** Bare inputs: relative mode, no anchors (the resting shell defaults). */
function relInputs(minTs = 0): AxisInputs {
  return {
    minTs,
    timeMode: "rel",
    tz: "utc",
    clockSyncAnchors: [],
    clockOffsetNs: null,
  };
}

/** Absolute-mode inputs with a single fixed monotonic->wall offset. */
function absInputs(offsetNs: number, tz: "utc" | "local" = "utc"): AxisInputs {
  return {
    minTs: 0,
    timeMode: "abs",
    tz,
    clockSyncAnchors: [],
    clockOffsetNs: offsetNs,
  };
}

// 2021-01-01T00:00:00Z, in ns since the epoch.
const DAY0_NS = 1_609_459_200 * 1e9;
const HOUR_NS = 3_600 * 1e9;
const DAY_NS = 86_400 * 1e9;

describe("pickTickInterval (F1 nice-values)", () => {
  it("targets max(4, floor(drawW/100)) ticks and snaps to a nice value", () => {
    // drawW 800 -> 8 target ticks; 4e9 / 8 = 5e8 -> exact nice value.
    expect(pickTickInterval(0, 4e9, 800)).toBe(5e8);
    // drawW 300 -> floor 3, clamped to 4; 1e9/4 = 2.5e8 -> snaps up to 5e8.
    expect(pickTickInterval(0, 1e9, 300)).toBe(5e8);
  });

  it("floors the target at 4 ticks on very narrow panels", () => {
    // drawW 50 -> floor 0, clamped to 4; 1e9/4 = 2.5e8 -> 5e8.
    expect(pickTickInterval(0, 1e9, 50)).toBe(5e8);
  });

  it("falls back to the raw interval past the largest nice value", () => {
    // 1e12 span over 4 ticks = 2.5e11 > 1e10 (largest nice value).
    expect(pickTickInterval(0, 1e12, 100)).toBe(1e12 / 4);
  });
});

describe("tickTimestamps (F1 firstTick loop)", () => {
  it("emits every interval multiple in [ceil(start/int)*int, end]", () => {
    expect(tickTimestamps(0, 1e9, 5e8)).toEqual([0, 5e8, 1e9]);
    expect(tickTimestamps(1e8, 1e9, 5e8)).toEqual([5e8, 1e9]);
  });

  it("never spins on a degenerate viewport", () => {
    expect(tickTimestamps(5, 5, 0)).toEqual([]);
    expect(tickTimestamps(10, 0, 1e8)).toEqual([]);
  });
});

describe("nsToDrawX / alignment invariant (A13, three widths)", () => {
  const viewStart = 1_000_000;
  const viewEnd = 4_000_000_000;
  // A representative timestamp set, incl. the edges.
  const samples = [
    viewStart,
    viewStart + 1,
    (viewStart + viewEnd) / 2,
    viewEnd - 7,
    viewEnd,
  ];
  const widths = [
    { pw: 420, scrollbarW: 0 }, // narrow
    { pw: 1024, scrollbarW: 15 }, // medium, with a lane scrollbar
    { pw: 2560, scrollbarW: 0 }, // wide
  ];

  for (const { pw, scrollbarW } of widths) {
    it(`axis tick x == every track's canvas-local x (pw=${pw}, sb=${scrollbarW})`, () => {
      const opts = { pw, scrollbarW, viewStart, viewEnd, dpr: 1 };
      const axisGeo = trackGeometry(trackById("timeline"), opts);
      const others = ["lanes", "cpu", "queue", "spans", "events"].map((id) =>
        trackGeometry(trackById(id), opts),
      );
      for (const ns of samples) {
        const axisX = nsToDrawX(ns, viewStart, viewEnd, axisGeo.time.drawW);
        for (const g of others) {
          // Canvas-local x a lane/panel draws content at (same nsToX form).
          const laneLocalX = nsToDrawX(ns, viewStart, viewEnd, g.time.drawW);
          expect(axisX).toBe(laneLocalX);
          // And, tied to the frozen-core mapping: the axis's FULL-panel x
          // (gutter + local) equals the shared nsToPanelX.
          expect(LABEL_W + axisX).toBeCloseTo(g.time.nsToPanelX(ns), 9);
        }
      }
    });
  }
});

describe("clockOffsetForTimestamp (legacy port)", () => {
  it("uses the whole-trace offset when there are no anchors", () => {
    expect(clockOffsetForTimestamp([], 1234, 999)).toBe(1234);
    expect(clockOffsetForTimestamp([], null, 999)).toBeNull();
  });

  it("uses the single anchor's offset", () => {
    const a = [{ monotonicNs: 500, realtimeNs: 1500 }];
    expect(clockOffsetForTimestamp(a, null, 0)).toBe(1000);
  });

  it("picks the nearest anchor (<=) among many", () => {
    const a = [
      { monotonicNs: 0, realtimeNs: 1000 },
      { monotonicNs: 100, realtimeNs: 2000 },
    ];
    expect(clockOffsetForTimestamp(a, null, 40)).toBe(1000); // nearer to 0
    expect(clockOffsetForTimestamp(a, null, 60)).toBe(1900); // nearer to 100
    expect(clockOffsetForTimestamp(a, null, -5)).toBe(1000); // clamp low
    expect(clockOffsetForTimestamp(a, null, 999)).toBe(1900); // clamp high
  });
});

describe("wallClockNs", () => {
  it("adds the offset; null when no anchor resolves", () => {
    expect(wallClockNs(absInputs(0), DAY0_NS)).toBe(DAY0_NS);
    expect(wallClockNs(relInputs(), DAY0_NS)).toBeNull(); // clockOffsetNs null
  });
});

describe("date-qualification amendment (T25)", () => {
  it("crossesDayBoundaryNs true only across a UTC calendar day", () => {
    expect(crossesDayBoundaryNs(DAY0_NS + HOUR_NS, DAY0_NS + 2 * HOUR_NS, false)).toBe(false);
    expect(crossesDayBoundaryNs(DAY0_NS + 23 * HOUR_NS, DAY0_NS + 25 * HOUR_NS, false)).toBe(true);
  });

  it("local mode compares LOCAL calendar dates", () => {
    const d = new Date(2026, 3, 10, 0, 0, 0); // local midnight
    const t = d.getTime() * 1e6; // ms -> ns
    expect(crossesDayBoundaryNs(t - HOUR_NS, t + HOUR_NS, true)).toBe(true);
    expect(crossesDayBoundaryNs(t + HOUR_NS, t + 2 * HOUR_NS, true)).toBe(false);
  });

  it("isDateQualified: abs + day-crossing -> true; same-day -> false", () => {
    const inputs = absInputs(0);
    // Same day (01:00 -> 02:00 UTC on 2021-01-01).
    expect(isDateQualified(inputs, DAY0_NS + HOUR_NS, DAY0_NS + 2 * HOUR_NS)).toBe(false);
    // Crosses midnight (23:00 -> next-day 01:00).
    expect(isDateQualified(inputs, DAY0_NS + 23 * HOUR_NS, DAY0_NS + 25 * HOUR_NS)).toBe(true);
  });

  it("relative mode is NEVER date-qualified (offsets have no calendar day)", () => {
    // Even a multi-day span stays plain in relative mode.
    expect(isDateQualified(relInputs(), DAY0_NS, DAY0_NS + 3 * DAY_NS)).toBe(false);
  });

  it("absolute mode with no anchor is not qualified (relative fallback)", () => {
    const noAnchor: AxisInputs = { ...absInputs(0), clockOffsetNs: null };
    expect(isDateQualified(noAnchor, DAY0_NS, DAY0_NS + 3 * DAY_NS)).toBe(false);
  });
});

describe("label formatting (fmtTs parity + amendment)", () => {
  it("fmtDuration: +s/+ms/+µs/+ns by magnitude", () => {
    expect(fmtDuration(1.5e9)).toBe("+1.50s");
    expect(fmtDuration(2.5e6)).toBe("+2.50ms");
    expect(fmtDuration(3.4e3)).toBe("+3.4µs");
    expect(fmtDuration(42)).toBe("+42ns");
  });

  it("fmtWallClockLabel: HH:MM:SS, MM-DD prefix when withDate", () => {
    const wall = DAY0_NS + 23 * HOUR_NS + 61 * 1e9; // 23:01:01 UTC
    expect(fmtWallClockLabel(wall, false, false)).toBe("23:01:01");
    expect(fmtWallClockLabel(wall, false, true)).toBe("01-01 23:01:01");
  });

  it("fmtAxisTick: relative mode -> offset from minTs", () => {
    const inputs = relInputs(DAY0_NS);
    expect(fmtAxisTick(inputs, DAY0_NS + 1.5e9, false)).toBe("+1.50s");
  });

  it("fmtAxisTick: absolute same-day -> time only; day-crossing -> dated", () => {
    const inputs = absInputs(0);
    const tick = DAY0_NS + 23 * HOUR_NS; // 23:00:00 on 2021-01-01
    expect(fmtAxisTick(inputs, tick, false)).toBe("23:00:00");
    expect(fmtAxisTick(inputs, tick, true)).toBe("01-01 23:00:00");
  });

  it("fmtAxisTick: absolute with no anchor falls back to relative", () => {
    const noAnchor: AxisInputs = { ...absInputs(0), clockOffsetNs: null, minTs: DAY0_NS };
    expect(fmtAxisTick(noAnchor, DAY0_NS + 2e9, false)).toBe("+2.00s");
  });
});

// A minimal recording 2D context: captures the draw ops renderTimeAxis
// makes (node has no canvas). Enough to assert the F1 ruler is drawn.
interface FillTextCall {
  text: string;
  x: number;
}
interface Recording {
  fills: number;
  strokes: number;
  labels: FillTextCall[];
}
function recordingCtx(): { ctx: CanvasRenderingContext2D; rec: Recording } {
  const rec: Recording = { fills: 0, strokes: 0, labels: [] };
  const ctx = {
    fillStyle: "",
    strokeStyle: "",
    font: "",
    textAlign: "" as CanvasTextAlign,
    clearRect() {},
    fillRect() {
      rec.fills++;
    },
    beginPath() {},
    moveTo() {},
    lineTo() {},
    stroke() {
      rec.strokes++;
    },
    fillText(text: string, x: number) {
      rec.labels.push({ text, x });
    },
  } as unknown as CanvasRenderingContext2D;
  return { ctx, rec };
}

describe("renderTimeAxis (F1 ruler)", () => {
  const viewStart = 0;
  const viewEnd = 4e9;
  const geo: PanelGeometry = trackGeometry(trackById("timeline"), {
    pw: 900,
    scrollbarW: 0,
    viewStart,
    viewEnd,
    dpr: 1,
  });

  it("paints a background but no ticks before a trace loads", () => {
    const { ctx, rec } = recordingCtx();
    renderTimeAxis(ctx, geo, viewStart, viewEnd, relInputs(), false);
    expect(rec.fills).toBe(1); // just the background
    expect(rec.labels.length).toBe(0);
  });

  it("draws one tick+label per interval multiple at nsToDrawX, once loaded", () => {
    const { ctx, rec } = recordingCtx();
    renderTimeAxis(ctx, geo, viewStart, viewEnd, relInputs(), true);
    const interval = pickTickInterval(viewStart, viewEnd, geo.time.drawW);
    const ticks = tickTimestamps(viewStart, viewEnd, interval);
    expect(rec.labels.length).toBe(ticks.length);
    expect(ticks.length).toBeGreaterThanOrEqual(2);
    // Each label at its shared-mapping x, text == fmtAxisTick.
    rec.labels.forEach((call, i) => {
      const t = ticks[i]!;
      expect(call.x).toBe(nsToDrawX(t, viewStart, viewEnd, geo.time.drawW));
      expect(call.text).toBe(fmtAxisTick(relInputs(), t, false));
    });
  });

  it("date-qualifies every label when the absolute span crosses a day", () => {
    const inputs = absInputs(0);
    const { ctx, rec } = recordingCtx();
    // A ~2h window straddling UTC midnight.
    const vs = DAY0_NS + 23 * HOUR_NS;
    const ve = DAY0_NS + 25 * HOUR_NS;
    const g2: PanelGeometry = trackGeometry(trackById("timeline"), {
      pw: 900,
      scrollbarW: 0,
      viewStart: vs,
      viewEnd: ve,
      dpr: 1,
    });
    renderTimeAxis(ctx, g2, vs, ve, inputs, true);
    expect(rec.labels.length).toBeGreaterThan(0);
    for (const call of rec.labels) {
      expect(call.text).toMatch(/^\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/);
    }
  });
});

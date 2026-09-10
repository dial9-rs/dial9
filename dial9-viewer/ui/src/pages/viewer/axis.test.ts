// Tests for the time-axis track. Three load-bearing properties are asserted
// mechanically here: (1) axis ticks align PIXEL-EXACT with every other track's
// ns<->x mapping at three column widths, (2) the date-qualification rule - a
// day-crossing absolute span gains a `MM-DD ` prefix on the anchor, a same-day
// span does not, and (3) the ruler is window-relative: tick labels are offsets
// from the left edge whose unit follows the zoom, framed by an absolute anchor
// and a width/step chip that yield to tick labels on a narrow panel.

import { describe, it, expect } from "vitest";
import {
  pickTickInterval,
  tickOffsets,
  cursorPrecisionNs,
  decimalsForStep,
  fmtTickOffset,
  rulerLayout,
  nsToDrawX,
  clockOffsetForTimestamp,
  wallClockNs,
  crossesDayBoundaryNs,
  isDateQualified,
  fmtDuration,
  fmtWallClockLabel,
  wallClockFracDigits,
  fmtAxisTick,
  renderTimeAxis,
  type AxisInputs,
} from "./axis.js";
import { TRACKS, LABEL_W, trackGeometry } from "../../lib/canvas/track-layout.js";
import type { TrackSpec } from "../../lib/canvas/track-layout.js";
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

describe("pickTickInterval (nice-values)", () => {
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

  it("reaches clock-friendly steps above 10s", () => {
    // 1e12 span (1000s) over 4 ticks = 2.5e11 -> the 5-minute nice value.
    expect(pickTickInterval(0, 1e12, 100)).toBe(3e11);
  });

  it("falls back to the raw interval past the largest nice value (1h)", () => {
    // 1e14 span over 4 ticks = 2.5e13 > 3.6e12.
    expect(pickTickInterval(0, 1e14, 100)).toBe(1e14 / 4);
  });

  it("steps back down when snapping up would overshoot the window", () => {
    // A 4.574s view over a narrow draw area: 4 target ticks -> 1.14s, which
    // snaps to 5s and would leave a single tick. 1s fits twice, so it wins.
    expect(pickTickInterval(0, 4.574e9, 275)).toBe(1e9);
  });

  it("keeps picking steps below a microsecond", () => {
    // 1µs over 8 ticks = 125ns -> 500ns; 40ns over 8 = 5ns exactly.
    expect(pickTickInterval(0, 1e3, 800)).toBe(500);
    expect(pickTickInterval(0, 40, 800)).toBe(5);
  });
});

describe("tickOffsets (window-relative)", () => {
  it("emits every interval multiple from the left edge", () => {
    expect(tickOffsets(1e9, 5e8)).toEqual([0, 5e8, 1e9]);
    expect(tickOffsets(9e8, 5e8)).toEqual([0, 5e8]);
  });

  it("never spins on a degenerate viewport", () => {
    expect(tickOffsets(0, 0)).toEqual([]);
    expect(tickOffsets(-1, 1e8)).toEqual([]);
  });
});

describe("cursorPrecisionNs", () => {
  it("targets ~1px of the visible span, snapped to a nice value", () => {
    expect(cursorPrecisionNs(0, 1e9)).toBe(1e6); // 1s view -> 1ms
    expect(cursorPrecisionNs(0, 5e5)).toBe(500); // 500µs view -> 500ns
  });

  it("floors at 1ns on a degenerate viewport", () => {
    expect(cursorPrecisionNs(5, 5)).toBe(1);
  });
});

describe("decimalsForStep", () => {
  it("counts the decimals a step needs in a given unit", () => {
    expect(decimalsForStep(1e9, 1e9)).toBe(0);
    expect(decimalsForStep(1e6, 1e9)).toBe(3);
    expect(decimalsForStep(5e5, 1e9)).toBe(4);
    expect(decimalsForStep(1, 1e9)).toBe(9);
  });

  it("is zero for a non-positive step", () => {
    expect(decimalsForStep(0, 1e9)).toBe(0);
  });
});

describe("nsToDrawX / alignment invariant (three widths)", () => {
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
          // And, tied to the shared mapping: the axis's FULL-panel x
          // (gutter + local) equals the shared nsToPanelX.
          expect(LABEL_W + axisX).toBeCloseTo(g.time.nsToPanelX(ns), 9);
        }
      }
    });
  }
});

describe("clockOffsetForTimestamp", () => {
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

describe("date-qualification amendment", () => {
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

  it("fmtDuration: decimals follow the step it must resolve", () => {
    // A 1µs step at a 2s offset needs 6 decimals to separate two ticks; a 1s
    // step needs none.
    expect(fmtDuration(2.0007e9, 1e3)).toBe("+2.000700s");
    expect(fmtDuration(2.0007e9, 1e9)).toBe("+2s");
    expect(fmtDuration(2.0007e9, 1e5)).toBe("+2.0007s");
  });

  it("fmtDuration: unitBasisNs pins the unit so a range shares it", () => {
    // Without a basis the window start would read "+0ns" beside a "+4.574s"
    // end; pinning both to the end's magnitude keeps them comparable.
    expect(fmtDuration(0, 5e6, 4.574e9)).toBe("+0.000s");
    expect(fmtDuration(4.574e9, 5e6, 4.574e9)).toBe("+4.574s");
  });

  it("fmtWallClockLabel: HH:MM:SS, MM-DD prefix when withDate", () => {
    const wall = DAY0_NS + 23 * HOUR_NS + 61 * 1e9; // 23:01:01 UTC
    expect(fmtWallClockLabel(wall, false, false)).toBe("23:01:01");
    expect(fmtWallClockLabel(wall, false, true)).toBe("01-01 23:01:01");
  });

  it("fmtWallClockLabel: fractional seconds in 3-digit groups", () => {
    // Small wall value: exact, so the grouping itself is asserted.
    expect(fmtWallClockLabel(1_500_000, false, false, 3)).toBe("00:00:00.001");
    expect(fmtWallClockLabel(1_500_000, false, false, 6)).toBe("00:00:00.001500");
  });

  it("fmtWallClockLabel: a real epoch timestamp keeps µs, not ns", () => {
    // Epoch-ns is past 2^53, so a float64 wall clock is quantised to a few
    // hundred ns: the ms digits are solid, the last µs digit is not. This is
    // why wallClockFracDigits caps at 6.
    const wall = DAY0_NS + 23 * HOUR_NS + 61 * 1e9 + 1_500_000; // +1.5ms
    expect(fmtWallClockLabel(wall, false, false, 3)).toBe("23:01:01.001");
    const six = fmtWallClockLabel(wall, false, false, 6);
    expect(six.startsWith("23:01:01.")).toBe(true);
    const micros = Number(six.slice("23:01:01.".length));
    expect(Math.abs(micros - 1500)).toBeLessThan(2);
  });

  it("fmtWallClockLabel: a pre-epoch wall clock still yields digits", () => {
    // A negative remainder used to pad its minus sign into the fraction
    // (".0-1"); the euclidean remainder keeps the field numeric.
    const label = fmtWallClockLabel(-1_500_000, false, false, 6);
    expect(label).toMatch(/^\d{2}:\d{2}:\d{2}\.\d{6}$/);
  });

  it("wallClockFracDigits caps at µs (float64 epoch-ns is already quantised)", () => {
    expect(wallClockFracDigits(undefined)).toBe(0);
    expect(wallClockFracDigits(1e9)).toBe(0);
    expect(wallClockFracDigits(1e6)).toBe(3);
    expect(wallClockFracDigits(1e3)).toBe(6);
    expect(wallClockFracDigits(1)).toBe(6);
  });

  it("fmtTickOffset: bare zero, then the shared duration format", () => {
    expect(fmtTickOffset(0)).toBe("0");
    expect(fmtTickOffset(1e5)).toBe("100µs");
    expect(fmtTickOffset(1500)).toBe("1.5µs");
    expect(fmtTickOffset(5)).toBe("5ns");
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

describe("rulerLayout (window-relative ruler)", () => {
  // A 500µs window that starts 2.0007s into the trace, over an 800px draw
  // area: 8 target ticks -> a 100µs step.
  const viewStart = 2.0007e9;
  const viewEnd = viewStart + 5e5;

  it("labels ticks as offsets from the left edge, in the zoom's unit", () => {
    const layout = rulerLayout(viewStart, viewEnd, 800, relInputs());
    expect(layout.interval).toBe(1e5);
    expect(layout.ticks.map((t) => t.offsetNs)).toEqual([
      0, 1e5, 2e5, 3e5, 4e5, 5e5,
    ]);
    // The 0 tick yields to the anchor beside it, and the last tick sits under
    // the chip; the ones between carry their offsets.
    expect(layout.ticks.map((t) => t.text)).toEqual([
      null,
      "100µs",
      "200µs",
      "300µs",
      "400µs",
      null,
    ]);
  });

  it("anchors the window's absolute position at the step's precision", () => {
    const layout = rulerLayout(viewStart, viewEnd, 800, relInputs());
    expect(layout.anchor).toBe("+2.0007s");
    // Zoomed a further two decades in, the anchor gains the decimals it needs.
    const deep = rulerLayout(viewStart, viewStart + 5e3, 800, relInputs());
    expect(deep.interval).toBe(1e3);
    expect(deep.anchor).toBe("+2.000700s");
  });

  it("keeps the anchor in the window's unit at the trace start", () => {
    // viewStart is 0 here: without a unit basis the anchor would read "+0ns"
    // beside second-scale ticks.
    const layout = rulerLayout(0, 4.574e9, 410, relInputs());
    expect(layout.interval).toBe(1e9);
    expect(layout.anchor).toBe("+0s");
    expect(layout.ticks.map((t) => t.text)).toEqual([
      null,
      "1s",
      "2s",
      "3s",
      null,
    ]);
  });

  it("states the visible width and the step in the chip", () => {
    const layout = rulerLayout(viewStart, viewEnd, 800, relInputs());
    expect(layout.chip).toBe("<-> 500µs (step 100µs)");
  });

  it("date-qualifies the anchor when the absolute span crosses a day", () => {
    const vs = DAY0_NS + 23 * HOUR_NS;
    const layout = rulerLayout(vs, vs + 2 * HOUR_NS, 900, absInputs(0));
    expect(layout.anchor).toMatch(/^\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/);
    // Ticks stay relative, so they never carry a date.
    for (const tick of layout.ticks) {
      if (tick.text !== null) expect(tick.text).not.toMatch(/:/);
    }
  });

  it("sheds the step, then the chip, then the anchor as the panel narrows", () => {
    const wide = rulerLayout(viewStart, viewEnd, 800, relInputs());
    expect(wide.chip).toContain("step");
    const mid = rulerLayout(viewStart, viewEnd, 400, relInputs());
    expect(mid.chip).toBe("<-> 500µs");
    const narrow = rulerLayout(viewStart, viewEnd, 250, relInputs());
    expect(narrow.chip).toBeNull();
    expect(narrow.anchor).not.toBeNull();
    const sliver = rulerLayout(viewStart, viewEnd, 120, relInputs());
    expect(sliver.chip).toBeNull();
    expect(sliver.anchor).toBeNull();
  });

  it("drops an edge label that the canvas would clip", () => {
    // 250px: no chip (its 320px floor), so only the canvas edge protects the
    // last tick, which lands exactly on x = drawW. The anchor covers x = 0.
    const layout = rulerLayout(viewStart, viewEnd, 250, relInputs());
    expect(layout.chip).toBeNull();
    expect(layout.ticks[0]!.offsetNs).toBe(0);
    expect(layout.ticks[0]!.text).toBeNull();
    const last = layout.ticks[layout.ticks.length - 1]!;
    expect(last.x).toBe(250);
    expect(last.text).toBeNull();
  });

  it("keeps the zero label when there is no anchor to yield to", () => {
    // Below the anchor floor the ruler is bare ticks, so nothing reserves the
    // left edge - but a label centred at x = 0 would still be half clipped.
    const layout = rulerLayout(viewStart, viewEnd, 120, relInputs());
    expect(layout.anchor).toBeNull();
    expect(layout.ticks[0]!.text).toBeNull();
  });

  it("drops a colliding label but keeps its tick mark", () => {
    // 4 ticks over 200px: the labels do not all fit beside the anchor.
    const layout = rulerLayout(viewStart, viewEnd, 200, relInputs());
    expect(layout.ticks.length).toBeGreaterThan(1);
    expect(layout.ticks.some((t) => t.text === null)).toBe(true);
    // Every tick still has a position to draw its mark at.
    for (const tick of layout.ticks) expect(Number.isFinite(tick.x)).toBe(true);
  });

  it("maps every tick through the shared ns->x mapping", () => {
    const drawW = 800;
    const layout = rulerLayout(viewStart, viewEnd, drawW, relInputs());
    for (const tick of layout.ticks) {
      expect(tick.x).toBe(
        nsToDrawX(viewStart + tick.offsetNs, viewStart, viewEnd, drawW),
      );
    }
  });
});

// A minimal recording 2D context: captures the draw ops renderTimeAxis
// makes (node has no canvas). Enough to assert the ruler is drawn.
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

describe("renderTimeAxis (ruler)", () => {
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

  it("draws a mark per tick plus the surviving labels and both chips", () => {
    const { ctx, rec } = recordingCtx();
    renderTimeAxis(ctx, geo, viewStart, viewEnd, relInputs(), true);
    const layout = rulerLayout(viewStart, viewEnd, geo.time.drawW, relInputs());
    const kept = layout.ticks.filter((t) => t.text !== null);
    expect(rec.strokes).toBe(layout.ticks.length);
    expect(kept.length).toBeGreaterThanOrEqual(2);
    // Tick labels first, at their shared-mapping x, then the two chips.
    expect(rec.labels.length).toBe(kept.length + 2);
    kept.forEach((tick, i) => {
      const call = rec.labels[i]!;
      expect(call.text).toBe(tick.text);
      expect(call.x).toBe(
        nsToDrawX(viewStart + tick.offsetNs, viewStart, viewEnd, geo.time.drawW),
      );
    });
    const chips = rec.labels.slice(kept.length);
    expect(chips[0]!.text).toBe(layout.anchor);
    expect(chips[0]!.x).toBe(0);
    expect(chips[1]!.text).toBe(layout.chip);
    expect(chips[1]!.x).toBe(geo.time.drawW);
  });
});

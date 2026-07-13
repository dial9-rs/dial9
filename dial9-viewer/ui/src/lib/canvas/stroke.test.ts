// DoD (T08): the stroke batcher emits <= drawW vertices for ARBITRARY
// series (F1: strokes must be pixel-bounded), one path per style, dash
// state hoisted. Random-series property tests + a recording context.

import { describe, it, expect } from "vitest";
import {
  downsampleSeriesToColumns,
  drawStrokeBatches,
  expandSteps,
  makeStrokeBatcher,
} from "./stroke.js";
import type { StrokePathContext, Vertex } from "./stroke.js";

interface Pt {
  t: number;
  v: number;
}

/** Deterministic PRNG so failures reproduce (mulberry32). */
function rng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function randomSeries(rand: () => number, n: number, tMax: number): Pt[] {
  const pts: Pt[] = [];
  for (let i = 0; i < n; i++) {
    pts.push({ t: rand() * tMax, v: rand() * 100 });
  }
  pts.sort((a, b) => a.t - b.t);
  return pts;
}

/** ns->x mapping mirroring panel_layout math over [0, tMax]. */
const mapX = (tMax: number, x0: number, drawW: number) => (p: Pt) =>
  x0 + (p.t / tMax) * drawW;

describe("downsampleSeriesToColumns", () => {
  it("DoD: arbitrary random series emit <= ceil(drawW) vertices", () => {
    const rand = rng(0xd1a19);
    for (let round = 0; round < 50; round++) {
      const drawW = 1 + Math.floor(rand() * 2000);
      const n = Math.floor(rand() * 10_000);
      const tMax = 1 + rand() * 1e9;
      const pts = randomSeries(rand, n, tMax);
      const out = downsampleSeriesToColumns(pts, {
        xOf: mapX(tMax, 0, drawW),
        yOf: (p) => p.v,
        drawW,
      });
      expect(out.length, `round ${round}: n=${n} drawW=${drawW}`)
        .toBeLessThanOrEqual(Math.ceil(drawW));
      // Pixel-bounded even when samples outnumber columns 100:1.
      expect(out.length).toBeLessThanOrEqual(n);
    }
  });

  it("emits vertices in strictly ascending column order, inside the draw area", () => {
    const rand = rng(42);
    const drawW = 300;
    const x0 = 100; // label gutter
    const tMax = 1e6;
    const pts = randomSeries(rand, 5000, tMax);
    const out = downsampleSeriesToColumns(pts, {
      xOf: mapX(tMax, x0, drawW),
      yOf: (p) => p.v,
      x0,
      drawW,
    });
    let prevCol = -1;
    for (const v of out) {
      expect(v.x).toBeGreaterThanOrEqual(x0);
      expect(v.x).toBeLessThanOrEqual(x0 + drawW);
      const col = Math.min(Math.ceil(drawW) - 1, Math.floor(v.x - x0));
      expect(col).toBeGreaterThan(prevCol);
      prevCol = col;
    }
  });

  it("default representative: last point in the column (step carry-out)", () => {
    // Three points in column 0, one in column 5 (drawW 10 over t 0..10).
    const pts: Pt[] = [
      { t: 0.1, v: 1 },
      { t: 0.5, v: 2 },
      { t: 0.9, v: 3 },
      { t: 5.5, v: 4 },
    ];
    const out = downsampleSeriesToColumns(pts, {
      xOf: (p) => p.t,
      yOf: (p) => p.v,
      drawW: 10,
    });
    expect(out.map((v) => v.y)).toEqual([3, 4]);
  });

  it("weightOf representative: largest weight wins (spikes survive)", () => {
    const pts: Pt[] = [
      { t: 0.1, v: 1 },
      { t: 0.5, v: 90 }, // the spike
      { t: 0.9, v: 3 },
    ];
    const out = downsampleSeriesToColumns(pts, {
      xOf: (p) => p.t,
      yOf: (p) => p.v,
      weightOf: (p) => p.v,
      drawW: 10,
    });
    expect(out).toHaveLength(1);
    expect(out[0]!.y).toBe(90);
  });

  it("clamps off-view samples into the edge columns (line keeps entry height)", () => {
    const pts: Pt[] = [
      { t: -50, v: 7 }, // the lowerBound-1 sample before the view
      { t: 5, v: 9 },
      { t: 500, v: 11 }, // beyond the right edge
    ];
    const out = downsampleSeriesToColumns(pts, {
      xOf: (p) => p.t,
      yOf: (p) => p.v,
      drawW: 10,
    });
    expect(out).toHaveLength(3);
    expect(out[0]).toEqual({ x: 0, y: 7 });
    expect(out[2]).toEqual({ x: 10, y: 11 });
  });

  it("empty input / non-positive drawW -> no vertices", () => {
    const opts = { xOf: (p: Pt) => p.t, yOf: (p: Pt) => p.v, drawW: 10 };
    expect(downsampleSeriesToColumns([], opts)).toEqual([]);
    expect(
      downsampleSeriesToColumns([{ t: 1, v: 1 }], { ...opts, drawW: 0 }),
    ).toEqual([]);
    expect(
      downsampleSeriesToColumns([{ t: 1, v: 1 }], { ...opts, drawW: -17 }),
    ).toEqual([]);
  });

  it("throws on unsorted input instead of garbling the line", () => {
    const pts: Pt[] = [
      { t: 9, v: 1 },
      { t: 1, v: 2 },
    ];
    expect(() =>
      downsampleSeriesToColumns(pts, {
        xOf: (p) => p.t,
        yOf: (p) => p.v,
        drawW: 10,
      }),
    ).toThrow(/sorted ascending/);
  });
});

describe("expandSteps", () => {
  it("inserts the horizontal-run knee for gaps >= 2 columns", () => {
    const line: Vertex[] = [
      { x: 0, y: 10 },
      { x: 5, y: 20 },
    ];
    expect(expandSteps(line)).toEqual([
      { x: 0, y: 10 },
      { x: 5, y: 10 }, // carry y=10 to the step edge
      { x: 5, y: 20 },
    ]);
  });

  it("adjacent columns get no knee (sub-pixel step)", () => {
    const line: Vertex[] = [
      { x: 0, y: 10 },
      { x: 1.5, y: 20 },
    ];
    expect(expandSteps(line)).toEqual(line);
  });

  it("stays pixel-bounded: <= 2x the input vertices", () => {
    const rand = rng(7);
    const line: Vertex[] = [];
    let x = 0;
    for (let i = 0; i < 500; i++) {
      x += rand() * 5;
      line.push({ x, y: rand() * 50 });
    }
    expect(expandSteps(line).length).toBeLessThanOrEqual(2 * line.length);
  });
});

describe("makeStrokeBatcher", () => {
  it("DoD: per style, series vertices stay <= ceil(drawW)", () => {
    const rand = rng(0xf1);
    const drawW = 750;
    const tMax = 1e6;
    const b = makeStrokeBatcher();
    // Many series funneled into two styles (e.g. all lanes' queue lines
    // share one style - the F1 batching win).
    for (let lane = 0; lane < 8; lane++) {
      b.series(lane % 2 === 0 ? "queue" : "active", randomSeries(rand, 3000, tMax), {
        xOf: mapX(tMax, 0, drawW),
        yOf: (p) => p.v,
        drawW,
      });
    }
    for (const [styleKey, batch] of b.batches()) {
      for (const pl of batch.polylines) {
        expect(pl.length, styleKey).toBeLessThanOrEqual(Math.ceil(drawW));
      }
    }
  });

  it("ticks dedupe per pixel column: a marker storm costs <= 1 subpath/column", () => {
    const rand = rng(0xabc);
    const b = makeStrokeBatcher();
    const drawW = 100;
    for (let i = 0; i < 10_000; i++) {
      b.tick("open-ended", rand() * drawW, 10, 30);
    }
    const batch = b.batches().get("open-ended")!;
    expect(batch.polylines.length).toBeLessThanOrEqual(drawW + 1);
    // First tick in a column wins; each subpath is one vertical segment.
    for (const pl of batch.polylines) {
      expect(pl).toHaveLength(2);
      expect(pl[0]!.x).toBe(pl[1]!.x);
    }
  });

  it("groups by style key across add order and keeps first-use order", () => {
    const b = makeStrokeBatcher();
    b.tick("a", 1, 0, 1);
    b.tick("b", 2, 0, 1);
    b.tick("a", 3, 0, 1);
    const keys = [...b.batches().keys()];
    expect(keys).toEqual(["a", "b"]);
    expect(b.batches().get("a")!.polylines).toHaveLength(2);
  });

  it("empty series / empty polylines contribute no subpath", () => {
    const b = makeStrokeBatcher();
    const opts = { xOf: (p: Pt) => p.t, yOf: (p: Pt) => p.v, drawW: 100 };
    b.series("queue", [] as Pt[], opts);
    b.stepSeries("queue", [] as Pt[], opts);
    b.polyline("queue", []);
    expect(b.batches().size).toBe(0);
  });

  it("stepSeries: downsamples then step-expands, staying pixel-bounded", () => {
    const b = makeStrokeBatcher();
    const drawW = 20;
    // Sparse regime: 3 samples across 20 columns -> knees appear.
    b.stepSeries(
      "queue",
      [
        { t: 0, v: 10 },
        { t: 10, v: 20 },
        { t: 19, v: 5 },
      ] as Pt[],
      { xOf: (p) => p.t, yOf: (p) => p.v, drawW },
    );
    const [pl] = b.batches().get("queue")!.polylines;
    expect(pl).toEqual([
      { x: 0, y: 10 },
      { x: 10, y: 10 }, // knee carries y=10 to the step edge
      { x: 10, y: 20 },
      { x: 19, y: 20 }, // knee
      { x: 19, y: 5 },
    ]);
    expect(pl!.length).toBeLessThanOrEqual(2 * Math.ceil(drawW));
  });
});

// ── drawStrokeBatches: one stroke per style, hoisted dash state ───────────

interface RecordingCtx extends StrokePathContext {
  calls: string[];
}

function makeRecordingCtx(): RecordingCtx {
  let stroke: string | CanvasGradient | CanvasPattern = "";
  let width = 0;
  const calls: string[] = [];
  return {
    calls,
    get strokeStyle() {
      return stroke;
    },
    set strokeStyle(v) {
      stroke = v;
      calls.push(`strokeStyle=${String(v)}`);
    },
    get lineWidth() {
      return width;
    },
    set lineWidth(v: number) {
      width = v;
      calls.push(`lineWidth=${v}`);
    },
    beginPath: () => calls.push("beginPath"),
    moveTo: (x, y) => calls.push(`moveTo(${x},${y})`),
    lineTo: (x, y) => calls.push(`lineTo(${x},${y})`),
    stroke: () => calls.push("stroke"),
    setLineDash: (segs) => calls.push(`setLineDash([${segs.join(",")}])`),
  };
}

describe("drawStrokeBatches", () => {
  const styles = {
    queue: { strokeStyle: "rgba(255,200,50,0.8)", lineWidth: 1.5 },
    "open-ended": {
      strokeStyle: "#ff9800",
      lineWidth: 2,
      lineDash: [3, 2] as const,
    },
  };
  const styleOf = (k: string) => styles[k as keyof typeof styles];

  it("one beginPath + one stroke per style, regardless of subpath count", () => {
    const b = makeStrokeBatcher();
    for (let x = 0; x < 40; x += 2) b.tick("open-ended", x, 10, 30);
    b.polyline("queue", [
      { x: 0, y: 1 },
      { x: 5, y: 2 },
      { x: 9, y: 1 },
    ]);
    const ctx = makeRecordingCtx();
    drawStrokeBatches(ctx, b.batches(), styleOf);

    expect(ctx.calls.filter((c) => c === "stroke")).toHaveLength(2);
    expect(ctx.calls.filter((c) => c === "beginPath")).toHaveLength(2);
  });

  it("F1: dash state is hoisted - one setLineDash per dashed style, not per marker", () => {
    const b = makeStrokeBatcher();
    for (let x = 0; x < 100; x += 1) b.tick("open-ended", x, 10, 30);
    const ctx = makeRecordingCtx();
    drawStrokeBatches(ctx, b.batches(), styleOf);

    const dashCalls = ctx.calls.filter((c) => c.startsWith("setLineDash"));
    // One to arm the pattern, one trailing reset to solid.
    expect(dashCalls).toEqual(["setLineDash([3,2])", "setLineDash([])"]);
  });

  it("leaves the context solid even when the last style was dashed", () => {
    const b = makeStrokeBatcher();
    b.polyline("queue", [
      { x: 0, y: 0 },
      { x: 1, y: 1 },
    ]);
    b.tick("open-ended", 5, 0, 10);
    const ctx = makeRecordingCtx();
    drawStrokeBatches(ctx, b.batches(), styleOf);
    expect(ctx.calls.at(-1)).toBe("setLineDash([])");
  });

  it("paths subpaths as moveTo + lineTo runs", () => {
    const b = makeStrokeBatcher();
    b.polyline("queue", [
      { x: 0, y: 1 },
      { x: 5, y: 2 },
    ]);
    b.polyline("queue", [
      { x: 8, y: 3 },
      { x: 9, y: 4 },
    ]);
    const ctx = makeRecordingCtx();
    drawStrokeBatches(ctx, b.batches(), styleOf);
    expect(ctx.calls).toEqual([
      "strokeStyle=rgba(255,200,50,0.8)",
      "lineWidth=1.5",
      "beginPath",
      "moveTo(0,1)",
      "lineTo(5,2)",
      "moveTo(8,3)",
      "lineTo(9,4)",
      "stroke",
    ]);
  });

  it("defaults lineWidth to 1 and skips short subpaths inside a drawable batch", () => {
    const b = makeStrokeBatcher();
    b.polyline("plain", [{ x: 3, y: 3 }]); // too short: skipped in-path
    b.polyline("plain", [
      { x: 0, y: 0 },
      { x: 4, y: 4 },
    ]);
    const ctx = makeRecordingCtx();
    drawStrokeBatches(ctx, b.batches(), () => ({ strokeStyle: "#fff" }));
    expect(ctx.calls).toEqual([
      "strokeStyle=#fff",
      "lineWidth=1",
      "beginPath",
      "moveTo(0,0)",
      "lineTo(4,4)",
      "stroke",
    ]);
  });

  it("skips styles with nothing drawable (no state churn, no empty stroke)", () => {
    const b = makeStrokeBatcher();
    b.polyline("queue", [{ x: 0, y: 1 }]); // single vertex: not strokable
    const ctx = makeRecordingCtx();
    drawStrokeBatches(ctx, b.batches(), styleOf);
    expect(ctx.calls).toEqual([]);
  });
});

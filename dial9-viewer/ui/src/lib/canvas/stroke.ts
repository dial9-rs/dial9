// Pixel-bounded, style-batched stroke building. Stroked primitives
// otherwise bypass the pixel-downsampling discipline and dominate CPU
// under pan storms. The rule this module names: every stroke is (a)
// pixel-bounded - at most one vertex per pixel column, downsampled BEFORE
// pathing - and (b) batched - one beginPath/stroke per STYLE, not per
// primitive, with dash state hoisted to the style switch.
//
// Everything here is pure data -> data; drawStrokeBatches applies a
// finished batch set to a context typed against a minimal structural
// surface (StrokePathContext), so the whole pipeline is Node-testable.

/** One path vertex in CSS px. */
export interface Vertex {
  x: number;
  y: number;
}

export interface ColumnSeriesOpts<P> {
  /** Point -> x in CSS px (e.g. layout.nsToPanelX(sample.t)). */
  xOf: (p: P) => number;
  /** Point -> y in CSS px. */
  yOf: (p: P) => number;
  /**
   * Column representative: among the points landing in one pixel column,
   * the one with the LARGEST weight wins (mirrors the frozen core's
   * pixelDownsampleSpans contract - e.g. weight by queue depth so spikes
   * survive downsampling). Default: the last point in the column wins
   * (step semantics - the value that carries into the next column).
   */
  weightOf?: (p: P) => number;
  /** Left edge of the draw area in CSS px. Default 0. */
  x0?: number;
  /** Draw-area width in CSS px. */
  drawW: number;
}

/**
 * Downsample a series to at most one vertex per pixel column. Returns
 * vertices in ascending column order, at most ceil(drawW) of them,
 * regardless of how many input points are visible.
 *
 * `points` MUST be sorted ascending by xOf (same contract as the frozen
 * core's pixelDownsampleSpans); out-of-order input throws rather than
 * silently producing a garbled line. X values outside [x0, x0 + drawW]
 * are clamped, so the samples just outside the view collapse into the edge
 * columns and the line keeps its entry/exit height.
 */
export function downsampleSeriesToColumns<P>(
  points: readonly P[],
  opts: ColumnSeriesOpts<P>,
): Vertex[] {
  const { xOf, yOf, weightOf, drawW } = opts;
  const x0 = opts.x0 ?? 0;
  const nCols = Math.ceil(drawW);
  if (nCols <= 0 || points.length === 0) return [];

  const out: Vertex[] = [];
  let col = -1;
  let repX = 0;
  let repY = 0;
  let repWeight = -Infinity;
  let hasRep = false;

  const flush = () => {
    if (hasRep) out.push({ x: repX, y: repY });
  };

  for (const p of points) {
    const xc = Math.min(Math.max(xOf(p), x0), x0 + drawW);
    const c = Math.min(nCols - 1, Math.floor(xc - x0));
    if (c < col) {
      throw new RangeError(
        "downsampleSeriesToColumns: points must be sorted ascending by xOf",
      );
    }
    if (c > col) {
      flush();
      col = c;
      repWeight = -Infinity;
      hasRep = false;
    }
    if (weightOf === undefined) {
      // Last point in the column wins.
      repX = xc;
      repY = yOf(p);
      hasRep = true;
    } else {
      const w = weightOf(p);
      if (w > repWeight) {
        repWeight = w;
        repX = xc;
        repY = yOf(p);
        hasRep = true;
      }
    }
  }
  flush();
  return out;
}

/**
 * Expand column vertices into a step-function polyline: between vertices
 * whose columns are >= 2 apart, insert the horizontal-run knee
 * (nextX, prevY) so sparse (zoomed-in) series keep their step shape.
 * Adjacent-column vertices get no knee - the step would be sub-pixel.
 *
 * Bound: at most one knee per vertex, so <= 2 * ceil(drawW) vertices -
 * still O(width), never O(samples).
 */
export function expandSteps(vertices: readonly Vertex[]): Vertex[] {
  const out: Vertex[] = [];
  let prev: Vertex | null = null;
  for (const v of vertices) {
    if (prev !== null && Math.floor(v.x) - Math.floor(prev.x) >= 2) {
      out.push({ x: v.x, y: prev.y });
    }
    out.push(v);
    prev = v;
  }
  return out;
}

/** One style's accumulated stroke work: subpaths for a single stroke(). */
export interface StrokeBatch {
  /**
   * Ordered subpaths; each is drawn moveTo(first) + lineTo(rest), all
   * inside ONE beginPath/stroke pair. Subpaths shorter than 2 vertices
   * are skipped at draw time.
   */
  polylines: Vertex[][];
}

export interface StrokeBatcher {
  /**
   * Add a series line: downsampled to one vertex per pixel column
   * (downsampleSeriesToColumns), then appended as one subpath of
   * `styleKey`'s batch.
   */
  series<P>(styleKey: string, points: readonly P[], opts: ColumnSeriesOpts<P>): void;
  /** series() + expandSteps(): a pixel-bounded step line. */
  stepSeries<P>(
    styleKey: string,
    points: readonly P[],
    opts: ColumnSeriesOpts<P>,
  ): void;
  /**
   * Add a pre-built subpath verbatim. Escape hatch for shapes that are
   * already pixel-bounded by construction; the caller owns that bound.
   */
  polyline(styleKey: string, vertices: readonly Vertex[]): void;
  /**
   * Add a vertical marker (the open-ended-poll / gap-edge case). Markers
   * of one style are deduplicated per pixel column (first wins), so a
   * million markers cost at most one 2-vertex subpath per column - and
   * the whole style strokes once, with its dash pattern set once,
   * instead of a setLineDash + stroke per marker.
   */
  tick(styleKey: string, x: number, yTop: number, yBottom: number): void;
  /** The accumulated per-style batches, in first-use order. */
  batches(): ReadonlyMap<string, StrokeBatch>;
}

export function makeStrokeBatcher(): StrokeBatcher {
  const byStyle = new Map<
    string,
    { batch: StrokeBatch; tickCols: Set<number> }
  >();

  const entry = (styleKey: string) => {
    let e = byStyle.get(styleKey);
    if (e === undefined) {
      e = { batch: { polylines: [] }, tickCols: new Set() };
      byStyle.set(styleKey, e);
    }
    return e;
  };

  return {
    series(styleKey, points, opts) {
      const vertices = downsampleSeriesToColumns(points, opts);
      if (vertices.length > 0) entry(styleKey).batch.polylines.push(vertices);
    },
    stepSeries(styleKey, points, opts) {
      const vertices = expandSteps(downsampleSeriesToColumns(points, opts));
      if (vertices.length > 0) entry(styleKey).batch.polylines.push(vertices);
    },
    polyline(styleKey, vertices) {
      if (vertices.length > 0) {
        entry(styleKey).batch.polylines.push([...vertices]);
      }
    },
    tick(styleKey, x, yTop, yBottom) {
      const e = entry(styleKey);
      const col = Math.floor(x);
      if (e.tickCols.has(col)) return;
      e.tickCols.add(col);
      e.batch.polylines.push([
        { x, y: yTop },
        { x, y: yBottom },
      ]);
    },
    batches() {
      const out = new Map<string, StrokeBatch>();
      for (const [k, e] of byStyle) out.set(k, e.batch);
      return out;
    },
  };
}

/** Canvas stroke state for one style key, applied once per batch. */
export interface StrokeStyleSpec {
  strokeStyle: string;
  /** Default 1. */
  lineWidth?: number;
  /** Dash pattern; omitted/empty = solid. Set ONCE per style (hoisted). */
  lineDash?: readonly number[];
}

/** Structural subset of CanvasRenderingContext2D used by the drawer. */
export interface StrokePathContext {
  strokeStyle: string | CanvasGradient | CanvasPattern;
  lineWidth: number;
  lineJoin: CanvasLineJoin;
  beginPath(): void;
  moveTo(x: number, y: number): void;
  lineTo(x: number, y: number): void;
  stroke(): void;
  setLineDash(segments: number[]): void;
}

/**
 * Apply finished batches to a context: per style, ONE style switch (dash
 * state included) + ONE beginPath + ONE stroke, however many subpaths the
 * batch holds. Leaves the context's dash state solid on return.
 */
export function drawStrokeBatches(
  ctx: StrokePathContext,
  batches: ReadonlyMap<string, StrokeBatch>,
  styleOf: (styleKey: string) => StrokeStyleSpec,
): void {
  let dashActive = false;
  // Round joins: a sharp downsampled spike (a diagonal V between adjacent
  // columns) miters its tip several px past the vertex - below the 0 baseline /
  // above the max. Scoped + restored so it never leaks to the caller's own
  // sharp-cornered strokes (e.g. the lanes' selection-outline rects).
  const prevJoin = ctx.lineJoin;
  ctx.lineJoin = "round";
  for (const [styleKey, batch] of batches) {
    let drawable = false;
    for (const pl of batch.polylines) {
      if (pl.length >= 2) {
        drawable = true;
        break;
      }
    }
    if (!drawable) continue;

    const spec = styleOf(styleKey);
    ctx.strokeStyle = spec.strokeStyle;
    ctx.lineWidth = spec.lineWidth ?? 1;
    const dash = spec.lineDash ?? [];
    if (dash.length > 0) {
      ctx.setLineDash(dash.slice());
      dashActive = true;
    } else if (dashActive) {
      ctx.setLineDash([]);
      dashActive = false;
    }

    ctx.beginPath();
    for (const pl of batch.polylines) {
      if (pl.length < 2) continue;
      const first = pl[0]!;
      ctx.moveTo(first.x, first.y);
      for (let i = 1; i < pl.length; i++) {
        const v = pl[i]!;
        ctx.lineTo(v.x, v.y);
      }
    }
    ctx.stroke();
  }
  if (dashActive) ctx.setLineDash([]);
  ctx.lineJoin = prevJoin;
}

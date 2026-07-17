// Columnar store for SpanData.allSpans (the reconstructed tracing spans).
// On a heavily-instrumented trace this is ~1M TracingSpan objects (~1 GB): each
// a fat object + a segments array (~2.7 {start,end,workerId} objects/span) + a
// per-span fields object + a unique spanId string. As typed-array columns +
// CSRs it drops to ~0.2 GB, resident and off the JS object heap.
//
// Research (3 agents) established: NO consumer compares spans by identity/=== or
// uses a span as a WeakMap key - all cross-links are by the spanId STRING. So a
// FRESH flyweight per .at(row) is safe (several sites hold a span ref across an
// intervening scan, so a single reused cursor is NOT safe - fresh is). Parent
// links are stored as the parent's ROW index (integer parent-pointer walks).
// `fields` is always consumed as a whole map (Object.entries) - no single-key
// hot path - so lazy whole-map materialization from the interned CSR is fine.

import type { DecodedFieldValue, TracingSpan } from "./index.js";

type Seg = { start: number; end: number; workerId: number };
type Fields = Record<string, DecodedFieldValue>;

/** A span materialized back out of the columns (fresh object; safe to retain
 * and read across scans). Structurally the frozen TracingSpan. */
export interface SpanView {
  start: number;
  end: number;
  spanId: string;
  spanName: string;
  fields: Fields;
  parentSpanId: string | null;
  segments: Seg[];
  activeNs: number;
  taskId: number | null;
  depth: number;
}

export class ColumnarSpans {
  // Scalar columns, row-parallel, sorted ascending by start.
  start: Float64Array;
  end: Float64Array;
  activeNs: Float64Array;
  depth: Uint16Array;
  /** NaN encodes null taskId. */
  taskIdRaw: Float64Array;
  /** index into spanNames. */
  spanNameId: Int32Array;
  /** row index of the parent span, or -1 for a root / unresolved parent. */
  parentRow: Int32Array;

  /** row -> spanId string (ids are ~unique, so one string per row). */
  readonly spanIds: string[];
  /** spanId -> row (external lookups: highlight, inspector, url, search). */
  readonly spanIdToRow: Map<string, number>;
  readonly spanNames: string[];

  // segments CSR: row r owns segStart/segEnd/segWorker[segOff[r], segOff[r+1]).
  segOff: Int32Array;
  segStart: Float64Array;
  segEnd: Float64Array;
  segWorker: Int32Array;

  // fields CSR (interned): row r owns fieldKeyId/fieldValId[fieldOff[r],
  // fieldOff[r+1]); valId 0 = null else fieldVals[valId-1].
  fieldOff: Int32Array;
  fieldKeyId: Int32Array;
  fieldValId: Int32Array;
  readonly fieldKeys: string[];
  readonly fieldVals: DecodedFieldValue[];

  readonly n: number;

  constructor(cols: {
    start: Float64Array; end: Float64Array; activeNs: Float64Array;
    depth: Uint16Array; taskIdRaw: Float64Array; spanNameId: Int32Array; parentRow: Int32Array;
    spanIds: string[]; spanIdToRow: Map<string, number>; spanNames: string[];
    segOff: Int32Array; segStart: Float64Array; segEnd: Float64Array; segWorker: Int32Array;
    fieldOff: Int32Array; fieldKeyId: Int32Array; fieldValId: Int32Array;
    fieldKeys: string[]; fieldVals: DecodedFieldValue[];
  }) {
    this.start = cols.start; this.end = cols.end; this.activeNs = cols.activeNs;
    this.depth = cols.depth; this.taskIdRaw = cols.taskIdRaw; this.spanNameId = cols.spanNameId;
    this.parentRow = cols.parentRow;
    this.spanIds = cols.spanIds; this.spanIdToRow = cols.spanIdToRow; this.spanNames = cols.spanNames;
    this.segOff = cols.segOff; this.segStart = cols.segStart; this.segEnd = cols.segEnd; this.segWorker = cols.segWorker;
    this.fieldOff = cols.fieldOff; this.fieldKeyId = cols.fieldKeyId; this.fieldValId = cols.fieldValId;
    this.fieldKeys = cols.fieldKeys; this.fieldVals = cols.fieldVals;
    this.n = cols.start.length;
  }

  get length(): number {
    return this.n;
  }

  // ── cheap scalar reads (no materialization) - for the render window scan ──
  startAt(r: number): number { return this.start[r]; }
  endAt(r: number): number { return this.end[r]; }
  spanIdAt(r: number): string { return this.spanIds[r]; }
  spanNameAt(r: number): string { return this.spanNames[this.spanNameId[r]]; }
  depthAt(r: number): number { return this.depth[r]; }
  taskIdAt(r: number): number | null { const t = this.taskIdRaw[r]; return Number.isNaN(t) ? null : t; }
  activeNsAt(r: number): number { return this.activeNs[r]; }
  parentSpanIdAt(r: number): string | null { const p = this.parentRow[r]; return p < 0 ? null : this.spanIds[p]; }

  /** Fresh {start,end,workerId}[] for row r (bounded ~few per span). */
  segmentsAt(r: number): Seg[] {
    const lo = this.segOff[r], hi = this.segOff[r + 1];
    const out = new Array<Seg>(hi - lo);
    for (let j = lo; j < hi; j++) out[j - lo] = { start: this.segStart[j], end: this.segEnd[j], workerId: this.segWorker[j] };
    return out;
  }

  /** Fresh fields map for row r, rebuilt from the interned CSR. */
  fieldsAt(r: number): Fields {
    const lo = this.fieldOff[r], hi = this.fieldOff[r + 1];
    const out: Fields = {};
    for (let j = lo; j < hi; j++) {
      const id = this.fieldValId[j];
      out[this.fieldKeys[this.fieldKeyId[j]]] = id === 0 ? null : this.fieldVals[id - 1];
    }
    return out;
  }

  /** Materialize row r as a fresh TracingSpan-shaped view (safe to retain). */
  at(r: number): SpanView {
    return {
      start: this.start[r], end: this.end[r],
      spanId: this.spanIds[r], spanName: this.spanNames[this.spanNameId[r]],
      fields: this.fieldsAt(r),
      parentSpanId: this.parentSpanIdAt(r),
      segments: this.segmentsAt(r),
      activeNs: this.activeNs[r],
      taskId: this.taskIdAt(r),
      depth: this.depth[r],
    };
  }

  /** All rows for a spanId (recycled ids can repeat; ~always one). Built lazily
   * only if a consumer needs the multimap; single-row lookups use spanIdToRow. */
  private _rowsById: Map<string, number[]> | null = null;
  rowsForSpanId(spanId: string): number[] {
    if (this._rowsById === null) {
      const m = new Map<string, number[]>();
      for (let r = 0; r < this.n; r++) {
        const id = this.spanIds[r];
        let arr = m.get(id);
        if (!arr) { arr = []; m.set(id, arr); }
        arr.push(r);
      }
      this._rowsById = m;
    }
    return this._rowsById.get(spanId) ?? [];
  }
}

/**
 * Accumulates spans (in final row order = sorted-by-start) into growable columns,
 * then finish()es into a ColumnarSpans. parentRow is supplied resolved (the
 * caller assigns rows first, then resolves parents via the same id->row map).
 */
export class ColumnarSpansBuilder {
  // Close-order columns (streaming: a span is pushed the moment it closes, so
  // the 982K fat SpanRec objects never accumulate). finish() sorts by start.
  private start: number[] = []; private end: number[] = []; private activeNs: number[] = [];
  private taskIdRaw: number[] = []; private spanNameId: number[] = [];
  private spanIds: string[] = []; private parentIds: (string | null)[] = [];
  private spanNames: string[] = []; private nameIntern = new Map<string, number>();

  private segOff: number[] = [0]; private segStart: number[] = []; private segEnd: number[] = []; private segWorker: number[] = [];

  private fieldOff: number[] = [0]; private fieldKeyId: number[] = []; private fieldValId: number[] = [];
  private fieldKeys: string[] = []; private keyIntern = new Map<string, number>();
  private fieldVals: DecodedFieldValue[] = []; private valIntern = new Map<unknown, number>();

  private internName(s: string): number {
    let i = this.nameIntern.get(s);
    if (i === undefined) { i = this.spanNames.length; this.spanNames.push(s); this.nameIntern.set(s, i); }
    return i;
  }
  private internKey(k: string): number {
    let i = this.keyIntern.get(k);
    if (i === undefined) { i = this.fieldKeys.length; this.fieldKeys.push(k); this.keyIntern.set(k, i); }
    return i;
  }
  private internVal(v: DecodedFieldValue): number {
    if (v == null) return 0;
    if (typeof v === "object") { this.fieldVals.push(v); return this.fieldVals.length; }
    let id = this.valIntern.get(v);
    if (id === undefined) { this.fieldVals.push(v); id = this.fieldVals.length; this.valIntern.set(v, id); }
    return id;
  }

  /** Push a closed span in close order. parentSpanId (string) is resolved to a
   * row in finish() once all rows exist. */
  push(
    start: number, end: number, spanId: string, spanName: string, parentSpanId: string | null,
    taskId: number | null, activeNs: number, segments: readonly Seg[], fields: Fields
  ): void {
    this.start.push(start); this.end.push(end); this.activeNs.push(activeNs);
    this.taskIdRaw.push(taskId == null ? NaN : taskId);
    this.spanNameId.push(this.internName(spanName));
    this.spanIds.push(spanId); this.parentIds.push(parentSpanId);
    for (const s of segments) { this.segStart.push(s.start); this.segEnd.push(s.end); this.segWorker.push(s.workerId); }
    this.segOff.push(this.segStart.length);
    for (const k in fields) { this.fieldKeyId.push(this.internKey(k)); this.fieldValId.push(this.internVal(fields[k])); }
    this.fieldOff.push(this.fieldKeyId.length);
  }

  /**
   * Sort by start (stable, close-order tiebreak - byte-identical to the fat
   * allSpans.sort), reorder every column + CSR into row order, resolve parentRow
   * via spanIdToRow, and compute depth. `parentBySpanId` (every entered span's
   * parent, incl still-open ones) drives the depth walk, matching frozen getDepth.
   */
  finish(parentBySpanId: Map<string, string | null>): { store: ColumnarSpans; maxDepth: number } {
    const N = this.start.length;
    const cStart = this.start;
    const perm = Array.from({ length: N }, (_, i) => i);
    perm.sort((a, b) => cStart[a] - cStart[b] || a - b);

    const spanIdToRow = new Map<string, number>();
    for (let r = 0; r < N; r++) spanIdToRow.set(this.spanIds[perm[r]], r);

    const start = new Float64Array(N), end = new Float64Array(N), activeNs = new Float64Array(N);
    const taskIdRaw = new Float64Array(N), spanNameId = new Int32Array(N), parentRow = new Int32Array(N);
    const spanIds = new Array<string>(N);
    for (let r = 0; r < N; r++) {
      const o = perm[r];
      start[r] = this.start[o]; end[r] = this.end[o]; activeNs[r] = this.activeNs[o];
      taskIdRaw[r] = this.taskIdRaw[o]; spanNameId[r] = this.spanNameId[o];
      spanIds[r] = this.spanIds[o];
      const pid = this.parentIds[o];
      parentRow[r] = pid != null ? (spanIdToRow.get(pid) ?? -1) : -1;
    }

    const depth = new Uint16Array(N);
    const depthCache = new Map<string, number>();
    const getDepth = (spanId: string | null, seen?: Set<string>): number => {
      if (spanId == null) return -1;
      const c = depthCache.get(spanId);
      if (c !== undefined) return c;
      if (seen && seen.has(spanId)) { depthCache.set(spanId, 0); return 0; }
      if (!parentBySpanId.has(spanId)) { depthCache.set(spanId, 0); return 0; }
      const v = seen || new Set<string>(); v.add(spanId);
      const d = getDepth(parentBySpanId.get(spanId) ?? null, v) + 1;
      depthCache.set(spanId, d); return d;
    };
    let maxDepth = 0;
    for (let r = 0; r < N; r++) { const d = getDepth(spanIds[r]); depth[r] = d; if (d > maxDepth) maxDepth = d; }

    // Reorder the segments CSR.
    const segOff = new Int32Array(N + 1);
    for (let r = 0; r < N; r++) { const o = perm[r]; segOff[r + 1] = segOff[r] + (this.segOff[o + 1] - this.segOff[o]); }
    const segStart = new Float64Array(segOff[N]), segEnd = new Float64Array(segOff[N]), segWorker = new Int32Array(segOff[N]);
    for (let r = 0; r < N; r++) {
      const o = perm[r]; let w = segOff[r];
      for (let j = this.segOff[o]; j < this.segOff[o + 1]; j++) { segStart[w] = this.segStart[j]; segEnd[w] = this.segEnd[j]; segWorker[w] = this.segWorker[j]; w++; }
    }

    // Reorder the fields CSR.
    const fieldOff = new Int32Array(N + 1);
    for (let r = 0; r < N; r++) { const o = perm[r]; fieldOff[r + 1] = fieldOff[r] + (this.fieldOff[o + 1] - this.fieldOff[o]); }
    const fieldKeyId = new Int32Array(fieldOff[N]), fieldValId = new Int32Array(fieldOff[N]);
    for (let r = 0; r < N; r++) {
      const o = perm[r]; let w = fieldOff[r];
      for (let j = this.fieldOff[o]; j < this.fieldOff[o + 1]; j++) { fieldKeyId[w] = this.fieldKeyId[j]; fieldValId[w] = this.fieldValId[j]; w++; }
    }

    const store = new ColumnarSpans({
      start, end, activeNs, depth, taskIdRaw, spanNameId, parentRow,
      spanIds, spanIdToRow, spanNames: this.spanNames,
      segOff, segStart, segEnd, segWorker,
      fieldOff, fieldKeyId, fieldValId, fieldKeys: this.fieldKeys, fieldVals: this.fieldVals,
    });
    return { store, maxDepth };
  }
}

/** True when `x` is the columnar store (dispatch guard for consumers). */
export function isColumnarSpans(x: unknown): x is ColumnarSpans {
  return x instanceof ColumnarSpans;
}

/** The Map-subset the spanByIdSingle / spansById consumers actually use, so a
 * columnar-backed adapter drops in for the fat `Map<string, ...>` unchanged. */
export interface SpanByIdSingle {
  has(spanId: string): boolean;
  get(spanId: string): SpanView | undefined;
  values(): IterableIterator<SpanView>;
}
export interface SpanByIdMulti {
  get(spanId: string): SpanView[] | undefined;
}

/** Lazy Map<spanId, SpanView> over the store: materializes a FRESH view per
 * get()/iteration (never retains 982K). Drop-in for url-selection.has,
 * inspector.get, search.values, spanAncestryAt's byId walk. */
export class LazySpanByIdSingle implements SpanByIdSingle {
  constructor(private readonly s: ColumnarSpans) {}
  has(spanId: string): boolean {
    return this.s.spanIdToRow.has(spanId);
  }
  get(spanId: string): SpanView | undefined {
    const r = this.s.spanIdToRow.get(spanId);
    return r === undefined ? undefined : this.s.at(r);
  }
  *values(): IterableIterator<SpanView> {
    for (let r = 0; r < this.s.length; r++) yield this.s.at(r);
  }
}

/** Lazy Map<spanId, SpanView[]> over the store (recycled ids -> multiple rows).
 * Drop-in for the render-highlight `spansById.get(id)`. */
export class LazySpansById implements SpanByIdMulti {
  constructor(private readonly s: ColumnarSpans) {}
  get(spanId: string): SpanView[] | undefined {
    const rows = this.s.rowsForSpanId(spanId);
    return rows.length ? rows.map((r) => this.s.at(r)) : undefined;
  }
}

/** Re-export for callers threading the view type. */
export type { TracingSpan };

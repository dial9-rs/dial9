// Columnar store for span-producing custom events (SpanEnter / SpanExit /
// SpanClose and annotated single-event spans), which dominate a heavily-instrumented
// trace's customEvents (millions of fat objects at ~240 B each). They are the
// ONLY custom events buildSpanData consumes, and it reads them read-only (name
// -> kind, timestamp, and a few base fields) without retaining the event object
// - so unlike the non-span custom events (which are used as WeakMap keys /
// pinned refs and must stay fat), span events can live as typed-array columns.
//
// The parser routes span events here via the `spanEventSink` parse option
// (mirroring buildSpanData's field extraction, so old traces round-trip
// identically); non-span custom events stay in the fat `customEvents` array.
// A columnar reimplementation of buildSpanData reads these columns by index.

import type { DecodedFieldValue } from "../../../decode.js";

/** Span-event kind, replacing the frozen buildSpanData name classification. */
export const SPAN_KIND = { Enter: 0, Exit: 1, Close: 2, Complete: 3 } as const;

/** Classify a custom-event name as a span event kind, or null if not one.
 * Mirrors buildSpanData's name matching (trace_analysis.js) EXACTLY. */
export function spanKindOf(name: string): number | null {
  if (
    name.startsWith("SpanEnter:") ||
    name.startsWith("SpanEnter__") ||
    name === "SpanEnterEvent"
  ) {
    return SPAN_KIND.Enter;
  }
  if (
    name.startsWith("SpanExit:") ||
    name.startsWith("SpanExit__") ||
    name === "SpanExitEvent"
  ) {
    return SPAN_KIND.Exit;
  }
  if (name.startsWith("SpanClose__") || name === "SpanCloseEvent") {
    return SPAN_KIND.Close;
  }
  return null;
}

// buildSpanData's base fields, excluded from a span's copied `fields`.
//
// Tested with `===` chains rather than Set.has: this runs once per FIELD of
// every span event (~4.7 fields x 175k events on the demo trace alone, and span
// events are the majority of a span-heavy trace), and the keys are string
// literals, so V8 compares internalized strings by pointer. A Set.has is a hash
// of the key first. `parent_span_id` leads because Enter events - the common
// kind - carry it.
function isBaseEnterField(k: string): boolean {
  return (
    k === "span_id" ||
    k === "worker_id" ||
    k === "parent_span_id" ||
    k === "span_name" ||
    k === "span_instance_id" ||
    k === "tid"
  );
}
function isBaseExitField(k: string): boolean {
  return (
    k === "span_id" ||
    k === "worker_id" ||
    k === "span_name" ||
    k === "span_instance_id" ||
    k === "tid"
  );
}
interface SingleEventSpanProjection {
  start: number;
  end: number;
  name: string;
  spanType: string;
  threadId: number | null;
  taskId: number | null;
  workerId: number | null;
  fields: Record<string, DecodedFieldValue>;
  units: Record<string, string> | null;
}

const INITIAL_CAP = 1 << 16;

export class ColumnarSpanEvents {
  kind: Uint8Array;
  ts: Float64Array;
  /** Number(fields.worker_id); NaN when absent (-> stays NaN, Number.isFinite
   * guards downstream, matching the fat Number(undefined) === NaN path). */
  workerId: Float64Array;
  /** index into `strings` (interned span_id); -1 = absent (-> "undefined"). */
  spanIdIdx: Int32Array;
  /** index into `strings` (interned parent_span_id); -1 = null. */
  parentIdx: Int32Array;
  /** index into `spanNames` (interned span_name); -1 = absent (-> "unknown"). */
  spanNameIdx: Int32Array;
  /** Dense event -> complete-event slot. -1 for tracing events. */
  completeIdx: Int32Array;
  /** Single-event-only lifecycle/context columns, allocated sparsely. */
  private completeStart: Float64Array;
  private completeThreadId: Float64Array;
  private completeTaskId: Float64Array;
  private completeWorkerId: Float64Array;
  private completeTypeIdx: Int32Array;
  private completeLen = 0;
  private completeCap = 1024;

  /** Interned span_id / parent_span_id strings (span_ids are ~unique so this is
   * roughly one string per span, exact-value, no f64 u64 precision loss). */
  strings: string[] = [];
  private strIntern = new Map<string, number>();
  spanNames: string[] = [];
  private nameIntern = new Map<string, number>();
  spanTypes: string[] = [];
  private typeIntern = new Map<string, number>();

  /** Non-base span fields (e.g. request_id, metric_name) as an INTERNED CSR:
   * event i owns extraKeyId/extraValId[extraOff[i], extraOff[i+1]). On a heavily
   * instrumented trace these fields recur on millions of span events; as a
   * per-event object Map they were the dominant object-heap cost (~8.5M objects
   * on the 13.7M trace). As interned typed columns the objects vanish - keys and
   * values (request_id/metric_name repeat heavily) intern to small pools. */
  extraOff: Int32Array;
  extraKeyId: Int32Array;
  extraValId: Int32Array;
  extraUnitId: Int32Array;
  extraKeys: string[] = [];
  private extraKeyIntern = new Map<string, number>();
  /** valId 0 = null; else extraVals[valId-1]. Primitives interned by value;
   * non-primitive values (arrays/objects) are boxed (no dedup, rare). */
  extraVals: DecodedFieldValue[] = [];
  private extraValIntern = new Map<unknown, number>();
  extraUnits: string[] = [];
  private extraUnitIntern = new Map<string, number>();
  private extraLen = 0;
  private _extraCap: number;

  private _len = 0;
  private _cap: number;
  /** Lazily-built permutation of indices sorted ascending by ts (stable), the
   * order buildSpanData needs (it sorts customEvents by timestamp). Span events
   * arrive in wire order, ~ts-sorted but not exactly, so a sort index is
   * required; stable tiebreak on index preserves the frozen sort's equal-ts
   * order. */
  private _tsIndex: Int32Array | null = null;

  constructor(cap = INITIAL_CAP) {
    this._cap = cap;
    this.kind = new Uint8Array(cap);
    this.ts = new Float64Array(cap);
    this.workerId = new Float64Array(cap);
    this.spanIdIdx = new Int32Array(cap);
    this.parentIdx = new Int32Array(cap);
    this.spanNameIdx = new Int32Array(cap);
    this.completeIdx = new Int32Array(cap);
    this.completeIdx.fill(-1);
    this.completeStart = new Float64Array(this.completeCap);
    this.completeThreadId = new Float64Array(this.completeCap);
    this.completeTaskId = new Float64Array(this.completeCap);
    this.completeWorkerId = new Float64Array(this.completeCap);
    this.completeTypeIdx = new Int32Array(this.completeCap);
    this.extraOff = new Int32Array(cap + 1);
    this._extraCap = cap;
    this.extraKeyId = new Int32Array(cap);
    this.extraValId = new Int32Array(cap);
    this.extraUnitId = new Int32Array(cap);
    this.extraUnitId.fill(-1);
  }

  private internKey(k: string): number {
    let i = this.extraKeyIntern.get(k);
    if (i === undefined) {
      i = this.extraKeys.length;
      this.extraKeys.push(k);
      this.extraKeyIntern.set(k, i);
    }
    return i;
  }
  private internVal(v: DecodedFieldValue): number {
    if (v == null) return 0;
    if (typeof v === "object") {
      // Arrays/records/bytes can't be value-interned; box (rare on span fields).
      this.extraVals.push(v);
      return this.extraVals.length;
    }
    let id = this.extraValIntern.get(v);
    if (id === undefined) {
      this.extraVals.push(v);
      id = this.extraVals.length;
      this.extraValIntern.set(v, id);
    }
    return id;
  }
  private growExtra(): void {
    const n = this._extraCap * 2;
    const gk = new Int32Array(n); gk.set(this.extraKeyId); this.extraKeyId = gk;
    const gv = new Int32Array(n); gv.set(this.extraValId); this.extraValId = gv;
    const gu = new Int32Array(n); gu.fill(-1); gu.set(this.extraUnitId); this.extraUnitId = gu;
    this._extraCap = n;
  }

  private growComplete(): void {
    const n = this.completeCap * 2;
    const start = new Float64Array(n);
    start.set(this.completeStart);
    this.completeStart = start;
    const threadId = new Float64Array(n);
    threadId.set(this.completeThreadId);
    this.completeThreadId = threadId;
    const taskId = new Float64Array(n);
    taskId.set(this.completeTaskId);
    this.completeTaskId = taskId;
    const workerId = new Float64Array(n);
    workerId.set(this.completeWorkerId);
    this.completeWorkerId = workerId;
    const typeIdx = new Int32Array(n);
    typeIdx.set(this.completeTypeIdx);
    this.completeTypeIdx = typeIdx;
    this.completeCap = n;
  }

  get length(): number {
    return this._len;
  }

  /** Indices sorted ascending by ts; stable (equal ts keep wire order), so a
   * columnar buildSpanData sees the same event order as `[...customEvents].sort`. */
  tsIndex(): Int32Array {
    if (this._tsIndex === null) {
      const perm = new Int32Array(this._len);
      for (let i = 0; i < this._len; i++) perm[i] = i;
      const ts = this.ts;
      // Array sort (stable in V8) with an index tiebreak for cross-engine stability.
      const arr = Array.from(perm);
      arr.sort((a, b) => ts[a]! - ts[b]! || a - b);
      this._tsIndex = Int32Array.from(arr);
    }
    return this._tsIndex;
  }

  /**
   * SINK ROUTER the parser calls per custom event: if `name` is a span event,
   * extract it into the columns and return true; otherwise return false so the
   * parser keeps it in the fat `customEvents` array. Keeps ALL span-name
   * classification here (mirrors buildSpanData), so the frozen parser only
   * delegates.
   */
  pushIfSpan(
    name: string,
    timestamp: number,
    v: Record<string, DecodedFieldValue>,
    singleEventSpan: SingleEventSpanProjection | null,
  ): boolean {
    if (singleEventSpan != null) {
      this.push(SPAN_KIND.Complete, timestamp, v, singleEventSpan);
      return true;
    }
    const kind = spanKindOf(name);
    if (kind === null) return false;
    this.push(kind, timestamp, v);
    return true;
  }

  private internStr(s: string): number {
    let i = this.strIntern.get(s);
    if (i === undefined) {
      i = this.strings.length;
      this.strings.push(s);
      this.strIntern.set(s, i);
    }
    return i;
  }
  private internName(s: string): number {
    let i = this.nameIntern.get(s);
    if (i === undefined) {
      i = this.spanNames.length;
      this.spanNames.push(s);
      this.nameIntern.set(s, i);
    }
    return i;
  }
  private internType(s: string): number {
    let i = this.typeIntern.get(s);
    if (i === undefined) {
      i = this.spanTypes.length;
      this.spanTypes.push(s);
      this.typeIntern.set(s, i);
    }
    return i;
  }
  private internUnit(s: string): number {
    let i = this.extraUnitIntern.get(s);
    if (i === undefined) {
      i = this.extraUnits.length;
      this.extraUnits.push(s);
      this.extraUnitIntern.set(s, i);
    }
    return i;
  }

  private grow(): void {
    const n = this._cap * 2;
    const g = <T extends { set(a: ArrayLike<number>): void }>(
      old: ArrayLike<number>,
      Ctor: new (len: number) => T
    ): T => {
      const next = new Ctor(n);
      next.set(old as ArrayLike<number>);
      return next;
    };
    this.kind = g(this.kind, Uint8Array);
    this.ts = g(this.ts, Float64Array);
    this.workerId = g(this.workerId, Float64Array);
    this.spanIdIdx = g(this.spanIdIdx, Int32Array);
    this.parentIdx = g(this.parentIdx, Int32Array);
    this.spanNameIdx = g(this.spanNameIdx, Int32Array);
    const completeIdx = new Int32Array(n);
    completeIdx.fill(-1);
    completeIdx.set(this.completeIdx);
    this.completeIdx = completeIdx;
    const eo = new Int32Array(n + 1);
    eo.set(this.extraOff);
    this.extraOff = eo;
    this._cap = n;
  }

  /**
   * SINK: the parser calls this for each span-classified custom event with its
   * kind and raw decoded `fields` (v). Field extraction mirrors buildSpanData:
   * base fields become columns; any non-base fields are kept in `extras` so a
   * hand-written span struct with custom fields still round-trips.
   */
  push(
    kind: number,
    timestamp: number,
    v: Record<string, DecodedFieldValue>,
    singleEventSpan: SingleEventSpanProjection | null = null,
  ): void {
    if (this._len === this._cap) this.grow();
    const i = this._len++;
    this.kind[i] = kind;
    this.ts[i] = timestamp;
    // Number(undefined) === NaN, matching the fat path's Number(v.worker_id).
    this.workerId[i] = v.worker_id != null ? Number(v.worker_id) : NaN;
    // span_id / parent_span_id are already strings on the wire, so skip the
    // String() round-trip in the common case (it was allocating a new identical
    // string 176k times on the demo trace); fall back only for the rare
    // numeric-encoded id.
    const sid = v.span_id;
    this.spanIdIdx[i] =
      sid == null ? -1 : this.internStr(typeof sid === "string" ? sid : String(sid));
    const pid = v.parent_span_id;
    this.parentIdx[i] =
      pid == null ? -1 : this.internStr(typeof pid === "string" ? pid : String(pid));
    const sn = kind === SPAN_KIND.Complete
      ? singleEventSpan?.name
      : v.span_name;
    this.spanNameIdx[i] = typeof sn === "string" && sn ? this.internName(sn) : -1;
    if (kind === SPAN_KIND.Complete) {
      if (singleEventSpan == null) {
        throw new Error("complete span event is missing its normalized projection");
      }
      if (this.completeLen === this.completeCap) this.growComplete();
      const completeIdx = this.completeLen++;
      this.completeIdx[i] = completeIdx;
      this.completeStart[completeIdx] = singleEventSpan.start;
      this.completeThreadId[completeIdx] = singleEventSpan.threadId ?? NaN;
      this.completeTaskId[completeIdx] = singleEventSpan.taskId ?? NaN;
      this.completeWorkerId[completeIdx] = singleEventSpan.workerId ?? NaN;
      this.completeTypeIdx[completeIdx] = this.internType(singleEventSpan.spanType);
    }

    // Non-base fields: interned into the CSR (matches buildSpanData's per-span
    // `fields`, which excludes the base keys). extraOff[i+1] closes event i.
    const isBase = kind === SPAN_KIND.Complete
      ? () => false
      : kind === SPAN_KIND.Exit
        ? isBaseExitField
        : isBaseEnterField;
    const extras = kind === SPAN_KIND.Complete ? singleEventSpan!.fields : v;
    for (const k in extras) {
      if (!isBase(k)) {
        if (this.extraLen === this._extraCap) this.growExtra();
        this.extraKeyId[this.extraLen] = this.internKey(k);
        this.extraValId[this.extraLen] = this.internVal(extras[k]!);
        const unit = singleEventSpan?.units?.[k];
        this.extraUnitId[this.extraLen] =
          unit == null ? -1 : this.internUnit(unit);
        this.extraLen++;
      }
    }
    this.extraOff[i + 1] = this.extraLen;
  }

  // ── Index accessors for the columnar buildSpanData ──
  /** span_id as the exact string buildSpanData keys by (String(v.span_id));
   * "undefined" when absent, matching String(undefined). */
  spanIdAt(i: number): string {
    const idx = this.spanIdIdx[i]!;
    return idx < 0 ? "undefined" : this.strings[idx]!;
  }
  /** parent_span_id string, or null (matching v.parent_span_id != null ? … : null). */
  parentAt(i: number): string | null {
    const idx = this.parentIdx[i]!;
    return idx < 0 ? null : this.strings[idx]!;
  }
  /** span_name, or "unknown" (matching v.span_name || "unknown"). */
  spanNameAt(i: number): string {
    const idx = this.spanNameIdx[i]!;
    return idx < 0 ? "unknown" : this.spanNames[idx]!;
  }
  /** Single-event start timestamp, or NaN for tracing events. */
  startAt(i: number): number {
    const idx = this.completeIdx[i]!;
    return idx < 0 ? NaN : this.completeStart[idx]!;
  }
  /** Complete-event OS thread id, or NaN for tracing events / absent data. */
  threadIdAt(i: number): number {
    const idx = this.completeIdx[i]!;
    return idx < 0 ? NaN : this.completeThreadId[idx]!;
  }
  /** Complete-event Tokio task id, or NaN for tracing events / absent data. */
  taskIdAt(i: number): number {
    const idx = this.completeIdx[i]!;
    return idx < 0 ? NaN : this.completeTaskId[idx]!;
  }
  /** Complete-event runtime worker id, or NaN when absent. */
  completeWorkerIdAt(i: number): number {
    const idx = this.completeIdx[i]!;
    return idx < 0 ? NaN : this.completeWorkerId[idx]!;
  }
  /** Producer/instrumentation family for a complete single-event span. */
  spanTypeAt(i: number): string {
    const idx = this.completeIdx[i]!;
    return idx < 0 ? "tracing" : this.spanTypes[this.completeTypeIdx[idx]!]!;
  }
  /** Non-base fields for this event ({} when none), rebuilt from the interned
   * CSR - matches the fat buildSpanData per-span `fields`. */
  extraFieldsAt(i: number): Record<string, DecodedFieldValue> {
    const lo = this.extraOff[i]!, hi = this.extraOff[i + 1]!;
    if (lo === hi) return {};
    const out: Record<string, DecodedFieldValue> = {};
    for (let j = lo; j < hi; j++) {
      const id = this.extraValId[j]!;
      out[this.extraKeys[this.extraKeyId[j]!]!] = id === 0 ? null : this.extraVals[id - 1]!;
    }
    return out;
  }
  /** Attribute units for this event, or null when none are declared. */
  extraUnitsAt(i: number): Record<string, string> | null {
    const lo = this.extraOff[i]!, hi = this.extraOff[i + 1]!;
    const out: Record<string, string> = {};
    for (let j = lo; j < hi; j++) {
      const unitId = this.extraUnitId[j]!;
      if (unitId >= 0) {
        out[this.extraKeys[this.extraKeyId[j]!]!] = this.extraUnits[unitId]!;
      }
    }
    return Object.keys(out).length > 0 ? out : null;
  }
}

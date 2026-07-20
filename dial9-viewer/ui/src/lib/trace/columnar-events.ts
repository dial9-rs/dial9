// Columnar (struct-of-arrays) store for `trace.events`, the scheduler-event
// stream that dominates a large trace's memory (13M+ events at ~836 B/event as
// fat objects -> ~11.5 GB; as typed-array columns -> ~73 B/event -> ~1 GB, and
// no per-event object means no major-GC thrash during parse).
//
// It is a drop-in for the fat `events` array on two fronts:
//   1. SINK: the parser writes into it via `.push(fatEvent)` (see the parser's
//      eventSink option), so nothing outside changes how events are produced.
//      The pushed fat object is read field-by-field and DROPPED (transient,
//      young-gen) rather than retained.
//   2. READ: `.length`, `for..of` (a REUSED flyweight view - safe only for
//      consumers that don't retain the view past one step), `.at(i)`
//      (materializes a fresh plain object), `.map`/`.filter`/`.some`. Consumers
//      that RETAIN events (buildWorkerSpans, computeSegmentEdgePolls) do NOT use
//      the flyweight; they read the raw columns by index (exposed below).
//
// customEvents stay fat: they are used as WeakMap keys and spread via
// Object.entries(fields), so they can't share this representation.

/** Event-type discriminants, mirroring the frozen core's EVENT_TYPES. */
export const EVT = {
  PollStart: 0,
  PollEnd: 1,
  WorkerPark: 2,
  WorkerUnpark: 3,
  QueueSample: 4,
  WakeEvent: 9,
} as const;

/** The plain-object shape a materialized event exposes (the flyweight and
 * `.at(i)` both present this). Mirrors the fat parser event exactly. */
export interface EventLike {
  eventType: number;
  timestamp: number;
  workerId: number;
  localQueue: number;
  globalQueue: number;
  cpuTime: number;
  /** null when this park->unpark was not schedstat-sampled (distinct from 0). */
  schedWait: number | null;
  taskId: number;
  /** spawn-location string, or null. (Parser sets spawnLocId === spawnLoc.) */
  spawnLocId: string | null;
  spawnLoc: string | null;
  /** undefined on pre-tid traces and on non-park/unpark events. */
  tid: number | undefined;
  wakerTaskId: number;
  wokenTaskId: number;
  targetWorker: number;
}

const INITIAL_CAP = 1 << 16;

export class ColumnarEvents {
  eventType: Uint8Array;
  ts: Float64Array;
  workerId: Int32Array;
  localQueue: Int32Array;
  globalQueue: Int32Array;
  cpuTime: Float64Array;
  /** NaN encodes null (unsampled sched_wait). */
  schedWaitRaw: Float64Array;
  taskId: Float64Array;
  /** -1 encodes null; else an index into spawnList. */
  spawnLocIdx: Int32Array;
  /** NaN encodes undefined (no tid). */
  tidRaw: Float64Array;
  /** NaN encodes "not a wake event". */
  wakerTaskIdRaw: Float64Array;
  wokenTaskIdRaw: Float64Array;

  spawnList: string[] = [];
  private spawnIntern = new Map<string, number>();

  private _len = 0;
  private _cap: number;
  minTs = Infinity;
  maxTs = -Infinity;

  /** Lazily-built permutation of indices sorted ascending by ts (~4 bytes/event
   * external). Events arrive in wire order, which is ~99.99% ts-sorted but has
   * rare full-span outliers (segment-concat boundary), so a time window is NOT
   * a contiguous index range - it must be found through this sort index. */
  private _tsIndex: Int32Array | null = null;

  private readonly view: ReusedEventCursor;

  constructor(cap = INITIAL_CAP) {
    this._cap = cap;
    this.eventType = new Uint8Array(cap);
    this.ts = new Float64Array(cap);
    this.workerId = new Int32Array(cap);
    this.localQueue = new Int32Array(cap);
    this.globalQueue = new Int32Array(cap);
    this.cpuTime = new Float64Array(cap);
    this.schedWaitRaw = new Float64Array(cap);
    this.taskId = new Float64Array(cap);
    this.spawnLocIdx = new Int32Array(cap);
    this.tidRaw = new Float64Array(cap);
    this.wakerTaskIdRaw = new Float64Array(cap);
    this.wokenTaskIdRaw = new Float64Array(cap);
    this.view = new ReusedEventCursor(this);
  }

  get length(): number {
    return this._len;
  }

  private intern(s: string | null | undefined): number {
    if (s == null) return -1;
    let i = this.spawnIntern.get(s);
    if (i === undefined) {
      i = this.spawnList.length;
      this.spawnList.push(s);
      this.spawnIntern.set(s, i);
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
    this.eventType = g(this.eventType, Uint8Array);
    this.ts = g(this.ts, Float64Array);
    this.workerId = g(this.workerId, Int32Array);
    this.localQueue = g(this.localQueue, Int32Array);
    this.globalQueue = g(this.globalQueue, Int32Array);
    this.cpuTime = g(this.cpuTime, Float64Array);
    this.schedWaitRaw = g(this.schedWaitRaw, Float64Array);
    this.taskId = g(this.taskId, Float64Array);
    this.spawnLocIdx = g(this.spawnLocIdx, Int32Array);
    this.tidRaw = g(this.tidRaw, Float64Array);
    this.wakerTaskIdRaw = g(this.wakerTaskIdRaw, Float64Array);
    this.wokenTaskIdRaw = g(this.wokenTaskIdRaw, Float64Array);
    this._cap = n;
  }

  /**
   * SINK entry point: the parser calls `state.events.push(fatEvent)`; we read
   * its fields into the columns and let the fat object die. `num`-coercion has
   * already happened in the parser, so fields are numbers | null | undefined |
   * string(spawnLoc).
   */
  push(e: EventLike): void {
    if (this._len === this._cap) this.grow();
    const i = this._len++;
    this.eventType[i] = e.eventType;
    const t = e.timestamp;
    this.ts[i] = t;
    if (t < this.minTs) this.minTs = t;
    if (t > this.maxTs) this.maxTs = t;
    this.workerId[i] = e.workerId ?? 0;
    this.localQueue[i] = e.localQueue ?? 0;
    this.globalQueue[i] = e.globalQueue ?? 0;
    this.cpuTime[i] = e.cpuTime ?? 0;
    this.schedWaitRaw[i] = e.schedWait == null ? NaN : e.schedWait;
    this.taskId[i] = e.taskId ?? 0;
    this.spawnLocIdx[i] = this.intern(e.spawnLoc ?? null);
    this.tidRaw[i] = e.tid == null ? NaN : e.tid;
    this.wakerTaskIdRaw[i] = e.wakerTaskId == null ? NaN : e.wakerTaskId;
    this.wokenTaskIdRaw[i] = e.wokenTaskId == null ? NaN : e.wokenTaskId;
  }

  /**
   * ZERO-ALLOCATION sink path: the parser calls this with primitives instead of
   * building a fat event object, so a large trace produces NO transient
   * per-event objects (which otherwise churn the GC during parse). Field
   * semantics match `push`: schedWait/tid null|undefined -> NaN sentinel,
   * spawnLoc string|null -> interned index, wake fields NaN when absent.
   * Positional to avoid an options object per call.
   */
  pushEvent(
    eventType: number,
    timestamp: number,
    workerId: number,
    localQueue: number,
    globalQueue: number,
    cpuTime: number,
    schedWait: number | null,
    taskId: number,
    spawnLoc: string | null,
    tid: number | undefined,
    wakerTaskId: number | undefined,
    wokenTaskId: number | undefined
  ): void {
    if (this._len === this._cap) this.grow();
    const i = this._len++;
    this.eventType[i] = eventType;
    this.ts[i] = timestamp;
    if (timestamp < this.minTs) this.minTs = timestamp;
    if (timestamp > this.maxTs) this.maxTs = timestamp;
    this.workerId[i] = workerId;
    this.localQueue[i] = localQueue;
    this.globalQueue[i] = globalQueue;
    this.cpuTime[i] = cpuTime;
    this.schedWaitRaw[i] = schedWait == null ? NaN : schedWait;
    this.taskId[i] = taskId;
    this.spawnLocIdx[i] = this.intern(spawnLoc);
    this.tidRaw[i] = tid == null ? NaN : tid;
    this.wakerTaskIdRaw[i] = wakerTaskId == null ? NaN : wakerTaskId;
    this.wokenTaskIdRaw[i] = wokenTaskId == null ? NaN : wokenTaskId;
  }

  // ── Column decoders (sentinel -> semantic value) for index-based consumers ──
  spawnLocAt(i: number): string | null {
    const idx = this.spawnLocIdx[i]!;
    return idx < 0 ? null : this.spawnList[idx]!;
  }
  schedWaitAt(i: number): number | null {
    const v = this.schedWaitRaw[i]!;
    return Number.isNaN(v) ? null : v;
  }
  tidAt(i: number): number | undefined {
    const v = this.tidRaw[i];
    return Number.isNaN(v) ? undefined : v;
  }

  /** Materialize a fresh, independent plain event at index `i` (safe to
   * retain). */
  at(i: number): EventLike | undefined {
    if (i < 0) i += this._len;
    if (i < 0 || i >= this._len) return undefined;
    return materialize(this, i);
  }

  /** Flyweight iteration: yields the SAME view object each step (fields read
   * live from columns). Safe ONLY for consumers that read fields within the
   * loop body and never retain the view. */
  [Symbol.iterator](): Iterator<EventLike> {
    const view = this.view;
    const n = this._len;
    let i = 0;
    return {
      next(): IteratorResult<EventLike> {
        if (i >= n) return { done: true, value: undefined };
        view._i = i++;
        return { done: false, value: view };
      },
    };
  }

  /** Array-like `.map`: applies `fn` to a flyweight view per element and
   * collects results. `fn` must copy out primitives (not retain the view). */
  map<T>(fn: (e: EventLike, i: number) => T): T[] {
    const out = new Array<T>(this._len);
    const view = this.view;
    for (let i = 0; i < this._len; i++) {
      view._i = i;
      out[i] = fn(view, i);
    }
    return out;
  }

  /** Array-like `.filter`: returns MATERIALIZED (safe-to-retain) events. */
  filter(fn: (e: EventLike, i: number) => boolean): EventLike[] {
    const out: EventLike[] = [];
    const view = this.view;
    for (let i = 0; i < this._len; i++) {
      view._i = i;
      if (fn(view, i)) out.push(materialize(this, i));
    }
    return out;
  }

  some(fn: (e: EventLike, i: number) => boolean): boolean {
    const view = this.view;
    for (let i = 0; i < this._len; i++) {
      view._i = i;
      if (fn(view, i)) return true;
    }
    return false;
  }

  /**
   * Whole-trace event-density histogram straight off the ts column - the
   * minimap overview, without the intermediate `.map(e => e.timestamp)` array
   * (a 13.7M-number allocation today). Bin-for-bin identical to
   * binTimestamps(this.map(e => e.timestamp), {startNs,endNs}, resolution).
   */
  densityBins(startNs: number, endNs: number, resolution: number): number[] {
    const span = endNs - startNs;
    if (span <= 0 || resolution <= 0) return [];
    const bins = new Array<number>(resolution).fill(0);
    const ts = this.ts;
    const n = this._len;
    for (let i = 0; i < n; i++) {
      const t = ts[i]!;
      if (t < startNs || t > endNs) continue;
      let idx = Math.floor(((t - startNs) / span) * resolution);
      if (idx >= resolution) idx = resolution - 1;
      else if (idx < 0) idx = 0;
      bins[idx] = bins[idx]! + 1;
    }
    return bins;
  }

  // ── Windowing substrate (viewport-scoped derive) ──

  /** Permutation of event indices sorted ascending by ts (stable). Built once,
   * cached. V8 TypedArray.sort is stable (ES2019+), so equal-ts events keep
   * their original (wire) order - matching the per-worker sorts downstream. */
  tsIndex(): Int32Array {
    if (this._tsIndex === null) {
      const n = this._len;
      const idx = new Int32Array(n);
      for (let i = 0; i < n; i++) idx[i] = i;
      const ts = this.ts;
      idx.sort((a, b) => ts[a]! - ts[b]!);
      this._tsIndex = idx;
    }
    return this._tsIndex;
  }

  /**
   * Half-open permutation-index range [lo, hi) covering every event whose ts is
   * in [t0, t1]. The original event indices in the window are
   * `tsIndex()[lo], …, tsIndex()[hi-1]` (in ts order). Correct regardless of
   * wire order. O(log n).
   */
  windowRange(t0: number, t1: number): { lo: number; hi: number } {
    const perm = this.tsIndex();
    const ts = this.ts;
    const n = this._len;
    // lower_bound: first k with ts[perm[k]] >= t0
    let a = 0, b = n;
    while (a < b) { const m = (a + b) >>> 1; if (ts[perm[m]!]! < t0) a = m + 1; else b = m; }
    const lo = a;
    // upper_bound: first k with ts[perm[k]] > t1
    a = lo; b = n;
    while (a < b) { const m = (a + b) >>> 1; if (ts[perm[m]!]! <= t1) a = m + 1; else b = m; }
    return { lo, hi: a };
  }
}

function materialize(c: ColumnarEvents, i: number): EventLike {
  const workerId = c.workerId[i]!;
  return {
    eventType: c.eventType[i]!,
    timestamp: c.ts[i]!,
    workerId,
    localQueue: c.localQueue[i]!,
    globalQueue: c.globalQueue[i]!,
    cpuTime: c.cpuTime[i]!,
    schedWait: c.schedWaitAt(i),
    taskId: c.taskId[i]!,
    spawnLocId: c.spawnLocAt(i),
    spawnLoc: c.spawnLocAt(i),
    tid: c.tidAt(i),
    wakerTaskId: c.wakerTaskIdRaw[i]!,
    wokenTaskId: c.wokenTaskIdRaw[i]!,
    targetWorker: workerId,
  };
}

/**
 * The reused flyweight. Its fields are getters that read the store's columns at
 * the current index `_i`; sentinels are decoded to match the fat event exactly
 * (schedWait NaN->null, tid NaN->undefined, spawnLoc idx->string|null). Wake
 * fields read as-is (NaN on non-wake events, which never read them).
 */
class ReusedEventCursor implements EventLike {
  _i = 0;
  private readonly c: ColumnarEvents;
  constructor(c: ColumnarEvents) { this.c = c; }
  get eventType(): number { return this.c.eventType[this._i]!; }
  get timestamp(): number { return this.c.ts[this._i]!; }
  get workerId(): number { return this.c.workerId[this._i]!; }
  get localQueue(): number { return this.c.localQueue[this._i]!; }
  get globalQueue(): number { return this.c.globalQueue[this._i]!; }
  get cpuTime(): number { return this.c.cpuTime[this._i]!; }
  get schedWait(): number | null { return this.c.schedWaitAt(this._i); }
  get taskId(): number { return this.c.taskId[this._i]!; }
  get spawnLocId(): string | null { return this.c.spawnLocAt(this._i); }
  get spawnLoc(): string | null { return this.c.spawnLocAt(this._i); }
  get tid(): number | undefined { return this.c.tidAt(this._i); }
  get wakerTaskId(): number { return this.c.wakerTaskIdRaw[this._i]!; }
  get wokenTaskId(): number { return this.c.wokenTaskIdRaw[this._i]!; }
  get targetWorker(): number { return this.c.workerId[this._i]!; }
}

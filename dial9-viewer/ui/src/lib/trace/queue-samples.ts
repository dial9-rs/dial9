// Per-worker local-queue depth samples.
//
// Each worker keeps two typed arrays, 9 bytes a sample: samples arrive grouped
// by worker and in timestamp order, nothing needs sorting or boxing.
// `local` is u8 on the wire (telemetry/format.rs).

/** One worker's samples, read by index. */
export interface QueueSampleSlice {
  readonly length: number;
  tAt(i: number): number;
  localAt(i: number): number;
  /** Largest index with t <= val, or -1. Matches render.ts' lowerBoundT. */
  lastAtOrBefore(val: number): number;
  /** Smallest index with t >= val, or `length`. Matches render.ts' upperBoundT. */
  firstAtOrAfter(val: number): number;
  /** Index of the sample closest to `ns`, or -1 when empty. */
  nearest(ns: number): number;
  /** `{t, local}` objects for one worker, for the compat path's record map. */
  toRecords(): { t: number; local: number }[];
  /** Objects for `[start, end]`; the step-line batcher needs the shape, and
   *  only ever asks for the visible window. */
  recordsIn(start: number, end: number): { t: number; local: number }[];
}

const EMPTY: QueueSampleSlice = {
  length: 0,
  tAt: () => NaN,
  localAt: () => 0,
  lastAtOrBefore: () => -1,
  firstAtOrAfter: () => 0,
  nearest: () => -1,
  toRecords: () => [],
  recordsIn: () => [],
};

class WorkerSamples implements QueueSampleSlice {
  private t: Float64Array;
  private local: Uint8Array;
  private n = 0;

  constructor(cap = 256) {
    this.t = new Float64Array(cap);
    this.local = new Uint8Array(cap);
  }

  get length(): number {
    return this.n;
  }
  tAt(i: number): number {
    return this.t[i]!;
  }
  localAt(i: number): number {
    return this.local[i]!;
  }

  push(t: number, local: number): void {
    if (this.n === this.t.length) {
      const grown = new Float64Array(this.n * 2);
      grown.set(this.t);
      this.t = grown;
      const grownLocal = new Uint8Array(this.n * 2);
      grownLocal.set(this.local);
      this.local = grownLocal;
    }
    this.t[this.n] = t;
    this.local[this.n] = local;
    this.n++;
  }

  lastAtOrBefore(val: number): number {
    let lo = 0, hi = this.n - 1, ans = -1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (this.t[mid]! <= val) { ans = mid; lo = mid + 1; } else hi = mid - 1;
    }
    return ans;
  }

  firstAtOrAfter(val: number): number {
    let lo = 0, hi = this.n - 1, ans = this.n;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (this.t[mid]! >= val) { ans = mid; hi = mid - 1; } else lo = mid + 1;
    }
    return ans;
  }

  nearest(ns: number): number {
    if (this.n === 0) return -1;
    let lo = 0, hi = this.n - 1;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (this.t[mid]! < ns) lo = mid + 1;
      else hi = mid;
    }
    // lo is the first index with t >= ns; its predecessor may be closer.
    if (lo > 0 && Math.abs(this.t[lo - 1]! - ns) <= Math.abs(this.t[lo]! - ns)) {
      return lo - 1;
    }
    return lo;
  }

  /** `{t, local}` objects, for the callers that must match the frozen builder. */
  toRecords(): { t: number; local: number }[] {
    return this.recordsIn(0, this.n - 1);
  }

  recordsIn(start: number, end: number): { t: number; local: number }[] {
    const lo = Math.max(0, start);
    const hi = Math.min(this.n - 1, end);
    if (hi < lo) return [];
    const out = new Array<{ t: number; local: number }>(hi - lo + 1);
    for (let i = lo; i <= hi; i++) {
      out[i - lo] = { t: this.t[i]!, local: this.local[i]! };
    }
    return out;
  }
}

export class QueueSampleIndex {
  private readonly byWorker = new Map<number, WorkerSamples>();

  /** Register a worker so it reads back as present-but-empty, as the frozen
   *  builder's pre-seeded `workerQueueSamples[w] = []` does. */
  ensure(workerId: number): void {
    if (!this.byWorker.has(workerId)) this.byWorker.set(workerId, new WorkerSamples());
  }

  push(workerId: number, t: number, local: number): void {
    let w = this.byWorker.get(workerId);
    if (w === undefined) {
      w = new WorkerSamples();
      this.byWorker.set(workerId, w);
    }
    w.push(t, local);
  }

  forWorker(workerId: number): QueueSampleSlice {
    return this.byWorker.get(workerId) ?? EMPTY;
  }

  workerIds(): number[] {
    return [...this.byWorker.keys()];
  }

  /** The frozen builder's shape. Allocates per sample. */
  toRecordMap(): Record<number, { t: number; local: number }[]> {
    const out: Record<number, { t: number; local: number }[]> = {};
    for (const [w, s] of this.byWorker) out[w] = s.toRecords();
    return out;
  }

  /** Wrap the frozen builder's map in the same interface. */
  static fromRecordMap(
    map: Record<number, readonly { t: number; local: number }[]>,
  ): QueueSampleIndex {
    const idx = new QueueSampleIndex();
    for (const [key, arr] of Object.entries(map)) {
      const w = Number(key);
      idx.ensure(w);
      for (const s of arr) idx.push(w, s.t, s.local);
    }
    return idx;
  }
}

/** Shared empty index, so callers never special-case null. */
export const EMPTY_QUEUE_SAMPLES = new QueueSampleIndex();

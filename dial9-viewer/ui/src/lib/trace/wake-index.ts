// Wake storage.
//
// Wakes sit in flat columns, with an index per lookup, by woken task (Task
// tab, scheduling delays) and by target worker (lane renderer). A lookup's
// group `g` owns `idx[off[g] .. off[g] + len)`, each entry a row in the
// columns, so neither lookup copies the wake.

/** Column-index view of one group's wakes, ascending by timestamp. */
export interface WakeSlice {
  readonly length: number;
  timestampAt(i: number): number;
  wakerTaskIdAt(i: number): number;
  wokenTaskIdAt(i: number): number;
  targetWorkerAt(i: number): number;
}

/** The object shape the frozen core and the Task tab consume. */
export interface WakeRecord {
  timestamp: number;
  wakerTaskId: number;
  wokenTaskId: number;
  targetWorker: number;
}

/** Start offset + length of one group inside a view's index array. */
type Span = { at: number; len: number };

const EMPTY: WakeSlice = {
  length: 0,
  timestampAt: () => NaN,
  wakerTaskIdAt: () => NaN,
  wokenTaskIdAt: () => NaN,
  targetWorkerAt: () => NaN,
};

class Slice implements WakeSlice {
  readonly length: number;
  private readonly w: WakeIndex;
  private readonly idx: Int32Array;
  private readonly at: number;

  constructor(w: WakeIndex, idx: Int32Array, span: Span) {
    this.w = w;
    this.idx = idx;
    this.at = span.at;
    this.length = span.len;
  }

  timestampAt(i: number): number {
    return this.w.ts[this.idx[this.at + i]!]!;
  }
  wakerTaskIdAt(i: number): number {
    return this.w.waker[this.idx[this.at + i]!]!;
  }
  wokenTaskIdAt(i: number): number {
    return this.w.woken[this.idx[this.at + i]!]!;
  }
  targetWorkerAt(i: number): number {
    return this.w.worker[this.idx[this.at + i]!]!;
  }
}

export class WakeIndex {
  readonly ts: Float64Array;
  readonly waker: Float64Array;
  readonly woken: Float64Array;
  readonly worker: Int32Array;
  private readonly taskSpans: Map<number, Span>;
  private readonly taskIdx: Int32Array;
  private readonly workerSpans: Map<number, Span>;
  private readonly workerIdx: Int32Array;

  private constructor(
    ts: Float64Array,
    waker: Float64Array,
    woken: Float64Array,
    worker: Int32Array,
    taskSpans: Map<number, Span>,
    taskIdx: Int32Array,
    workerSpans: Map<number, Span>,
    workerIdx: Int32Array,
  ) {
    this.ts = ts;
    this.waker = waker;
    this.woken = woken;
    this.worker = worker;
    this.taskSpans = taskSpans;
    this.taskIdx = taskIdx;
    this.workerSpans = workerSpans;
    this.workerIdx = workerIdx;
  }

  get length(): number {
    return this.ts.length;
  }

  forTask(taskId: number): WakeSlice {
    const span = this.taskSpans.get(taskId);
    return span === undefined ? EMPTY : new Slice(this, this.taskIdx, span);
  }

  forWorker(workerId: number): WakeSlice {
    const span = this.workerSpans.get(workerId);
    return span === undefined ? EMPTY : new Slice(this, this.workerIdx, span);
  }

  /** One task's wakes as objects, for `computePollWakes` and the Task tab,
   *  which retain them. One task at a time, so the allocation stays small. */
  materializeForTask(taskId: number): WakeRecord[] {
    const s = this.forTask(taskId);
    const out: WakeRecord[] = new Array<WakeRecord>(s.length);
    for (let i = 0; i < s.length; i++) {
      out[i] = {
        timestamp: s.timestampAt(i),
        wakerTaskId: s.wakerTaskIdAt(i),
        wokenTaskId: s.wokenTaskIdAt(i),
        targetWorker: s.targetWorkerAt(i),
      };
    }
    return out;
  }

  /** Both views as the frozen builder's object maps. One object per wake, so
   *  only for the callers that must match its shape. */
  toRecordMaps(): {
    byTask: Record<number, WakeRecord[]>;
    byWorker: Record<number, WakeRecord[]>;
  } {
    const rec = (i: number): WakeRecord => ({
      timestamp: this.ts[i]!,
      wakerTaskId: this.waker[i]!,
      wokenTaskId: this.woken[i]!,
      targetWorker: this.worker[i]!,
    });
    // Walk the views, not the rows: only they carry the per-group order.
    const fill = (
      spans: Map<number, Span>,
      idx: Int32Array,
    ): Record<number, WakeRecord[]> => {
      const out: Record<number, WakeRecord[]> = {};
      for (const [key, sp] of spans) {
        const arr = new Array<WakeRecord>(sp.len);
        for (let i = 0; i < sp.len; i++) arr[i] = rec(idx[sp.at + i]!);
        out[key] = arr;
      }
      return out;
    };
    return {
      byTask: fill(this.taskSpans, this.taskIdx),
      byWorker: fill(this.workerSpans, this.workerIdx),
    };
  }

  /** Woken task ids present, for callers that scan every task. */
  taskIds(): number[] {
    return [...this.taskSpans.keys()];
  }

  /** Build from wakes emitted in event order. */
  static build(
    count: number,
    fill: (
      emit: (ts: number, waker: number, woken: number, worker: number) => void,
    ) => void,
  ): WakeIndex {
    const ts = new Float64Array(count);
    const waker = new Float64Array(count);
    const woken = new Float64Array(count);
    const worker = new Int32Array(count);
    let n = 0;
    fill((t, wk, wo, w) => {
      ts[n] = t;
      waker[n] = wk;
      woken[n] = wo;
      worker[n] = w;
      n++;
    });

    const group = (
      key: (i: number) => number,
    ): [Map<number, Span>, Int32Array] => {
      const spans = new Map<number, Span>();
      for (let i = 0; i < n; i++) {
        const k = key(i);
        const s = spans.get(k);
        if (s === undefined) spans.set(k, { at: 0, len: 1 });
        else s.len++;
      }
      let at = 0;
      for (const s of spans.values()) {
        s.at = at;
        at += s.len;
      }
      const cursor = new Map<number, number>();
      const idx = new Int32Array(n);
      for (let i = 0; i < n; i++) {
        const k = key(i);
        const o = cursor.get(k) ?? 0;
        idx[spans.get(k)!.at + o] = i;
        cursor.set(k, o + 1);
      }
      // Event order is near-ts-order but not exactly: events are emitted per
      // thread, then concatenated per segment.
      for (const sp of spans.values()) {
        if (sp.len < 2) continue;
        const slice = Array.from(idx.subarray(sp.at, sp.at + sp.len));
        slice.sort((a, b) => ts[a]! - ts[b]!);
        idx.set(slice, sp.at);
      }
      return [spans, idx];
    };

    const [taskSpans, taskIdx] = group((i) => woken[i]!);
    const [workerSpans, workerIdx] = group((i) => worker[i]!);
    return new WakeIndex(
      ts, waker, woken, worker, taskSpans, taskIdx, workerSpans, workerIdx,
    );
  }

  /** No wakes; for callers with no trace loaded. */
  static empty(): WakeIndex {
    return WakeIndex.build(0, () => {});
  }

  /**
   * Wrap the frozen builder's object arrays in the same interface. Its records
   * carry no `wokenTaskId`: the map key is the woken task.
   */
  static fromRecords(
    byTask: Record<number, { timestamp: number; wakerTaskId: number; targetWorker: number }[]>,
  ): WakeIndex {
    const rows: { woken: number; rec: { timestamp: number; wakerTaskId: number; targetWorker: number } }[] = [];
    for (const [key, arr] of Object.entries(byTask)) {
      const woken = Number(key);
      for (const rec of arr) rows.push({ woken, rec });
    }
    return WakeIndex.build(rows.length, (emit) => {
      for (const { woken, rec } of rows) {
        emit(rec.timestamp, rec.wakerTaskId, woken, rec.targetWorker);
      }
    });
  }
}

/** Shared empty index, so callers never special-case null. */
export const EMPTY_WAKE_INDEX: WakeIndex = WakeIndex.empty();

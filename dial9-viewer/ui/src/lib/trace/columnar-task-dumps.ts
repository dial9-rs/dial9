// A task dump is one captured stack for an idle task.
//
// Timestamps in one column, frames concatenated into another with a per-dump
// offset, and a CSR index grouping dumps by task. Consumers ask for one task
// at a time, so records are materialized on demand.

import type { TaskDump } from "../../types/trace.js";

/** Start offset + length of one group inside the by-task index. */
type Span = { at: number; len: number };

const INITIAL = 1 << 12;

/** How many tasks' materialized dumps to keep. Consumers read the selected
 *  task repeatedly across renders; selection changes rarely. */
const CACHE_SIZE = 4;

export class ColumnarTaskDumps {
  private ts = new Float64Array(INITIAL);
  private taskOf = new Float64Array(INITIAL);
  /** Dump `d` owns `frameIdx[frameOff[d] .. frameOff[d + 1])`. */
  private frameOff = new Int32Array(INITIAL + 1);
  private frameIdx = new Int32Array(INITIAL * 8);
  private frameCount = 0;
  private n = 0;

  private frames: string[] = [];
  private frameIntern = new Map<string, number>();

  private byTask: Map<number, Span> | null = null;
  private taskIdx: Int32Array | null = null;
  private cache = new Map<number, TaskDump[]>();

  get size(): number {
    return this.groups().size;
  }

  /**
   * SINK: called once per TaskDumpEvent with the wire addresses, before hex
   * formatting. Formatting here keeps the parser from building a per-dump
   * string array just to hand it over.
   */
  pushDump(
    taskId: number,
    timestamp: number,
    callchain: readonly (string | number | bigint)[],
  ): void {
    if (this.n === this.ts.length) {
      this.ts = grow(this.ts, Float64Array);
      this.taskOf = grow(this.taskOf, Float64Array);
      const off = new Int32Array(this.ts.length + 1);
      off.set(this.frameOff);
      this.frameOff = off;
    }
    const d = this.n++;
    this.ts[d] = timestamp;
    this.taskOf[d] = taskId;
    this.frameOff[d] = this.frameCount;
    for (const addr of callchain) {
      if (this.frameCount === this.frameIdx.length) {
        this.frameIdx = grow(this.frameIdx, Int32Array);
      }
      this.frameIdx[this.frameCount++] = this.intern(addr);
    }
    this.frameOff[d + 1] = this.frameCount;
    this.byTask = null;
    this.cache.clear();
  }

  /** Wire address -> index of its "0x…" form, matching the parser's internHex. */
  private intern(addr: string | number | bigint): number {
    const key = String(addr);
    let i = this.frameIntern.get(key);
    if (i === undefined) {
      i = this.frames.length;
      this.frames.push("0x" + BigInt(addr).toString(16));
      this.frameIntern.set(key, i);
    }
    return i;
  }

  /** Group dumps by task, each group ascending by timestamp. */
  private groups(): Map<number, Span> {
    if (this.byTask !== null) return this.byTask;
    const spans = new Map<number, Span>();
    for (let d = 0; d < this.n; d++) {
      const k = this.taskOf[d]!;
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
    const idx = new Int32Array(this.n);
    for (let d = 0; d < this.n; d++) {
      const k = this.taskOf[d]!;
      const o = cursor.get(k) ?? 0;
      idx[spans.get(k)!.at + o] = d;
      cursor.set(k, o + 1);
    }
    // The frozen parser sorts each task's dumps by timestamp; match it.
    for (const s of spans.values()) {
      if (s.len < 2) continue;
      const slice = Array.from(idx.subarray(s.at, s.at + s.len));
      slice.sort((a, b) => this.ts[a]! - this.ts[b]!);
      idx.set(slice, s.at);
    }
    this.byTask = spans;
    this.taskIdx = idx;
    return spans;
  }

  /** Dump count for a task, without materializing anything. */
  countForTask(taskId: number): number {
    return this.groups().get(taskId)?.len ?? 0;
  }

  /** Map-compatible read: one task's dumps as records, or undefined. */
  get(taskId: number): TaskDump[] | undefined {
    const span = this.groups().get(taskId);
    if (span === undefined) return undefined;
    const hit = this.cache.get(taskId);
    if (hit !== undefined) return hit;
    const idx = this.taskIdx!;
    const out = new Array<TaskDump>(span.len);
    for (let i = 0; i < span.len; i++) {
      const d = idx[span.at + i]!;
      const lo = this.frameOff[d]!;
      const hi = this.frameOff[d + 1]!;
      const chain = new Array<string>(hi - lo);
      for (let f = lo; f < hi; f++) chain[f - lo] = this.frames[this.frameIdx[f]!]!;
      out[i] = { timestamp: this.ts[d]!, callchain: chain };
    }
    if (this.cache.size >= CACHE_SIZE) {
      this.cache.delete(this.cache.keys().next().value as number);
    }
    this.cache.set(taskId, out);
    return out;
  }

  has(taskId: number): boolean {
    return this.groups().has(taskId);
  }

  /** Task ids with at least one dump. */
  keys(): IterableIterator<number> {
    return this.groups().keys();
  }

  /** Materializes every task in turn. For tests and one-shot exports, not
   *  render paths. */
  *entries(): IterableIterator<[number, TaskDump[]]> {
    for (const taskId of this.groups().keys()) yield [taskId, this.get(taskId)!];
  }
}

function grow<T extends { length: number; set(a: T): void }>(
  old: T,
  Ctor: new (len: number) => T,
): T {
  const next = new Ctor(old.length * 2);
  next.set(old);
  return next;
}

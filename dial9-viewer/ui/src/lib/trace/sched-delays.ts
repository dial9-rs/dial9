// A runnable-to-poll scheduling delay, one row per delay. A busy runtime
// produces millions of them, and every one carries a poll.
//
// The list is only ever counted and scanned once, by the wake-delay detector,
// which keeps a bounded slice of what it scans. So a row holds its poll as a
// (worker, index) pair and materializes it when something reads it.

import type { SchedDelayView } from "./columnar-worker-spans.js";

/** Materializes `poll` for a row. The store supplies its own `pollAt`. */
export interface PollSource {
  pollAt(worker: number, index: number): SchedDelayView["poll"] | undefined;
}

/** The reads consumers make of a delay list; an array satisfies it. */
export interface SchedDelayList {
  readonly length: number;
  at(i: number): SchedDelayView | undefined;
  [Symbol.iterator](): IterableIterator<SchedDelayView>;
}

const INITIAL = 1 << 12;

export class ColumnarSchedDelays implements SchedDelayList {
  private wakeTime = new Float64Array(INITIAL);
  private pollTime = new Float64Array(INITIAL);
  private delay = new Float64Array(INITIAL);
  private taskId = new Float64Array(INITIAL);
  private wakerTaskId = new Float64Array(INITIAL);
  private worker = new Int32Array(INITIAL);
  private pollIdx = new Int32Array(INITIAL);
  private n = 0;
  private cap = INITIAL;

  /** Rows in ascending wakeTime order; null until `finish`. */
  private order: Int32Array | null = null;

  private readonly polls: PollSource;

  constructor(polls: PollSource) {
    this.polls = polls;
  }

  get length(): number {
    return this.n;
  }

  private grow(): void {
    const c = this.cap * 2;
    const g = <T extends { set(a: ArrayLike<number>): void }>(
      old: ArrayLike<number>,
      Ctor: new (n: number) => T,
    ): T => {
      const next = new Ctor(c);
      next.set(old as ArrayLike<number>);
      return next;
    };
    this.wakeTime = g(this.wakeTime, Float64Array);
    this.pollTime = g(this.pollTime, Float64Array);
    this.delay = g(this.delay, Float64Array);
    this.taskId = g(this.taskId, Float64Array);
    this.wakerTaskId = g(this.wakerTaskId, Float64Array);
    this.worker = g(this.worker, Int32Array);
    this.pollIdx = g(this.pollIdx, Int32Array);
    this.cap = c;
  }

  push(
    wakeTime: number,
    pollTime: number,
    delay: number,
    taskId: number,
    wakerTaskId: number,
    worker: number,
    pollIdx: number,
  ): void {
    if (this.n === this.cap) this.grow();
    const i = this.n++;
    this.wakeTime[i] = wakeTime;
    this.pollTime[i] = pollTime;
    this.delay[i] = delay;
    this.taskId[i] = taskId;
    this.wakerTaskId[i] = wakerTaskId;
    this.worker[i] = worker;
    this.pollIdx[i] = pollIdx;
  }

  /** Order rows by wakeTime. Rows arrive grouped by worker, so this is what
   *  makes the list read chronologically. */
  finish(): void {
    const pos = Array.from({ length: this.n }, (_, i) => i);
    const w = this.wakeTime;
    pos.sort((a, b) => w[a]! - w[b]! || a - b);
    this.order = Int32Array.from(pos);
  }

  /** `delay` alone, for a scan that filters before it reads a whole row. */
  delayAt(i: number): number {
    return this.delay[this.row(i)]!;
  }

  private row(i: number): number {
    return this.order ? this.order[i]! : i;
  }

  at(i: number): SchedDelayView | undefined {
    if (i < 0 || i >= this.n) return undefined;
    const r = this.row(i);
    return {
      wakeTime: this.wakeTime[r]!,
      pollTime: this.pollTime[r]!,
      delay: this.delay[r]!,
      taskId: this.taskId[r]!,
      wakerTaskId: this.wakerTaskId[r]!,
      worker: this.worker[r]!,
      poll: this.polls.pollAt(this.worker[r]!, this.pollIdx[r]!)!,
    };
  }

  *[Symbol.iterator](): IterableIterator<SchedDelayView> {
    for (let i = 0; i < this.n; i++) yield this.at(i)!;
  }
}

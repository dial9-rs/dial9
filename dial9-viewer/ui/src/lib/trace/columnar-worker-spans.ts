// Columnar poll-span store + per-worker LOD, the Phase-2/3 data layer.
//
// buildWorkerSpans reconstructs ~5.3M poll/park/active spans for a 13M-event
// trace. As fat objects that's ~1.9 GB; as typed-array COLUMNS it's ~160 MB,
// resident and off the JS object heap. The render + hit-test then read the
// columns for the visible [t0,t1] via binary search (zoomed in) or the per-lane
// mipmap (zoomed out) instead of iterating an object array every frame.
//
// This module is the store + its query surface; it is populated from the
// existing object output today (so the whole pipeline can migrate behind a
// field-for-field parity test) and will be written to directly by a columnar
// buildWorkerSpans next.

import { SegmentForest, spanBucketMerge, type SpanAgg } from "./segment-forest.js";
import type { CpuSample } from "./columnar-cpu-samples.js";
import type { SpanList } from "./query.js";
import type {
  PointOfInterest,
  PointOfInterestType,
  SchedDelay,
  WorkerLane,
  WorkerSpansResult,
} from "../../types/trace.js";

/** Severity floor for "spawn-delay", in microseconds. The frozen core keeps its
 *  own copy (it must stay self-contained); columnar-pois.test.ts pins them. */
export const DEFAULT_SPAWN_DELAY_THRESHOLD_US = 100;

/** Zero is honoured; anything non-finite or negative falls back to the default
 *  rather than admitting every task. */
export function spawnDelayThresholdNs(thresholdUs?: number): number {
  const us =
    typeof thresholdUs === "number" && Number.isFinite(thresholdUs) && thresholdUs >= 0
      ? thresholdUs
      : DEFAULT_SPAWN_DELAY_THRESHOLD_US;
  return us * 1000;
}

/** Options for pointsOfInterest, mirroring filterPointsOfInterest's opts. */
export interface PoiOpts {
  hasSchedWait?: boolean;
  sortByWorst?: boolean;
  taskInstrumented?: Map<number, boolean>;
  /** Required for the "spawn-delay" filter. */
  taskSpawnTimes?: Map<number, number>;
  spawnDelayThresholdUs?: number;
  /** Required for "off-cpu-poll"; see DetectorInputs.hasOnCpuSamples. */
  hasOnCpuSamples?: boolean;
  /** Required for "off-cpu-active"; see DetectorInputs.hasWorkerCpuTime. */
  hasWorkerCpuTime?: boolean;
  /**
   * Keep at most this many points, under the ACTIVE order - the worst `limit`
   * when `sortByWorst`, else the earliest. Detectors like "uninstrumented" match
   * essentially every poll on a lightly-instrumented trace (millions), and
   * materializing them all costs more memory than the trace itself. Omit for
   * the unbounded result.
   */
  limit?: number;
  /** Receives the TRUE match count, before `limit` truncates. Counts remain
   *  honest even when the returned list is capped. */
  onTotal?: (matched: number) => void;
}

/** A poll span materialized back out of the columns (for hover/click/parity).
 * `cpuSamples`/`schedSamples` are present only after attachCpuSamples() has run;
 * they hold the ACTUAL CpuSample refs (via the CSR index), matching the fat
 * `poll.cpuSamples` the frozen attachCpuSamples pushes. Absent (undefined) when
 * the poll caught no samples - mirroring the fat path's `poll.cpuSamples ??=`. */
export interface PollView {
  start: number;
  end: number;
  taskId: number;
  spawnLocId: string | null;
  spawnLoc: string | null;
  openEnded: boolean;
  cpuSamples?: CpuSample[];
  schedSamples?: CpuSample[];
}
/** A park (idle) span. `schedWait` null when the park->unpark was unsampled. */
export interface ParkView {
  start: number;
  end: number;
  schedWait: number | null;
}

/** A park as consumers read it off EITHER lane representation. The two spell an
 * unsampled park->unpark wait differently -- the columnar store reports null,
 * the frozen fat path omits the field -- so readers must test `!= null`. */
export interface ParkLike {
  start: number;
  end: number;
  schedWait?: number | null;
}
/** An active (on-CPU) span with its cpu/wall ratio. */
export interface ActiveView {
  start: number;
  end: number;
  ratio: number;
}

/** A wake->poll scheduling delay (feeds the wake-delay POI). `poll` is a fresh
 * PollView (consumers read poll.end); shape matches the frozen schedDelay. */
export interface SchedDelayView {
  wakeTime: number;
  pollTime: number;
  delay: number;
  taskId: number;
  wakerTaskId: number;
  worker: number;
  poll: PollView;
}

interface WakeRec {
  timestamp: number;
  wakerTaskId: number;
}

/** Per-task poll index as a typed-array CSR (replaces the fat Map<taskId,
 * {start,end,workerId}[]>). Task `slot` owns [off[slot], off[slot+1]) in the
 * flat start/end/worker arrays, sorted ascending by start. Built once, cached,
 * shared by schedulingDelays + buildSpanDataColumnar's reconstructSpanSegments. */
export interface PollsByTaskCSR {
  slotOf: Map<number, number>;
  off: Int32Array;
  start: Float64Array;
  end: Float64Array;
  worker: Float64Array;
  /** 1 = no matching PollEnd, so `end` is a bound, not the poll's real end. */
  openEnded: Uint8Array;
}

/** Per-task poll aggregate for the Tasks tab index. Scalars only - reduced from
 *  the poll columns without materializing any span. */
export interface TaskPollAggregate {
  taskId: number;
  pollCount: number;
  totalPollNs: number;
  longestPollNs: number;
  /** Start of the task's earliest poll (the CSR slice is start-sorted). */
  firstPollStart: number;
  /** Worker the earliest poll ran on - the "where did this task first run"
   *  reveal target. */
  firstPollWorker: number;
  /** Distinct workers the task ever polled on. */
  workerCount: number;
}

/**
 * Array-like flyweight over a column: `{length, at(i)}` (so it drops into
 * findSpanAt/firstVisibleByEnd), plus `find` and iteration for the `.find(...)`
 * / `for..of` consumers. `at(i)` and iteration MATERIALIZE a fresh view per
 * element (never a reused flyweight), so retaining a returned element is safe -
 * this backs the interaction paths (hover/click/search/task-detail), NOT the
 * per-frame draw loop (which reads the store's windowRange/mipmap directly).
 */
export class ColumnSpanView<T> implements SpanList<T>, Iterable<T> {
  private readonly _len: number;
  private readonly _at: (i: number) => T | undefined;
  constructor(_len: number, _at: (i: number) => T | undefined) {
    this._len = _len;
    this._at = _at;
  }
  get length(): number {
    return this._len;
  }
  at(i: number): T | undefined {
    return this._at(i < 0 ? this._len + i : i);
  }
  find(pred: (v: T, i: number) => boolean): T | undefined {
    for (let i = 0; i < this._len; i++) {
      const v = this._at(i)!;
      if (pred(v, i)) return v;
    }
    return undefined;
  }
  *[Symbol.iterator](): Iterator<T> {
    for (let i = 0; i < this._len; i++) yield this._at(i)!;
  }
}

/** A columnar-backed lane, structurally the WorkerLane shape consumers read.
 * `store`/`w` are the render's dispatch hook: present ONLY on columnar lanes, so
 * the draw path can branch to the store's windowed downsamplers; a fat WorkerLane
 * has neither. */
export interface WorkerLaneView {
  polls: ColumnSpanView<PollView>;
  parks: ColumnSpanView<ParkView>;
  actives: ColumnSpanView<ActiveView>;
  cpuSampleTimes: number[];
  store: ColumnarWorkerSpans;
  w: number;
}

/** A lane as consumers receive it: fat objects on small traces, columnar on
 * big ones. Narrow with isColumnarLane before touching either representation. */
export type LaneSpans = WorkerLane | WorkerLaneView;

/** buildWorkerSpans' result as the APP sees it. Identical to the frozen core's
 * shape except that lanes may be columnar: the frozen function only ever
 * returns fat lanes, so widening WorkerSpansResult itself would misdescribe
 * it. */
export type LaneWorkerSpans = Omit<WorkerSpansResult, "workerSpans"> & {
  workerSpans: Record<number, LaneSpans>;
};

/** True when `lane` is columnar-backed (dispatch guard for render + hit-test). */
export function isColumnarLane(lane: LaneSpans): lane is WorkerLaneView {
  return (lane as Partial<WorkerLaneView>).store !== undefined;
}

/** Which representation the POI + sched-delay detectors run against. The frozen
 * detectors take fat lanes; the columnar store scans its raw columns instead.
 * A discriminated pair so a consumer cannot reach for the wrong one. */
export type LaneSource =
  | { columnar: true; store: ColumnarWorkerSpans }
  | { columnar: false; workerSpans: Record<number, WorkerLane> };

/** The lanes as the frozen fat-path consumers require them. Throws rather than
 * substituting a default: a columnar lane reaching a frozen consumer means the
 * store and lane caches disagreed, and silently dropping it would surface as
 * empty spans/POIs on exactly the big traces this path exists for. */
export function fatLanes(lanes: Record<number, LaneSpans>): Record<number, WorkerLane> {
  const fat: Record<number, WorkerLane> = {};
  for (const [worker, lane] of Object.entries(lanes)) {
    if (isColumnarLane(lane)) {
      throw new Error(`columnar lane for worker ${worker} reached a fat-path consumer`);
    }
    fat[Number(worker)] = lane;
  }
  return fat;
}

/** Pair `store` with the lanes it was built from, so a consumer cannot reach
 * for the representation it is not on. */
export function laneSource(
  store: ColumnarWorkerSpans | null | undefined,
  lanes: Record<number, LaneSpans>,
): LaneSource {
  return store
    ? { columnar: true, store }
    : { columnar: false, workerSpans: fatLanes(lanes) };
}

/** Per-worker poll columns, sorted ascending by start (buildWorkerSpans already
 * emits polls in start order per worker). */
class WorkerPollColumns {
  start: Float64Array;
  end: Float64Array;
  taskId: Float64Array;
  /** index into the store's shared spawn-string table; -1 = null. */
  spawnIdx: Int32Array;
  /** 1 = openEnded (no matching PollEnd), else 0. */
  openEnded: Uint8Array;
  private _forest: SegmentForest<SpanAgg> | null = null;

  // Park spans (start/end sorted): end + NaN-encoded schedWait.
  parkStart: Float64Array;
  parkEnd: Float64Array;
  /** NaN encodes null (unsampled park->unpark). */
  parkSchedWait: Float64Array;
  // Active spans: start/end + cpu/wall ratio.
  activeStart: Float64Array;
  activeEnd: Float64Array;
  activeRatio: Float64Array;

  // cpu/sched samples per poll, as CSR (compressed sparse row) over the sample
  // store's index space. Poll i owns cpuIdx[cpuOff[i], cpuOff[i+1]). Null until
  // attachCpuSamples runs. `cpuSampleTimes` mirrors the fat lane field
  // (source!==1 sample timestamps in sample-iteration order) for drawCpuTicks.
  cpuOff: Int32Array | null = null;
  cpuIdx: Int32Array | null = null;
  schedOff: Int32Array | null = null;
  schedIdx: Int32Array | null = null;
  cpuSampleTimes: number[] = [];

  readonly n: number;
  readonly nParks: number;
  readonly nActives: number;

  constructor(n: number, nParks: number, nActives: number) {
    this.n = n;
    this.nParks = nParks;
    this.nActives = nActives;
    this.start = new Float64Array(n);
    this.end = new Float64Array(n);
    this.taskId = new Float64Array(n);
    this.spawnIdx = new Int32Array(n);
    this.openEnded = new Uint8Array(n);
    this.parkStart = new Float64Array(nParks);
    this.parkEnd = new Float64Array(nParks);
    this.parkSchedWait = new Float64Array(nParks);
    this.activeStart = new Float64Array(nActives);
    this.activeEnd = new Float64Array(nActives);
    this.activeRatio = new Float64Array(nActives);
  }

  /** Binary search: rightmost poll with start <= ts, or -1. Mirrors the frozen
   * attachCpuSamples / findSpanAt search on the start column. */
  pollCovering(ts: number): number {
    let lo = 0, hi = this.n - 1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (this.start[mid]! <= ts) lo = mid + 1;
      else hi = mid - 1;
    }
    return hi >= 0 && ts <= this.end[hi]! ? hi : -1;
  }

  /** Lazily-built per-worker mipmap over the polls, keyed by index (which is
   * start order), aggregating by longest-duration + count. */
  forest(): SegmentForest<SpanAgg> {
    if (this._forest === null) {
      const leaves: SpanAgg[] = new Array(this.n);
      for (let i = 0; i < this.n; i++) {
        leaves[i] = { dur: this.end[i]! - this.start[i]!, count: 1, idx: i };
      }
      this._forest = new SegmentForest(leaves, spanBucketMerge);
    }
    return this._forest;
  }
}

export class ColumnarWorkerSpans {
  private readonly byWorker = new Map<number, WorkerPollColumns>();
  readonly spawnStrings: string[] = [];
  private readonly spawnIntern = new Map<string, number>();
  /** Set by attachCpuSamples; pollAt reads it to hand back real sample refs. */
  private cpuSamplesArr: readonly CpuSample[] | null = null;
  /** Lazily-built, cached per-task poll CSR (shared by schedulingDelays +
   * buildSpanDataColumnar). ~127 MB typed vs 5.3M {start,end,workerId} objects. */
  private _pollsByTaskCSR: PollsByTaskCSR | null = null;

  private intern(s: string | null): number {
    if (s == null) return -1;
    let i = this.spawnIntern.get(s);
    if (i === undefined) {
      i = this.spawnStrings.length;
      this.spawnStrings.push(s);
      this.spawnIntern.set(s, i);
    }
    return i;
  }

  /** @internal builder hooks (ColumnarWorkerSpansBuilder, same module). */
  _internSpawn(s: string | null): number {
    return this.intern(s);
  }
  _ingestWorker(w: number, cols: WorkerPollColumns): void {
    this.byWorker.set(w, cols);
  }

  /** Columnarize the existing object output. Temporary bridge: the same store
   * will soon be filled directly by a columnar buildWorkerSpans. */
  static fromWorkerSpans(
    workerSpans: Record<number, WorkerLane>
  ): ColumnarWorkerSpans {
    const store = new ColumnarWorkerSpans();
    for (const key of Object.keys(workerSpans)) {
      const w = Number(key);
      const lane = workerSpans[w]!;
      const { polls, parks, actives } = lane;
      const cols = new WorkerPollColumns(polls.length, parks.length, actives.length);
      for (let i = 0; i < polls.length; i++) {
        const p = polls[i]!;
        cols.start[i] = p.start;
        cols.end[i] = p.end;
        cols.taskId[i] = p.taskId;
        cols.spawnIdx[i] = store.intern(p.spawnLoc ?? null);
        cols.openEnded[i] = p.openEnded ? 1 : 0;
      }
      for (let i = 0; i < parks.length; i++) {
        cols.parkStart[i] = parks[i]!.start;
        cols.parkEnd[i] = parks[i]!.end;
        cols.parkSchedWait[i] = parks[i]!.schedWait == null ? NaN : parks[i]!.schedWait!;
      }
      for (let i = 0; i < actives.length; i++) {
        cols.activeStart[i] = actives[i]!.start;
        cols.activeEnd[i] = actives[i]!.end;
        cols.activeRatio[i] = actives[i]!.ratio;
      }
      store.byWorker.set(w, cols);
    }
    return store;
  }

  workers(): number[] {
    return [...this.byWorker.keys()];
  }
  pollCount(w: number): number {
    return this.byWorker.get(w)?.n ?? 0;
  }

  parkCount(w: number): number {
    return this.byWorker.get(w)?.nParks ?? 0;
  }
  activeCount(w: number): number {
    return this.byWorker.get(w)?.nActives ?? 0;
  }

  /** Columnar half of the hasWorkerCpuTime gate (see DetectorInputs). */
  anyActiveCpuTime(): boolean {
    for (const c of this.byWorker.values()) {
      for (let i = 0; i < c.nActives; i++) {
        if (c.activeRatio[i]! > 0 && c.activeEnd[i]! > c.activeStart[i]!) return true;
      }
    }
    return false;
  }

  /**
   * Columnar port of the frozen attachCpuSamples: bin each cpu/sched sample onto
   * the poll covering its timestamp (per worker, binary search on the start
   * column), and write sample.spawnLoc/inPoll - byte-identical to the fat pass.
   * Instead of pushing sample refs onto poll objects it fills a per-worker CSR
   * (cpuOff/cpuIdx, schedOff/schedIdx) over the sample store's index space, so
   * pollAt() can hand back the same sample arrays without 5.3M fat poll objects.
   *
   * source===1 -> sched sample; else cpu sample (and its ts joins cpuSampleTimes,
   * matching `if (sample.source !== 1) spans.cpuSampleTimes.push(...)`).
   */
  attachCpuSamples(samples: readonly CpuSample[]): {
    pollsWithCpuSamples: number;
    pollsWithSchedSamples: number;
  } {
    this.cpuSamplesArr = samples;
    const m = samples.length;
    // Pass 1: resolve each sample's (worker cols, poll idx, isSched); count per
    // poll; write spawnLoc/inPoll + cpuSampleTimes exactly as the frozen pass.
    const samplePoll = new Int32Array(m).fill(-1);
    for (const c of this.byWorker.values()) {
      c.cpuOff = new Int32Array(c.n + 1);
      c.schedOff = new Int32Array(c.n + 1);
      c.cpuSampleTimes = [];
    }
    for (let si = 0; si < m; si++) {
      const sample = samples[si]!;
      const c = this.byWorker.get(sample.workerId);
      if (!c) { sample.spawnLoc = null; continue; }
      const isSched = sample.source === 1;
      if (!isSched) c.cpuSampleTimes.push(sample.timestamp);
      const pi = c.pollCovering(sample.timestamp);
      if (pi >= 0) {
        samplePoll[si] = pi;
        // count in the [i+1] slot so a prefix-sum turns offsets into starts.
        if (isSched) c.schedOff![pi + 1]!++; else c.cpuOff![pi + 1]!++;
        const spawn = c.spawnIdx[pi]! < 0 ? null : this.spawnStrings[c.spawnIdx[pi]!]!;
        sample.spawnLoc = spawn;
        sample.inPoll = true;
      } else {
        sample.spawnLoc = null;
        sample.inPoll = false;
      }
    }
    // Prefix-sum counts -> CSR offsets; allocate idx arrays.
    let pollsWithCpuSamples = 0, pollsWithSchedSamples = 0;
    for (const c of this.byWorker.values()) {
      const cpuOff = c.cpuOff!, schedOff = c.schedOff!;
      for (let i = 0; i < c.n; i++) {
        if (cpuOff[i + 1]! > 0) pollsWithCpuSamples++;
        if (schedOff[i + 1]! > 0) pollsWithSchedSamples++;
        cpuOff[i + 1]! += cpuOff[i]!;
        schedOff[i + 1]! += schedOff[i]!;
      }
      c.cpuIdx = new Int32Array(cpuOff[c.n]!);
      c.schedIdx = new Int32Array(schedOff[c.n]!);
    }
    // Pass 2: place sample indices into the CSR via a per-poll cursor. Iterating
    // samples in the same order preserves within-poll order (== fat push order).
    const cpuCur = new Map<number, Int32Array>();
    const schedCur = new Map<number, Int32Array>();
    for (const [w, c] of this.byWorker) {
      cpuCur.set(w, c.cpuOff!.slice(0, c.n));
      schedCur.set(w, c.schedOff!.slice(0, c.n));
    }
    for (let si = 0; si < m; si++) {
      const pi = samplePoll[si]!;
      if (pi < 0) continue;
      const sample = samples[si]!;
      const c = this.byWorker.get(sample.workerId)!;
      if (sample.source === 1) c.schedIdx![schedCur.get(sample.workerId)![pi]!++] = si;
      else c.cpuIdx![cpuCur.get(sample.workerId)![pi]!++] = si;
    }
    return { pollsWithCpuSamples, pollsWithSchedSamples };
  }

  /** Materialize poll `i` of worker `w` (hover/click/parity). Reconstructs
   * cpuSamples/schedSamples from the CSR (real sample refs) when attached. */
  pollAt(w: number, i: number): PollView | undefined {
    const c = this.byWorker.get(w);
    if (!c || i < 0 || i >= c.n) return undefined;
    const s = c.spawnIdx[i]! < 0 ? null : this.spawnStrings[c.spawnIdx[i]!]!;
    const view: PollView = {
      start: c.start[i]!, end: c.end[i]!, taskId: c.taskId[i]!,
      spawnLocId: s, spawnLoc: s, openEnded: c.openEnded[i] === 1,
    };
    if (this.cpuSamplesArr && c.cpuOff && c.cpuOff[i + 1]! > c.cpuOff[i]!) {
      view.cpuSamples = this.sliceSamples(c.cpuIdx!, c.cpuOff[i]!, c.cpuOff[i + 1]!);
    }
    if (this.cpuSamplesArr && c.schedOff && c.schedOff[i + 1]! > c.schedOff[i]!) {
      view.schedSamples = this.sliceSamples(c.schedIdx!, c.schedOff[i]!, c.schedOff[i + 1]!);
    }
    return view;
  }

  private sliceSamples(idx: Int32Array, lo: number, hi: number): CpuSample[] {
    const arr = this.cpuSamplesArr!;
    const out = new Array<CpuSample>(hi - lo);
    for (let k = lo; k < hi; k++) out[k - lo] = arr[idx[k]!]!;
    return out;
  }

  // ── cheap poll scalar reads (no PollView / sample-slice materialization) ──
  // For the every-frame render window loops (open-ended markers, waker/sched
  // scans) that need one column, not a full flyweight. Callers reach these
  // through a WorkerLaneView, whose `w` came from this store, so an unknown
  // worker is a wiring bug: throw rather than return a sentinel that would
  // silently paint a wrong (or invisible) marker every frame.
  private columnsFor(w: number): WorkerPollColumns {
    const c = this.byWorker.get(w);
    if (c === undefined) throw new Error(`no poll columns for worker ${w}`);
    return c;
  }
  pollStartAt(w: number, i: number): number { return this.columnsFor(w).start[i]!; }
  pollEndAt(w: number, i: number): number { return this.columnsFor(w).end[i]!; }
  pollOpenEndedAt(w: number, i: number): boolean { return this.columnsFor(w).openEnded[i] === 1; }

  /** Per-worker cpu-sample tick timestamps (source!==1), for drawCpuTicks. */
  cpuSampleTimes(w: number): number[] {
    return this.byWorker.get(w)?.cpuSampleTimes ?? [];
  }

  /** All polls of a task across workers, sorted by start (task-detail view).
   * Scans the raw taskId columns and materializes ONLY the task's polls, not the
   * whole flyweight lane (which iterating workerSpans[w].polls would do). */
  pollsForTask(taskId: number): PollView[] {
    const out: PollView[] = [];
    for (const [w, c] of this.byWorker) {
      for (let i = 0; i < c.n; i++) {
        if (c.taskId[i] === taskId) out.push(this.pollAt(w, i)!);
      }
    }
    out.sort((a, b) => a.start - b.start);
    return out;
  }

  /**
   * Columnar port of the frozen filterPointsOfInterest: scans RAW poll/park
   * columns (no per-span materialization - the whole point, vs the fat detector
   * iterating workerSpans[w].polls which would materialize every flyweight) and
   * emits a POI only for matches, with a lightweight {start,end,taskId} span for
   * the rail's jump. wake-delay reads the supplied schedDelays (already columnar
   * via schedulingDelays). Output (points + sort) is identical to the fat path.
   */
  pointsOfInterest(
    filterType: PointOfInterestType,
    workerIds: readonly number[],
    schedDelays: readonly (SchedDelayView | SchedDelay)[],
    opts: PoiOpts = {}
  ): PointOfInterest[] {
    const hasSchedWait = !!opts.hasSchedWait;
    const points: PointOfInterest[] = [];
    const pollSpanForJump = (c: WorkerPollColumns, i: number): { start: number; end: number; taskId: number } => ({
      start: c.start[i]!, end: c.end[i]!, taskId: c.taskId[i]!,
    });

    const order = opts.sortByWorst
      ? (a: PointOfInterest, b: PointOfInterest): number => b.value - a.value
      : (a: PointOfInterest, b: PointOfInterest): number => a.time - b.time;
    const cap = opts.limit !== undefined && opts.limit > 0 ? opts.limit : Infinity;
    let matched = 0;
    // Bounded retention: let the buffer grow to 2x the cap, then sort and drop
    // the tail. Amortized O(n) with O(cap) live objects, so a detector matching
    // millions of polls never holds more than a bounded slice of them.
    const compact = (): void => {
      points.sort(order);
      points.length = Math.min(points.length, cap);
    };
    const add = (p: PointOfInterest): void => {
      matched++;
      points.push(p);
      if (points.length >= cap * 2) compact();
    };

    for (const w of workerIds) {
      const c = this.byWorker.get(w);
      if (!c) continue;
      if (filterType === "sched") {
        for (let i = 0; i < c.nParks; i++) {
          const sw = c.parkSchedWait[i]!;
          if (hasSchedWait && !Number.isNaN(sw) && sw > 100) {
            add({
              time: c.parkEnd[i]! - sw, worker: w, type: "sched", value: sw,
              span: { start: c.parkStart[i]!, end: c.parkEnd[i]! },
            });
          }
        }
      } else if (filterType === "long-poll") {
        for (let i = 0; i < c.n; i++) {
          const durMs = (c.end[i]! - c.start[i]!) / 1e6;
          if (durMs > 1) add({ time: c.start[i]!, worker: w, type: "long-poll", value: durMs, span: pollSpanForJump(c, i) });
        }
      } else if (filterType === "cpu-sampled") {
        const cpuOff = c.cpuOff, schedOff = c.schedOff;
        for (let i = 0; i < c.n; i++) {
          const cpuCount = cpuOff ? cpuOff[i + 1]! - cpuOff[i]! : 0;
          const schedCount = schedOff ? schedOff[i + 1]! - schedOff[i]! : 0;
          if (cpuCount + schedCount > 0) {
            add({ time: c.start[i]!, worker: w, type: "cpu-sampled", value: (c.end[i]! - c.start[i]!) / 1e6, span: pollSpanForJump(c, i) });
          }
        }
      } else if (filterType === "off-cpu-poll" && opts.hasOnCpuSamples) {
        // schedOff is NOT consulted: off-CPU stacks confirm the finding.
        const cpuOff = c.cpuOff;
        for (let i = 0; i < c.n; i++) {
          // An open-ended poll's `end` is a fabricated bound, so an unobserved
          // gap would otherwise rank as the worst off-CPU poll in the trace.
          if (c.openEnded[i] === 1) continue;
          if (cpuOff && cpuOff[i + 1]! - cpuOff[i]! > 0) continue;
          const durMs = (c.end[i]! - c.start[i]!) / 1e6;
          if (durMs > 1) {
            add({ time: c.start[i]!, worker: w, type: "off-cpu-poll", value: durMs, span: pollSpanForJump(c, i) });
          }
        }
      } else if (filterType === "off-cpu-active" && opts.hasWorkerCpuTime) {
        // Thresholds mirror the red-flags script's `cpu-contention` check.
        for (let i = 0; i < c.nActives; i++) {
          const start = c.activeStart[i]!, end = c.activeEnd[i]!, ratio = c.activeRatio[i]!;
          const wall = end - start;
          if (wall > 1e6 && ratio < 0.5) {
            add({ time: start, worker: w, type: "off-cpu-active", value: (1 - ratio) * wall, span: { start, end } });
          }
        }
      }
    }

    if (filterType === "wake-delay") {
      for (const sd of schedDelays) {
        const delayUs = sd.delay / 1000;
        if (delayUs > 100) {
          add({ time: sd.wakeTime, worker: sd.worker, type: "wake-delay", value: delayUs, span: sd.poll, schedDelay: sd });
        }
      }
    }

    if (filterType === "uninstrumented" && opts.taskInstrumented) {
      const ti = opts.taskInstrumented;
      for (const w of workerIds) {
        const c = this.byWorker.get(w);
        if (!c) continue;
        for (let i = 0; i < c.n; i++) {
          const tid = c.taskId[i];
          if (tid && ti.get(tid) === false) {
            add({ time: c.start[i]!, worker: w, type: "uninstrumented", value: (c.end[i]! - c.start[i]!) / 1e6, span: pollSpanForJump(c, i) });
          }
        }
      }
    }

    if (filterType === "spawn-delay" && opts.taskSpawnTimes) {
      const spawns = opts.taskSpawnTimes;
      const thresholdNs = spawnDelayThresholdNs(opts.spawnDelayThresholdUs);
      // Each CSR task slice is start-sorted, so slot 0 is the task's first poll
      // across every worker - no reduction pass, no PollView materialized.
      //
      // `openEnded` is deliberately NOT filtered (unlike total/longest in
      // taskAggregates): a PollStart proves the START this measures from, and
      // only `end` is the unproven bound.
      const csr = this.pollsByTaskCSR();
      for (const [taskId, slot] of csr.slotOf) {
        const lo = csr.off[slot]!;
        if (csr.off[slot + 1]! <= lo) continue;
        // Unknown spawn: task tracking off, or it fell outside a windowed load.
        const spawnTs = spawns.get(taskId);
        if (spawnTs === undefined) continue;
        const start = csr.start[lo]!;
        const delay = start - spawnTs;
        if (delay > thresholdNs) {
          add({
            time: spawnTs,
            worker: csr.worker[lo]!,
            type: "spawn-delay",
            value: delay / 1000,
            span: { start, end: csr.end[lo]!, taskId },
          });
        }
      }
    }

    compact();
    opts.onTotal?.(matched);
    return points as PointOfInterest[];
  }

  /** Task owning the poll covering timestamp `t` on worker `w`, or null - the
   * columnar resolveSpanTask (buildSpanData span->task lookup). Non-overlapping
   * sorted polls => at most one covers `t`, so pollCovering is exact. */
  resolveSpanTaskAt(w: number, t: number): number | null {
    const c = this.byWorker.get(w);
    if (!c) return null;
    const i = c.pollCovering(t);
    if (i < 0) return null;
    const tid = c.taskId[i];
    return tid ? tid : null;
  }

  /** Columnar indexPollsByTask as a cached typed-array CSR (task -> its polls
   * {start,end,worker} sorted by start, merged across workers). Only the
   * lightweight index is built - never a PollView, never per-poll objects. */
  pollsByTaskCSR(): PollsByTaskCSR {
    if (this._pollsByTaskCSR) return this._pollsByTaskCSR;
    const slotOf = new Map<number, number>();
    const counts: number[] = [];
    // Pass 1: dense-slot each task + count.
    for (const c of this.byWorker.values()) {
      for (let i = 0; i < c.n; i++) {
        const t = c.taskId[i];
        if (!t) continue;
        let s = slotOf.get(t);
        if (s === undefined) { s = counts.length; slotOf.set(t, s); counts.push(0); }
        counts[s]!++;
      }
    }
    const T = counts.length;
    const off = new Int32Array(T + 1);
    for (let s = 0; s < T; s++) off[s + 1] = off[s]! + counts[s]!;
    const start = new Float64Array(off[T]!);
    const end = new Float64Array(off[T]!);
    const worker = new Float64Array(off[T]!);
    const openEnded = new Uint8Array(off[T]!);
    const cur = off.slice(0, T);
    // Pass 2: fill.
    for (const [w, c] of this.byWorker) {
      for (let i = 0; i < c.n; i++) {
        const t = c.taskId[i];
        if (!t) continue;
        const p = cur[slotOf.get(t)!]!++;
        start[p] = c.start[i]!; end[p] = c.end[i]!; worker[p] = w;
        openEnded[p] = c.openEnded[i]!;
      }
    }
    // Sort each task slice by start (stable - matches the fat arr.sort tie-break).
    for (let s = 0; s < T; s++) sortTriByStart(start, end, worker, openEnded, off[s]!, off[s + 1]!);
    return (this._pollsByTaskCSR = { slotOf, off, start, end, worker, openEnded });
  }

  /**
   * Per-task poll aggregates for the task index, reduced straight off the CSR:
   * poll count, total + longest poll time, and the first poll's start/worker
   * (the CSR slice is start-sorted, so slot 0 is the first). Never materializes
   * a PollView - the whole point of the columnar path. Tasks that never polled
   * are absent here; the caller unions them in from the spawn map.
   *
   * An open-ended poll has no matching PollEnd, so its `end` is only an upper
   * bound - the interval up to it can be mostly idle (a park whose events were
   * lost, or a block_in_place handoff at an unknown instant). It still counts as
   * a poll and still names its workers, but its duration is unknown and is left
   * out of `total`/`longest` rather than reported as time the task spent
   * running. Including it made an unobserved gap read as the task's longest
   * poll.
   */
  taskAggregates(): TaskPollAggregate[] {
    const csr = this.pollsByTaskCSR();
    const out: TaskPollAggregate[] = [];
    for (const [taskId, slot] of csr.slotOf) {
      const lo = csr.off[slot]!;
      const hi = csr.off[slot + 1]!;
      if (hi <= lo) continue;
      let total = 0;
      let longest = 0;
      const workers = new Set<number>();
      for (let p = lo; p < hi; p++) {
        workers.add(csr.worker[p]!);
        if (csr.openEnded[p] === 1) continue;
        const dur = csr.end[p]! - csr.start[p]!;
        total += dur;
        if (dur > longest) longest = dur;
      }
      out.push({
        taskId,
        pollCount: hi - lo,
        totalPollNs: total,
        longestPollNs: longest,
        firstPollStart: csr.start[lo]!,
        firstPollWorker: csr.worker[lo]!,
        workerCount: workers.size,
      });
    }
    return out;
  }

  /**
   * Columnar port of the frozen pixelDownsampleSpans for POLLS: one
   * representative (weight-winning) poll per pixel column across [viewStart,
   * viewEnd], iterating the start column from `startIdx`. Weight matches the
   * render: a selected task's polls force-win (Infinity), else later start wins
   * - computed from raw columns so only the <=pw WINNERS are materialized (never
   * per-candidate). Byte-identical representatives to the frozen reducer.
   */
  downsamplePolls(
    w: number,
    startIdx: number,
    viewStart: number,
    viewEnd: number,
    pw: number,
    selectedTaskId: number | null
  ): PollView[] {
    const c = this.byWorker.get(w);
    const out: PollView[] = [];
    if (!c || pw <= 0 || viewEnd <= viewStart) return out;
    const span2px = pw / (viewEnd - viewStart);
    const weight = (i: number): number =>
      selectedTaskId && c.taskId[i] === selectedTaskId ? Infinity : c.start[i]!;
    let curPx = -1, bestIdx = -1, bestW = -Infinity;
    for (let i = startIdx; i < c.n; i++) {
      const st = c.start[i]!;
      if (st > viewEnd) break;
      let px = ((st - viewStart) * span2px) | 0;
      if (px < 0) px = 0; else if (px >= pw) px = pw - 1;
      if (px !== curPx) {
        if (bestIdx >= 0) out.push(this.pollAt(w, bestIdx)!);
        curPx = px; bestIdx = i; bestW = weight(i);
      } else {
        const wg = weight(i);
        if (wg > bestW) { bestW = wg; bestIdx = i; }
      }
    }
    if (bestIdx >= 0) out.push(this.pollAt(w, bestIdx)!);
    return out;
  }

  /** Columnar pixelDownsampleSpans for ACTIVES: longest (end-start) wins the
   * pixel column - the CPU-tint representative in drawLaneBackground. */
  downsampleActives(
    w: number,
    startIdx: number,
    viewStart: number,
    viewEnd: number,
    pw: number
  ): ActiveView[] {
    const c = this.byWorker.get(w);
    const out: ActiveView[] = [];
    if (!c || pw <= 0 || viewEnd <= viewStart) return out;
    const span2px = pw / (viewEnd - viewStart);
    let curPx = -1, bestIdx = -1, bestW = -Infinity;
    for (let i = startIdx; i < c.nActives; i++) {
      const st = c.activeStart[i]!;
      if (st > viewEnd) break;
      let px = ((st - viewStart) * span2px) | 0;
      if (px < 0) px = 0; else if (px >= pw) px = pw - 1;
      const dur = c.activeEnd[i]! - st;
      if (px !== curPx) {
        if (bestIdx >= 0) out.push(this.activeAt(w, bestIdx)!);
        curPx = px; bestIdx = i; bestW = dur;
      } else if (dur > bestW) { bestW = dur; bestIdx = i; }
    }
    if (bestIdx >= 0) out.push(this.activeAt(w, bestIdx)!);
    return out;
  }

  /** Columnar pixelDownsampleSpans for PARKS: longest (end-start) wins the
   * pixel column - the idle-band representative in drawParks. */
  downsampleParks(
    w: number,
    startIdx: number,
    viewStart: number,
    viewEnd: number,
    pw: number
  ): ParkView[] {
    const c = this.byWorker.get(w);
    const out: ParkView[] = [];
    if (!c || pw <= 0 || viewEnd <= viewStart) return out;
    const span2px = pw / (viewEnd - viewStart);
    let curPx = -1, bestIdx = -1, bestW = -Infinity;
    for (let i = startIdx; i < c.nParks; i++) {
      const st = c.parkStart[i]!;
      if (st > viewEnd) break;
      let px = ((st - viewStart) * span2px) | 0;
      if (px < 0) px = 0; else if (px >= pw) px = pw - 1;
      const dur = c.parkEnd[i]! - st;
      if (px !== curPx) {
        if (bestIdx >= 0) out.push(this.parkAt(w, bestIdx)!);
        curPx = px; bestIdx = i; bestW = dur;
      } else if (dur > bestW) { bestW = dur; bestIdx = i; }
    }
    if (bestIdx >= 0) out.push(this.parkAt(w, bestIdx)!);
    return out;
  }

  /** First park index whose end >= viewStart. */
  firstVisiblePark(w: number, viewStart: number): number {
    const c = this.byWorker.get(w);
    if (!c) return 0;
    let lo = 0, hi = c.nParks - 1;
    while (lo <= hi) { const mid = (lo + hi) >> 1; if (c.parkEnd[mid]! < viewStart) lo = mid + 1; else hi = mid - 1; }
    return lo;
  }

  /** First poll index whose end >= viewStart (start-sorted) - the columnar
   * firstVisible; feeds downsamplePolls' startIdx. */
  firstVisiblePoll(w: number, viewStart: number): number {
    const c = this.byWorker.get(w);
    if (!c) return 0;
    let lo = 0, hi = c.n - 1;
    while (lo <= hi) { const mid = (lo + hi) >> 1; if (c.end[mid]! < viewStart) lo = mid + 1; else hi = mid - 1; }
    return lo;
  }

  /** First active index whose end >= viewStart. */
  firstVisibleActive(w: number, viewStart: number): number {
    const c = this.byWorker.get(w);
    if (!c) return 0;
    let lo = 0, hi = c.nActives - 1;
    while (lo <= hi) { const mid = (lo + hi) >> 1; if (c.activeEnd[mid]! < viewStart) lo = mid + 1; else hi = mid - 1; }
    return lo;
  }

  /** A columnar-backed lane view for worker `w` - the WorkerLane shape the
   * interaction/model consumers read, without materializing the fat spans. */
  laneView(w: number): WorkerLaneView {
    return {
      polls: new ColumnSpanView(this.pollCount(w), (i) => this.pollAt(w, i)),
      parks: new ColumnSpanView(this.parkCount(w), (i) => this.parkAt(w, i)),
      actives: new ColumnSpanView(this.activeCount(w), (i) => this.activeAt(w, i)),
      cpuSampleTimes: this.cpuSampleTimes(w),
      store: this,
      w,
    };
  }

  /** The full workerSpans dict (one lane view per worker), the drop-in for the
   * fat buildWorkerSpans `workerSpans` on the columnar path. Cheap: one view
   * object per worker, not per span. */
  workerLanes(): Record<number, WorkerLaneView> {
    const out: Record<number, WorkerLaneView> = {};
    for (const w of this.byWorker.keys()) out[w] = this.laneView(w);
    return out;
  }
  parkAt(w: number, i: number): ParkView | undefined {
    const c = this.byWorker.get(w);
    if (!c || i < 0 || i >= c.nParks) return undefined;
    const sw = c.parkSchedWait[i]!;
    return { start: c.parkStart[i]!, end: c.parkEnd[i]!, schedWait: Number.isNaN(sw) ? null : sw };
  }
  activeAt(w: number, i: number): ActiveView | undefined {
    const c = this.byWorker.get(w);
    if (!c || i < 0 || i >= c.nActives) return undefined;
    return { start: c.activeStart[i]!, end: c.activeEnd[i]!, ratio: c.activeRatio[i]! };
  }

  /**
   * Half-open index range [lo, hi) of worker w's polls that intersect [t0, t1]
   * (end >= t0 and start <= t1). Binary search on the start column for the
   * right edge; a left prefix scan for the (rare) long poll straddling t0 -
   * mirrors the draw-time clip today. O(log n + result).
   */
  windowRange(w: number, t0: number, t1: number): { lo: number; hi: number } {
    const c = this.byWorker.get(w);
    if (!c) return { lo: 0, hi: 0 };
    const start = c.start;
    const n = c.n;
    // hi = first poll with start > t1
    let a = 0, b = n;
    while (a < b) { const m = (a + b) >>> 1; if (start[m]! <= t1) a = m + 1; else b = m; }
    const hi = a;
    // lo = first poll (by start order) whose end >= t0. Since polls are
    // start-sorted (not end-sorted), scan back from the start-lower-bound; the
    // number of long straddlers is small in practice.
    a = 0; b = hi;
    while (a < b) { const m = (a + b) >>> 1; if (c.end[m]! < t0) a = m + 1; else b = m; }
    // a is a start-order lower bound on end>=t0 among a run; walk back to catch
    // an earlier long poll whose end also reaches t0.
    let lo = a;
    while (lo > 0 && c.end[lo - 1]! >= t0) lo--;
    return { lo, hi };
  }

  /** LOD query for worker w over [t0,t1] at `resolution` ns/pixel. Returns one
   * representative poll (longest) + count per pixel bucket - the zoomed-out
   * render path. */
  mipmap(
    w: number,
    t0: number,
    t1: number,
    resolution: number
  ): { ts: number; dur: number; count: number; idx: number }[] {
    const c = this.byWorker.get(w);
    if (!c || c.n === 0) return [];
    const { lo, hi } = this.windowRange(w, t0, t1);
    if (lo >= hi) return [];
    const forest = c.forest();
    const out: { ts: number; dur: number; count: number; idx: number }[] = [];
    // Walk buckets of `resolution` ns across the window; for each, the poll
    // index sub-range is found by scanning start-sorted starts.
    let k = lo;
    for (let bucketStart = Math.floor(c.start[lo]! / resolution) * resolution;
         bucketStart <= t1 && k < hi;
         bucketStart += resolution) {
      const bucketEnd = bucketStart + resolution;
      const bLo = k;
      while (k < hi && c.start[k]! < bucketEnd) k++;
      if (k > bLo) {
        const agg = forest.query(bLo, k);
        if (agg) out.push({ ts: c.start[agg.idx]!, dur: agg.dur, count: agg.count, idx: agg.idx });
      }
    }
    return out;
  }

  /**
   * Columnar port of the frozen computeSchedulingDelays. For each poll of a
   * task, find the latest wake before its start; if that wake landed mid an
   * earlier poll of the same task, measure from that poll's end. Emits a
   * schedDelay per poll with delay in (0, 1e9), sorted by wakeTime - identical
   * to the fat pass. `pollsByTask` (5.3M poll refs in the fat pass) becomes a
   * per-task CSR over Float64 start/end columns (~85 MB, not 1.9 GB of objects),
   * transient to this call.
   */
  schedulingDelays(
    workerIds: number[],
    wakesByTask: Record<number, WakeRec[]>
  ): SchedDelayView[] {
    // Shared per-task poll CSR (cached; also used by buildSpanDataColumnar).
    const { slotOf: slot, off, start: pStart, end: pEnd } = this.pollsByTaskCSR();
    const out: SchedDelayView[] = [];
    for (const w of workerIds) {
      const c = this.byWorker.get(w);
      if (!c) continue;
      for (let i = 0; i < c.n; i++) {
        const taskId = c.taskId[i];
        if (!taskId) continue;
        const wakes = wakesByTask[taskId];
        if (!wakes || !wakes.length) continue;
        const sStart = c.start[i]!;
        // latest wake with timestamp <= sStart
        let lo = 0, hi = wakes.length - 1, best = -1;
        while (lo <= hi) {
          const mid = (lo + hi) >> 1;
          if (wakes[mid]!.timestamp <= sStart) { best = mid; lo = mid + 1; } else hi = mid - 1;
        }
        if (best < 0) continue;
        const wake = wakes[best]!;
        let effectiveWake = wake.timestamp;
        const s = slot.get(taskId);
        if (s !== undefined) {
          // rightmost poll of the task with start <= wake.timestamp
          let plo = off[s]!, phi = off[s + 1]! - 1, pbest = -1;
          while (plo <= phi) {
            const pmid = (plo + phi) >> 1;
            if (pStart[pmid]! <= wake.timestamp) { pbest = pmid; plo = pmid + 1; } else phi = pmid - 1;
          }
          if (pbest >= 0 && pStart[pbest]! < sStart && wake.timestamp <= pEnd[pbest]!) {
            effectiveWake = pEnd[pbest]!;
          }
        }
        const delay = sStart - effectiveWake;
        if (delay > 0 && delay < 1e9) {
          out.push({
            wakeTime: effectiveWake, pollTime: sStart, delay, taskId,
            wakerTaskId: wake.wakerTaskId, worker: w, poll: this.pollAt(w, i)!,
          });
        }
      }
    }
    out.sort((a, b) => a.wakeTime - b.wakeTime);
    return out;
  }
}

/** Per-worker growable number[] columns during a direct build; frozen to typed
 * arrays in finish(). */
class WorkerColBuilder {
  pStart: number[] = []; pEnd: number[] = []; pTask: number[] = []; pSpawn: number[] = []; pOpen: number[] = [];
  kStart: number[] = []; kEnd: number[] = []; kWait: number[] = [];
  aStart: number[] = []; aEnd: number[] = []; aRatio: number[] = [];
}

/**
 * Builds a ColumnarWorkerSpans DIRECTLY from the span state machine, never
 * materializing fat {start,end,...} span objects: the reconstruction pushes
 * scalars into per-worker number[] columns (~200 MB transient for 5.3M spans vs
 * 1.9 GB of objects), frozen to typed arrays in finish(). Spawn strings are
 * interned into the store's shared table. Parks with a null schedWait push NaN.
 */
export class ColumnarWorkerSpansBuilder {
  private readonly builders = new Map<number, WorkerColBuilder>();
  private readonly store = new ColumnarWorkerSpans();

  constructor(workerIds: readonly number[]) {
    for (const w of workerIds) this.builders.set(w, new WorkerColBuilder());
  }

  private col(w: number): WorkerColBuilder {
    let b = this.builders.get(w);
    if (b === undefined) { b = new WorkerColBuilder(); this.builders.set(w, b); }
    return b;
  }

  pushPoll(w: number, start: number, end: number, taskId: number, spawnLoc: string | null, openEnded: boolean): void {
    const b = this.col(w);
    b.pStart.push(start); b.pEnd.push(end); b.pTask.push(taskId);
    b.pSpawn.push(this.store._internSpawn(spawnLoc)); b.pOpen.push(openEnded ? 1 : 0);
  }
  pushPark(w: number, start: number, end: number, schedWait: number | null): void {
    const b = this.col(w);
    b.kStart.push(start); b.kEnd.push(end); b.kWait.push(schedWait == null ? NaN : schedWait);
  }
  pushActive(w: number, start: number, end: number, ratio: number): void {
    const b = this.col(w);
    b.aStart.push(start); b.aEnd.push(end); b.aRatio.push(ratio);
  }

  finish(): ColumnarWorkerSpans {
    for (const [w, b] of this.builders) {
      const cols = new WorkerPollColumns(b.pStart.length, b.kStart.length, b.aStart.length);
      cols.start.set(b.pStart); cols.end.set(b.pEnd); cols.taskId.set(b.pTask);
      cols.spawnIdx.set(b.pSpawn); cols.openEnded.set(b.pOpen);
      cols.parkStart.set(b.kStart); cols.parkEnd.set(b.kEnd); cols.parkSchedWait.set(b.kWait);
      cols.activeStart.set(b.aStart); cols.activeEnd.set(b.aEnd); cols.activeRatio.set(b.aRatio);
      this.store._ingestWorker(w, cols);
    }
    return this.store;
  }
}

/** Stable sort of the parallel (start,end,worker) slice [lo,hi) by start
 * ascending; ties keep insertion order (== the fat arr.sort tie-break, so the
 * mid-poll binary search + segment reconstruction pick the same poll). */
function sortTriByStart(
  pStart: Float64Array, pEnd: Float64Array, pWorker: Float64Array, pOpen: Uint8Array,
  lo: number, hi: number
): void {
  const n = hi - lo;
  if (n < 2) return;
  // Array.prototype.sort is stable in V8; the index tiebreak keeps it stable
  // across engines regardless.
  const arr = Array.from({ length: n }, (_, k) => lo + k);
  arr.sort((x, y) => pStart[x]! - pStart[y]! || x - y);
  const ts = new Float64Array(n), te = new Float64Array(n), tw = new Float64Array(n);
  const to = new Uint8Array(n);
  for (let k = 0; k < n; k++) {
    ts[k] = pStart[arr[k]!]!; te[k] = pEnd[arr[k]!]!; tw[k] = pWorker[arr[k]!]!;
    to[k] = pOpen[arr[k]!]!;
  }
  pStart.set(ts, lo);
  pEnd.set(te, lo);
  pWorker.set(tw, lo);
  pOpen.set(to, lo);
}

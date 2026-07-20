// Columnar reimplementation of the frozen core's buildWorkerSpans
// (trace_analysis.js). The frozen version RETAINS event objects (pushes them
// into per-worker buckets, sorts, re-iterates), which is incompatible with the
// flyweight columnar store. This version retains INDICES into the column store,
// sorts indices by the ts column, and runs the identical span state machine
// reading fields by index. Output is field-for-field identical to the frozen
// function (verified in worker-spans-columnar.test.ts) EXCEPT `perWorker`, which
// no consumer reads (it returns index buckets instead of event objects).
//
// The span state machine emits through a SpanEmitter so the SAME reconstruction
// feeds either the fat WorkerLane dict (FatSpanEmitter, the default - keeps the
// frozen-parity output + test) or the typed-array store directly
// (StoreSpanEmitter, the big-trace path - never builds a fat span object).
//
// Keep this in lockstep with trace_analysis.js buildWorkerSpans (lines ~438-625).

import { EVT } from "./columnar-events.js";
import type { ColumnarEvents } from "./columnar-events.js";
import { ColumnarWorkerSpansBuilder, type ColumnarWorkerSpans } from "./columnar-worker-spans.js";
import type {
  BlockInPlaceGap,
  PollSpan,
  WorkerLane,
  WorkerSpansResult,
} from "../../../trace_analysis.js";

type Rec<T> = Record<string, T>;

const defaultMeta = { taskId: 0, spawnLocId: 0 as number | string | null, spawnLoc: null as string | null };

/** Sink for reconstructed spans. `schedWait` omitted (undefined) => the
 * trace-end park close, which the frozen path pushes WITHOUT a schedWait key. */
interface SpanEmitter {
  poll(w: number, start: number, end: number, taskId: number, spawnLocId: number | string | null, spawnLoc: string | null, openEnded: boolean): void;
  park(w: number, start: number, end: number, schedWait?: number | null): void;
  active(w: number, start: number, end: number, ratio: number): void;
}

/** Builds the fat WorkerLane dict, byte-identical to the frozen output. */
class FatSpanEmitter implements SpanEmitter {
  readonly workerSpans: Rec<WorkerLane> = {};
  ensure(w: number): void {
    if (this.workerSpans[w] === undefined) this.workerSpans[w] = { polls: [], parks: [], actives: [], cpuSampleTimes: [] };
  }
  poll(w: number, start: number, end: number, taskId: number, spawnLocId: number | string | null, spawnLoc: string | null, openEnded: boolean): void {
    const poll: PollSpan = { start, end, taskId, spawnLocId, spawnLoc };
    if (openEnded) poll.openEnded = true;
    this.workerSpans[w]!.polls.push(poll);
  }
  park(w: number, start: number, end: number, schedWait?: number | null): void {
    // Omit the key entirely on the trace-end close, matching the frozen push.
    this.workerSpans[w]!.parks.push(schedWait === undefined ? { start, end } : { start, end, schedWait });
  }
  active(w: number, start: number, end: number, ratio: number): void {
    this.workerSpans[w]!.actives.push({ start, end, ratio });
  }
}

/** Builds the typed-array store directly - no fat span object ever exists. */
class StoreSpanEmitter implements SpanEmitter {
  private readonly b: ColumnarWorkerSpansBuilder;
  constructor(b: ColumnarWorkerSpansBuilder) {
    this.b = b;
  }
  poll(w: number, start: number, end: number, taskId: number, _spawnLocId: number | string | null, spawnLoc: string | null, openEnded: boolean): void {
    this.b.pushPoll(w, start, end, taskId, spawnLoc, openEnded);
  }
  park(w: number, start: number, end: number, schedWait?: number | null): void {
    this.b.pushPark(w, start, end, schedWait ?? null);
  }
  active(w: number, start: number, end: number, ratio: number): void {
    this.b.pushActive(w, start, end, ratio);
  }
}

/** Everything buildWorkerSpans returns except `workerSpans` (which the caller
 * gets in whichever representation it asked for). */
interface SpanAggregates {
  perWorker: Rec<number[]>;
  queueSamples: { t: number; global: number }[];
  workerQueueSamples: Rec<{ t: number; local: number }[]>;
  maxLocalQueue: number;
  wakesByTask: Rec<{ timestamp: number; wakerTaskId: number; targetWorker: number }[]>;
  wakesByWorker: Rec<{ timestamp: number; wakerTaskId: number; wokenTaskId: number }[]>;
}

/** The shared span state machine; emits polls/parks/actives through `emit`. */
function reconstruct(
  store: ColumnarEvents,
  workerIds: readonly number[],
  maxTs: number,
  blockInPlaceGaps: readonly BlockInPlaceGap[] | undefined,
  emit: SpanEmitter,
): SpanAggregates {
  const openPoll: Rec<number | null> = {};
  const openPark: Rec<number | null> = {};
  const openUnpark: Rec<{ timestamp: number; cpuTime: number } | null> = {};
  const openPollMeta: Rec<{ taskId: number; spawnLocId: number | string | null; spawnLoc: string | null }> = {};

  const gapsByW: Rec<BlockInPlaceGap[]> = {};
  if (blockInPlaceGaps && blockInPlaceGaps.length > 0) {
    for (const g of blockInPlaceGaps) (gapsByW[g.workerId] ??= []).push(g);
  }

  const workerQueueSamples: Rec<{ t: number; local: number }[]> = {};
  let maxLocalQueue = 1;
  const wakesByTask: Rec<{ timestamp: number; wakerTaskId: number; targetWorker: number }[]> = {};
  const wakesByWorker: Rec<{ timestamp: number; wakerTaskId: number; wokenTaskId: number }[]> = {};

  for (const w of workerIds) workerQueueSamples[w] = [];

  const eventType = store.eventType;
  const ts = store.ts;
  const workerId = store.workerId;
  const localQueue = store.localQueue;
  const cpuTime = store.cpuTime;
  const taskId = store.taskId;
  const globalQueue = store.globalQueue;
  const wakerRaw = store.wakerTaskIdRaw;
  const wokenRaw = store.wokenTaskIdRaw;
  const n = store.length;

  // First pass: bucket event INDICES by worker; index wake events.
  const perWorker: Rec<number[]> = {};
  for (let i = 0; i < n; i++) {
    const et = eventType[i];
    if (et === EVT.WakeEvent) {
      const woken = wokenRaw[i]!;
      const target = workerId[i]!; // targetWorker === workerId for wake events
      const waker = wakerRaw[i]!;
      (wakesByTask[woken] ??= []).push({ timestamp: ts[i]!, wakerTaskId: waker, targetWorker: target });
      (wakesByWorker[target] ??= []).push({ timestamp: ts[i]!, wakerTaskId: waker, wokenTaskId: woken });
    } else if (et !== EVT.QueueSample) {
      (perWorker[workerId[i]!] ??= []).push(i);
    }
  }

  // Stable sort of indices by ts (JS sort is stable, so equal-ts events keep
  // their original event order, matching the frozen object sort).
  for (const key in perWorker) perWorker[key]!.sort((a, b) => ts[a]! - ts[b]!);
  for (const key in wakesByTask) wakesByTask[key]!.sort((a, b) => a.timestamp - b.timestamp);
  for (const key in wakesByWorker) wakesByWorker[key]!.sort((a, b) => a.timestamp - b.timestamp);

  for (const wKey in perWorker) {
    const w = Number(wKey); // numeric worker id for the emitter/store keys
    for (const i of perWorker[wKey]!) {
      const et = eventType[i];
      const t = ts[i]!;

      if (et === EVT.PollStart || et === EVT.WorkerPark || et === EVT.WorkerUnpark) {
        workerQueueSamples[wKey]!.push({ t, local: localQueue[i]! });
        if (localQueue[i]! > maxLocalQueue) maxLocalQueue = localQueue[i]!;
      }

      if (et === EVT.PollStart) {
        if (openPoll[wKey] != null) {
          const meta = openPollMeta[wKey] || defaultMeta;
          emit.poll(w, openPoll[wKey]!, t, meta.taskId, meta.spawnLocId, meta.spawnLoc, true);
        }
        openPoll[wKey] = t;
        const sl = store.spawnLocAt(i);
        openPollMeta[wKey] = { taskId: taskId[i]!, spawnLocId: sl, spawnLoc: sl };
      } else if (et === EVT.PollEnd) {
        if (openPoll[wKey] != null) {
          const meta = openPollMeta[wKey] || defaultMeta;
          emit.poll(w, openPoll[wKey]!, t, meta.taskId, meta.spawnLocId, meta.spawnLoc, false);
          openPoll[wKey] = null;
        }
      } else if (et === EVT.WorkerPark) {
        if (openPoll[wKey] != null) {
          const meta = openPollMeta[wKey] || defaultMeta;
          emit.poll(w, openPoll[wKey]!, t, meta.taskId, meta.spawnLocId, meta.spawnLoc, true);
          openPoll[wKey] = null;
        }
        openPark[wKey] = t;
        const ou = openUnpark[wKey];
        if (ou != null) {
          const activeStart = ou.timestamp;
          const activeEnd = t;
          const wGaps = gapsByW[wKey];
          let crossesGap = false;
          if (wGaps) {
            for (const g of wGaps) {
              if (g.startNs >= activeEnd) break;
              if (g.endNs > activeStart) { crossesGap = true; break; }
            }
          }
          if (!crossesGap) {
            const wallDelta = activeEnd - activeStart;
            const cpuDelta = cpuTime[i]! - ou.cpuTime;
            const ratio = wallDelta > 0 ? Math.min(cpuDelta / wallDelta, 1.0) : 1.0;
            emit.active(w, activeStart, activeEnd, ratio);
          }
          openUnpark[wKey] = null;
        }
      } else if (et === EVT.WorkerUnpark) {
        if (openPark[wKey] != null) {
          emit.park(w, openPark[wKey]!, t, store.schedWaitAt(i));
          openPark[wKey] = null;
        }
        openUnpark[wKey] = { timestamp: t, cpuTime: cpuTime[i]! };
      }
    }
  }

  // Close open park spans at trace end; open polls are discarded (see frozen).
  for (const w of workerIds) {
    if (openPark[w] != null) emit.park(w, openPark[w]!, maxTs);
  }

  // Global queue samples, in original event order (not per-worker sorted).
  const queueSamples: { t: number; global: number }[] = [];
  for (let i = 0; i < n; i++) {
    if (eventType[i] === EVT.QueueSample) queueSamples.push({ t: ts[i]!, global: globalQueue[i]! });
  }

  return { perWorker, queueSamples, workerQueueSamples, maxLocalQueue, wakesByTask, wakesByWorker };
}

/** Fat-output path: field-for-field identical to the frozen buildWorkerSpans. */
export function buildWorkerSpansColumnar(
  store: ColumnarEvents,
  workerIds: readonly number[],
  maxTs: number,
  blockInPlaceGaps: readonly BlockInPlaceGap[] | undefined
): WorkerSpansResult {
  const emit = new FatSpanEmitter();
  for (const w of workerIds) emit.ensure(w);
  const agg = reconstruct(store, workerIds, maxTs, blockInPlaceGaps, emit);
  return {
    workerSpans: emit.workerSpans,
    // The ONE field that is not frozen-identical: index buckets, not event
    // objects (see the header). No consumer reads it, so the divergence stays
    // confined to this field instead of blanking the whole result's type.
    perWorker: agg.perWorker as unknown as WorkerSpansResult["perWorker"],
    queueSamples: agg.queueSamples,
    workerQueueSamples: agg.workerQueueSamples,
    maxLocalQueue: agg.maxLocalQueue,
    wakesByTask: agg.wakesByTask,
    wakesByWorker: agg.wakesByWorker,
  };
}

/** Result of the store path: the typed-array store plus the non-span
 * aggregates (queue samples, wake indices) buildWorkerSpans also produces. */
export interface WorkerSpansStoreResult {
  store: ColumnarWorkerSpans;
  queueSamples: { t: number; global: number }[];
  workerQueueSamples: Rec<{ t: number; local: number }[]>;
  maxLocalQueue: number;
  wakesByTask: Rec<{ timestamp: number; wakerTaskId: number; targetWorker: number }[]>;
  wakesByWorker: Rec<{ timestamp: number; wakerTaskId: number; wokenTaskId: number }[]>;
}

/**
 * Store-output path: builds a ColumnarWorkerSpans DIRECTLY (no fat span objects,
 * ~160 MB typed columns vs ~1.9 GB objects for a 13M-event trace). The span
 * data is identical to buildWorkerSpansColumnar's fat output (same state
 * machine), just materialized as columns. cpuSampleTimes / cpu-sched samples are
 * filled afterward by store.attachCpuSamples.
 */
export function buildWorkerSpansColumnarStore(
  store: ColumnarEvents,
  workerIds: readonly number[],
  maxTs: number,
  blockInPlaceGaps: readonly BlockInPlaceGap[] | undefined
): WorkerSpansStoreResult {
  const builder = new ColumnarWorkerSpansBuilder(workerIds);
  const agg = reconstruct(store, workerIds, maxTs, blockInPlaceGaps, new StoreSpanEmitter(builder));
  return {
    store: builder.finish(),
    queueSamples: agg.queueSamples,
    workerQueueSamples: agg.workerQueueSamples,
    maxLocalQueue: agg.maxLocalQueue,
    wakesByTask: agg.wakesByTask,
    wakesByWorker: agg.wakesByWorker,
  };
}

// Golden test: store.pointsOfInterest (raw-column scan) must emit the same POIs
// as the frozen filterPointsOfInterest over the fat worker spans, for every
// detector type - so poi.ts / minimap-poi.ts can scan columns instead of
// materializing every poll from the flyweight lane views.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "./load.js";
import { buildWorkerSpans, filterPointsOfInterest, computeSchedulingDelays, attachCpuSamples } from "./index.js";
import { deriveWorkerIds } from "../../components/canvas/lanes/data.js";
import { ColumnarWorkerSpans } from "./columnar-worker-spans.js";

// eslint-disable-next-line @typescript-eslint/no-explicit-any
let ws: any;
let store: ColumnarWorkerSpans;
let workerIds: number[];
// eslint-disable-next-line @typescript-eslint/no-explicit-any
let fatSched: any[];
// eslint-disable-next-line @typescript-eslint/no-explicit-any
let colSched: any[];
let taskInstrumented: Map<number, boolean>;

beforeAll(async () => {
  const b = readFileSync(fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url)));
  const raw = b[0] === 0x1f && b[1] === 0x8b ? new Uint8Array(gunzipSync(b)) : new Uint8Array(b);
  const trace = await parseTraceBuffer(raw);
  workerIds = deriveWorkerIds(trace);
  const r = buildWorkerSpans(trace.events, workerIds, trace.maxTs ?? 0, trace.blockInPlaceGaps);
  ws = r.workerSpans;
  attachCpuSamples(trace.cpuSamples, ws); // so the fat cpu-sampled detector sees samples
  store = ColumnarWorkerSpans.fromWorkerSpans(ws);
  store.attachCpuSamples(trace.cpuSamples as never);
  fatSched = computeSchedulingDelays(ws, workerIds, r.wakesByTask);
  colSched = store.schedulingDelays(workerIds, r.wakesByTask as never);
  // Synthesize an instrumentation map so "uninstrumented" has matches.
  taskInstrumented = new Map();
  let flip = false;
  for (const w of workerIds) for (const p of ws[w].polls) {
    if (!taskInstrumented.has(p.taskId)) taskInstrumented.set(p.taskId, (flip = !flip));
  }
});

// Multiset compare (sortByWorst can order equal-value ties differently between
// the column scan and the fat scan; the COUNT + set is what matters).
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const key = (p: any) => `${p.time}|${p.worker}|${p.type}|${p.value}|${p.span?.start}|${p.span?.end}`;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const bag = (arr: any[]) => arr.map(key).sort();

describe("store.pointsOfInterest matches frozen filterPointsOfInterest", () => {
  for (const type of ["long-poll", "sched", "cpu-sampled", "wake-delay", "uninstrumented"] as const) {
    it(`${type}: same POIs (time/worker/value/span)`, () => {
      const opts = { hasSchedWait: true, sortByWorst: true, taskInstrumented };
      const fat = filterPointsOfInterest(type, ws, workerIds, fatSched, opts);
      const col = store.pointsOfInterest(type, workerIds, colSched, opts);
      expect(col.length, `${type} count`).toBe(fat.length);
      expect(bag(col)).toEqual(bag(fat));
    });
  }
});

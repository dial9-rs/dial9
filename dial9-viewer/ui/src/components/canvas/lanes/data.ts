// The lanes track's frame-invariant input: worker ids, runtime groups,
// reconstructed spans, per-worker queue series, wake indices, and the span-id
// index the highlight reads. Assembled from the shared once-per-trace
// derivations (lib/trace/derived.ts), which the other tracks read directly.
//
// This is the DERIVED input to render.ts. The mount wires it through the
// store's derived() cache keyed by the `trace` slice, so it recomputes only
// when a new trace loads/reparses - not on viewport or selection change.

import {
  deriveRuntimeGroups,
  deriveRuntimeMetrics,
  deriveWorkerIds,
  sharedSpanData,
  sharedWorkerSpans,
} from "../../../lib/trace/derived.js";
import {
  metricsRuntimeNames,
  type RuntimeMetrics,
} from "../../../lib/trace/runtime-metrics-model.js";
import { buildLaneIdentities, type LaneIdentity } from "./chrome.js";
import { spansById as buildSpanByIdSingle } from "../../../lib/trace/index.js";
import { LazySpansById, LazySpanByIdSingle } from "../../../lib/trace/columnar-spans.js";
import type { ColumnarSpans, SpanByIdMulti, SpanByIdSingle } from "../../../lib/trace/columnar-spans.js";
import type { LaneSpans } from "../../../lib/trace/columnar-worker-spans.js";
import type {
  ParsedTrace,
  RuntimeGroup,
  TracingSpan,
  WorkerWake,
} from "../../../types/trace.js";

/** Everything render.ts needs that is invariant across viewport/selection. */
export interface LaneData {
  /** Worker ids in render order (runtime-group order, single-runtime = sorted). */
  workerIds: number[];
  /** The runtime groups in render order. One group (single-runtime) is the
   *  common case; the renderer draws headers only when there is more than one. */
  runtimeGroups: RuntimeGroup[];
  /** Per-runtime scheduler metrics, drawn in each runtime's summary lane. Empty
   *  for pre-RuntimeMetrics traces. */
  runtimeMetrics: RuntimeMetrics;
  /** Runtime names that have a metric series (so a summary lane is emitted).
   *  Pair with the store's fold map to build `laneRowLayout`'s metrics opts. */
  metricsRuntimes: ReadonlySet<string>;
  /** worker id -> its owning runtime's accent, painted as the lane's left rail
   *  so a lane's runtime is readable without the group header on screen. */
  laneIdentity: ReadonlyMap<number, LaneIdentity>;
  /** Runtime-group name -> its accent (the same colour its lanes' rails carry). */
  runtimeAccents: ReadonlyMap<string, string>;
  /** worker id -> its owning runtime-group name, so the label gutter can resolve
   *  a worker row's accent from the row alone. */
  workerRuntime: ReadonlyMap<number, string>;
  /** Reconstructed poll/park/active spans per worker, CPU samples attached. */
  workerSpans: Record<number, LaneSpans>;
  /** Per-worker local-queue samples, sorted by t. */
  workerQueueSamples: Record<number, { t: number; local: number }[]>;
  /** Wake events indexed by target worker. */
  wakesByWorker: Record<number, WorkerWake[]>;
  /** span id -> every span instance sharing it (highlight lookup, no scan). A
   * real Map on the fat path; a lazy store-backed adapter on the columnar path. */
  spansById: SpanByIdMulti;
  /** All completed spans, start-sorted (containing-span lookup). EMPTY on the
   * columnar path - read `columnarSpans` instead. */
  allSpans: TracingSpan[];
  /** span id -> a single span, for the ancestor walk (Map | lazy adapter). */
  spanByIdSingle: SpanByIdSingle;
  /** Columnar span store on the main-thread path; scan/window consumers dispatch
   * on it. Undefined on the fat/worker path. */
  columnarSpans?: ColumnarSpans | undefined;
  hasCpuTime: boolean;
  hasSchedWait: boolean;
  hasTaskTracking: boolean;
}

/**
 * Assemble the lanes track's frame-invariant input for one parsed trace: the
 * shared span/queue/wake derivations plus the span-id index the highlight
 * reads. The mount caches this per trace (store.derived over the `trace`
 * slice), so a pan or selection change never re-runs it.
 */
export function deriveLaneData(trace: ParsedTrace): LaneData {
  const runtimeGroups = deriveRuntimeGroups(trace);
  const workerIds = runtimeGroups.flatMap((g) => g.workerIds);
  const spanResult = sharedWorkerSpans(trace);
  const workerSpans = spanResult.workerSpans;

  // A group gets a summary lane iff its runtime has a metric series. The
  // inferred default group ("main") maps to the empty wire name. Keyed by GROUP
  // name so it matches laneRowLayout's per-group check.
  const runtimeMetrics = deriveRuntimeMetrics(trace);
  const metricsRuntimes = metricsRuntimeNames(runtimeGroups, runtimeMetrics);

  // Lane identity: each worker's runtime accent + group name, by group order.
  const identities = buildLaneIdentities(runtimeGroups);

  // Span-id index: id -> every instance (recycled ids highlight every
  // instance, but O(1) lookup instead of a full allSpans scan). On the columnar
  // path these are lazy adapters over the store (no 982K view maps built).
  const spanData = sharedSpanData(trace);
  const cs = spanData.columnarSpans;
  let spansById: SpanByIdMulti;
  let spanByIdSingle: SpanByIdSingle;
  if (cs) {
    spansById = new LazySpansById(cs);
    spanByIdSingle = new LazySpanByIdSingle(cs);
  } else {
    const m = new Map<string, TracingSpan[]>();
    for (const s of spanData.allSpans) {
      let bucket = m.get(s.spanId);
      if (!bucket) { bucket = []; m.set(s.spanId, bucket); }
      bucket.push(s);
    }
    spansById = m;
    spanByIdSingle = buildSpanByIdSingle(spanData.allSpans);
  }

  return {
    workerIds,
    runtimeGroups,
    runtimeMetrics,
    metricsRuntimes,
    laneIdentity: identities.byWorker,
    runtimeAccents: identities.byRuntime,
    workerRuntime: identities.workerRuntime,
    workerSpans,
    workerQueueSamples: spanResult.workerQueueSamples,
    wakesByWorker: spanResult.wakesByWorker,
    spansById,
    allSpans: spanData.allSpans,
    spanByIdSingle,
    columnarSpans: cs,
    hasCpuTime: trace.hasCpuTime,
    hasSchedWait: trace.hasSchedWait,
    hasTaskTracking: trace.hasTaskTracking,
  };
}

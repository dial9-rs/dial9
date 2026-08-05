// Columnar buildSpanData. Span-producing events (SpanEnter/Exit/Close and
// annotated single-event spans) live in a ColumnarSpanEvents store (typed columns)
// instead of the fat customEvents array, so this reimplements buildSpanData's
// span-event parse reading the columns in ts order (NO
// `[...customEvents].sort()` spread, which would materialize millions of fat
// objects). The span->task resolution + active segment reconstruction run over
// the columnar worker-span store. Output is byte-identical to frozen
// buildSpanData(customEvents, fatWorkerSpans), verified in
// span-data-columnar.test.ts.
//
// Keep the enter/exit/close/depth/children logic in lockstep with
// trace_analysis.js buildSpanData (~lines 1221-1404).

import { SPAN_KIND, type ColumnarSpanEvents } from "./columnar-span-events.js";
import type { ColumnarWorkerSpans, PollsByTaskCSR } from "./columnar-worker-spans.js";
import { ColumnarSpansBuilder } from "./columnar-spans.js";
import type {
  BlockInPlaceGap,
  DecodedFieldValue,
  SpanData,
  TidWorkerBinding,
  TracingSpan,
  UnmatchedSpan,
} from "./index.js";

type Seg = TracingSpan["segments"][number];
type Fields = Record<string, DecodedFieldValue>;

interface SpanRec {
  spanName: string;
  fields: Fields;
  parentSpanId: string | null;
  taskId: number | null;
  segments: Seg[];
}

/** Intersect each raw segment with the owning task's polls (the CSR slice
 * [lo,hi) of pollsByTaskCSR, sorted by start, non-overlapping): the overlaps are
 * the true on-CPU segments (idle falls between). Reads the typed CSR arrays
 * directly - no per-poll object. Same logic + tie-break as frozen
 * reconstructSpanSegments. */
function reconstructSpanSegments(rawSegments: Seg[], csr: PollsByTaskCSR, slot: number): Seg[] {
  const lo0 = csr.off[slot]!, hi0 = csr.off[slot + 1]!;
  if (lo0 === hi0) return rawSegments;
  const { start, end, worker } = csr;
  const out: Seg[] = [];
  for (const seg of rawSegments) {
    // First poll whose end >= seg.start (ends monotonic: non-overlapping).
    let lo = lo0, hi = hi0;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (end[mid]! < seg.start) lo = mid + 1;
      else hi = mid;
    }
    for (let i = lo; i < hi0; i++) {
      if (start[i]! > seg.end) break;
      const a = Math.max(start[i]!, seg.start);
      const b = Math.min(end[i]!, seg.end);
      if (b > a) out.push({ start: a, end: b, workerId: worker[i]! });
    }
  }
  return out.length > 0 ? out : rawSegments;
}

/** Single-event lifecycle intersection. Unlike tracing reconstruction, absence
 * of poll evidence must remain empty instead of treating the lifecycle as active. */
function singleEventActiveSegments(startNs: number, endNs: number, csr: PollsByTaskCSR, slot: number): Seg[] {
  const lo0 = csr.off[slot]!, hi0 = csr.off[slot + 1]!;
  const { start, end, worker } = csr;
  const out: Seg[] = [];
  let lo = lo0, hi = hi0;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (end[mid]! <= startNs) lo = mid + 1;
    else hi = mid;
  }
  for (let i = lo; i < hi0; i++) {
    if (start[i]! >= endNs) break;
    const a = Math.max(start[i]!, startNs);
    const b = Math.min(end[i]!, endNs);
    if (b > a) out.push({ start: a, end: b, workerId: worker[i]! });
  }
  return out;
}

function timestampIsInHandoffGap(
  timestamp: number,
  handoffGaps: readonly BlockInPlaceGap[] | undefined,
): boolean {
  if (handoffGaps != null) {
    for (const gap of handoffGaps) {
      if (gap.startNs > timestamp) break;
      if (timestamp < gap.endNs) return true;
    }
  }
  return false;
}

function workerForTidAt(
  bindings: readonly TidWorkerBinding[] | undefined,
  timestamp: number,
  handoffGaps: readonly BlockInPlaceGap[] | undefined,
): number | undefined {
  if (timestampIsInHandoffGap(timestamp, handoffGaps)) return undefined;
  if (bindings == null || bindings.length === 0) return undefined;
  let lo = 0, hi = bindings.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (bindings[mid]!.timestamp <= timestamp) lo = mid + 1;
    else hi = mid;
  }
  return lo === 0 ? undefined : bindings[lo - 1]!.workerId;
}

/**
 * buildSpanData over the columnar span-event + worker-span stores. Equivalent to
 * buildSpanData(customEvents, workerSpans) where customEvents' span events are
 * `spanEvents` and taskId/segments resolve from `store`.
 */
export function buildSpanDataColumnar(
  spanEvents: ColumnarSpanEvents,
  store: ColumnarWorkerSpans,
  tidBindings?: ReadonlyMap<number, readonly TidWorkerBinding[]>,
  blockInPlaceGaps?: readonly BlockInPlaceGap[],
): SpanData {
  // spanId -> correlation data of the currently-open enter. New traces carry
  // taskId directly from the namespaced field; workerId remains for the
  // old-trace fallback.
  const openEnters = new Map<string, { timestamp: number; workerId: number; taskId: number | null }>();
  // Live span records keyed by spanId; streamed to the builder on Close / EOT.
  const spanMap = new Map<string, SpanRec>();
  // Lightweight spanId -> parentSpanId (strings only), for the depth walk -
  // replaces the fat spanMeta. Transient; not returned. Matches the frozen
  // getDepth chain (must include every entered span, incl still-open parents).
  const parentBySpanId = new Map<string, string | null>();

  // Streaming: build the poll-by-task CSR once, then convert each span to
  // columns the MOMENT it closes (drop the rec) - the 982K fat SpanRec objects
  // never accumulate. finish() sorts the close-order columns by start.
  const csr = store.pollsByTaskCSR();
  const builder = new ColumnarSpansBuilder();
  let singleEventOrdinal = 0;
  const handoffGapsByTid = new Map<number, BlockInPlaceGap[]>();
  for (const gap of blockInPlaceGaps ?? []) {
    for (const tid of [gap.fromTid, gap.toTid]) {
      let gaps = handoffGapsByTid.get(tid);
      if (gaps == null) {
        gaps = [];
        handoffGapsByTid.set(tid, gaps);
      }
      gaps.push(gap);
    }
  }

  const finalizeSpan = (spanId: string): void => {
    const rec = spanMap.get(spanId);
    if (rec && rec.segments.length > 0) {
      rec.segments.sort((a, b) => a.start - b.start);
      const start = rec.segments[0]!.start;
      const end = rec.segments[rec.segments.length - 1]!.end;
      const seg0 = rec.segments[0]!;
      const taskId =
        rec.taskId ??
        (Number.isFinite(seg0.workerId)
          ? store.resolveSpanTaskAt(seg0.workerId, seg0.start)
          : null);
      const slot = taskId != null ? csr.slotOf.get(taskId) : undefined;
      const segments = slot !== undefined ? reconstructSpanSegments(rec.segments, csr, slot) : rec.segments;
      let activeNs = 0;
      for (const s of segments) activeNs += s.end - s.start;
      builder.push(start, end, spanId, rec.spanName, rec.parentSpanId, taskId, activeNs, segments, rec.fields);
    }
    spanMap.delete(spanId);
  };

  // Walk span events in ts order (stable) - the frozen sort of customEvents.
  const perm = spanEvents.tsIndex();
  for (let k = 0; k < perm.length; k++) {
    const i = perm[k]!;
    const kind = spanEvents.kind[i];
    const ts = spanEvents.ts[i]!;

    if (kind === SPAN_KIND.Complete) {
      const spanId = `single-event:${singleEventOrdinal++}`;
      const start = spanEvents.startAt(i);
      const end = spanEvents.endAt(i);

      const directTask = spanEvents.taskIdAt(i);
      const threadId = spanEvents.threadIdAt(i);
      const directWorker = spanEvents.completeWorkerIdAt(i);
      const handoffGaps = Number.isFinite(threadId)
        ? handoffGapsByTid.get(threadId)
        : undefined;
      const workerId = timestampIsInHandoffGap(start, handoffGaps)
        ? 255
        : Number.isFinite(directWorker)
          ? directWorker
          : Number.isFinite(threadId)
            ? (workerForTidAt(tidBindings?.get(threadId), start, handoffGaps) ?? 255)
            : 255;
      let taskId = Number.isFinite(directTask) && directTask > 0 ? directTask : null;
      if (taskId == null && workerId !== 255) {
        taskId = store.resolveSpanTaskAt(workerId, start);
      }
      const slot = taskId != null ? csr.slotOf.get(taskId) : undefined;
      const segments = slot !== undefined
        ? singleEventActiveSegments(start, end, csr, slot)
        : [];
      let activeNs = 0;
      for (const segment of segments) activeNs += segment.end - segment.start;
      const spanName = spanEvents.spanNameAt(i);
      const fields = spanEvents.extraFieldsAt(i);
      const spanType = spanEvents.spanTypeAt(i);
      const units = spanEvents.extraUnitsAt(i);
      builder.push(
        start,
        end,
        spanId,
        spanName,
        null,
        taskId,
        activeNs,
        segments,
        fields,
        spanType,
        units,
      );
      parentBySpanId.set(spanId, null);
    } else if (kind === SPAN_KIND.Enter) {
      const workerId = spanEvents.workerId[i]!; // Number(v.worker_id); NaN if absent
      const rawTaskId = spanEvents.taskId[i]!;
      const taskId = Number.isFinite(rawTaskId) && rawTaskId !== 0 ? rawTaskId : null;
      const spanId = spanEvents.spanIdAt(i); // String(v.span_id)
      const parentSpanId = spanEvents.parentAt(i);
      const spanName = spanEvents.spanNameAt(i);
      const fields = spanEvents.extraFieldsAt(i);
      // Skip a duplicate enter (already open on another worker) to keep the first.
      if (openEnters.has(spanId)) continue;
      openEnters.set(spanId, { timestamp: ts, workerId, taskId });
      if (!spanMap.has(spanId)) {
        spanMap.set(spanId, { spanName, fields, parentSpanId, taskId, segments: [] });
      }
      parentBySpanId.set(spanId, parentSpanId);
    } else if (kind === SPAN_KIND.Exit) {
      const spanId = spanEvents.spanIdAt(i);
      const enter = openEnters.get(spanId);
      if (enter) {
        openEnters.delete(spanId);
        const exitFields = spanEvents.extraFieldsAt(i);
        let rec = spanMap.get(spanId);
        if (!rec) {
          rec = { spanName: spanEvents.spanNameAt(i), fields: {}, parentSpanId: null, taskId: enter.taskId, segments: [] };
          spanMap.set(spanId, rec);
        }
        if (Object.keys(exitFields).length > 0) rec.fields = exitFields;
        // Retain the ENTER worker for the legacy poll-overlap fallback.
        rec.segments.push({ start: enter.timestamp, end: ts, workerId: enter.workerId });
      }
    } else {
      // Close: finalize + allow span-id recycling.
      const spanId = spanEvents.spanIdAt(i);
      openEnters.delete(spanId);
      finalizeSpan(spanId);
    }
  }

  // Unmatched = open enters that never exited (no segment -> never closed).
  // Read their meta from spanMap BEFORE the finalize loop clears it. Fields are
  // dead at runtime (only .length is read), but keep the real values for parity.
  const unmatchedSpans: UnmatchedSpan[] = [];
  for (const [spanId, enter] of openEnters) {
    const rec = spanMap.get(spanId);
    unmatchedSpans.push({
      start: enter.timestamp,
      spanId,
      workerId: enter.workerId,
      spanName: rec?.spanName || "unknown",
      fields: rec?.fields || {},
      parentSpanId: rec?.parentSpanId ?? null,
    });
  }
  unmatchedSpans.sort((a, b) => a.start - b.start);

  // Finalize spans still open at end of trace (streams them to the builder).
  for (const [spanId] of spanMap) finalizeSpan(spanId);

  // Sort by start + resolve parentRow/depth (byte-identical row order to the fat
  // allSpans.sort - stable, close-order tiebreak).
  const { store: columnarSpans, maxDepth } = builder.finish(parentBySpanId);

  // parent -> children in ROW (start-sorted) order, matching the frozen build.
  const childrenByParent = new Map<string | null, string[]>();
  for (let r = 0; r < columnarSpans.length; r++) {
    const key = columnarSpans.parentSpanIdAt(r);
    let arr = childrenByParent.get(key);
    if (!arr) { arr = []; childrenByParent.set(key, arr); }
    arr.push(columnarSpans.spanIdAt(r));
  }

  // allSpans + spanMeta stay empty on the columnar path - consumers read the
  // store via columnarSpans.
  return { allSpans: [], columnarSpans, spanMeta: new Map(), maxDepth, unmatchedSpans, childrenByParent };
}

// Lane hover-tooltip DATA assembly. A pure assembler so it is testable and so
// the tooltip never rescans: every lookup is a binary-search / indexed query
// helper, not a linear per-mousemove scan.

import {
  activeTaskCountAt,
  findContainingSpan,
  findSpanAt,
  nearestByT,
} from "../../../lib/trace/query.js";
import type { ColumnarSpans } from "../../../lib/trace/columnar-spans.js";
import type {
  BlockInPlaceGap,
  TracingSpan,
} from "../../../types/trace.js";
import type { LaneSpans, ParkLike } from "../../../lib/trace/columnar-worker-spans.js";

/** A worker's state at the hovered instant. */
export type WorkerHoverState = "active" | "parked" | "block_in_place" | "polling";

export interface HoverPollInfo {
  durationNs: number;
  openEnded: boolean;
  taskId: number | null;
  spawnLoc: string | null;
  cpuSampleCount: number;
  schedSampleCount: number;
  /** Spans whose segments run inside this poll on this worker. */
  spanCount: number;
  /** name -> occurrence count, for the "(name x N, ...)" summary. */
  spanNames: Record<string, number>;
}

export interface HoverSpanInfo {
  name: string;
  durationNs: number;
  fields: [string, unknown][];
  parentName: string | null;
}

/** Structured hover readout the tooltip renders. */
export interface LaneHoverData {
  workerId: number;
  ns: number;
  state: WorkerHoverState;
  /** on-CPU ratio 0..1 for the active period at `ns`, null if none/no CPU. */
  onCpuRatio: number | null;
  parkDurationNs: number | null;
  kernelSchedDelayNs: number | null;
  blockInPlace: { fromTid: number; toTid: number; durationNs: number } | null;
  poll: HoverPollInfo | null;
  globalQueue: number | null;
  localQueue: number | null;
  activeTaskCount: number | null;
  span: HoverSpanInfo | null;
  /** The cursor becomes a pointer where a click opens Poll Detail. */
  hasClickableStack: boolean;
}

export interface LaneHoverInput {
  workerId: number;
  ns: number;
  spans: LaneSpans;
  /** All completed spans, start-sorted (span detail + span-in-poll count).
   * EMPTY on the columnar path - read `columnarSpans` instead. */
  allSpans: readonly TracingSpan[];
  /** Columnar span store (main-thread path); span lookups dispatch on it. */
  columnarSpans?: ColumnarSpans | undefined;
  /** Global injection-queue series {t, global}, sorted by t. */
  queueSamples: readonly { t: number; global: number }[];
  /** This worker's local-queue series {t, local}, sorted by t. */
  localQueueSamples: readonly { t: number; local: number }[];
  /** Active-task-count timeline {t, count}, sorted by t. */
  activeTaskSamples: readonly { t: number; count: number }[];
  blockInPlaceGaps: readonly BlockInPlaceGap[];
  hasCpuTime: boolean;
  hasSchedWait: boolean;
  hasTaskTracking: boolean;
}

/**
 * Assemble the hover readout for one worker at one timestamp. Pure; uses only
 * indexed lookups (findSpanAt binary search over the worker's
 * parks/polls/actives, findContainingSpan for span detail, binary-search
 * nearest for queue depths). Precedence: active -> parked -> block_in_place ->
 * polling, with poll info layered on when a poll spans the instant.
 */
export function assembleLaneHover(input: LaneHoverInput): LaneHoverData {
  const { workerId, ns, spans } = input;

  let state: WorkerHoverState = "active";
  let onCpuRatio: number | null = null;
  let parkDurationNs: number | null = null;
  let kernelSchedDelayNs: number | null = null;
  let blockInPlace: LaneHoverData["blockInPlace"] = null;

  if (input.hasCpuTime) {
    const active = findSpanAt(spans.actives, ns);
    if (active) onCpuRatio = active.ratio;
  }

  const park = findSpanAt<ParkLike>(spans.parks, ns);
  if (park) {
    state = "parked";
    parkDurationNs = park.end - park.start;
    onCpuRatio = null;
    if (input.hasSchedWait && park.schedWait != null) {
      kernelSchedDelayNs = park.schedWait;
    }
  }

  for (const g of input.blockInPlaceGaps) {
    if (g.workerId === workerId && ns >= g.startNs && ns < g.endNs) {
      state = "block_in_place";
      blockInPlace = {
        fromTid: g.fromTid,
        toTid: g.toTid,
        durationNs: g.endNs - g.startNs,
      };
      break;
    }
  }

  let poll: HoverPollInfo | null = null;
  let hasClickableStack = false;
  const hitPoll = findSpanAt(spans.polls, ns);
  if (hitPoll) {
    state = "polling";
    const cpuCount = hitPoll.cpuSamples ? hitPoll.cpuSamples.length : 0;
    const schedCount = hitPoll.schedSamples ? hitPoll.schedSamples.length : 0;
    if (cpuCount > 0 || schedCount > 0) hasClickableStack = true;

    // Spans whose segments run entirely inside this poll on this worker.
    let spanCount = 0;
    const spanNames: Record<string, number> = {};
    const cs = input.columnarSpans;
    if (cs) {
      for (let r = 0; r < cs.length; r++) {
        if (cs.start[r]! > hitPoll.end) break; // start-sorted
        if (cs.end[r]! < hitPoll.start) continue;
        const lo = cs.segOff[r]!, hi = cs.segOff[r + 1]!;
        for (let j = lo; j < hi; j++) {
          if (cs.segWorker[j]! === workerId && cs.segStart[j]! >= hitPoll.start && cs.segEnd[j]! <= hitPoll.end) {
            spanCount++;
            const nm = cs.spanNameAt(r);
            spanNames[nm] = (spanNames[nm] ?? 0) + 1;
          }
        }
      }
    } else {
      for (const sp of input.allSpans) {
        if (sp.start > hitPoll.end) break; // start-sorted: nothing later overlaps
        if (sp.end < hitPoll.start) continue;
        for (const seg of sp.segments) {
          if (seg.workerId === workerId && seg.start >= hitPoll.start && seg.end <= hitPoll.end) {
            spanCount++;
            spanNames[sp.spanName] = (spanNames[sp.spanName] ?? 0) + 1;
          }
        }
      }
    }

    poll = {
      durationNs: hitPoll.end - hitPoll.start,
      openEnded: hitPoll.openEnded === true,
      taskId: input.hasTaskTracking && hitPoll.taskId ? hitPoll.taskId : null,
      spawnLoc: hitPoll.spawnLoc ?? null,
      cpuSampleCount: cpuCount,
      schedSampleCount: schedCount,
      spanCount,
      spanNames,
    };
  }

  const nearestGlobal = nearestByT(input.queueSamples, ns);
  const nearestLocal = nearestByT(input.localQueueSamples, ns);

  // Span detail: outermost span with a segment on this worker at `ns`.
  let span: HoverSpanInfo | null = null;
  const hitSpan = findContainingSpan(input.allSpans, workerId, ns, input.columnarSpans);
  if (hitSpan) {
    let parentName: string | null = null;
    if (hitSpan.parentSpanId != null) {
      const cs2 = input.columnarSpans;
      const parent = cs2
        ? (() => { const r = cs2.spanIdToRow.get(hitSpan.parentSpanId!); if (r === undefined) return undefined; const p = cs2.at(r); return p.start <= hitSpan.start && p.end >= hitSpan.end ? p : undefined; })()
        : input.allSpans.find(
            (s) => s.spanId === hitSpan.parentSpanId && s.start <= hitSpan.start && s.end >= hitSpan.end,
          );
      parentName = parent ? parent.spanName : null;
    }
    span = {
      name: hitSpan.spanName,
      durationNs: hitSpan.end - hitSpan.start,
      fields: Object.entries(hitSpan.fields),
      parentName,
    };
  }

  return {
    workerId,
    ns,
    state,
    onCpuRatio,
    parkDurationNs,
    kernelSchedDelayNs,
    blockInPlace,
    poll,
    globalQueue: nearestGlobal ? nearestGlobal.global : null,
    localQueue: nearestLocal ? nearestLocal.local : null,
    activeTaskCount: activeTaskCountAt(input.activeTaskSamples, ns),
    span,
    hasClickableStack,
  };
}

// Re-resolve the URL's canvas-selection anchors against a loaded trace into a
// selection patch. Anchors that no longer resolve (a span id absent from this
// trace, an off-anchor poll/event) are silently dropped, so viewport + task +
// filters still restore.

import { deriveLaneData } from "../../components/canvas/lanes/index.js";
import type { LaneData } from "../../components/canvas/lanes/index.js";
import {
  buildPinnedEvent,
  resolveClusterTask,
  resolveTaskForEvent,
  type EventDrawBucket,
} from "./events-model.js";
import type { ParsedTrace, CustomTraceEvent } from "../../lib/trace/index.js";
import type { SelectionSlice } from "../../types/state.js";
import type { ViewerUrlState } from "./url-state.js";
import {
  focusViewport,
  matchFocusSpan,
  type FocusCandidate,
  type FocusLink,
} from "./focus-link.js";


/**
 * The spans a focus link may match, from whichever representation this trace
 * uses. The columnar path never materializes fat span objects - `allSpans` is
 * empty there - so adapt its rows lazily rather than building 900K views.
 */
function* focusCandidates(lane: LaneData): Generator<FocusCandidate> {
  const cs = lane.columnarSpans;
  if (cs) {
    for (let r = 0; r < cs.length; r++) {
      yield {
        spanId: cs.spanIdAt(r),
        start: cs.startAt(r),
        end: cs.endAt(r),
        spanName: cs.spanNameAt(r),
        taskId: cs.taskIdAt(r),
      };
    }
    return;
  }
  for (const s of lane.allSpans) {
    yield {
      spanId: s.spanId,
      start: s.start,
      end: s.end,
      spanName: s.spanName,
      taskId: s.taskId,
    };
  }
}


/**
 * The focused span plus its ancestor chain - the set the lanes highlight.
 *
 * Walks the parents off the ALREADY-DERIVED lane data. computeSpanTrackData
 * would re-run buildSpanData over every event in the trace, which the viewer
 * deliberately does once per trace and shares. Cycle-safe: stops when a parent
 * is already in the chain.
 */
function focusChain(lane: LaneData, spanId: string): Set<string> {
  const chain = new Set<string>([spanId]);
  const cs = lane.columnarSpans;
  const parentOf = cs
    ? (id: string): string | null => {
        const r = cs.spanIdToRow.get(id);
        return r === undefined ? null : cs.parentSpanIdAt(r);
      }
    : (id: string): string | null => lane.spanByIdSingle.get(id)?.parentSpanId ?? null;
  let parentId = parentOf(spanId);
  while (parentId != null && !chain.has(parentId)) {
    chain.add(parentId);
    parentId = parentOf(parentId);
  }
  return chain;
}

/**
 * A resolved `focus_*` deep link: the selection patch to apply plus the window
 * to frame. Null viewport means "no span matched" - the caller falls back to a
 * plain pan onto the requested time window.
 */
export interface ResolvedFocus {
  patch: Partial<SelectionSlice>;
  viewport: { viewStart: number; viewEnd: number };
}

/**
 * Resolve a `focus_*` link onto the loaded trace, landing ON the span rather
 * than merely near it in time. Returns null when nothing matches, so the caller
 * can fall back to the plain time-window pan.
 */
export function resolveFocusLink(
  trace: ParsedTrace,
  link: FocusLink,
): ResolvedFocus | null {
  const lane = deriveLaneData(trace);
  const span = matchFocusSpan(focusCandidates(lane), link, trace.clockOffsetNs);
  if (span === null) return null;

  const patch: Partial<SelectionSlice> = {
    spanFocus: { spanId: span.spanId, chain: focusChain(lane, span.spanId) },
    focusedSpanId: span.spanId,
  };
  // The owning task, when the trace resolved one, so the task detail track
  // opens on the same instance the span belongs to.
  if (span.taskId != null) patch.selectedTaskId = span.taskId;

  // A trace with no resolved bounds cannot clamp; fall back to the span's own
  // extent, which focusViewport then pads.
  return {
    patch,
    viewport: focusViewport(span, {
      minTs: trace.minTs ?? span.start,
      maxTs: trace.maxTs ?? span.end,
    }),
  };
}

/** Resolve the URL's selection anchors into a selection-slice patch. */
export function resolveUrlSelection(
  trace: ParsedTrace,
  url: ViewerUrlState,
): Partial<SelectionSlice> {
  const patch: Partial<SelectionSlice> = {};
  const lane = deriveLaneData(trace);

  // Span: focus + a minimal highlight chain (the span itself).
  if (url.selectedSpanId !== undefined && lane.spanByIdSingle.has(url.selectedSpanId)) {
    patch.spanFocus = { spanId: url.selectedSpanId, chain: new Set([url.selectedSpanId]) };
    patch.focusedSpanId = url.selectedSpanId;
  }

  // Span-panel subtree focus, carried independently of the lane highlight. When
  // present it wins over the fallback the span block set above; otherwise that
  // fallback stands (so an old `span`-only URL keeps its prior behavior).
  if (url.focusedSpanId !== undefined && lane.spanByIdSingle.has(url.focusedSpanId)) {
    patch.focusedSpanId = url.focusedSpanId;
  }

  // Poll: matched by start AND task - start alone is ambiguous across workers
  // (two workers can poll at the same instant); a task's poll at a given start
  // is unique.
  if (url.poll !== undefined) {
    const { startNs, taskId } = url.poll;
    for (const w of lane.workerIds) {
      const poll = lane.workerSpans[w]?.polls.find(
        (p) => p.start === startNs && p.taskId === taskId,
      );
      if (poll !== undefined) {
        patch.pollDetail = poll;
        break;
      }
    }
  }

  // Pinned event: the cluster of events at the anchor timestamp. The pixel
  // fields of the bucket are render-only; buildPinnedEvent reads only
  // events/representative/taskId, so a minimal bucket suffices.
  if (url.pinnedEventTs !== undefined) {
    const events = trace.customEvents.filter((e) => e.timestamp === url.pinnedEventTs);
    if (events.length > 0) {
      const taskOf = (ev: CustomTraceEvent): number | null =>
        resolveTaskForEvent(ev, lane.workerSpans, lane.workerIds);
      const bucket: EventDrawBucket = {
        events,
        representative: events[0]!,
        px: 0, w: 0, y: 0, h: 0,
        taskId: resolveClusterTask(events, taskOf),
        baseAlpha: 1, color: "", secondColor: null, hitX: 0, hitW: 0,
      };
      patch.pinnedEvent = buildPinnedEvent(bucket, lane.workerSpans, lane.workerIds);
    }
  }

  // Ranges restore directly (sidebarRange opens the region-analysis panel).
  if (url.sidebarRange !== undefined) patch.sidebarRange = url.sidebarRange;
  if (url.spawnedRange !== undefined) patch.spawnedTasksRange = url.spawnedRange;

  return patch;
}

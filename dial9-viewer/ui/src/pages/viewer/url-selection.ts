// Re-resolve the URL's canvas-selection anchors against a loaded trace into a
// selection patch. Anchors that no longer resolve (a span id absent from this
// trace, an off-anchor poll/event) are silently dropped, so viewport + task +
// filters still restore.

import { deriveLaneData } from "../../components/canvas/lanes/index.js";
import {
  buildPinnedEvent,
  resolveClusterTask,
  resolveTaskForEvent,
  type EventDrawBucket,
} from "./events-model.js";
import type { ParsedTrace, CustomTraceEvent } from "../../lib/trace/index.js";
import type { SelectionSlice } from "../../types/state.js";
import type { ViewerUrlState } from "./url-state.js";

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

  // Poll: the first poll on any worker whose start matches the anchor.
  if (url.pollStartNs !== undefined) {
    for (const w of lane.workerIds) {
      const poll = lane.workerSpans[w]?.polls.find((p) => p.start === url.pollStartNs);
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

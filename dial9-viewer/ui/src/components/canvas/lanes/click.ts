// components/canvas/lanes/click.ts - lane-click resolution semantics
// (features/02 G13 select-task, G14 span auto-focus, G15 stack popup).
//
// A PURE resolver: given the worker + timestamp a click resolved to (the
// GESTURE - drag-vs-click discrimination, x->ns, y->worker - is T23's
// interaction machine), it returns the selection-slice patch and whether to
// open the Poll Detail. T23 dispatches the patch; T31 renders the popup.
// Isolating it here makes the exact legacy toggle/focus semantics testable
// without a DOM (the behavioral-differ contract on J2/J4).
//
// Uses the T09 query helpers only (taskAt, findContainingSpan,
// spanAncestryAt) - no O(allSpans) rescans beyond the one containing-span
// lookup the legacy handler itself did (03 F6).

import { findContainingSpan, spanAncestryAt, taskAt } from "../../../lib/trace/query.js";
import type { PollSpan, TracingSpan } from "../../../types/trace.js";
import type { SpanFocus } from "../../../types/state.js";

/** Everything the resolver needs, pulled from LaneData + current selection. */
export interface LaneClickInput {
  /** Worker the click resolved to (T23's workerAtClientY equivalent). */
  workerId: number;
  /** Timestamp under the click (trace-monotonic ns). */
  ns: number;
  /** The clicked worker's polls (LaneData.workerSpans[workerId].polls). */
  polls: readonly PollSpan[];
  /** All completed spans, start-sorted (LaneData.allSpans). */
  allSpans: readonly TracingSpan[];
  /** span id -> span, for the ancestor walk (LaneData.spanByIdSingle). */
  spanById: ReadonlyMap<string, TracingSpan>;
  /** The currently-selected task (selection.selectedTaskId), for toggle. */
  currentSelectedTaskId: number | null;
}

/** The resolved selection changes + Poll Detail signal. */
export interface LaneClickResult {
  /** New selected task (null clears it). */
  selectedTaskId: number | null;
  /** New focused span + highlight chain (null clears it). */
  spanFocus: SpanFocus | null;
  /** New span-panel focus id (mirrors spanFocus.spanId; null clears). */
  focusedSpanId: string | null;
  /** Always: any lane click clears the pinned custom-event marker (G13). */
  clearPinnedEvent: true;
  /** Poll to open in Poll Detail when it has CPU/sched samples (G15), else null. */
  openStackFor: PollSpan | null;
  /** True when this click toggled OFF the already-selected task (G13). */
  toggledOff: boolean;
}

/**
 * Resolve a lane click to its selection + popup effects (G13/G14/G15).
 * Mirrors viewer.html:5362-5453 exactly:
 *  - find the poll at `ns`; its task becomes the selection (G13);
 *  - clicking the SAME task again clears task AND span focus (single un-click);
 *  - additively, walk the outermost span containing `ns` on this worker and
 *    focus it + its ancestor chain (G14);
 *  - open Poll Detail if the poll carries CPU or sched samples (G15).
 */
export function resolveLaneClick(input: LaneClickInput): LaneClickResult {
  const hit = taskAt(input.polls, input.ns);
  const poll = hit ? hit.poll : null;
  const foundTask = hit ? hit.taskId : null;

  // G15: Poll Detail opens only when the poll has samples to show.
  const openStackFor =
    poll &&
    ((poll.cpuSamples && poll.cpuSamples.length > 0) ||
      (poll.schedSamples && poll.schedSamples.length > 0))
      ? poll
      : null;

  // G14: outermost containing span on this worker + ancestor chain.
  let spanFocus: SpanFocus | null = null;
  const containing = findContainingSpan(input.allSpans, input.workerId, input.ns);
  if (containing) {
    const ancestry = spanAncestryAt(containing, input.spanById, input.ns);
    spanFocus = { spanId: ancestry.outermost.spanId, chain: ancestry.ids };
  }
  const focusedSpanId = spanFocus ? spanFocus.spanId : null;

  // G13: toggle off when re-clicking the selected task; else adopt the new
  // selection (task may be null - clears task but keeps additive span focus).
  const togglingOff = foundTask !== null && foundTask === input.currentSelectedTaskId;
  if (togglingOff) {
    return {
      selectedTaskId: null,
      spanFocus: null,
      focusedSpanId: null,
      clearPinnedEvent: true,
      openStackFor,
      toggledOff: true,
    };
  }
  return {
    selectedTaskId: foundTask,
    spanFocus,
    focusedSpanId,
    clearPinnedEvent: true,
    openStackFor,
    toggledOff: false,
  };
}

// Lane-click resolution semantics: select-task, span auto-focus, stack popup.
//
// A PURE resolver: given the worker + timestamp a click resolved to, it returns
// the selection-slice patch and whether to open the Poll Detail. Isolating it
// here makes the toggle/focus semantics testable without a DOM.
//
// Uses the query helpers only (taskAt, findContainingSpan, spanAncestryAt) - no
// O(allSpans) rescans beyond the one containing-span lookup.

import { findContainingSpan, spanAncestryAt, taskAt } from "../../../lib/trace/query.js";
import type { PollSpan, TracingSpan } from "../../../types/trace.js";
import type { SpanFocus } from "../../../types/state.js";

/** Everything the resolver needs, pulled from LaneData + current selection. */
export interface LaneClickInput {
  /** Worker the click resolved to. */
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
  /** Always: any lane click clears the pinned custom-event marker. */
  clearPinnedEvent: true;
  /** Poll to open in Poll Detail when it has CPU/sched samples, else null. */
  openStackFor: PollSpan | null;
  /** True when this click toggled OFF the already-selected task. */
  toggledOff: boolean;
}

/**
 * Resolve a lane click to its selection + popup effects:
 *  - find the poll at `ns`; its task becomes the selection;
 *  - clicking the SAME task again clears task AND span focus (single un-click);
 *  - additively, walk the outermost span containing `ns` on this worker and
 *    focus it + its ancestor chain;
 *  - open Poll Detail if the poll carries CPU or sched samples.
 */
export function resolveLaneClick(input: LaneClickInput): LaneClickResult {
  const hit = taskAt(input.polls, input.ns);
  const poll = hit ? hit.poll : null;
  const foundTask = hit ? hit.taskId : null;

  // Poll Detail opens only when the poll has samples to show.
  const openStackFor =
    poll &&
    ((poll.cpuSamples && poll.cpuSamples.length > 0) ||
      (poll.schedSamples && poll.schedSamples.length > 0))
      ? poll
      : null;

  // Outermost containing span on this worker + ancestor chain.
  let spanFocus: SpanFocus | null = null;
  const containing = findContainingSpan(input.allSpans, input.workerId, input.ns);
  if (containing) {
    const ancestry = spanAncestryAt(containing, input.spanById, input.ns);
    spanFocus = { spanId: ancestry.outermost.spanId, chain: ancestry.ids };
  }
  const focusedSpanId = spanFocus ? spanFocus.spanId : null;

  // Toggle off when re-clicking the selected task; else adopt the new
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

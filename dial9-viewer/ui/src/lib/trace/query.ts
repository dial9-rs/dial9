// Per-interaction read helpers: poll-at-timestamp binary search, the
// containing-span scan and ancestor walk from the lane-click handler
// (cycle-guarded), and task lookup on a poll hit. `enclosingSpans` is
// re-exported typed from the frozen core.

// MUST precede the trace_analysis.js import: its factory reads the
// TraceParser browser global in bundled entries (see core-globals.ts).
import "./core-globals.js";
import { enclosingSpans } from "../../../trace_analysis.js";
import type { PollSpan, TracingSpan } from "../../../trace_analysis.js";

export { enclosingSpans };

/**
 * Binary search for the span containing a given timestamp. Spans must be
 * non-overlapping and sorted by start time (the shape buildWorkerSpans
 * produces for polls/parks/actives). Returns null when no span contains
 * `ns`.
 */
export function findSpanAt<S extends { start: number; end: number }>(
  spans: readonly S[],
  ns: number
): S | null {
  let lo = 0;
  let hi = spans.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const s = spans[mid]!;
    if (s.end < ns) lo = mid + 1;
    else if (s.start > ns) hi = mid - 1;
    else return s;
  }
  return null;
}

/**
 * The task under a lane click: the poll at `ns`, if any, and its task.
 * `taskId` is null when the poll carries no task - the parser materializes
 * taskId 0 for traces without task tracking, treated here as "no task".
 */
export function taskAt(
  polls: readonly PollSpan[],
  ns: number
): { poll: PollSpan; taskId: number | null } | null {
  const poll = findSpanAt(polls, ns);
  if (!poll) return null;
  return { poll, taskId: poll.taskId ? poll.taskId : null };
}

/**
 * Find a span whose time range contains `ns` AND which has a segment
 * actively executing on `workerId` at `ns` - the span under a lane click.
 * First match in allSpans order (sorted by start).
 */
export function findContainingSpan(
  allSpans: readonly TracingSpan[],
  workerId: number,
  ns: number
): TracingSpan | null {
  for (const s of allSpans) {
    if (s.end < ns || s.start > ns) continue;
    for (const seg of s.segments) {
      if (seg.workerId === workerId && seg.start <= ns && seg.end >= ns) {
        return s;
      }
    }
  }
  return null;
}

/** Build the spanId -> span index the ancestor walk reads. */
export function spansById(
  allSpans: readonly TracingSpan[]
): Map<string, TracingSpan> {
  const byId = new Map<string, TracingSpan>();
  for (const s of allSpans) byId.set(s.spanId, s);
  return byId;
}

/** Defense against parent cycles in malformed span data. */
export const SPAN_ANCESTRY_CYCLE_LIMIT = 1024;

/** Result of walking a span's ancestor chain at an instant. */
export interface SpanAncestry {
  /** The outermost ancestor whose [start, end] still contains the instant. */
  outermost: TracingSpan;
  /** The walked chain's span ids (span itself + qualifying ancestors). */
  ids: Set<string>;
}

/**
 * Walk up the parent chain from `span` to the outermost ancestor whose
 * [start, end] still contains `ns`, collecting the chain's span ids along
 * the way (the span-panel focus + highlight set). The walk stops at a
 * missing parent, a parent not containing `ns`, or after
 * SPAN_ANCESTRY_CYCLE_LIMIT steps.
 *
 * Guard note: guarding on the id SET's size stops growing once a cycle
 * revisits a span - a parent cycle shorter than the limit would hang. This
 * counts iterations instead: identical behavior on well-formed data (a
 * distinct-ancestor chain also stops after 1024 steps), but actual cycles
 * terminate.
 */
export function spanAncestryAt(
  span: TracingSpan,
  byId: ReadonlyMap<string, TracingSpan>,
  ns: number
): SpanAncestry {
  let cur = span;
  let outermost = span;
  const ids = new Set<string>([span.spanId]);
  let steps = 0;
  while (cur.parentSpanId != null) {
    const parent = byId.get(cur.parentSpanId);
    if (!parent) break;
    if (parent.start <= ns && parent.end >= ns) {
      outermost = parent;
      ids.add(parent.spanId);
      cur = parent;
    } else {
      break;
    }
    // Defense against cycles in malformed data:
    if (++steps >= SPAN_ANCESTRY_CYCLE_LIMIT) break;
  }
  return { outermost, ids };
}

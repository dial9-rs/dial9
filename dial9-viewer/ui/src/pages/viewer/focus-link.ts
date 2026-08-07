// Non-destructive `focus_*` deep links.
//
// Emitted by the Span Explorer's exemplar Jump and by Tokio Stats' exemplar
// links, these pan/zoom the loaded trace onto one instance WITHOUT filtering
// events out. They are deliberately distinct from `start`/`end`, which the
// viewer treats as a hard parse filter that would drop every surrounding event
// and load an empty page for a narrow single-span window.
//
// `focus_span_name`, when present, lets the viewer select the exact SPAN rather
// than merely pan to a time window - a long span's window overlaps dozens of
// others, so time alone is not enough to identify it.

/**
 * The minimum a span must expose to be matched. Deliberately NOT TracingSpan:
 * on the columnar path the viewer never materializes fat span objects, so the
 * caller adapts whichever representation the trace uses.
 */
export interface FocusCandidate {
  spanId: string;
  start: number;
  end: number;
  spanName: string;
  taskId: number | null;
}

/** The parsed `focus_*` anchor. `startNs` is required; the rest refine it. */
export interface FocusLink {
  /** Wall-clock epoch ns (NOT the monotonic clock client spans use). */
  startNs: number;
  endNs?: number;
  /** Disambiguates when many spans overlap the window. */
  spanName?: string;
  workerId?: number;
  taskId?: number;
}

function num(v: string | null): number | null {
  if (v == null || v.length === 0) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

/** Read the `focus_*` anchor from a query string; null when absent. */
export function readFocusLink(search: string): FocusLink | null {
  const p = new URLSearchParams(search);
  const startNs = num(p.get("focus_start"));
  if (startNs == null) return null;
  const link: FocusLink = { startNs };
  const endNs = num(p.get("focus_end"));
  if (endNs != null) link.endNs = endNs;
  const spanName = p.get("focus_span_name");
  if (spanName != null && spanName.length > 0) link.spanName = spanName;
  const workerId = num(p.get("focus_worker"));
  if (workerId != null) link.workerId = workerId;
  const taskId = num(p.get("focus_task"));
  if (taskId != null) link.taskId = taskId;
  return link;
}

/**
 * The focus window on the trace's own MONOTONIC clock.
 *
 * `focus_*` values are wall-clock; client spans are monotonic, so the link is
 * unusable without the trace's offset. A null offset means the trace carried no
 * clock anchor - treat it as zero rather than dropping the link, matching how
 * the plain pan path handles it.
 */
export function focusWindow(
  link: FocusLink,
  clockOffsetNs: number | null,
): { start: number; end: number } {
  const offset = clockOffsetNs ?? 0;
  const start = link.startNs - offset;
  return { start, end: (link.endNs ?? link.startNs) - offset };
}

/**
 * The span a focus link points at, or null when nothing matches.
 *
 * Candidates must overlap the window and, when `spanName` is given, carry that
 * name. Among those, the winner is the one whose edges sit closest to the
 * requested window - the exemplar's own span is the tightest, boundary-aligned
 * match, while an enclosing span scores far worse.
 */
export function matchFocusSpan(
  candidates: Iterable<FocusCandidate>,
  link: FocusLink,
  clockOffsetNs: number | null,
): FocusCandidate | null {
  const { start, end } = focusWindow(link, clockOffsetNs);
  if (!Number.isFinite(start)) return null;
  let best: FocusCandidate | null = null;
  let bestScore = Infinity;
  for (const s of candidates) {
    if (link.spanName != null && s.spanName !== link.spanName) continue;
    if (s.end < start || s.start > end) continue;
    const score = Math.abs(s.start - start) + Math.abs(s.end - end);
    if (score < bestScore) {
      bestScore = score;
      best = s;
    }
  }
  return best;
}

/** Minimum framed width (ns), so a sub-microsecond span is still visible. */
const MIN_VIEW_NS = 1e6;
/** How much of the span's own duration to show around it. */
const CONTEXT_FACTOR = 5;
/** Fraction of the framed window that sits BEFORE the span. */
const LEAD_IN = 0.3;

/**
 * Frame the viewport on a matched span: roughly 5x its duration so it sits in
 * context rather than filling the screen edge to edge, clamped to the trace.
 */
export function focusViewport(
  span: Pick<FocusCandidate, "start" | "end">,
  bounds: { minTs: number; maxTs: number },
): { viewStart: number; viewEnd: number } {
  const dur = Math.max(span.end - span.start, 0);
  const viewDur = Math.max(dur * CONTEXT_FACTOR, MIN_VIEW_NS);
  const viewStart = Math.max(bounds.minTs, span.start - viewDur * LEAD_IN);
  return { viewStart, viewEnd: Math.min(bounds.maxTs, viewStart + viewDur) };
}

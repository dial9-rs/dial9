// The at-cursor info readout, rehomed into the persistent inspector surface.
// Computes the readout and writes it to the `transient.atCursor` store
// contract; the inspector renders it. `renderAtCursorStub` proves the wiring by
// rendering it into the inspector slot.
//
// PURE compute (Node-testable): the readout is bounded lookups over the
// overlay's derived series (data.ts), never a trace rescan. The stub renderer
// does zero measure-after-write (textContent/lit-html only) - it rides the
// transient channel like the crosshair, so it never triggers a track redraw.

import { html, render } from "lit-html";
import type { AtCursorReadout, SegmentEntry } from "../../types/state.js";

/** The frame-invariant series the readout reads (subset of OverlayData). */
export interface AtCursorInput {
  workerIds: readonly number[];
  queueSamples: readonly { t: number; global: number }[];
  workerQueueSamples: Readonly<Record<number, readonly { t: number; local: number }[]>>;
  activeTaskSamples: readonly { t: number; count: number }[];
}

/** Nearest sample to `ns` by |t - ns|; binary search. */
function nearestByT<T extends { t: number }>(arr: readonly T[], ns: number): T | null {
  if (arr.length === 0) return null;
  let lo = 0;
  let hi = arr.length - 1;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (arr[mid]!.t < ns) lo = mid + 1;
    else hi = mid;
  }
  const cand = arr[lo]!;
  if (lo > 0) {
    const prev = arr[lo - 1]!;
    if (Math.abs(prev.t - ns) <= Math.abs(cand.t - ns)) return prev;
  }
  return cand;
}

/** Active-task count at `ns`: the latest sample with t <= ns (step). */
function activeTaskCountAt(
  samples: readonly { t: number; count: number }[],
  ns: number,
): number | null {
  if (samples.length === 0) return null;
  let lo = 0;
  let hi = samples.length - 1;
  let ans = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (samples[mid]!.t <= ns) {
      ans = mid;
      lo = mid + 1;
    } else hi = mid - 1;
  }
  return ans >= 0 ? samples[ans]!.count : 0;
}

/**
 * Windowed-data completeness at `ns`. Maps the covering segment's residency
 * onto the readout's coverage signal so the surface never presents a truncated
 * window as whole: an "oversized" covering segment -> "oversized"; a
 * listed/fetching/evicted one (partial data at this edge) -> "truncated"; a
 * parsed one, or the whole-trace (non-segmented) path where the map is empty,
 * -> "complete".
 */
export function coverageAt(
  segments: ReadonlyMap<string, SegmentEntry>,
  ns: number,
): AtCursorReadout["coverage"] {
  if (segments.size === 0) return "complete";
  let covering: SegmentEntry | null = null;
  for (const seg of segments.values()) {
    if (ns >= seg.extent.startNs && ns <= seg.extent.endNs) {
      covering = seg;
      break;
    }
  }
  if (covering === null) return "complete";
  switch (covering.state) {
    case "oversized":
      return "oversized";
    case "parsed":
      return "complete";
    case "listed":
    case "fetching":
    case "evicted":
      return "truncated";
  }
}

/**
 * Compute the at-cursor readout at `ns` for the worker under the cursor
 * (`workerId`, null when off any lane): nearest global-queue sample, MAX
 * local-queue across workers, step active-task count. `activeTaskCount` is null
 * when the trace has no task timeline (the line is omitted then).
 */
export function computeAtCursorReadout(
  input: AtCursorInput,
  ns: number,
  workerId: number | null,
  coverage: AtCursorReadout["coverage"],
): AtCursorReadout {
  const nearestGlobal = nearestByT(input.queueSamples, ns);

  let localMax: number | null = null;
  for (const w of input.workerIds) {
    const samples = input.workerQueueSamples[w];
    if (!samples || samples.length === 0) continue;
    const s = nearestByT(samples, ns);
    if (s !== null) localMax = localMax === null ? s.local : Math.max(localMax, s.local);
  }

  return {
    ns,
    workerId,
    globalQueue: nearestGlobal !== null ? nearestGlobal.global : null,
    localMax,
    activeTaskCount: activeTaskCountAt(input.activeTaskSamples, ns),
    coverage,
  };
}

// Inspector stub (proves the store wiring).

const STUB_CLASS = "d9-atcursor-readout";

/** The inspector body the readout mirrors into (statically templated). */
function inspectorBody(root: ParentNode): HTMLElement | null {
  return root.querySelector<HTMLElement>(".d9-inspector-body");
}

/**
 * Ensure the at-cursor readout stub exists in the inspector body (idempotent),
 * then render `readout` into it. Appends into a container with no lit-html
 * dynamic binding (only static content), so the shell's declarative re-renders
 * do not clobber it. Null hides the readout.
 */
export function renderAtCursorStub(root: ParentNode, readout: AtCursorReadout | null): void {
  const body = inspectorBody(root);
  if (!body) return;
  let host = body.querySelector<HTMLElement>(`.${STUB_CLASS}`);
  if (!host) {
    host = body.ownerDocument.createElement("div");
    host.className = STUB_CLASS;
    host.setAttribute("aria-label", "At-cursor stats");
    // Insert first so it reads as the "current moment" header.
    body.insertBefore(host, body.firstChild);
  }
  if (readout === null) {
    render(html`<span class="d9-atcursor-empty">Hover the timeline for at-cursor stats</span>`, host);
    return;
  }
  const num = (v: number | null): string => (v !== null ? String(v) : "-");
  const coverageNote =
    readout.coverage === "truncated"
      ? html` <span class="d9-atcursor-warn">partial window</span>`
      : readout.coverage === "oversized"
        ? html` <span class="d9-atcursor-warn">oversized segment</span>`
        : "";
  render(
    html`
      <div class="d9-atcursor-row">
        <span class="label">Worker</span>
        <span class="value">${readout.workerId !== null ? String(readout.workerId) : "-"}</span>
        <span class="label">Global Q</span>
        <span class="value">${num(readout.globalQueue)}</span>
        <span class="label">Local max</span>
        <span class="value">${num(readout.localMax)}</span>
        ${readout.activeTaskCount !== null
          ? html`<span class="label">Active tasks</span>
              <span class="value">${String(readout.activeTaskCount)}</span>`
          : ""}${coverageNote}
      </div>
    `,
    host,
  );
}

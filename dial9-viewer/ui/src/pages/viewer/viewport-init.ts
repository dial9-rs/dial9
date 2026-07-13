// src/pages/viewer/viewport-init.ts - initialize the viewport slice from a
// freshly loaded trace (T21).
//
// loadTraceInWorker only writes the `trace` slice (T16 owns the load, not
// the view). The viewport's navigable bounds and initial window come from
// the trace's time extent, so the shell wires a small subscriber: when a
// NEW trace becomes resident, fit the view to [minTs, maxTs] and pin those
// as the navigable bounds (features/02 H3 "Fit All" is the resting view).
// Keyed on trace identity so a later pan/zoom (which never replaces the
// trace slice) is not clobbered; a genuine reload (new trace object) refits.

import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";

/**
 * Subscribe `store` so the viewport fits each newly loaded trace. Returns
 * the unsubscribe function.
 */
export function initViewportFromTrace(store: ViewerStore): () => void {
  let lastTrace: StoreState["trace"]["trace"] | null = null;
  return store.subscribe(["trace"], (state) => {
    const trace = state.trace.trace;
    if (trace === null || trace === lastTrace) return;
    lastTrace = trace;
    if (trace.minTs === null || trace.maxTs === null) return;
    store.update("viewport", {
      viewStart: trace.minTs,
      viewEnd: trace.maxTs,
      minTs: trace.minTs,
      maxTs: trace.maxTs,
    });
  });
}

import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";

/**
 * Subscribe `store` so the viewport fits each newly loaded trace (keyed on
 * trace identity, so a later pan/zoom is not clobbered). Returns the
 * unsubscribe function.
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

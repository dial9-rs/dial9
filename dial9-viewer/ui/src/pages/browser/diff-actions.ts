// Differential comparison (A/B) verbs.
//
// Two captured aggregate scopes launched into either viz via the shared
// FlamegraphDiff scope-link codec (?diff=1&a=<b64>&b=<b64>), which both
// flamegraph.html and tokio_stats.html consume. Held in the store only.

// Leaf seam modules, NOT the lib barrels: the barrel indexes evaluate
// modules that import trace_analysis.js / trace_parser.js at init (they
// expect <script>-established globals), which this page must not load.
import {
  addDiffCapture,
  chooseTarget,
  removeDiffSide,
  swapDiffCapture,
} from "../../lib/canvas/flamegraph_diff.js";
import type { BrowserEls } from "./dom.js";
import { selectionScope } from "./open-links.js";
import type { BrowserStore } from "./state.js";

export interface DiffActions {
  addToDiff(): void;
  clearDiff(): void;
  swapDiff(): void;
  clearDiffSide(side: "a" | "b"): void;
  launchDiff(kind: "flamegraph" | "tokio"): void;
}

export function createDiffActions(store: BrowserStore, els: BrowserEls): DiffActions {
  // Capture the current selection as one side (A first, then B) of a diff.
  function addToDiff(): void {
    const scope = selectionScope(store, els);
    if (!scope) {
      if (!els.bucketInput.value.trim()) alert("Bucket is required");
      return;
    }
    store.update("diff", addDiffCapture(store.getState().diff, scope));
  }

  function clearDiff(): void {
    store.update("diff", { a: null, b: null });
  }

  // Swap A and B (no-op unless both are set - the codec fills A first).
  function swapDiff(): void {
    store.update("diff", swapDiffCapture(store.getState().diff));
  }

  // Remove one side (clearing A promotes B to A so there's never a B-without-A).
  function clearDiffSide(side: "a" | "b"): void {
    store.update("diff", removeDiffSide(store.getState().diff, side));
  }

  // Launch the captured A/B diff in the chosen viz via the shared scope-link
  // codec, so flamegraph and tokio-stats receive the same two-scope encoding.
  function launchDiff(kind: "flamegraph" | "tokio"): void {
    const d = store.getState().diff;
    if (!d.a || !d.b) return;
    const { page, search } = chooseTarget(kind, { hasDiff: true, diffA: d.a, diffB: d.b });
    window.open(page + "?" + search, "_blank");
  }

  return { addToDiff, clearDiff, swapDiff, clearDiffSide, launchDiff };
}

// The diff view's URL persistence: the `diff_zoom` / `diff_search` query params
// (flamegraph_view_state.js readDiffState/writeDiffState). Kept separate from
// diff-mode.ts (which pulls the DOM-mounting diff view) so the pure codec is
// unit-testable without a browser. `zoom` is the merged-root-inclusive frame
// path, TAB-joined; `search` is the highlight regex.

import type { DiffViewState } from "../../lib/canvas/index.js";

const SEP = "\t";

/** Parse the diff view's zoom/highlight from a query string or params. */
export function readDiffState(search: string | URLSearchParams): DiffViewState {
  const p = typeof search === "string" ? new URLSearchParams(search) : search;
  const out: DiffViewState = {};
  const zoom = (p.get("diff_zoom") || "").split(SEP).filter((s) => s.length > 0);
  if (zoom.length) out.zoom = zoom;
  const searchQ = p.get("diff_search");
  if (searchQ) out.search = searchQ;
  return out;
}

/** Write the diff view's zoom/highlight into `params` (set present, delete absent). */
export function writeDiffState(params: URLSearchParams, st: DiffViewState): void {
  const zoom = st.zoom && st.zoom.length ? st.zoom.join(SEP) : null;
  if (zoom) params.set("diff_zoom", zoom);
  else params.delete("diff_zoom");
  if (st.search) params.set("diff_search", st.search);
  else params.delete("diff_search");
}

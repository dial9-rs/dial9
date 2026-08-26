// Typed seam over the frozen two-sided diff VIEW (flamegraph_diff_view.js): it
// self-mounts its own DOM (header, breadcrumb, two canvas panels) into a
// container and owns the per-side `/api/flamegraph` SSE streams. Pages compose
// it through this re-export instead of importing the core module directly.
//
// flamegraph_diff_view.js resolves its dependencies through BARE globals in the
// browser (`FlamegraphDiff`, `FlamegraphApi`, `formatHumanDuration`);
// ./diff-globals seeds them, imported FIRST so it runs before this file's
// re-export links the view. The seeding is load-order critical: the canvas
// barrel pulls this seam into EVERY page (the viewer included, for the shared
// widget), so an unseeded FlamegraphDiff would throw on plain page load.
import "./diff-globals.js";

export {
  createDiffView,
  apiUrlFor,
  scopeLabel,
  isSearchFocusKey,
} from "../../../flamegraph_diff_view.js";
export type {
  DiffViewOptions,
  DiffViewHandle,
  DiffViewState,
} from "../../../flamegraph_diff_view.js";

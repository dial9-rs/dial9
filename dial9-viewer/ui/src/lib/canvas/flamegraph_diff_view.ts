// Typed seam over the frozen two-sided diff VIEW (flamegraph_diff_view.js): it
// self-mounts its own DOM (header, breadcrumb, two canvas panels) into a
// container and owns the per-side `/api/flamegraph` SSE streams. Pages compose
// it through this re-export instead of importing the core module directly.
//
// flamegraph_diff_view.js resolves flamegraph_diff.js / flamegraph_api.js /
// sse.js with literal require(...) calls that Vite's CommonJS interop resolves
// at build time, so those ride into the bundle with it. Its one bare-global
// dependency is `formatHumanDuration` (the panel stats line), so - like the
// widget's export/core-globals seed chain - this MUST seed that global before
// the view renders.

import { formatHumanDuration } from "../../../format.js";

// The frozen view reads `formatHumanDuration` off the global scope (browser
// <script src> contract). Seed it for bundled entries; `??=` keeps a real page
// that already loaded format.js untouched.
(globalThis as Record<string, unknown>)["formatHumanDuration"] ??= formatHumanDuration;

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

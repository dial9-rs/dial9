// lib/canvas/flamegraph.ts - typed seam over the frozen flamegraph widget
// (T13; architecture 2.7's "lib/canvas re-exports the core's drawing math
// through its own barrel under the same rule"). flamegraph.js is frozen
// core: the shared canvas flamegraph (rendering, zoom stacks, search,
// tooltip, export menu). Pages compose it through this re-export instead
// of importing the ui-root module (scripts/check-core-imports.mjs).
//
// Bundling note: flamegraph.js resolves its own dependencies with literal
// `require("./trace_analysis.js")` / `require("./flamegraph_export.js")`
// calls, which Vite's CommonJS interop resolves at build time - both ride
// into the page bundle with it (the F66 "export module missing" fallback
// is unreachable in bundled entries, exactly like legacy pages that load
// all three <script src> tags).

export { createFlamegraph, filterCpuSamples } from "../../../flamegraph.js";
export type {
  FlamegraphDataSample,
  FlamegraphInstance,
  FlamegraphSetDataOptions,
} from "../../../flamegraph.js";

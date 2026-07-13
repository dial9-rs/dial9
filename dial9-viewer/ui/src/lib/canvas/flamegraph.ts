// Typed seam over the frozen flamegraph widget (flamegraph.js): the shared
// canvas flamegraph (rendering, zoom stacks, search, tooltip, export
// menu). Pages compose it through this re-export instead of importing the
// core module directly.
//
// flamegraph.js resolves its own dependencies with literal
// require("./trace_analysis.js") / require("./flamegraph_export.js") calls
// that Vite's CommonJS interop resolves at build time, so both ride into
// the page bundle with it.

// MUST precede the flamegraph.js import: its factory reads the
// TraceAnalysis and FlamegraphExport browser globals in bundled entries
// (see export-globals.ts / core-globals.ts for the seed chain).
import "./export-globals.js";
export { createFlamegraph, filterCpuSamples } from "../../../flamegraph.js";
export type {
  FlamegraphDataSample,
  FlamegraphInstance,
  FlamegraphSetDataOptions,
} from "../../../flamegraph.js";

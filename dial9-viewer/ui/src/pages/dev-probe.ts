// Placeholder Vite entry: `vite build` needs at least one input before any
// page migrates (T13/T14 introduce the real page entries). Also serves as
// the trivial `src/` module the T04 dev-loop DoD edits to demonstrate HMR.
export const uiScaffoldVersion = 1;

// T16: pull the worker load pipeline into the rollup graph so every
// `npm run build` exercises Vite's worker bundling (load.ts's inline
// `new Worker(new URL("./worker/trace-worker.ts", import.meta.url))`
// emits the trace-worker chunk under dist/assets) BEFORE any real page
// migrates - otherwise the first build-time validation of the worker
// entry would wait for T13. Re-exporting the value keeps rollup from
// tree-shaking the import; page tickets replace this probe with real
// entries that consume the barrel for real.
export { loadTraceInWorker as probeWorkerLoadPipeline } from "../lib/trace/index.js";

// Typed seam over the frozen tokio-stats helpers (tokio_stats_api.js). Surfaces
// the heat and worker-summary helpers; coverage/refine helpers are reimplemented
// in src/pages/tokio-stats.
// lib/trace is the sanctioned frozen-core import boundary.

export {
  busynessHeat,
  hostBusyPct,
  hostWorkerCounts,
  latencyHeat,
} from "../../../tokio_stats_api.js";

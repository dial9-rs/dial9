// Typed seam over the frozen tokio-stats helpers (tokio_stats_api.js). Surfaces
// the exemplar deep-link builder and the latency heat scale; the
// coverage/refine helpers are reimplemented in src/pages/tokio-stats.
// lib/trace is the sanctioned frozen-core import boundary.

export { exemplarViewerUrl, latencyHeat } from "../../../tokio_stats_api.js";
export type { ExemplarViewerOpts } from "../../../tokio_stats_api.js";

// Typed seam over the frozen tokio-stats helpers (tokio_stats_api.js). Only the
// exemplar deep-link builder is surfaced; the coverage/latency/refine helpers
// are reimplemented in src/pages/tokio-stats. lib/trace is the sanctioned
// frozen-core import boundary.

export { exemplarViewerUrl } from "../../../tokio_stats_api.js";
export type { ExemplarViewerOpts } from "../../../tokio_stats_api.js";

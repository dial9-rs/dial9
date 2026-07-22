// Typed re-exports of the aggregated-mode (`?api=1`) display/format helpers
// from flamegraph_api.js, shared verbatim with the legacy flamegraph page
// so the coverage badge, picker conversions and facet options cannot drift
// between the two UIs. formatHumanDuration rides along from the frozen
// format.js. (The client-side plateau/auto-stop helper is gone: aggregation is
// a server-driven SSE stream now, so the server owns the refine/stop loop.)

export {
  coveragePercent,
  foldErrorNotice,
  formatCoverageBadge,
  hostFacetOptions,
  msToNs,
  nextMaxFiles,
  nsToMs,
  nsToPickerUtc,
  pickerUtcToNs,
  refinementWorkDepth,
  shouldAdoptRefinementSnapshot,
} from "../../../flamegraph_api.js";
export type { FacetOption, LegacyCoverage } from "../../../flamegraph_api.js";

export { formatHumanDuration } from "../../../format.js";

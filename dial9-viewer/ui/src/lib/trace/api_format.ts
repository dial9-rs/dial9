// Typed re-exports of the aggregated-mode (`?api=1`) display/format helpers
// from flamegraph_api.js, keeping the coverage badge, picker conversions and
// facet options on the shared implementation. formatHumanDuration rides along
// from the frozen format.js. (The client-side plateau/auto-stop helper is gone:
// aggregation is a server-driven SSE stream now, so the server owns the loop.)

export {
  coveragePercent,
  decodeFlamegraphTree,
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
export type {
  DecodedFlamegraphNode,
  FacetOption,
  LegacyCoverage,
} from "../../../flamegraph_api.js";

export { formatHumanDuration } from "../../../format.js";

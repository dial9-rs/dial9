// Typed re-exports of the aggregated-mode (`?api=1`) display/format helpers
// from flamegraph_api.js, shared verbatim with the legacy flamegraph page
// so the coverage badge, auto-stop plateau, picker conversions and facet
// options cannot drift between the two UIs. formatHumanDuration rides along
// from the frozen format.js.

export {
  coveragePercent,
  formatCoverageBadge,
  hostFacetOptions,
  nextMaxFiles,
  nsToPickerUtc,
  pickerUtcToNs,
  shouldAutoStopRefining,
} from "../../../flamegraph_api.js";
export type { FacetOption, LegacyCoverage } from "../../../flamegraph_api.js";

export { formatHumanDuration } from "../../../format.js";

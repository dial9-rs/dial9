// lib/trace/api_format.ts - typed re-exports of the aggregated-mode
// (`?api=1`) display/format helpers (T13).
//
// flamegraph_api.js is NOT frozen core (post-freeze page module) but it
// lives at the ui/ root in browser-global + CommonJS-guard form, is still
// loaded by the LEGACY flamegraph page via <script src>, and is unit-tested
// (tests/core/flamegraph_api.test.ts). The migrated page reuses it verbatim
// through this seam - one implementation on both sides of the dual-UI
// switch means the coverage badge, auto-stop plateau, picker conversions
// and facet options cannot drift while both pages are servable. The import
// lives in lib/trace because the boundary check
// (scripts/check-core-imports.mjs) admits ui-root .js imports only here
// and in lib/canvas, and these helpers belong to the /api/flamegraph
// domain that ./aggregates.ts owns.
//
// Deliberately NOT re-exported: isCoverageFrozen (the typed port in
// ./aggregates.ts is the src/-side implementation - same semantics) and
// sourceFacetOptions / threadFacetOptions (CODE-ONLY dead helpers at HEAD:
// no page consumes them - see features/03 section P notes).
//
// formatHumanDuration rides along from the frozen format.js (the legacy
// api mode loads format.js for exactly this one call in its stats line).

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

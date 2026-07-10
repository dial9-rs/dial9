// lib/trace/prefixes.ts - typed re-exports of the frozen core's S3
// prefix-discovery heuristics (T14; features/01 D4, issue #471).
//
// The implementations live in prefix_detect.js (frozen core) and are typed
// by src/types/prefix_detect.d.ts; lib/trace is the sanctioned import seam
// (scripts/check-core-imports.mjs). The browser page consumes these through
// the barrel for its prefix-discovery flow.

export { isDateLayer, lastSegment } from "../../../prefix_detect.js";

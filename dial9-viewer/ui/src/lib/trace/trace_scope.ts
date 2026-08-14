// Typed seam over the shared trace_scope.js scope codec: it maps a heatmap /
// table selection to a compact, stateless scope (bucket + prefix + service +
// host set + time window) so a large selection's deep link stays under
// CloudFront's 8192-byte request-URI cap and re-resolves in any browser.
//
// Only the scope-building/encoding surface is re-exported: parseKey /
// extractPrefix / objectTraceUrls have their own canonical seams (keys.ts,
// object-urls.ts) whose TypeScript reimplementations page code uses everywhere
// else. scopeFromKeys deliberately keeps trace_scope.js's own
// positional key parser so a scope built here matches viewer.html and
// flamegraph.html exactly.
//
// trace_scope.js is dependency-free (only URLSearchParams), so it needs no
// global-seed chain like the flamegraph widget does.

export {
  scopeFromKeys,
  encodeScope,
  encodeAggregationParams,
  readScope,
  hasScope,
  resolveScope,
} from "../../../trace_scope.js";
export type {
  EncodeScopeOptions,
  EncodedScope,
  TraceScope,
} from "../../../trace_scope.js";

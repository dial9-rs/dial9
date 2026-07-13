// src/pages/flamegraph/query.ts - pure URL/label logic for the flamegraph
// page (T13), extracted from the legacy inline bootstrap
// (flamegraph.html:92-642) so the N10 URL contract is unit-testable:
//
// - trace-URL resolution against the ORIGIN ROOT (the legacy page lives at
//   /flamegraph.html, so its relative `?trace=` values are root-relative
//   by construction; the migrated entry lives at /new/flamegraph.html and
//   hands the fetch to a worker whose script URL is under /assets/, so
//   both would silently mis-resolve a relative value without this);
// - the exact-mode loading-phase labels (features/03 F9);
// - the aggregated-mode (`?api=1`) /api/flamegraph query and browser-URL
//   query builders (F168/F180), byte-compatible with the legacy
//   buildApiUrl/updateBrowserUrl including parameter order.

/**
 * Resolve each `trace=` component against the origin root. Relative
 * values ("demo-trace.bin") and root-relative values ("/api/object?...")
 * resolve exactly as they do on the root-level legacy page; absolute URLs
 * pass through untouched.
 */
export function resolveTraceUrls(
  rawUrls: readonly string[],
  origin: string
): string[] {
  return rawUrls.map((u) => new URL(u, origin + "/").toString());
}

/**
 * The exact-mode loading label (F9). Streaming fuses fetch+parse, so the
 * label is "Loading ..." throughout; buffered mode shows "Fetching ..."
 * then "Parsing trace..." (all with the U+2026 ellipsis, as legacy).
 */
export function loadingLabel(
  mode: "stream" | "buffered",
  phase: "fetching" | "parsing",
  urlCount: number
): string {
  if (mode === "stream") {
    return urlCount > 1 ? `Loading ${urlCount} traces\u2026` : "Loading trace\u2026";
  }
  if (phase === "fetching") {
    return urlCount > 1 ? `Fetching ${urlCount} traces\u2026` : "Fetching trace\u2026";
  }
  return "Parsing trace\u2026";
}

/**
 * The aggregated-mode query state, seeded from the page URL and mutated
 * by the toolbar (api-mode.ts). `facets` carries every non-host facet
 * filter - the three seeded keys plus any facet the backend later
 * reports - in insertion order (the order rides into the query string,
 * matching the legacy filterState object).
 */
export interface ApiQueryState {
  /** data_dir passthrough (local-dir mode); empty/absent skipped. */
  dataDir: string | null;
  bucket: string | null;
  prefix: string | null;
  service: string | null;
  /** Host filter; each entry becomes a repeated `host=` param. */
  hosts: readonly string[];
  /** Facet name -> value; empty values are kept but not serialized. */
  facets: Readonly<Record<string, string>>;
  /** Epoch-ns strings (pickerUtcToNs form); null omitted. */
  startNs: string | null;
  endNs: string | null;
  /** "Refine more" ceiling; null = backend default. */
  maxFiles: number | null;
}

/** Seed the non-host facet filters from the page URL (legacy order). */
export function seedFacetState(params: URLSearchParams): Record<string, string> {
  return {
    // Source defaults to "cpu" (the on-CPU view).
    source: params.get("source") || "cpu",
    thread_class: params.get("thread_class") || "",
    spawn_location: params.get("spawn_location") || "",
  };
}

function appendScope(p: URLSearchParams, state: ApiQueryState): void {
  if (state.dataDir) p.set("data_dir", state.dataDir);
  if (state.bucket) p.set("bucket", state.bucket);
  if (state.prefix) p.set("prefix", state.prefix);
  if (state.service) p.set("service", state.service);
  for (const h of state.hosts) p.append("host", h);
  // Send all non-empty facet filters as query params.
  for (const [k, v] of Object.entries(state.facets)) {
    if (!v) continue;
    p.set(k, v);
  }
  if (state.startNs) p.set("start_ns", state.startNs);
  if (state.endNs) p.set("end_ns", state.endNs);
}

/**
 * The GET /api/flamegraph URL for one poll (F168/F173). The first poll
 * per scope is read-only (`refine` omitted); refining polls send the
 * literal `refine=true` (the backend param is a serde bool - "1" would
 * 400).
 */
export function buildApiUrl(
  state: ApiQueryState,
  refine: boolean,
  origin: string
): string {
  const u = new URL("/api/flamegraph", origin);
  appendScope(u.searchParams, state);
  if (state.maxFiles != null) u.searchParams.set("max_files", String(state.maxFiles));
  if (refine) u.searchParams.set("refine", "true");
  return u.toString();
}

/**
 * The browser-URL query for Apply / facet changes (F180). Rebuilt from
 * scratch - `api=1` + scope + facet state + picker times, NO max_files
 * and no `ui` param - exactly like the legacy updateBrowserUrl.
 */
export function buildBrowserQuery(state: ApiQueryState): string {
  const p = new URLSearchParams();
  p.set("api", "1");
  appendScope(p, state);
  return p.toString();
}

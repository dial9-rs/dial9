// Pure URL/label logic for the flamegraph page, extracted so the URL contract
// is unit-testable: trace-URL resolution against the origin root, the exact-
// mode loading-phase labels, and the aggregated-mode (`?api=1`) /api/flamegraph
// query and browser-URL builders.

import {
  writeRequestParams,
  writeUrlParams,
} from "../../lib/trace/index.js";
import type { SourceScope } from "../../lib/trace/index.js";

/**
 * Resolve each `trace=` component against the origin root. Relative
 * values ("demo-trace.bin") and root-relative values ("/api/object?...")
 * resolve against `/`; absolute URLs pass through untouched.
 */
export function resolveTraceUrls(
  rawUrls: readonly string[],
  origin: string
): string[] {
  return rawUrls.map((u) => new URL(u, origin + "/").toString());
}

/**
 * The exact-mode loading label. Streaming fuses fetch+parse, so the label is
 * "Loading ..." throughout; buffered mode shows "Fetching ..." then "Parsing
 * trace..." (all with the U+2026 ellipsis).
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
 * The aggregated-mode query state, seeded from the page URL and mutated by the
 * toolbar (api-mode.ts). `facets` carries every non-host facet filter - the
 * three seeded keys plus any facet the backend later reports - in insertion
 * order (the order rides into the query string).
 */
export interface ApiQueryState {
  /** data_dir passthrough (local-dir mode); empty/absent skipped. */
  dataDir: string | null;
  /** Canonical bucket, region, and credential identity. */
  source: SourceScope;
  prefix: string | null;
  service: string | null;
  /** Host filter; each entry becomes a repeated `host=` param. */
  hosts: readonly string[];
  /** Facet name -> value; empty values are kept but not serialized. */
  facets: Readonly<Record<string, string>>;
  /** Epoch-ns strings (pickerUtcToNs form); null omitted. */
  startNs: string | null;
  endNs: string | null;
  /**
   * Poll-duration band bounds (min_poll_ns/max_poll_ns), ns strings; null
   * omitted. Set by the minimap brush / band inputs (msToNs of the ms boxes).
   */
  minPollNs: string | null;
  maxPollNs: string | null;
  /** "Refine more" ceiling; null = backend default. */
  maxFiles: number | null;
  /**
   * Span-type filter carried by Span Explorer deep links: scope the tree to
   * samples enclosed by a span of this type, optionally within a duration band.
   * FIXED for the life of the view - not toolbar-editable - so these live
   * alongside the other load-scope fields rather than in the facet state.
   */
  spanTypeUid: string | null;
  minSpanNs: string | null;
  maxSpanNs: string | null;
  /** Current aggregate inspect frame, retained by bounded server projections. */
  inspect: string | null;
}

/** Seed the non-host facet filters from the page URL. */
export function seedFacetState(params: URLSearchParams): Record<string, string> {
  return {
    // Source defaults to "cpu" (the on-CPU view).
    source: params.get("source") || "cpu",
    thread_class: params.get("thread_class") || "",
    spawn_location: params.get("spawn_location") || "",
  };
}

// The data + facet + window params (everything except the bucket+region+role
// identity, which the two callers layer on differently: a request URL omits the
// role, a shareable link carries it). `data_dir` and `bucket` are mutually
// exclusive (local-dir vs S3), so putting the identity's bucket ahead of this
// costs no ordering the caller relies on.
function appendScope(p: URLSearchParams, state: ApiQueryState): void {
  if (state.dataDir) p.set("data_dir", state.dataDir);
  if (state.prefix) p.set("prefix", state.prefix);
  if (state.service) p.set("service", state.service);
  for (const h of state.hosts) p.append("host", h);
  for (const [k, v] of Object.entries(state.facets)) {
    if (!v) continue;
    p.set(k, v);
  }
  if (state.startNs) p.set("start_ns", state.startNs);
  if (state.endNs) p.set("end_ns", state.endNs);
  if (state.minPollNs) p.set("min_poll_ns", state.minPollNs);
  if (state.maxPollNs) p.set("max_poll_ns", state.maxPollNs);
  if (state.spanTypeUid) p.set("span_type_uid", state.spanTypeUid);
  if (state.minSpanNs) p.set("min_span_ns", state.minSpanNs);
  if (state.maxSpanNs) p.set("max_span_ns", state.maxSpanNs);
  if (state.inspect) p.set("inspect", state.inspect);
}

/**
 * The GET /api/flamegraph URL for the aggregation stream. The endpoint is an
 * SSE stream: the server emits the already-folded snapshot first, then folds to
 * the cap and pushes a fresh tree per file, closing when done. There is no
 * client `refine` flag - the server owns the refine/stop loop.
 */
export function buildApiUrl(state: ApiQueryState, origin: string): string {
  const u = new URL("/api/flamegraph", origin);
  // Request URL: bucket+region only. The role is header-only (restored into
  // creds at boot), so it must NOT appear here — a role on both header and
  // query is the server's ConflictingCredentials 400.
  writeRequestParams(u.searchParams, state.source);
  appendScope(u.searchParams, state);
  if (state.maxFiles != null) u.searchParams.set("max_files", String(state.maxFiles));
  u.searchParams.set("format", "flat-v1");
  return u.toString();
}

/**
 * The browser-URL query for Apply / facet changes. Rebuilt from scratch -
 * `api=1` + scope + facet state + picker times, with no `ui` param.
 *
 * `max_files` rides along so a copied link reopens at the depth on screen
 * rather than resetting to the backend default.
 */
export function buildBrowserQuery(state: ApiQueryState): string {
  const p = new URLSearchParams();
  p.set("api", "1");
  // Address-bar link: carries the role (aws_role_arn) so a fresh tab restores
  // the identity at boot; it is never re-emitted onto a request URL from there.
  writeUrlParams(p, state.source);
  appendScope(p, state);
  if (state.maxFiles != null) p.set("max_files", String(state.maxFiles));
  return p.toString();
}

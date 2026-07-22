// The page's URL contract: the load scope read once at boot, the
// /api/span-stats request URL per stream mode, and the address-bar sync.
//
// Two modes share this page:
//   - RAW (`?trace=<url>`): the trace is fetched and parsed in the browser and
//     the catalog is built client-side. No backend, so no exemplars/flamegraphs.
//   - AGGREGATE (`?api=1` / `?bucket=` / `?data_dir=`): /api/span-stats streams
//     server-computed statistics over SSE.

import { formatAttrFilterParams, setMaxFilesParam } from "../../lib/trace/index.js";
import type { AttrFilter, StreamMode } from "../../lib/trace/index.js";

/** The load scope, fixed for the page's lifetime (read once at boot). */
export interface PageScope {
  /** Raw-trace source URL; null in aggregate mode. */
  trace: string | null;
  dataDir: string | null;
  bucket: string | null;
  region: string | null;
  prefix: string | null;
  service: string | null;
  hosts: string[];
}

/** Everything the request/address-bar builders need beyond the fixed scope. */
export interface ViewState {
  startNs: string | null;
  endNs: string | null;
  selectedUid: string | null;
  bandMinNs: number | null;
  bandMaxNs: number | null;
  attrFilters: readonly AttrFilter[];
  maxFiles: number | null;
}

export function readScope(params: URLSearchParams): PageScope {
  return {
    trace: params.get("trace"),
    dataDir: params.get("data_dir"),
    bucket: params.get("bucket"),
    region: params.get("aws_region"),
    prefix: params.get("prefix"),
    service: params.get("service"),
    hosts: params.getAll("host"),
  };
}

/**
 * Does this URL address the aggregation backend at all? Raw mode wins outright;
 * otherwise any of the explicit aggregate selectors turns the stream on.
 */
export function isAggregateMode(params: URLSearchParams, scope: PageScope): boolean {
  if (scope.trace != null) return false;
  return params.get("api") === "1" || scope.bucket != null || scope.dataDir != null;
}

/** Append the fixed scope params shared by every request and link. */
function appendScope(p: URLSearchParams, scope: PageScope): void {
  if (scope.dataDir) p.set("data_dir", scope.dataDir);
  if (scope.bucket) p.set("bucket", scope.bucket);
  if (scope.region) p.set("aws_region", scope.region);
  if (scope.prefix) p.set("prefix", scope.prefix);
  if (scope.service) p.set("service", scope.service);
  for (const h of scope.hosts) p.append("host", h);
}

/**
 * The /api/span-stats URL for one stream.
 *
 * The duration band scopes only the backend's bounded exemplar candidates;
 * catalog statistics are unaffected by it. Attribute filters, by contrast,
 * narrow the whole aggregate server-side (counts, histograms, composition).
 *
 * An `exemplars` stream reads only already-folded spans parts and parses NO
 * additional raw files, so it is cheap and never drives the refine loop.
 */
export function buildApiUrl(
  mode: StreamMode,
  scope: PageScope,
  view: ViewState,
  origin: string,
): string {
  const u = new URL("/api/span-stats", origin);
  appendScope(u.searchParams, scope);
  if (view.startNs) u.searchParams.set("start_ns", view.startNs);
  if (view.endNs) u.searchParams.set("end_ns", view.endNs);
  if (view.bandMinNs != null) u.searchParams.set("min_span_ns", String(view.bandMinNs));
  if (view.bandMaxNs != null) u.searchParams.set("max_span_ns", String(view.bandMaxNs));
  for (const a of formatAttrFilterParams(view.attrFilters)) u.searchParams.append("attr", a);
  setMaxFilesParam(u.searchParams, view.maxFiles);
  if (mode === "exemplars") {
    u.searchParams.set("exemplars_only", "true");
    if (view.selectedUid) u.searchParams.set("span_type_uid", view.selectedUid);
  }
  return u.toString();
}

/**
 * The shareable address-bar query for the current view.
 *
 * RAW mode keeps its `trace` param and never gains `api=1`: the legacy page
 * dropped both on the first selection, so reloading or sharing the resulting
 * URL landed in aggregate mode with no scope.
 */
export function buildBrowserQuery(scope: PageScope, view: ViewState): string {
  const p = new URLSearchParams();
  if (scope.trace != null) p.set("trace", scope.trace);
  else p.set("api", "1");
  appendScope(p, scope);
  if (view.startNs) p.set("start_ns", view.startNs);
  if (view.endNs) p.set("end_ns", view.endNs);
  if (view.selectedUid) p.set("span_type_uid", view.selectedUid);
  if (view.bandMinNs != null) p.set("min_span_ns", String(view.bandMinNs));
  if (view.bandMaxNs != null) p.set("max_span_ns", String(view.bandMaxNs));
  for (const a of formatAttrFilterParams(view.attrFilters)) p.append("attr", a);
  setMaxFilesParam(p, view.maxFiles);
  return p.toString();
}

/** The scope key an exemplar set belongs to: its duration bounds. */
export function exemplarScopeKey(view: Pick<ViewState, "bandMinNs" | "bandMaxNs">): string {
  return `${view.bandMinNs ?? ""}:${view.bandMaxNs ?? ""}`;
}

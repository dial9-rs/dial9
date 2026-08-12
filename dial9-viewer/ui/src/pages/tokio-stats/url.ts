// The page's URL contract: pure functions over the query string so the
// round-trip (parse -> sync) is Node-testable.

import {
  EMPTY_SOURCE_SCOPE,
  readPlainSourceScope,
  writeUrlParams,
} from "../../lib/trace/index.js";
import type { SourceScope } from "../../lib/trace/index.js";

/** Scope params, read once from the URL and never edited in-page. */
export interface ScopeParams {
  source: SourceScope;
  prefix: string | null;
  service: string | null;
  /** Repeatable `host` param, in order (getAll semantics). */
  host: string[];
}

/** One period's bounds as epoch-ns strings (null = unset bound). */
export interface PeriodBounds {
  startNs: string | null;
  endNs: string | null;
}

/** Pull the scope dimensions out of a query params bag (page scope, or a diff side). */
export function scopeFromParams(
  params: URLSearchParams,
  fallback: SourceScope = EMPTY_SOURCE_SCOPE,
): ScopeParams {
  return {
    source: readPlainSourceScope(params, fallback),
    prefix: params.get("prefix"),
    service: params.get("service"),
    host: params.getAll("host"),
  };
}

/** Read the page scope params once. Values are preserved verbatim by sync. */
export function readScope(
  params: URLSearchParams,
  fallback: SourceScope = EMPTY_SOURCE_SCOPE,
): ScopeParams {
  return scopeFromParams(params, fallback);
}

/**
 * Restore the comparison periods from the URL. Multi-period via
 * `p{i}_start_ns`/`p{i}_end_ns` for i = 1..10 (either bound present creates the
 * period); falls back to a single `start_ns`/`end_ns` period; both absent -> one
 * blank period. Mirrors addPeriod's `|| null` normalization.
 */
export function parseInitialPeriods(params: URLSearchParams): PeriodBounds[] {
  const out: PeriodBounds[] = [];
  let found = false;
  for (let i = 1; i <= 10; i++) {
    const s = params.get(`p${i}_start_ns`);
    const e = params.get(`p${i}_end_ns`);
    if (s || e) {
      out.push({ startNs: s || null, endNs: e || null });
      found = true;
    }
  }
  if (!found) {
    out.push({
      startNs: params.get("start_ns") || null,
      endNs: params.get("end_ns") || null,
    });
  }
  return out;
}

/**
 * Rebuild the query string from scratch: keep bucket/prefix/service/host* from
 * the ORIGINAL scope, write `p{i+1}_start_ns`/`_end_ns` per period (omitting
 * unset bounds), and DROP everything else (start_ns/end_ns become p1_*, and
 * unknown params including `ui=` are stripped - which is why ui-switch.js has
 * pinWouldBounce). Returns the query WITHOUT the leading "?".
 */
export function buildSyncQuery(
  scope: ScopeParams,
  periods: readonly PeriodBounds[],
): string {
  const u = new URLSearchParams();
  // Address-bar link: bucket + aws_region + aws_role_arn, so a fresh tab opened
  // from it restores the identity at boot. The role rides only here, never on
  // the /api/tokio-stats request URL (writeShareableParams vs the request built
  // in main.ts, which omits the role).
  writeUrlParams(u, scope.source);
  if (scope.prefix) u.set("prefix", scope.prefix);
  if (scope.service) u.set("service", scope.service);
  for (const h of scope.host) u.append("host", h);
  periods.forEach((p, i) => {
    if (p.startNs) u.set(`p${i + 1}_start_ns`, p.startNs);
    if (p.endNs) u.set(`p${i + 1}_end_ns`, p.endNs);
  });
  return u.toString();
}

/**
 * Should the page auto-load on open? True when the ORIGINAL URL carried a
 * non-empty `start_ns` or `bucket` - checked against the original query, before
 * the first syncUrl rewrites start_ns into p1_* (truthy check, so `?start_ns=`
 * empty does not trigger it).
 */
export function shouldAutoLoad(params: URLSearchParams): boolean {
  return !!(params.get("start_ns") || params.get("bucket"));
}

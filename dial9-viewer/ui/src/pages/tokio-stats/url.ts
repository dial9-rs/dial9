// The page's URL contract: pure functions over the query string so the
// round-trip (parse -> sync) is Node-testable.

import {
  makeSourceScope,
  writeShareableParams,
} from "../../lib/trace/index.js";
import type { SourceScope } from "../../lib/trace/index.js";

/** Scope params, read once from the URL and never edited in-page. */
export interface ScopeParams {
  bucket: string | null;
  /** AWS region the bucket lives in; carried on the request URL + shared link. */
  region: string | null;
  /**
   * Assume-role ARN to read the bucket as. Carried on the SHARED link only —
   * restored into the creds store at boot and sent as a HEADER thereafter. The
   * /api/tokio-stats REQUEST URL never carries it (a role on both header and
   * query is the server's ConflictingCredentials 400).
   */
  roleArn: string | null;
  prefix: string | null;
  service: string | null;
  /** Repeatable `host` param, in order (getAll semantics). */
  host: string[];
}

/** This scope's source+credential identity as a SourceScope. */
export function sourceScope(scope: ScopeParams): SourceScope {
  return makeSourceScope(scope.bucket, scope.region, scope.roleArn);
}

/** One period's bounds as epoch-ns strings (null = unset bound). */
export interface PeriodBounds {
  startNs: string | null;
  endNs: string | null;
}

/** Pull the scope dimensions out of a query params bag (page scope, or a diff side). */
export function scopeFromParams(params: URLSearchParams): ScopeParams {
  return {
    bucket: params.get("bucket"),
    region: params.get("aws_region"),
    roleArn: params.get("aws_role_arn"),
    prefix: params.get("prefix"),
    service: params.get("service"),
    host: params.getAll("host"),
  };
}

/** Read the page scope params once. Values are preserved verbatim by sync. */
export function readScope(params: URLSearchParams): ScopeParams {
  return scopeFromParams(params);
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
  writeShareableParams(u, sourceScope(scope));
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

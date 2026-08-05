// The source + credential identity a shareable dial9 link carries: which
// bucket to read, in which region, and (for the assume-role path) which role to
// assume. Every viewer surface — the S3 browser's deep links, the trace viewer,
// the flamegraph, tokio-stats, and the span explorer — used to re-implement
// "which of these ride in the URL, and how", and they drifted (some carried
// region, some the role, some neither). This module is the single owner of that
// decision so every page agrees.
//
// The one rule worth understanding — and the reason region and the role are NOT
// symmetric — comes straight from the server's credential parser
// (dial9-viewer/src/server/credentials.rs, `parse_cred_inputs`):
//
//   • A role ARN may arrive via the `x-dial9-aws-role-arn` HEADER *or* the
//     `aws_role_arn` query param, but NOT BOTH — both together is a hard
//     `CredError::ConflictingCredentials` (HTTP 400). So the role travels on
//     exactly ONE transport per request. We choose the header: at boot a page
//     folds the incoming role into its credentials store (`applyToCreds`), and
//     thereafter it rides as the header on every `/api/*` request. The role
//     therefore appears in SHAREABLE URLs only (address bar / links that open a
//     fresh tab), NEVER on an `/api/*` request URL — see `writeRequestParams`
//     vs `writeShareableParams`.
//
//   • Region is the opposite: the server tolerates it on both transports (the
//     header wins when both are set), AND the only place an *ambient*-identity
//     (no stored creds) cross-region read learns the bucket's region is the
//     `aws_region` query param — `resolve_with_region`'s `CredSource::Default`
//     arm reads it there and nowhere else. And `Dial9Creds.setRegion` is a
//     no-op when nothing is stored (there is no "region-only" credential). So
//     region CANNOT be header-only: it must ride the request URL too, or an
//     ambient cross-region page never reaches the right endpoint. Region is
//     carried on both request and shareable URLs, and also folded into creds so
//     a role/BYOC read signs the right regional endpoint.

/** The bucket + region + reader-role identity a shareable link carries. */
export interface SourceScope {
  /** S3 bucket; "" when absent (a same-page selection with no explicit bucket). */
  bucket: string;
  /** AWS region the bucket lives in; "" when unknown / the server default. */
  region: string;
  /**
   * Reader-role ARN to assume for this bucket; "" when the identity is not a
   * role (static BYOC keys, or the server's ambient identity). Not a secret —
   * the ARN grants nothing on its own; the server must be separately allowed to
   * assume it — which is why it is safe to carry in a shareable URL.
   */
  roleArn: string;
}

/** The empty identity (no bucket / region / role). */
export const EMPTY_SOURCE_SCOPE: SourceScope = { bucket: "", region: "", roleArn: "" };

/** Build a SourceScope from possibly-absent fields, normalizing to "". */
export function makeSourceScope(
  bucket: string | null | undefined,
  region: string | null | undefined,
  roleArn: string | null | undefined,
): SourceScope {
  return { bucket: bucket || "", region: region || "", roleArn: roleArn || "" };
}

// The two vocabularies. `NAMESPACED` (`s_*`) is the trace viewer's scope-link
// dialect (kept distinct from the viewer's own `host`/`from`/`start` params);
// `PLAIN` is what the server endpoints and the S3-browser landing page read.
// These names mirror trace_scope.js's `P` table and the server's
// credentials::{QUERY_ROLE_ARN, QUERY_REGION} constants exactly, so a scope
// this module writes round-trips through the frozen full-scope codec unchanged.
const NAMESPACED = { bucket: "s_bucket", region: "s_region", roleArn: "s_role_arn" } as const;
const PLAIN = { bucket: "bucket", region: "aws_region", roleArn: "aws_role_arn" } as const;

/** Read the identity from the namespaced `s_*` vocabulary (viewer scope links). */
export function readNamespacedSourceScope(params: URLSearchParams): SourceScope {
  return makeSourceScope(
    params.get(NAMESPACED.bucket),
    params.get(NAMESPACED.region),
    params.get(NAMESPACED.roleArn),
  );
}

/** Read the identity from the plain vocabulary (aggregate pages / landing page). */
export function readPlainSourceScope(params: URLSearchParams): SourceScope {
  return makeSourceScope(
    params.get(PLAIN.bucket),
    params.get(PLAIN.region),
    params.get(PLAIN.roleArn),
  );
}

/**
 * Write the identity for an `/api/*` REQUEST URL: `bucket` + `aws_region`, and
 * deliberately NOT `aws_role_arn`. The role rides as the header (via
 * `applyToCreds`); emitting it here too would be a role in both header and
 * query — the server's `ConflictingCredentials` 400. This is the single-
 * transport rule the whole module exists to enforce. Region is safe on the
 * request URL (header wins if both are set) and is required for the ambient
 * cross-region read, so it stays.
 */
export function writeRequestParams(params: URLSearchParams, scope: SourceScope): void {
  if (scope.bucket) params.set(PLAIN.bucket, scope.bucket);
  if (scope.region) params.set(PLAIN.region, scope.region);
  // No roleArn: header-only. See the module header + writeShareableParams.
}

/**
 * Write the identity for a SHAREABLE link that opens a fresh tab on an
 * aggregate page (address-bar sync, or a "open in flamegraph" link): `bucket` +
 * `aws_region` + `aws_role_arn`. The role is safe here because the opened page
 * reads it once at boot, folds it into its creds store (`applyToCreds`), and
 * from then on only ever sends it as the header — so no single `/api/*` request
 * ever carries the role on two transports.
 */
export function writeShareableParams(params: URLSearchParams, scope: SourceScope): void {
  if (scope.bucket) params.set(PLAIN.bucket, scope.bucket);
  if (scope.region) params.set(PLAIN.region, scope.region);
  if (scope.roleArn) params.set(PLAIN.roleArn, scope.roleArn);
}

/**
 * Write the identity in the namespaced `s_*` vocabulary (the viewer's scope
 * links). Like `writeShareableParams` this is a shareable link, so the role
 * (`s_role_arn`) is carried; the viewer restores it via `applyToCreds` at boot
 * and never re-emits it on its own `/api/*` requests. (The S3 browser builds
 * full viewer scope links through trace_scope.js, which writes these same
 * names; this exists for pages/tests that carry only the identity subset.)
 */
export function writeNamespacedParams(params: URLSearchParams, scope: SourceScope): void {
  if (scope.bucket) params.set(NAMESPACED.bucket, scope.bucket);
  if (scope.region) params.set(NAMESPACED.region, scope.region);
  if (scope.roleArn) params.set(NAMESPACED.roleArn, scope.roleArn);
}

/**
 * The minimal credentials-store surface `applyToCreds` needs. Structurally
 * satisfied by `Dial9Creds` and by the viewer scope-boot test double.
 */
export interface SourceScopeCredentials {
  /** The active credential (or null if none is stored). */
  get(): { region?: string | undefined } | null;
  /** Store an assume-role ARN as the active credential (optionally its region). */
  setRoleArn(roleArn: string, opts?: { region?: string }): unknown;
  /** Patch the region onto whatever credential is stored; no-op if none is. */
  setRegion(region: string): unknown;
}

/**
 * Fold the incoming identity into the credentials store at boot, so the role
 * (and region) travel as HEADERS on subsequent `/api/*` requests rather than as
 * query params. This is what lets a shared link opened in a FRESH session (no
 * stored creds) read the bucket at all.
 *
 * The asymmetry, restated where it bites:
 *  - A role always CAN be stored (it becomes the active credential), so a
 *    present role is folded in unconditionally, carrying its region along so the
 *    assumed-role client signs the right regional endpoint.
 *  - Region alone CANNOT be stored — `setRegion` is a no-op on an empty store
 *    (there is no region-only credential). So we patch the region only onto an
 *    already-stored credential (a role we just set, or BYOC keys). When nothing
 *    is stored and there is no role, the region cannot be a header; it rides the
 *    request URL instead (see `writeRequestParams`), which is exactly how an
 *    ambient cross-region read reaches the server.
 */
export function applyToCreds(scope: SourceScope, creds: SourceScopeCredentials): void {
  if (scope.roleArn) {
    creds.setRoleArn(scope.roleArn, scope.region ? { region: scope.region } : undefined);
    return;
  }
  if (scope.region) {
    const stored = creds.get();
    if (stored !== null && stored.region !== scope.region) {
      creds.setRegion(scope.region);
    }
  }
}

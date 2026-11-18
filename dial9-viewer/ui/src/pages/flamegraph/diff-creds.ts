// Side-B credentials for the differential flamegraph: header encoding, the
// /api/credentials/check request that validates them against B's scope, and
// the headers B's reads carry.

/** In-memory credentials for side B (never stored, never in the URL). */
export interface SideBCreds {
  accessKeyId: string;
  secretAccessKey: string;
  sessionToken?: string;
  region?: string;
}

const REGION_HEADER = "x-dial9-aws-region";

/** Build x-dial9-aws-* headers from an explicit credential set. */
function credsToHeaders(c: SideBCreds): Record<string, string> {
  const h: Record<string, string> = {
    "x-dial9-aws-access-key-id": c.accessKeyId,
    "x-dial9-aws-secret-access-key": c.secretAccessKey,
  };
  if (c.sessionToken) h["x-dial9-aws-session-token"] = c.sessionToken;
  if (c.region) h[REGION_HEADER] = c.region;
  return h;
}

/**
 * The credential check for side B's scope: B's bucket, scoped to B's prefix
 * when it has one, signed first in B's linked region unless the pasted
 * credentials name their own.
 */
export function sideBCheckRequest(
  scope: URLSearchParams,
  creds: SideBCreds,
): { url: string; headers: Record<string, string> } {
  const query = new URLSearchParams();
  const bucket = scope.get("bucket");
  if (bucket) query.set("bucket", bucket);
  const prefix = scope.get("prefix")?.trim();
  if (prefix) query.set("prefix", prefix);
  const qs = query.toString();
  const region = scope.get("aws_region")?.trim();
  return {
    url: "/api/credentials/check" + (qs ? "?" + qs : ""),
    headers: credsToHeaders(creds.region || !region ? creds : { ...creds, region }),
  };
}

/**
 * Headers for side B's reads: the pasted B credentials once supplied,
 * otherwise this tab's credentials pinned to B's linked region. The region
 * header outranks the `aws_region` query parameter, so inheriting A's would
 * send a cross-region B to A's endpoint and never yield the auth failure that
 * prompts for B's credentials.
 */
export function sideBHeaders(
  scope: URLSearchParams,
  creds: SideBCreds | null,
  tabHeaders: Record<string, string>,
): Record<string, string> {
  if (creds !== null) return credsToHeaders(creds);
  const region = scope.get("aws_region")?.trim();
  return region ? { ...tabHeaders, [REGION_HEADER]: region } : tabHeaders;
}

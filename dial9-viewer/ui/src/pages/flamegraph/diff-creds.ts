// Side-B credentials for the differential flamegraph: header encoding and the
// /api/credentials/check request that validates them against B's scope.

/** In-memory credentials for side B (never stored, never in the URL). */
export interface SideBCreds {
  accessKeyId: string;
  secretAccessKey: string;
  sessionToken?: string;
  region?: string;
}

/** Build x-dial9-aws-* headers from an explicit credential set. */
export function credsToHeaders(c: SideBCreds): Record<string, string> {
  const h: Record<string, string> = {
    "x-dial9-aws-access-key-id": c.accessKeyId,
    "x-dial9-aws-secret-access-key": c.secretAccessKey,
  };
  if (c.sessionToken) h["x-dial9-aws-session-token"] = c.sessionToken;
  if (c.region) h["x-dial9-aws-region"] = c.region;
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

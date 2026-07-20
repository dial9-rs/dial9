// Credentialed fetch for the browser page. Wraps fetch to attach the
// bring-your-own-credentials headers (if any are stored) to every /api/*
// request. No-op when no creds are stored.

export function apiFetch(url: string, opts: RequestInit = {}): Promise<Response> {
  const credHeaders = window.Dial9Creds ? window.Dial9Creds.headers() : {};
  return fetch(url, {
    ...opts,
    headers: { ...(opts.headers ?? {}), ...credHeaders },
  });
}

/** The first few keys in a bucket over the last day, for the empty-result hint
 * ("nothing matched -- here is what the bucket actually looks like"). Empty on
 * any failure: this is a diagnostic aid, never the page's answer. */
export async function sampleBucketKeys(bucket: string, limit = 5): Promise<string[]> {
  const nowSec = Math.floor(Date.now() / 1000);
  const resp = await apiFetch(
    `/api/browse?bucket=${encodeURIComponent(bucket)}&from=${nowSec - 86400}&to=${nowSec}`,
  );
  if (!resp.ok) return [];
  const objects = ((await resp.json()) as BrowseResponse).objects ?? [];
  return objects.slice(0, limit).map((o) => o.key);
}

/** GET /api/config response fields the page reads. */
export interface ApiConfig {
  default_bucket?: string | undefined;
  default_prefix?: string | undefined;
  aggregation_enabled?: boolean | undefined;
  supports_byo_credentials?: boolean | undefined;
  /** Bucket-picker filter substring; may be absent on servers predating
   * the field - the client falls back to "dial9". */
  bucket_filter?: string | undefined;
}

/** GET /api/browse response fields the page reads. */
export interface BrowseResponse {
  objects?:
    | { key: string; size: number; last_modified?: string | undefined }[]
    | undefined;
  truncated?: boolean | undefined;
}

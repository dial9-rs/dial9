// Credentialed fetch for the browser page. Wraps fetch to attach the
// bring-your-own-credentials headers (if any are stored) to every /api/*
// request. No-op when no creds are stored.

import { buildBrowseUrl } from "./browse-query.js";

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
export async function sampleBucketKeys(
  bucket: string,
  options: { limit?: number; service?: string | undefined } = {},
): Promise<string[]> {
  const nowSec = Math.floor(Date.now() / 1000);
  const resp = await apiFetch(
    buildBrowseUrl({
      bucket,
      from: nowSec - 86400,
      to: nowSec,
      service: options.service,
    }),
  );
  if (!resp.ok) return [];
  const objects = ((await resp.json()) as BrowseResponse).objects ?? [];
  return objects.slice(0, options.limit ?? 5).map((o) => o.key);
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

/** GET /api/services response fields the page reads. */
export interface ServicesResponse {
  services: string[];
  /** Additive metadata; omitted by servers predating host-count discovery. */
  service_metadata?:
    | { service: string; host_count: number }[]
    | undefined;
  truncated?: boolean | undefined;
}

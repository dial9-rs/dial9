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

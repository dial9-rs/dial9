// Credentialed fetch for the browser page (T14; features/01 I5). Wrapper
// around fetch that attaches the bring-your-own-credentials headers (if any
// are stored) to every /api/* request. No-op when no creds are stored.
// Ported verbatim from the legacy `apiFetch`.

export function apiFetch(url: string, opts: RequestInit = {}): Promise<Response> {
  const credHeaders = window.Dial9Creds ? window.Dial9Creds.headers() : {};
  return fetch(url, {
    ...opts,
    headers: { ...(opts.headers ?? {}), ...credHeaders },
  });
}

/** GET /api/config response fields the page reads (features/01 A5). */
export interface ApiConfig {
  default_bucket?: string | undefined;
  default_prefix?: string | undefined;
  aggregation_enabled?: boolean | undefined;
  supports_byo_credentials?: boolean | undefined;
}

/** GET /api/browse response fields the page reads (#582; features/01 F2). */
export interface BrowseResponse {
  objects?:
    | { key: string; size: number; last_modified?: string | undefined }[]
    | undefined;
  truncated?: boolean | undefined;
}

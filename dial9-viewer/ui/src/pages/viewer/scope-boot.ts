import { Dial9Creds } from "../../lib/trace/creds.js";
import { Dial9Session } from "../../lib/trace/session.js";
import {
  hasScope,
  readScope,
  resolveScope,
  type TraceScope,
} from "../../lib/trace/trace_scope.js";
import { initialUrlLabel } from "./load-controller.js";

export interface ScopeLoadTarget {
  scopeLoading(label: string): () => boolean;
  loadUrls(urls: readonly string[], label: string): void;
  scopeFailed(): void;
}

export interface ScopeBootCredentials {
  get(): { region?: string | undefined } | null;
  setRegion(region: string): unknown;
  has(): boolean;
  headers(): Record<string, string>;
}

export interface ScopeBootOptions {
  search: string;
  hasInlineUrls: boolean;
  loadChrome: ScopeLoadTarget;
  onError(message: string): void;
  /** Test seam for the credentialed `/api/browse` request. */
  fetchJson?: ((url: string) => Promise<unknown>) | undefined;
  /** Test seam for the tab-scoped credentials store. */
  creds?: ScopeBootCredentials | undefined;
}

/**
 * Load the compact `s_*` scope in a viewer URL when no inline `trace=` values
 * take precedence. Returns whether the URL contained a valid scope.
 */
export async function bootScopeFromSearch(
  options: ScopeBootOptions,
): Promise<boolean> {
  if (options.hasInlineUrls) return false;

  const params = new URLSearchParams(options.search);
  if (!hasScope(params)) return false;
  const scope = readScope(params);
  if (scope === null) return false;

  const creds = options.creds ?? Dial9Creds;
  const fetchJson =
    options.fetchJson ??
    ((url: string) => fetchJsonWithCreds(url, creds));
  await loadFromScope(
    options.loadChrome,
    scope,
    options.onError,
    fetchJson,
    creds,
  );
  return true;
}

/**
 * Resolve a boot `s_*` scope to its trace-component URLs and load them. The S3
 * browser emits a compact scope (bucket/prefix/service/host-set + window) for
 * large selections instead of one `?trace=` per file, so the viewer must
 * re-list the matching files via `/api/browse` before it has anything to load
 * (mirrors the legacy viewer's loadTraceFromScope). Credentialed like the
 * browser page: the scope's pinned region is folded into Dial9Creds so every
 * request carries the region header (a cross-region bucket lists empty
 * otherwise), and `/api/browse` is fetched with the BYO-credentials headers.
 */
async function loadFromScope(
  loadChrome: ScopeLoadTarget,
  scope: TraceScope,
  onError: (message: string) => void,
  fetchJson: (url: string) => Promise<unknown>,
  creds: ScopeBootCredentials,
): Promise<void> {
  // Fold the scope's pinned region into the creds store so /api/browse and the
  // subsequent /api/object fetches sign for the bucket's actual region.
  if (scope.region) {
    const stored = creds.get();
    if (stored !== null && stored.region !== scope.region) {
      creds.setRegion(scope.region);
    }
  }

  const isCurrent = loadChrome.scopeLoading("Loading trace selection…");
  try {
    const urls = await resolveScope(scope, fetchJson);
    if (!isCurrent()) return;
    if (urls.length === 0) {
      onError(
        "No traces found for this selection's time range and hosts. They may " +
          "have expired (S3 lifecycle).",
      );
      loadChrome.scopeFailed();
      return;
    }
    loadChrome.loadUrls(urls, initialUrlLabel(urls.length));
  } catch (err) {
    if (!isCurrent()) return;
    const raw = err instanceof Error ? err.message : String(err);
    if (/HTTP 401/.test(raw) && !creds.has()) {
      onError(
        "This trace requires AWS credentials. Open it from the dial9 home " +
          "page after applying your credentials, or this tab won't have them.",
      );
    } else {
      onError("Error resolving trace selection: " + raw);
    }
    loadChrome.scopeFailed();
  }
}

/**
 * Credentialed JSON fetch for scope resolution: attaches the BYO-credentials
 * headers (if any) the same way the browser page's apiFetch does, so
 * `/api/browse` runs under the user's creds (BYOC) or the task's ambient
 * identity (no creds). Throws with the response body on a non-2xx status so the
 * caller can distinguish a 401 (missing creds) from other errors.
 */
async function fetchJsonWithCreds(
  url: string,
  creds: ScopeBootCredentials,
): Promise<unknown> {
  const resp = await Dial9Session.fetch(url, { headers: creds.headers() });
  if (!resp.ok) {
    const body = await resp.text().catch(() => "");
    throw new Error(`HTTP ${resp.status}${body ? ": " + body : ""}`);
  }
  return resp.json();
}

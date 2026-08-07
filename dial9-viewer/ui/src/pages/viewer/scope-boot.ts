import { Dial9Creds } from "../../lib/trace/creds.js";
import { Dial9Session } from "../../lib/trace/session.js";
import {
  hasScope,
  readScope,
  resolveScope,
  type TraceScope,
} from "../../lib/trace/trace_scope.js";
import {
  applyToCreds,
  readNamespacedSourceScope,
  sourceScopeFromStored,
  type SourceScope,
} from "../../lib/trace/source-scope.js";
import { initialUrlLabel } from "./load-controller.js";
import type { ReparseRange } from "../../lib/trace/index.js";

export interface ScopeLoadTarget {
  scopeLoading(label: string): () => boolean;
  loadUrls(urls: readonly string[], label: string, range?: ReparseRange): void;
  scopeFailed(): void;
}

export interface ScopeBootCredentials {
  get(): import("../../lib/trace/source-scope.js").StoredSourceCredentials;
  setAmbient(): unknown;
  setLiteralMode(): unknown;
  setRegion(region: string): unknown;
  setRoleArn(roleArn: string, opts?: { region?: string }): unknown;
  has(): boolean;
  headers(): Record<string, string>;
}

export interface ScopeBootOptions {
  search: string;
  hasInlineUrls: boolean;
  loadChrome: ScopeLoadTarget;
  onError(message: string): void;
  dataRange?: ReparseRange;
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
  const source = readNamespacedSourceScope(
    params,
    sourceScopeFromStored("", creds.get()),
  );
  const fetchJson =
    options.fetchJson ??
    ((url: string) => fetchJsonWithCreds(url, creds));
  await loadFromScope(
    options.loadChrome,
    scope,
    source,
    options.onError,
    fetchJson,
    creds,
    options.dataRange,
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
  source: SourceScope,
  onError: (message: string) => void,
  fetchJson: (url: string) => Promise<unknown>,
  creds: ScopeBootCredentials,
  dataRange?: ReparseRange,
): Promise<void> {
  // Restore the scope's reader-role ARN (and region) into the creds store so
  // this tab has an identity to read the bucket with. Without this, a link
  // opened in a fresh session (no stored creds) carries a bucket+region but no
  // role, and every /api/browse 401s — the exact "open it from the home page"
  // failure. The role is folded in as a HEADER (via Dial9Creds); resolveScope's
  // /api/browse request never re-emits it as a query param, so the two-transport
  // ConflictingCredentials 400 can't happen. Region rides along so the assumed-
  // role client signs the right regional endpoint. See lib/trace/source-scope.ts
  // for why region and the role are not symmetric.
  applyToCreds(source, creds);

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
    loadChrome.loadUrls(urls, initialUrlLabel(urls.length), dataRange);
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

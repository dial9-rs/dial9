// Type declarations for `trace_scope.js` - the compact, stateless *scope* that
// replaces one `trace=/api/object?…` component per file in the viewer /
// flamegraph / tokio-stats navigation URL. A scope is bounded by host count,
// not file count, so a large heatmap selection stays under CloudFront's
// 8192-byte request-URI cap and re-resolves in any browser. See
// src/types/decode.d.ts for the declaration-form rationale.
//
// CommonJS-guarded and bundled through the typed lib/trace boundary
// (src/lib/trace/trace_scope.ts). The landing page shares this exact codec with
// viewer.html / flamegraph.html, so every entry maps a selection to the same
// scope.

declare module "*/trace_scope.js" {
  /**
   * A heatmap/table selection as a compact scope: the bucket (+ its region),
   * key prefix, a single service (empty when the box spans several), the
   * distinct host set, and the inclusive epoch-second window.
   */
  export interface TraceScope {
    bucket: string;
    /** AWS region the bucket lives in ("" when unknown/default). */
    region: string;
    /**
     * Reader-role ARN to assume for this bucket ("" when the identity isn't a
     * role). Carried so a link opened in a fresh session still has an identity;
     * not a secret (the server must be separately allowed to assume it).
     */
    roleArn: string;
    /** Explicit frontend credential mode; empty only for legacy scopes. */
    credentialMode: "ambient" | "literal" | "role" | "";
    prefix: string;
    /** Single service; "" when the selection spans more than one. */
    service: string;
    hosts: string[];
    /** Inclusive window start, epoch seconds. */
    from: number;
    /** Inclusive window end, epoch seconds. */
    to: number;
  }

  /** One parsed S3 key (positional, layout-tolerant; no layout discriminant). */
  export interface ParsedTraceScopeKey {
    service: string;
    host: string;
    bootId: string;
    epoch: number;
    segIndex: string;
  }

  export interface EncodeScopeOptions {
    /** URI-safe byte ceiling for the encoded query (default 7000). */
    limit?: number | undefined;
  }

  /**
   * An encoded scope query. `hostsDropped` is true when the host set was too
   * large to name inline and the scope degraded to "all hosts in the window",
   * so the caller can warn the result is broader than the literal selection.
   */
  export interface EncodedScope {
    query: string;
    hostsDropped: boolean;
  }

  /** Parse an S3 trace key into its service/host/boot/epoch metadata. */
  export function parseKey(key: string): ParsedTraceScopeKey;

  /** Everything before the recognized layout root; "" when it starts at root. */
  export function extractPrefix(key: string): string;

  /** One `/api/object?bucket&key` URL per key (raw, still-gzipped bytes). */
  export function objectTraceUrls(bucket: string, keys: readonly string[]): string[];

  /**
   * Derive a scope from a selection's keys and optional `[t0, t1]` window
   * (epoch seconds). When no window is supplied it is derived from the keys'
   * epochs; returns null when neither a window nor any parseable epoch exists
   * (an unrecognized key layout), so callers never build an Infinity window.
   */
  export function scopeFromKeys(
    source: {
      bucket: string;
      region: string;
      credentials:
        | { kind: "ambient" }
        | { kind: "literal"; accessKeyId: string; secretAccessKey: string; sessionToken?: string | undefined }
        | { kind: "role"; roleArn: string };
    },
    keys: readonly string[],
    t0: number | null,
    t1: number | null,
  ): TraceScope | null;

  /** Backward-compatible positional form. */
  export function scopeFromKeys(
    bucket: string,
    keys: readonly string[],
    t0: number | null,
    t1: number | null,
    region?: string,
    roleArn?: string,
    credentialMode?: "ambient" | "literal" | "role",
  ): TraceScope | null;

  /**
   * Write the scope onto a copy of `baseParams` (unmutated) using the
   * namespaced `s_*` params, returning a URI-safe query + the host-drop flag.
   */
  export function encodeScope(
    baseParams: URLSearchParams | null | undefined,
    scope: TraceScope,
    opts?: EncodeScopeOptions
  ): EncodedScope;

  /**
   * Write the scope onto a copy of `baseParams` (unmutated) using the
   * un-namespaced aggregation vocabulary (`bucket`/`prefix`/`service`/
   * repeatable `host`/`aws_region`, window as `start_ns`/`end_ns` in
   * NANOSECONDS) the /api/flamegraph and /api/tokio-stats endpoints read.
   */
  export function encodeAggregationParams(
    baseParams: URLSearchParams | null | undefined,
    scope: TraceScope,
    opts?: EncodeScopeOptions
  ): EncodedScope;

  /** Read a scope back from URL params, or null when no time window is present. */
  export function readScope(params: URLSearchParams): TraceScope | null;

  /** True when the params carry a scope (at least the from/to window). */
  export function hasScope(params: URLSearchParams): boolean;

  /**
   * Resolve a scope to the `/api/object` URLs its files map to, re-listing the
   * window via `/api/browse` (`fetchJson` is an injected credentialed fetch).
   */
  export function resolveScope(
    scope: TraceScope,
    fetchJson: (url: string) => Promise<unknown>
  ): Promise<string[]>;
}

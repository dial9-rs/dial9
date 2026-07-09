// Hand-written type declarations for the frozen-core file `creds.js`
// (bring-your-own-credentials store). See src/types/decode.d.ts for the
// declaration-form rationale. Verified against creds.js and call sites in
// index.html (apply path: parse/set/get/has/clear/listBuckets) and
// flamegraph.html / tokio_stats.html (headers()).
//
// Note: creds.js also publishes `window.Dial9Creds` as the stable
// userscript contract; that global is intentionally NOT declared here --
// typed src/ code should import the module instead.

declare module "*/creds.js" {
  /** Credentials as persisted in storage (trimmed; empty optionals omitted). */
  export interface StoredCredentials {
    accessKeyId: string;
    secretAccessKey: string;
    sessionToken?: string;
    region?: string;
  }

  export interface SetCredentialsInput {
    accessKeyId: string;
    secretAccessKey: string;
    sessionToken?: string;
    region?: string;
    /**
     * Validate against /api/credentials/check and store the resolved
     * region. Also implied when no region is given (browser only).
     */
    autoDetectRegion?: boolean;
    /** Bucket to validate against; defaults to the server default. */
    bucket?: string;
  }

  /** Result of the /api/credentials/check round trip (or the skip path). */
  export interface CredentialCheckResult {
    ok: boolean;
    /** Resolved region; null on failure, undefined when the check was skipped. */
    region?: string | null;
    error: string | null;
  }

  /** Injectable storage backend (test seam; sessionStorage-compatible). */
  export interface CredentialStorage {
    getItem(key: string): string | null;
    setItem(key: string, value: string): void;
    removeItem(key: string): void;
  }

  export interface Dial9CredsApi {
    /** Stored credentials, or null if none are set (or unparseable). */
    get(): StoredCredentials | null;
    /** True if a usable credential set (key id + secret) is stored. */
    has(): boolean;
    /**
     * Store credentials, optionally validating and resolving the region.
     * Never clears stored creds on a failed bucket check. Rejects when
     * accessKeyId/secretAccessKey are missing.
     */
    set(creds: SetCredentialsInput): Promise<CredentialCheckResult>;
    /**
     * Parse credentials from a pasted blob (STS AssumeRole JSON or a flat
     * object; tolerates snake_case/SCREAMING_CASE keys). Throws when the
     * required fields can't be found.
     */
    parse(text: string): {
      accessKeyId: string;
      secretAccessKey: string;
      sessionToken?: string;
      region?: string;
    };
    /**
     * Validate stored credentials and detect the bucket's region via
     * POST /api/credentials/check (browser only).
     */
    check(bucket?: string): Promise<CredentialCheckResult>;
    /**
     * Buckets visible to the stored credentials (GET /api/buckets).
     * Throws on HTTP error with the server's message.
     */
    listBuckets(): Promise<string[]>;
    /** Clear stored credentials and notify listeners. */
    clear(): void;
    /**
     * x-dial9-aws-* request headers from the stored credentials; empty
     * object when nothing is stored (safe to spread unconditionally).
     */
    headers(): Record<string, string>;
    /** Test seam: inject a fake storage backend. */
    _setStorage(storage: CredentialStorage): void;
  }

  export const Dial9Creds: Dial9CredsApi;
}

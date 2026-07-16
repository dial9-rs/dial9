// Type declarations for the frozen-core file `creds.js`
// (bring-your-own-credentials store). See src/types/decode.d.ts for the
// declaration-form rationale.
//
// Note: creds.js also publishes `window.Dial9Creds` as the stable
// userscript contract; that global is intentionally NOT declared here --
// typed src/ code should import the module instead.
//
// The store carries exactly one of two credential transports per request,
// mirroring the server's `CredSource` union (src/server/credentials.rs):
// static bring-your-own keys, OR an assume-role ARN. `get()` normalizes
// storage to a discriminated union so consumers branch on `kind` instead of
// re-deriving which transport applies, and the "both at once" pair is
// unrepresentable.

declare module "*/creds.js" {
  /** Static bring-your-own key pair (the `x-dial9-aws-access-key-id` path). */
  export interface StaticCredentials {
    kind: "static";
    accessKeyId: string;
    secretAccessKey: string;
    sessionToken?: string | undefined;
    region?: string | undefined;
  }

  /** An assume-role ARN the server assumes with its own identity (the linkable
   * `?aws_role_arn=` path). Never coexists with static keys. */
  export interface RoleCredentials {
    kind: "role";
    roleArn: string;
    region?: string | undefined;
  }

  /**
   * The active credential as `get()` returns it: exactly one transport,
   * discriminated by `kind`. Named `StoredCredentials` for continuity (it is
   * what storage resolves to). Narrow on `kind` before reading a transport's
   * fields.
   */
  export type StoredCredentials = StaticCredentials | RoleCredentials;

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
    /** The active credential, or null if none is usable (or unparseable). */
    get(): StoredCredentials | null;
    /** True if a usable credential set (static keys or a role ARN) is stored. */
    has(): boolean;
    /**
     * Store static credentials, optionally validating and resolving the
     * region. Never clears stored creds on a failed bucket check. Rejects when
     * accessKeyId/secretAccessKey are missing. Replaces any stored role ARN so
     * the two transports never coexist.
     */
    set(creds: SetCredentialsInput): Promise<CredentialCheckResult>;
    /**
     * Store an assume-role ARN as the active credential (the linkable
     * `?aws_role_arn=` path), clearing any static keys. Throws on a malformed
     * ARN (see `isValidRoleArn`) so a bad link fails loudly. Returns the
     * stored role credential.
     */
    setRoleArn(roleArn: string, opts?: { region?: string | undefined }): RoleCredentials;
    /**
     * Patch the region onto whatever credential is stored, preserving its
     * kind (the one region-update entry point for both transports). No-op
     * (returns null) when nothing is stored.
     */
    setRegion(region: string): StoredCredentials | null;
    /**
     * Syntactic check that `arn` names a single IAM role, mirroring the
     * server's `is_valid_role_arn`, so a malformed value is rejected up front.
     */
    isValidRoleArn(arn: string): boolean;
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
     * x-dial9-aws-* request headers for the active credential's transport;
     * empty object when nothing is stored (safe to spread unconditionally).
     */
    headers(): Record<string, string>;
    /** Test seam: inject a fake storage backend. */
    _setStorage(storage: CredentialStorage): void;
  }

  export const Dial9Creds: Dial9CredsApi;
}
